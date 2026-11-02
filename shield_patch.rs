//! Build 25F84, arm64e: validate SkyLight and patch only the target's private code page.
use std::ffi::CStr;
use std::io;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr::null_mut;
use std::time::Instant;

type Result<T> = std::result::Result<T, String>;
const UNSLID_TEXT: u64 = 0x186CD9000;
const CONTEXT_START: u64 = 0x186F94B28;
const UUID: [u8; 16] = [0x6F, 0x19, 0x78, 0xA2, 0x1B, 0x8A, 0x3A, 0x44, 0x90, 0x3D, 0x53, 0x2E, 0xED, 0x71, 0x06, 0xEB];
const CONTEXT: [u32; 13] = [
    0xB4000156, 0xF940C668, 0xB4000148, 0xA903FFE8, 0xA902FFF6,
    0x9100E3E0, 0x9100A3E1, 0xD2800002, 0x97FA2BE3, 0x14000003,
    0xF940C268, 0xB83B691F, 0x9100435A,
];
const PATCHES: [(usize, u32); 2] = [(9, 0xB94002D6), (11, 0xB83B6916)];
const READ: i32 = 1;
const WRITE: i32 = 2;
const EXECUTE: i32 = 4;
const COPY: i32 = 0x10;

extern "C" {
    static mach_task_self_: u32;
    fn task_for_pid(task: u32, pid: c_int, port: *mut u32) -> c_int;
    fn task_info(task: u32, flavor: u32, info: *mut i32, count: *mut u32) -> c_int;
    fn mach_port_deallocate(task: u32, port: u32) -> c_int;
    fn mach_error_string(code: c_int) -> *const c_char;
    fn mach_vm_read_overwrite(task: u32, addr: u64, size: u64, data: u64, out: *mut u64) -> c_int;
    fn mach_vm_write(task: u32, addr: u64, data: usize, size: u32) -> c_int;
    fn mach_vm_protect(task: u32, addr: u64, size: u64, maximum: c_int, protection: c_int) -> c_int;
    fn mach_vm_region_recurse(task: u32, addr: *mut u64, size: *mut u64, depth: *mut u32, info: *mut i32, count: *mut u32) -> c_int;
    fn mach_vm_machine_attribute(task: u32, addr: u64, size: u64, attribute: u32, value: *mut i32) -> c_int;
    fn ptrace(request: c_int, pid: c_int, addr: *mut c_void, data: c_int) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, size: u32) -> c_int;
    fn pthread_sigmask(how: c_int, set: *const u32, previous: *mut u32) -> c_int;
}

fn mach(code: c_int, operation: &str) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        let message = unsafe { CStr::from_ptr(mach_error_string(code)) }.to_string_lossy();
        Err(format!("{operation}: {message} ({code})"))
    }
}

struct Task(u32);

impl Task {
    fn for_pid(pid: i32) -> Result<Self> {
        let mut port = 0;
        mach(unsafe { task_for_pid(mach_task_self_, pid, &mut port) }, "task_for_pid (requires root and suitable SIP policy)")?;
        Ok(Self(port))
    }

    fn read(&self, addr: u64, bytes: &mut [u8]) -> Result<()> {
        let mut size = 0;
        mach(unsafe { mach_vm_read_overwrite(self.0, addr, bytes.len() as u64, bytes.as_mut_ptr() as u64, &mut size) }, "mach_vm_read_overwrite")?;
        if size != bytes.len() as u64 {
            return Err(format!("short read at {addr:#x}: {size}/{} bytes", bytes.len()));
        }
        Ok(())
    }

    fn skylight_base(&self) -> Result<u64> {
        // task_dyld_info is packed to 4-byte alignment: two u64 fields and one i32.
        let mut info = [0i32; 5];
        let mut count = info.len() as u32;
        mach(unsafe { task_info(self.0, 17, info.as_mut_ptr(), &mut count) }, "TASK_DYLD_INFO")?;
        let addr = info[0] as u32 as u64 | ((info[1] as u32 as u64) << 32);
        let size = info[2] as u32 as u64 | ((info[3] as u32 as u64) << 32);
        if count != 5 || info[4] != 1 || size < 160 {
            return Err("unsupported dyld info layout".into());
        }
        // dyld_all_image_infos v12+: sharedCacheSlide is at offset 152 on arm64.
        let mut dyld = [0u8; 160];
        self.read(addr, &mut dyld)?;
        if word(&dyld, 0) < 12 {
            return Err("dyld info predates sharedCacheSlide".into());
        }
        let slide = u64::from_le_bytes(dyld[152..160].try_into().unwrap());
        let base = UNSLID_TEXT.checked_add(slide).ok_or("invalid shared cache slide")?;
        let mut header = [0u8; 32];
        self.read(base, &mut header)?;
        if word(&header, 0) != 0xFEEDFACF || word(&header, 4) != 0x0100000C
            || word(&header, 8) & 0x00FF_FFFF != 2 || word(&header, 12) != 6 {
            return Err("expected an arm64e dylib at SkyLight base".into());
        }
        let size = word(&header, 20) as usize;
        if size > 65536 {
            return Err("invalid Mach-O load command size".into());
        }
        let mut commands = vec![0; size];
        self.read(base + 32, &mut commands)?;
        validate_uuid(&commands, word(&header, 16))?;
        Ok(base)
    }

    fn protection(&self, addr: u64, len: usize) -> Result<i32> {
        let mut start = addr;
        let mut size = 0;
        // Descend shared-cache submaps to get the leaf page's actual protections.
        // vm_region_submap_info_64 is packed to 4-byte alignment (76 bytes).
        let mut info = [0i32; 19];
        let mut count = info.len() as u32;
        let mut depth = 1024;
        mach(unsafe { mach_vm_region_recurse(self.0, &mut start, &mut size, &mut depth, info.as_mut_ptr(), &mut count) }, "mach_vm_region_recurse")?;
        if start > addr || addr - start + len as u64 > size || count < 16 || info[12] != 0 {
            return Err("patch crosses a VM region boundary".into());
        }
        Ok(info[0])
    }

    fn protect(&self, addr: u64, len: usize, protection: i32) -> Result<()> {
        mach(unsafe { mach_vm_protect(self.0, addr, len as u64, 0, protection) }, "mach_vm_protect")
    }

    fn write(&self, addr: u64, bytes: &[u8]) -> Result<()> {
        mach(unsafe { mach_vm_write(self.0, addr, bytes.as_ptr() as usize, bytes.len() as u32) }, "mach_vm_write")?;
        let mut cache = 6; // MATTR_VAL_CACHE_FLUSH, as used by debugserver on arm64.
        mach(unsafe { mach_vm_machine_attribute(self.0, addr, bytes.len() as u64, 1, &mut cache) }, "instruction cache flush")?;
        let mut verify = vec![0; bytes.len()];
        self.read(addr, &mut verify)?;
        if verify != bytes {
            return Err(format!("write verification failed at {addr:#x}"));
        }
        Ok(())
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        unsafe { mach_port_deallocate(mach_task_self_, self.0) };
    }
}

// Defer normal shutdown signals until the traced process has been detached.
// The production watcher is single-threaded; each test uses its own thread mask.
struct SignalMask(u32);

impl SignalMask {
    fn block_shutdown() -> Result<Self> {
        let signals = (1u32 << 0) | (1 << 1) | (1 << 14); // SIGHUP, SIGINT, SIGTERM
        let mut previous = 0;
        let code = unsafe { pthread_sigmask(1, &signals, &mut previous) }; // SIG_BLOCK
        if code != 0 {
            return Err(format!("block shutdown signals: {}", io::Error::from_raw_os_error(code)));
        }
        Ok(Self(previous))
    }
}

impl Drop for SignalMask {
    fn drop(&mut self) {
        let code = unsafe { pthread_sigmask(3, &self.0, null_mut()) }; // SIG_SETMASK
        if code != 0 {
            eprintln!("restore signal mask: {}", io::Error::from_raw_os_error(code));
        }
    }
}

// PT_ATTACH enables CS_DEBUGGED and stops all threads. Plain PT_ATTACH uses
// waitpid on macOS; PT_ATTACHEXC would require a Mach exception server instead.
struct Trace {
    pid: i32,
    _signals: SignalMask,
}

impl Trace {
    fn attach(pid: i32) -> Result<Self> {
        let signals = SignalMask::block_shutdown()?;
        if unsafe { ptrace(10, pid, null_mut(), 0) } != 0 {
            return Err(format!("ptrace attach: {}", io::Error::last_os_error()));
        }
        let trace = Self { pid, _signals: signals };
        loop {
            let mut status = 0;
            let got = unsafe { waitpid(pid, &mut status, 0) };
            if got == pid {
                if status & 0xFF != 0x7F {
                    return Err(format!("pid {pid} exited before patching"));
                }
                return Ok(trace);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(format!("waitpid: {err}"));
            }
        }
    }

    fn detach(&mut self) -> Result<()> {
        if self.pid != 0 {
            if unsafe { ptrace(11, self.pid, 1usize as *mut c_void, 0) } != 0 {
                return Err(format!("ptrace detach: {}", io::Error::last_os_error()));
            }
            self.pid = 0;
        }
        Ok(())
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        if let Err(err) = self.detach() {
            eprintln!("{err}");
        }
    }
}

fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn validate_uuid(commands: &[u8], count: u32) -> Result<()> {
    let mut offset = 0;
    let mut found = false;
    for _ in 0..count {
        if offset + 8 > commands.len() {
            return Err("truncated Mach-O load command".into());
        }
        let command = word(commands, offset);
        let size = word(commands, offset + 4) as usize;
        if size < 8 || size % 8 != 0 || size > commands.len() - offset {
            return Err("invalid Mach-O load command size".into());
        }
        if command == 0x1B { // LC_UUID
            if size != 24 || commands[offset + 8..offset + 24] != UUID {
                return Err("SkyLight UUID mismatch; this patch requires build 25F84".into());
            }
            found = true;
        }
        offset += size;
    }
    if !found || offset != commands.len() {
        return Err("SkyLight UUID missing or malformed load commands".into());
    }
    Ok(())
}

fn validate_context(bytes: &[u8; 52]) -> Result<&'static str> {
    let mut patched = 0;
    for (i, expected) in CONTEXT.iter().enumerate() {
        let have = word(bytes, i * 4);
        if let Some((_, new)) = PATCHES.iter().find(|(index, _)| *index == i) {
            if have == *new {
                patched += 1;
                continue;
            }
        }
        if have != *expected {
            return Err(format!("unexpected instruction at {:#x}: {have:08x}", CONTEXT_START + i as u64 * 4));
        }
    }
    Ok(match patched { 0 => "ORIGINAL", 2 => "PATCHED", _ => "PARTIAL" })
}

// The caller holds every target thread stopped. Save bytes and permissions so
// errors restore the prior code before the trace guard resumes the process.
fn write_code(task: &Task, addr: u64, bytes: &[u8]) -> Result<()> {
    let protection = task.protection(addr, bytes.len())?;
    if protection != (READ | EXECUTE) {
        return Err(format!("expected read/execute code page, got {protection:#x}"));
    }
    let mut original = vec![0; bytes.len()];
    task.read(addr, &mut original)?;
    task.protect(addr, bytes.len(), READ | WRITE | COPY)?;
    let result = task.write(addr, bytes);
    let rollback = if result.is_err() { task.write(addr, &original) } else { Ok(()) };
    let restore = task.protect(addr, bytes.len(), protection);
    if restore.is_err() {
        // Retry restoration before returning control to the caller's detach guard.
        task.protect(addr, bytes.len(), protection)?;
    }
    rollback.map_err(|err| format!("patch failed and rollback failed: {err}"))?;
    result
}

pub fn run(pid: i32, apply: bool) -> Result<()> {
    let started = Instant::now();
    let mut path = [0u8; 4096];
    let n = unsafe { proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if n <= 0 {
        return Err(format!("proc_pidpath: {}", io::Error::last_os_error()));
    }
    let path = CStr::from_bytes_until_nul(&path).map_err(|_| "invalid process path")?.to_bytes();
    if path != b"/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer"
        && path != b"/System/Library/PrivateFrameworks/SkyLight.framework/Versions/A/Resources/WindowServer" {
        return Err(format!("pid {pid} is not the system WindowServer"));
    }
    let task = Task::for_pid(pid)?;
    let base = task.skylight_base()?;
    let site = base + (CONTEXT_START - UNSLID_TEXT);
    let mut bytes = [0u8; 52];
    task.read(site, &mut bytes)?;
    let state = validate_context(&bytes)?;
    println!("pid {pid}  slide {:#x}  site {site:#x}  status: {state}", base - UNSLID_TEXT);
    if !apply || state == "PATCHED" {
        return Ok(());
    }
    let paused = Instant::now();
    let mut trace = Trace::attach(pid)?;
    let result: Result<()> = (|| {
        // Recheck after stopping to cover concurrent check/apply invocations.
        if task.skylight_base()? != base {
            return Err("SkyLight base changed before patching".into());
        }
        task.read(site, &mut bytes)?;
        if validate_context(&bytes)? == "PATCHED" {
            return Ok(());
        }
        for (index, instruction) in PATCHES {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&instruction.to_le_bytes());
        }
        // One write covers both changed words and the unchanged ldr between them.
        write_code(&task, site + 36, &bytes[36..48])?;
        task.read(site, &mut bytes)?;
        if validate_context(&bytes)? != "PATCHED" {
            return Err("patch verification failed".into());
        }
        Ok(())
    })();
    trace.detach()?;
    result?;
    println!("PATCHED; pause {:.3} ms, total {:.3} ms", paused.elapsed().as_secs_f64() * 1000.0, started.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, ChildStdout, Command, Stdio};
    use std::time::Duration;

    fn original_context() -> [u8; 52] {
        let mut bytes = [0; 52];
        for (i, word) in CONTEXT.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn context_states_and_rejection() {
        let mut bytes = original_context();
        assert_eq!(validate_context(&bytes).unwrap(), "ORIGINAL");
        for (n, (i, word)) in PATCHES.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
            assert_eq!(validate_context(&bytes).unwrap(), if n == 0 { "PARTIAL" } else { "PATCHED" });
        }
        for i in 0..13 {
            let mut invalid = bytes;
            invalid[i * 4] ^= 1;
            assert!(validate_context(&invalid).is_err(), "instruction {i}");
        }
    }

    #[test]
    fn uuid_validation() {
        let mut command = vec![0; 24];
        command[..4].copy_from_slice(&0x1Bu32.to_le_bytes());
        command[4..8].copy_from_slice(&24u32.to_le_bytes());
        command[8..24].copy_from_slice(&UUID);
        validate_uuid(&command, 1).unwrap();
        assert!(validate_uuid(&command, 0).is_err());
        assert!(validate_uuid(&command, 2).is_err());
        assert!(validate_uuid(&command[..20], 1).is_err());
        command[8] ^= 1;
        assert!(validate_uuid(&command, 1).is_err());
        command[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert!(validate_uuid(&command, 1).is_err());
    }

    #[test]
    fn restores_shutdown_signal_mask() {
        let mut before = 0;
        assert_eq!(unsafe { pthread_sigmask(1, std::ptr::null(), &mut before) }, 0);
        {
            let _mask = SignalMask::block_shutdown().unwrap();
            let mut during = 0;
            assert_eq!(unsafe { pthread_sigmask(1, std::ptr::null(), &mut during) }, 0);
            assert_eq!(during & 0x4003, 0x4003);
        }
        let mut after = 0;
        assert_eq!(unsafe { pthread_sigmask(1, std::ptr::null(), &mut after) }, 0);
        assert_eq!(before, after);
    }

    #[test]
    fn reads_real_shared_cache_metadata() {
        let task = Task::for_pid(std::process::id() as i32).unwrap();
        let base = task.skylight_base().unwrap();
        let mut bytes = [0; 52];
        task.read(base + CONTEXT_START - UNSLID_TEXT, &mut bytes).unwrap();
        // A patch in WindowServer must leave this process's shared cache original.
        assert_eq!(validate_context(&bytes).unwrap(), "ORIGINAL");
    }

    struct Fixture(Child, BufReader<ChildStdout>);

    impl Fixture {
        fn start(expected: u32, shared_cache: bool) -> (Self, u64) {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "shield_patch::tests::code_fixture", "--ignored", "--nocapture", "--test-threads=1"])
                .env("WS_SHIELD_FIXTURE_EXPECT", expected.to_string())
                .env("WS_SHIELD_FIXTURE_CACHE", if shared_cache { "1" } else { "0" })
                .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
            let stdout = child.stdout.take().unwrap();
            let mut fixture = Self(child, BufReader::new(stdout));
            let mut line = String::new();
            while fixture.1.read_line(&mut line).unwrap() != 0 {
                if let Some((_, address)) = line.split_once("FIXTURE ") {
                    return (fixture, address.trim().parse().unwrap());
                }
                line.clear();
            }
            panic!("fixture failed to start: {:?}", fixture.0.wait());
        }

        fn finish(&mut self) {
            self.0.stdin.take().unwrap().write_all(b"go\n").unwrap();
            for _ in 0..200 {
                if let Some(status) = self.0.try_wait().unwrap() {
                    let mut output = String::new();
                    self.1.read_to_string(&mut output).unwrap();
                    assert!(status.success(), "fixture exit: {status}: {output}");
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("fixture did not resume");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn patches_child_executable_code_and_restores_rx() {
        let (mut fixture, addr) = Fixture::start(42, false);
        let task = Task::for_pid(fixture.0.id() as i32).unwrap();
        let paused = Instant::now();
        let mut trace = Trace::attach(fixture.0.id() as i32).unwrap();
        write_code(&task, addr, &0x52800540u32.to_le_bytes()).unwrap(); // mov w0, #42
        assert_eq!(task.protection(addr, 4).unwrap(), READ | EXECUTE);
        trace.detach().unwrap();
        eprintln!("native child patch pause: {:.3} ms", paused.elapsed().as_secs_f64() * 1000.0);
        fixture.finish();
    }

    #[test]
    fn resumes_child_after_patch_error() {
        let (mut fixture, _) = Fixture::start(7, false);
        let task = Task::for_pid(fixture.0.id() as i32).unwrap();
        {
            let _trace = Trace::attach(fixture.0.id() as i32).unwrap();
            assert!(write_code(&task, 0, &[0; 4]).is_err());
        }
        fixture.finish();
    }

    #[test]
    fn patches_real_skylight_page_only_in_child() {
        let (mut fixture, addr) = Fixture::start(42, true);
        let task = Task::for_pid(fixture.0.id() as i32).unwrap();
        let mut bytes = original_context();
        for (index, instruction) in PATCHES {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&instruction.to_le_bytes());
        }
        let paused = Instant::now();
        let mut trace = Trace::attach(fixture.0.id() as i32).unwrap();
        write_code(&task, addr + 36, &bytes[36..48]).unwrap();
        assert_eq!(task.protection(addr, 52).unwrap(), READ | EXECUTE);
        trace.detach().unwrap();
        eprintln!("native SkyLight child patch pause: {:.3} ms", paused.elapsed().as_secs_f64() * 1000.0);
        let own = Task::for_pid(std::process::id() as i32).unwrap();
        let site = own.skylight_base().unwrap() + CONTEXT_START - UNSLID_TEXT;
        own.read(site, &mut bytes).unwrap();
        assert_eq!(validate_context(&bytes).unwrap(), "ORIGINAL");
        fixture.finish();
    }

    #[test]
    fn rejects_other_processes() {
        assert!(run(std::process::id() as i32, true).unwrap_err().contains("not the system WindowServer"));
    }

    #[test]
    #[ignore = "subprocess fixture for native memory tests"]
    fn code_fixture() {
        extern "C" {
            fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
            fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
            fn sys_icache_invalidate(addr: *mut c_void, len: usize);
            fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        }
        let expected: u32 = std::env::var("WS_SHIELD_FIXTURE_EXPECT").unwrap().parse().unwrap();
        if std::env::var("WS_SHIELD_FIXTURE_CACHE").unwrap() == "1" {
            let handle = unsafe { dlopen(b"/System/Library/PrivateFrameworks/SkyLight.framework/Versions/A/SkyLight\0".as_ptr().cast(), 2) };
            assert!(!handle.is_null());
            let task = Task::for_pid(std::process::id() as i32).unwrap();
            let addr = task.skylight_base().unwrap() + CONTEXT_START - UNSLID_TEXT;
            let mut bytes = [0; 52];
            task.read(addr, &mut bytes).unwrap();
            assert_eq!(validate_context(&bytes).unwrap(), "ORIGINAL");
            println!("FIXTURE {addr}");
            io::stdout().flush().unwrap();
            let mut line = String::new();
            io::stdin().read_line(&mut line).unwrap();
            task.read(addr, &mut bytes).unwrap();
            assert_eq!(validate_context(&bytes).unwrap(), "PATCHED");
            return;
        }
        let addr = unsafe { mmap(null_mut(), 16384, READ | WRITE, 0x1002, -1, 0) }; // MAP_ANON | MAP_PRIVATE
        assert_ne!(addr as isize, -1);
        unsafe {
            (addr as *mut u32).write(0x528000E0); // mov w0, #7
            (addr as *mut u32).add(1).write(0xD65F03C0); // ret
            sys_icache_invalidate(addr, 8);
            assert_eq!(mprotect(addr, 16384, READ | EXECUTE), 0);
        }
        let function: extern "C" fn() -> u32 = unsafe { std::mem::transmute(addr) };
        assert_eq!(function(), 7); // populate the instruction cache before the remote write
        println!("FIXTURE {}", addr as u64);
        io::stdout().flush().unwrap();
        let mut line = String::new();
        io::stdin().read_line(&mut line).unwrap();
        assert_eq!(function(), expected);
    }
}
