# WindowServer shield leak fix

针对 macOS **26.5.2 / build 25F84、Apple Silicon arm64e** 的内存补丁。
修复显示器每次休眠唤醒后 `Packages Display N Shield` 窗口持续累积的问题。
根因、两条 ARM64 指令和历史验证见 [windowserver.md](windowserver.md)。

## 文件与运行流程

| 文件 | 职责 |
|---|---|
| `ws-shield-watch.rs` | `watch`、`check`、`apply` 入口，kqueue 监听和每次开机的尝试次数限制 |
| `shield_patch.rs` | Mach 进程内存访问、dyld slide、UUID/指令校验、ptrace 暂停与恢复、写入和回读 |
| `debugger.entitlements` | 构建时为可执行文件添加 debugger entitlement |
| `build.sh` | 用本机 Rust 编译、ad-hoc 签名，生成 `ws-shield-watch` |
| `install-daemon.sh` | 检查构建产物，安装 root 所有的程序与 LaunchDaemon，清理旧脚本 |
| `com.wenyuan.ws-shield-fix.plist` | 开机启动并保持 watcher 运行 |
| `tests/` | 原生跨进程代码写入、真实 SkyLight 私有页、校验与错误恢复测试 |
| `count.swift` | 统计 shield 窗口，验证休眠唤醒前后的数量 |

运行流程：`launchd → Rust watcher → Rust 内存补丁`。
Rust 直接调用 Mach API，按 `TASK_DYLD_INFO` 的 shared cache slide 定位 SkyLight，
核对 UUID 和 13 条指令。应用时通过 `ptrace(PT_ATTACH)` 设置调试状态并暂停进程，
创建目标进程的私有代码页、写入两条指令、刷新指令缓存、回读校验、恢复 RX 权限，最后 detach。
暂停期间延后处理 SIGTERM、SIGINT、SIGHUP，detach 后恢复原来的信号屏蔽状态。
已经处于 `PATCHED` 状态时直接返回。`check` 读取进程内存并报告状态。

## 构建与安装

准备 Rust 和 Xcode Command Line Tools。访问 WindowServer 需要 root 和允许调试该系统进程的 SIP 策略；
原始测试机器的 SIP 已关闭。补丁作用于运行中的进程内存。

```sh
./build.sh
./tests/run.sh
sudo ./ws-shield-watch check
sudo ./install-daemon.sh
```

从仓库源码构建即可得到全部运行文件。安装后只需要一个 Rust 可执行文件和一个 plist：

```text
/usr/local/libexec/ws-shield-fix/ws-shield-watch
/Library/LaunchDaemons/com.wenyuan.ws-shield-fix.plist
```

watcher 在每个新 WindowServer 运行满 20 秒后尝试应用补丁，随后阻塞等待进程退出。
尝试过的 PID 写入 `/var/run/ws-shield-fix.pids`，每个 PID 每次开机尝试一次，每次开机最多 5 次，
失败的尝试也计入上限。日志在 `/var/log/ws-shield-fix.log`。

## 更新版本与开机自启

以后修改源码后，在项目目录重新构建并安装：

```sh
./build.sh
sudo ./install-daemon.sh
```

安装脚本会更新安装目录中的程序并立即重启 watcher。
之后每次开机，LaunchDaemon 都使用 `/usr/local/libexec/ws-shield-fix/ws-shield-watch` 中已安装的版本。

## 手动使用与验证

```sh
sudo ./ws-shield-watch check          # ORIGINAL / PARTIAL / PATCHED
sudo ./ws-shield-watch apply          # 应用并报告暂停、总耗时
sudo ./ws-shield-watch check 34611    # 可选：指定 WindowServer PID
sudo launchctl print system/com.wenyuan.ws-shield-fix
tail -n 30 /var/log/ws-shield-fix.log
swift count.swift
```

记录窗口数量后执行一次 `pmset displaysleepnow`，唤醒解锁，再运行 `swift count.swift`。
补丁生效时，shield 窗口数量保持原值。现有的泄漏窗口随 WindowServer 下次重启回收。

## 性能与验证范围

watcher 在稳定运行期间阻塞于 kqueue。`pgrep` 在等待新进程时每秒运行一次，
`ps` 在准备应用补丁时查询一次进程年龄。
补丁路径直接执行 Rust 函数，读取有限的 Mach-O 头和 52 字节指令上下文。

2026-11-02 本机验证：

- 旧 watcher 的实时快照：CPU 0.0%、RSS 928 KiB、累计 CPU 0.01 秒。
- Rust 测试子进程：可执行指令修改后执行结果正确，RX 权限恢复，失败路径恢复运行。
- 加载真实 SkyLight 的子进程：成功修改其 shared cache 私有代码页，父进程中的原始指令保持一致；一次暂停实测约 11.4 毫秒。
- 真实 WindowServer 的新实现需要通过上面的 root 命令继续验证；子进程耗时用于评估该路径的开销。

更详细的测试输出可用 `./tests/run.sh --nocapture` 查看。
测试列表中的 `code_fixture` 是由跨进程测试单独启动的子进程入口，主测试运行时标记为 ignored。

## 卸载

```sh
sudo launchctl bootout system/com.wenyuan.ws-shield-fix
sudo rm /Library/LaunchDaemons/com.wenyuan.ws-shield-fix.plist
sudo rm /usr/local/libexec/ws-shield-fix/ws-shield-watch
sudo rmdir /usr/local/libexec/ws-shield-fix
sudo rm -f /var/run/ws-shield-fix.pids
```

卸载停止自动应用补丁；当前进程中的补丁随 WindowServer 下次重启结束。

## 实现参考

- [Apple XNU ptrace 实现](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/mach_process.c)：调试状态和进程暂停/恢复。
- [LLVM debugserver 内存写入](https://github.com/llvm/llvm-project/blob/main/lldb/tools/debugserver/source/MacOSX/MachVMMemory.cpp)：Mach 写入与 ARM 指令缓存刷新。
- [LLVM debugserver 内存权限处理](https://github.com/llvm/llvm-project/blob/main/lldb/tools/debugserver/source/MacOSX/MachVMRegion.cpp)：shared cache 子映射和 copy-on-write 代码页。
