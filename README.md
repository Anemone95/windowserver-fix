# WindowServer shield leak fix

English | [简体中文](README.zh-CN.md)

A live memory patch for **macOS 26.5.2 / build 25F84, Apple Silicon arm64e**.
Fixes the accumulation of `Packages Display N Shield` windows after each display sleep/wake cycle.
See [windowserver.md](windowserver.md) (Chinese) for the root cause, the two ARM64 instruction changes, and earlier validation results.

## Files and execution flow

| File | Purpose |
|---|---|
| `ws-shield-watch.rs` | `watch`, `check`, and `apply` commands, kqueue monitoring, and per-boot attempt limits |
| `shield_patch.rs` | Mach memory access, dyld slide lookup, UUID/instruction checks, ptrace pause/resume, writes, and readback verification |
| `debugger.entitlements` | Debugger entitlement applied to the executable during the build |
| `build.sh` | Compiles and ad-hoc signs `ws-shield-watch` using the local Rust toolchain |
| `install-daemon.sh` | Validates the build, installs the root-owned executable and LaunchDaemon, and removes obsolete scripts |
| `com.wenyuan.ws-shield-fix.plist` | Starts the watcher at boot and keeps it running |
| `tests/` | Tests for executable code writes across processes, private SkyLight pages, validation, and error recovery |
| `count.swift` | Counts shield windows to compare before and after display sleep/wake |

Execution flow: `launchd → Rust watcher → Rust memory patch`.
Rust calls Mach APIs directly, locates SkyLight using the shared cache slide from `TASK_DYLD_INFO`,
and checks its UUID and 13 instructions. To apply the patch, it uses `ptrace(PT_ATTACH)` to enable debugging and pause the process,
creates a private code page in the target, writes the two instructions, flushes the instruction cache,
verifies the bytes, restores read/execute (RX) permissions, and detaches to resume execution.
SIGTERM, SIGINT, and SIGHUP are deferred during the pause; the original signal mask is restored after detaching.
If the state is already `PATCHED`, the command returns immediately. `check` reads process memory and reports the state.

## Build and install

Install Rust and the Xcode Command Line Tools. Accessing WindowServer requires root and a SIP policy that permits debugging this system process.
SIP was disabled on the original test machine. The patch applies to the running process's memory.

```sh
./build.sh
./tests/run.sh
sudo ./ws-shield-watch check
sudo ./install-daemon.sh
```

Building from this repository produces all required runtime files. The installation consists of one Rust executable and one plist:

```text
/usr/local/libexec/ws-shield-fix/ws-shield-watch
/Library/LaunchDaemons/com.wenyuan.ws-shield-fix.plist
```

The watcher attempts to patch each new WindowServer after it has run for 20 seconds, then blocks until that process exits.
Attempted PIDs are recorded in `/var/run/ws-shield-fix.pids`: one attempt per PID per boot, with at most five attempts per boot.
Failed attempts also count toward this limit. Logs are written to `/var/log/ws-shield-fix.log`.

## Updates and startup at boot

After changing the source, rebuild and reinstall from the project directory:

```sh
./build.sh
sudo ./install-daemon.sh
```

The installer updates the installed executable and immediately restarts the watcher.
At each subsequent boot, the LaunchDaemon runs the installed copy at `/usr/local/libexec/ws-shield-fix/ws-shield-watch`.

## Manual use and verification

```sh
sudo ./ws-shield-watch check          # ORIGINAL / PARTIAL / PATCHED
sudo ./ws-shield-watch apply          # Apply and report pause and total time
sudo ./ws-shield-watch check 34611    # Optional: specify the WindowServer PID
sudo launchctl print system/com.wenyuan.ws-shield-fix
tail -n 30 /var/log/ws-shield-fix.log
swift count.swift
```

Record the window count, run `pmset displaysleepnow`, wake and unlock the display, then run `swift count.swift` again.
With the patch applied, the shield window count stays at its baseline. Existing leaked windows are reclaimed when WindowServer next restarts.

## Performance and validation scope

During steady operation, the watcher blocks on kqueue. While waiting for a new process, it runs `pgrep` once per second.
Before attempting a patch, it queries the process age once with `ps`.
The patch runs directly as a Rust function and reads a bounded amount of Mach-O metadata plus 52 bytes of instruction context.

Local validation on 2026-11-02:

- A live snapshot of the earlier watcher showed 0.0% CPU, 928 KiB RSS, and 0.01 seconds of accumulated CPU time.
- Rust test subprocesses executed modified instructions correctly, restored RX permissions, and resumed after a patch error.
- A subprocess loading the real SkyLight framework successfully patched its private shared cache code page while the parent process retained the original instructions. One measured pause was approximately 11.4 ms.
- Validate the native implementation against the actual WindowServer using the root commands above. Subprocess timings provide an estimate of this path's overhead.

Use `./tests/run.sh --nocapture` for detailed test output.
The `code_fixture` entry is marked as ignored in the main test run because the cross-process tests launch it separately as a subprocess.

## Uninstall

```sh
sudo launchctl bootout system/com.wenyuan.ws-shield-fix
sudo rm /Library/LaunchDaemons/com.wenyuan.ws-shield-fix.plist
sudo rm /usr/local/libexec/ws-shield-fix/ws-shield-watch
sudo rmdir /usr/local/libexec/ws-shield-fix
sudo rm -f /var/run/ws-shield-fix.pids
```

Uninstalling stops automatic patch application. The current process retains its patch until WindowServer next restarts.

## Implementation references

- [Apple XNU ptrace implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/mach_process.c): debugging state and process pause/resume.
- [LLVM debugserver memory writes](https://github.com/llvm/llvm-project/blob/main/lldb/tools/debugserver/source/MacOSX/MachVMMemory.cpp): Mach writes and ARM instruction cache flushing.
- [LLVM debugserver memory protection](https://github.com/llvm/llvm-project/blob/main/lldb/tools/debugserver/source/MacOSX/MachVMRegion.cpp): shared cache submaps and copy-on-write code pages.
