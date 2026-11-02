//! Apply the shield-leak patch once to each new WindowServer.
//!
//! Blocks on kqueue NOTE_EXIT for the current WindowServer; after it exits, waits for
//! the replacement, lets it run SETTLE seconds, then runs ws-shield-fix.sh apply.
//! Patched pids are kept in /var/run (cleared at boot).
//!
//! Build: rustc -O --edition 2021 -o ws-shield-watch ws-shield-watch.rs
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::raw::{c_int, c_void};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::thread::sleep;
use std::time::Duration;

const NAME: &str = "WindowServer";
const STATE: &str = "/var/run/ws-shield-fix.pids";
const SETTLE: u64 = 20; // seconds a new WindowServer runs before it is patched
const MAX_PER_BOOT: usize = 5; // patch at most this many WindowServer instances per boot

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
const NOTE_EXIT: u32 = 0x8000_0000;

#[repr(C, align(8))]
struct Tm([u8; 64]);

extern "C" {
    fn kqueue() -> c_int;
    fn kevent(kq: c_int, changes: *const Kevent, nchanges: c_int, events: *mut Kevent, nevents: c_int, timeout: *const c_void) -> c_int;
    fn time(t: *mut i64) -> i64;
    fn localtime_r(t: *const i64, tm: *mut Tm) -> *mut Tm;
    fn strftime(s: *mut u8, max: usize, fmt: *const u8, tm: *const Tm) -> usize;
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
    etime.trim().replace('-', ":").split(':').filter_map(|p| p.parse::<u64>().ok()).fold(0, |s, p| s * 60 + p)
}

fn patched() -> Vec<i32> {
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

fn main() {
    let fix = std::env::current_exe().expect("current_exe").with_file_name("ws-shield-fix.sh");
    let kq = unsafe { kqueue() };
    assert!(kq >= 0, "kqueue failed");
    loop {
        let pid = wait_for_pid();
        let done = patched();
        if !done.contains(&pid) && done.len() < MAX_PER_BOOT {
            sleep(Duration::from_secs(SETTLE.saturating_sub(uptime(pid))));
            if current_pid() == Some(pid) {
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(STATE) {
                    let _ = writeln!(f, "{pid}");
                }
                log(&format!("patching {NAME} pid {pid}"));
                let _ = Command::new(&fix).arg("apply").status();
            }
        }
        log(&format!("watching {NAME} pid {pid}"));
        let change = Kevent { ident: pid as usize, filter: EVFILT_PROC, flags: EV_ADD | EV_ONESHOT, fflags: NOTE_EXIT, data: 0, udata: null_mut() };
        let mut event = Kevent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: null_mut() };
        unsafe { kevent(kq, &change, 1, &mut event, 1, null()) };
        log(&format!("{NAME} pid {pid} exited"));
        sleep(Duration::from_secs(1)); // the replacement takes longer than this to appear
    }
}
