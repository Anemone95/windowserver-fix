//! Apply the shield-leak patch once to each new WindowServer.
//!
//! Blocks on kqueue NOTE_EXIT for the current WindowServer; after it exits, waits for
//! the replacement, lets it run SETTLE seconds, then patches it through Mach APIs.
//! Attempted pids are kept in /var/run (cleared at boot).
//!
//! Build and sign: ./build.sh
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("ws-shield-watch requires macOS on Apple Silicon");
mod shield_patch;

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::raw::{c_int, c_void};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::thread::sleep;
use std::time::Duration;

const NAME: &str = "WindowServer";
const STATE: &str = "/var/run/ws-shield-fix.pids";
const SETTLE: u64 = 20; // seconds a new WindowServer runs before it is patched
const MAX_PER_BOOT: usize = 5; // attempt at most this many WindowServer instances per boot

#[repr(C)]
struct Kevent {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
    udata: *mut c_void,
}
const EVFILT_PROC: i16 = -5;
const EV_ADD: u16 = 0x0001;
const EV_ONESHOT: u16 = 0x0010;
const EV_ERROR: u16 = 0x4000;
const NOTE_EXIT: u32 = 0x8000_0000;

#[repr(C, align(8))]
struct Tm([u8; 64]);

extern "C" {
    fn kqueue() -> c_int;
    fn kevent(kq: c_int, changes: *const Kevent, nchanges: c_int, events: *mut Kevent, nevents: c_int, timeout: *const c_void) -> c_int;
    fn time(t: *mut i64) -> i64;
    fn localtime_r(t: *const i64, tm: *mut Tm) -> *mut Tm;
    fn strftime(s: *mut u8, max: usize, fmt: *const u8, tm: *const Tm) -> usize;
    fn geteuid() -> u32;
}

fn log(msg: &str) {
    let mut buf = [0u8; 32];
    let n = unsafe {
        let t = time(null_mut());
        let mut tm = Tm([0; 64]);
        localtime_r(&t, &mut tm);
        strftime(buf.as_mut_ptr(), buf.len(), b"%Y-%m-%d %H:%M:%S\0".as_ptr(), &tm)
    };
    println!("{} {msg}", std::str::from_utf8(&buf[..n]).unwrap_or("?"));
}

fn output(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd).args(args).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

fn current_pid() -> Option<i32> {
    output("/usr/bin/pgrep", &["-x", NAME]).split_whitespace().next()?.parse().ok()
}

fn uptime(pid: i32) -> u64 {
    let etime = output("/bin/ps", &["-o", "etime=", "-p", &pid.to_string()]);
    parse_elapsed(&etime).unwrap_or(0)
}

fn parse_elapsed(etime: &str) -> Option<u64> {
    let (days, clock) = match etime.trim().split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, etime.trim()),
    };
    let seconds = clock.split(':').try_fold(0, |s, p| p.parse::<u64>().ok().map(|p| s * 60 + p))?;
    Some(days * 86400 + seconds)
}

fn attempted() -> Vec<i32> {
    fs::read_to_string(STATE).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect()
}

fn wait_for_pid() -> i32 {
    loop {
        if let Some(pid) = current_pid() {
            return pid;
        }
        sleep(Duration::from_secs(1));
    }
}

fn wait_for_exit(kq: c_int, pid: i32) -> io::Result<()> {
    let change = Kevent { ident: pid as usize, filter: EVFILT_PROC, flags: EV_ADD | EV_ONESHOT, fflags: NOTE_EXIT, data: 0, udata: null_mut() };
    loop {
        if unsafe { kevent(kq, &change, 1, null_mut(), 0, null()) } >= 0 {
            break;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    loop {
        let mut event = Kevent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: null_mut() };
        let n = unsafe { kevent(kq, null(), 0, &mut event, 1, null()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 1 && event.flags & EV_ERROR != 0 {
            return Err(io::Error::from_raw_os_error(event.data as i32));
        }
        if n == 1 && event.ident == pid as usize && event.filter == EVFILT_PROC && event.fflags & NOTE_EXIT != 0 {
            return Ok(());
        }
    }
}

fn watch() {
    let kq = unsafe { kqueue() };
    assert!(kq >= 0, "kqueue failed");
    loop {
        let pid = wait_for_pid();
        let done = attempted();
        if !done.contains(&pid) && done.len() < MAX_PER_BOOT {
            sleep(Duration::from_secs(SETTLE.saturating_sub(uptime(pid))));
            if current_pid() == Some(pid) {
                let recorded = OpenOptions::new().create(true).append(true).open(STATE)
                    .and_then(|mut f| writeln!(f, "{pid}"));
                match recorded {
                    Ok(()) => {
                        log(&format!("patching {NAME} pid {pid}"));
                        match shield_patch::run(pid, true) {
                            Ok(()) => log(&format!("patch succeeded for pid {pid}")),
                            Err(err) => log(&format!("patch failed for pid {pid}: {err}")),
                        }
                    }
                    Err(err) => log(&format!("could not record attempt for pid {pid}: {err}; skipping patch")),
                }
            }
        } else if !done.contains(&pid) {
            log(&format!("{MAX_PER_BOOT} patch attempts reached this boot; skipping pid {pid}"));
        }
        log(&format!("watching {NAME} pid {pid}"));
        match wait_for_exit(kq, pid) {
            Ok(()) => log(&format!("{NAME} pid {pid} exited")),
            Err(err) => log(&format!("could not watch {NAME} pid {pid}: {err}")),
        }
        sleep(Duration::from_secs(1)); // the replacement takes longer than this to appear
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("watch");
    if !matches!(mode, "watch" | "check" | "apply") || args.len() > 2 || (mode == "watch" && args.len() > 1) {
        return Err("usage: ws-shield-watch [watch|check|apply] [pid]".into());
    }
    if output("/usr/bin/sw_vers", &["-buildVersion"]).trim() != "25F84" {
        return Err("this patch requires macOS build 25F84".into());
    }
    if unsafe { geteuid() } != 0 {
        return Err("run with sudo".into());
    }
    if mode == "watch" {
        watch();
        return Ok(());
    }
    let pid = match args.get(1) {
        Some(pid) => pid.parse::<i32>().ok().filter(|pid| *pid > 1).ok_or("invalid pid")?,
        None => current_pid().ok_or("WindowServer is not running")?,
    };
    shield_patch::run(pid, mode == "apply")
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{FromRawFd, OwnedFd};

    #[test]
    fn elapsed_time_formats() {
        assert_eq!(parse_elapsed(" 00:19\n"), Some(19));
        assert_eq!(parse_elapsed("02:03:04"), Some(7384));
        assert_eq!(parse_elapsed("1-02:03:04"), Some(93784));
        assert_eq!(parse_elapsed("33-00:00:00"), Some(2851200));
        assert_eq!(parse_elapsed(""), None);
        assert_eq!(parse_elapsed("bad:20"), None);
    }

    #[test]
    fn watches_child_exit() {
        let kq = unsafe { kqueue() };
        assert!(kq >= 0);
        let _fd = unsafe { OwnedFd::from_raw_fd(kq) };
        let mut child = Command::new("/bin/sleep").arg("0.2").spawn().unwrap();
        let result = wait_for_exit(kq, child.id() as i32);
        child.wait().unwrap();
        result.unwrap();
    }

    #[test]
    fn reports_kevent_errors() {
        assert!(wait_for_exit(-1, std::process::id() as i32).is_err());
    }
}
