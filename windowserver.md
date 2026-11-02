# WindowServer "Packages Display Shield" 窗口泄漏 — 根因与修复

- 日期：2026-11-02
- 机器：Apple M1 Pro，16 GB，3 显示器（内置 + HP E24m 1920×1080 + HP E24i 1200×1920）
- 系统：macOS 26.5.2 (25F84)，arm64e；SIP、authenticated-root 均已关闭
- 组件：`/System/Library/PrivateFrameworks/SkyLight.framework/.../WindowServer`（SkyLight）

---

## 1. 现象与影响

WindowServer 连续运行 33 天后占用约 14 GB，swap 用到 10.3/11 GB。
`CGWindowListCopyWindowInfo(.optionAll)` 显示有 500+ 个 WindowServer 自有窗口，
名字为 `Packages Display {1,2,3} Shield`，layer `2147483646`，全屏尺寸，`onscreen=false`。
每块显示器每次唤醒多出一个，长期不回收。

## 2. 复现（稳定）

```
# 统计当前 shield 窗口数
swift count.swift        # 见附录
# 触发一次：显示器休眠→唤醒
pmset displaysleepnow    # 黑屏后按键唤醒解锁
# 再统计：每块在线显示器 +1
```

本会话实测：每次休眠唤醒，`Packages Display N Shield` 稳定 +3（3 屏各 +1）。

## 3. 排查结论

触发条件唯一：**显示器休眠后唤醒**。单纯锁屏（不让显示器休眠）不增长。
已排除：yabai（含其 Dock 注入）、Dock、外接显示器、Reduce Motion。
→ 属 Apple SkyLight 自身 bug，与第三方工具无关。

## 4. 根因（指令级，静态逆向 SkyLight）

方法：`ipsw dyld extract` 取出 SkyLight，`otool -tV` 反汇编。地址为本 build（26.5.2）值。

### 4.1 泄漏窗口的生命周期

| 角色 | 函数 | 地址 |
|---|---|---|
| 创建 | `show_shield_windows(CGXSessionPackageData*)` | `0x186f94978` |
| 销毁 | `hide_shield_windows(CGXSessionPackageData*)` | `0x186f948a4` |
| 底层建窗 | `WS::create_shield_window_common(CFString*, tag, CGColor*, CGSRegionObject*)` | `0x186d34da0` |
| 唤醒调度 | `PKGWindowUpdateAppearanceForDisplayWake`（block_invoke / _2） | `0x186f9f29c` / `0x186f9f1bc` |
| 显示重配 | `CGXDisplayConfigurationChanged` | `0x186f94848` |

窗口名格式串 `"Packages Display %x Shield"`（SkyLight 内 `0x1871d9be7`）由 `show_shield_windows` 使用，
已逐字对上，**创建者确定为 `show_shield_windows`**。

### 4.2 CGXSessionPackageData 相关字段

| 偏移 | 含义 |
|---|---|
| `+0x180` | shield 窗口 **id 数组**（`int32[count]`，`malloc_type_calloc`，释放时据此逐个释放） |
| `+0x188` | 承载 shield 的 `PKGSpace` |
| `+0x190` | 数组元素个数（显示器数） |
| `+0x194` | **原子引用计数**，决定 show/hide 何时真正执行 |

### 4.3 引用计数协议（本身正确）

- show 侧 `block_invoke`：`fetch_add(+1)`，仅当旧值 `==0`（0→1）才调 `show_shield_windows`。
- hide 侧 `block_invoke_2`：`fetch_add(-1)`，旧值 `==1`（→0）才调 `hide_shield_windows`；
  减成负数则打印 `over-release of shield windows detected!` 并清零。

`hide_shield_windows` 的释放循环：

```
x8 = pkgData->[+0x180]            // 数组基址
for i in 0..[+0x190]:            // count
    CGXReleaseWindow( x8[i] )     // 逐个释放
free(x8); pkgData->[+0x180]=NULL; [+0x190]=0
PKGSpacesHide; PKGSpaceDestroy;  [+0x188]=NULL
```

### 4.4 缺陷点（确认）

`show_shield_windows` 建窗循环里，对 `+0x180` 数组的写入**只有两处**：

```
0x186f94a4c  str  x1, [x19, #0x180]   // 存数组基址（calloc 全 0）
0x186f94b54  str  wzr, [x8, x27]      // 仅“失败支路”：window==NULL 时写 0
```

成功支路（窗口建成、`PKGSpacesAddWindowsWithConnection` 加入 PKGSpace 之后）直接跳到循环尾：

```
0x186f94b28  cbz  x22, 0x186f94b50    // x22 = 新窗口；非空则继续
0x186f94b2c  ldr  x8,  [x19, #0x188]  // space
0x186f94b30  cbz  x8,  0x186f94b58    // 无 space 则跳过
0x186f94b34  ...  PKGSpacesAddWindowsWithConnection(space, window)
0x186f94b4c  b    0x186f94b58         // ← 直接进入下一轮，从不回写 window id
0x186f94b58  add  x26, x26, #0x10     // 迭代推进
0x186f94b5c  add  x27, x27, #0x4
```

即：**成功建成的 shield 窗口，其 id 从未被写回 `+0x180` 释放数组**，数组对所有真实窗口恒为 0。
于是 `hide_shield_windows` 的 `CGXReleaseWindow(array[i])` 全在对 0 做无效释放，
真正的窗口无人回收 → 每次休眠唤醒每屏漏一个，长期累积。

（唯一未进一步验证的环节：`PKGSpaceDestroy` 是否“本应”连带回收空间内成员窗口。无论是否叠加，
修复点都收敛到 `show_shield_windows` 回写缺失这一处。）

## 5. 修复（语义层）

把成功支路改成与失败支路对称——建成后把 window id 写回数组槽。
window id 在 `CGXWindow+0x0`（`+0x20` 是 window level，同函数 `0x186f94afc` 拿它和 `0x7ffffffe` 比较）：

```c
// show_shield_windows 建窗循环，每块显示器：
CGXWindow *w = create_shield_window_common(name, 0x200, color, region);
uint32_t *arr = pkgData->shield_ids;     // +0x180
if (w && pkgData->shield_space /*+0x188*/) {
    PKGSpacesAddWindowsWithConnection(pkgData->shield_space, w);
    arr[i] = *(uint32_t *)w;             // ← 补上：记录 window id（CGXWindow+0x0）
} else {
    arr[i] = 0;
}
```

id 偏移的佐证：其余 shield 创建者读 id 都用 `ldr w8, [win]`，再交给窗口排序 / 释放 API
（`shield_display_if_not_already_shielded` 存到 `+0x98`；`SLCADisplay::raise_shield` 存到 `+0x478`，
其 `lower_shield` 用 `CGXReleaseWindow([+0x478])` 回收）。`CGXReleaseWindow(id)` →
`window_by_id` → `CGXWindow::release_window`，与本修复走同一条回收路径。

有了这一行，`hide_shield_windows` 先 `CGXOrderWindowListWithOperation(.., arr, 0 /*out*/, ..)`，
再 `CGXReleaseWindow(arr[i])`，释放到真实窗口，show/hide 对称，泄漏消除。
这也是向 Apple 报告时描述的修复。

### 5.1 已应用：两条指令原地热补丁（2026-11-02，已验证）

利用失败支路进入 `0x186f94b50` 时 `x22 == NULL` 这一点，原地改两条指令即可，无需跳板：
成功支路把 window id 装进 `x22`，共用的写槽指令由 `wzr` 改为 `w22`。
`x22` 跨 `bl` 保留（callee-saved），且下一轮循环在 `0x186f94a6c` 重新装载，修改后不再被使用。

| 地址（unslid） | 原指令 | 新指令 |
|---|---|---|
| `0x186f94b4c` | `14000003` `b 0x186f94b58` | `b94002d6` `ldr w22, [x22]` |
| `0x186f94b54` | `b83b691f` `str wzr, [x8, x27]` | `b83b6916` `str w22, [x8, x27]` |

工具：`~/Work/windowserver-fix/`

- `shield_patch.rs`：直接通过 Mach API 读取 WindowServer 内存。核对 SkyLight UUID
  `6F1978A2-1B8A-3A44-903D-532EED7106EB`，按运行时 shared cache slide 定位，校验 `0x186f94b28..0x186f94b58`
  共 13 条指令后写入，写后刷新指令缓存并回读确认。
- `ws-shield-watch.rs`：提供 `watch`、`check`、`apply` 入口。应用时用 `ptrace(PT_ATTACH)`
  使目标获得 CS_DEBUGGED 并暂停进程；写入私有代码页后恢复 RX 权限，detach 恢复运行。
  `check` 读取和校验内存，已是 `PATCHED` 时 `apply` 直接返回。

手动运行：

```
sudo ~/Work/windowserver-fix/ws-shield-watch apply
sudo ~/Work/windowserver-fix/ws-shield-watch check    # 报告 ORIGINAL / PARTIAL / PATCHED
```

补丁驻留在 WindowServer 内存中，由 LaunchDaemon 在每个新 WindowServer（开机、注销后）上自动应用：

- `com.wenyuan.ws-shield-fix`（`/Library/LaunchDaemons/`，RunAtLoad + KeepAlive）常驻运行
  `/usr/local/libexec/ws-shield-fix/ws-shield-watch`（root 所有的副本）。它由 `ws-shield-watch.rs` 编译而来：
  Rust 无外部 crate，kqueue 和 Mach API 直接 FFI；构建与签名命令 `./build.sh`。
- 事件驱动：用 kqueue `EVFILT_PROC` / `NOTE_EXIT` 阻塞等待当前 WindowServer 退出（0 CPU、无定时唤醒）；
  退出后每秒查找新 WindowServer，新实例运行满 20 s 后调用 Rust 内部补丁函数。
- 每个 pid 尝试一次：写入 `/var/run/ws-shield-fix.pids`（开机清空），每次开机最多尝试 5 个实例。
- 开机时补丁发生在登录界面阶段；日志 `/var/log/ws-shield-fix.log`。
- 安装：先 `./build.sh`，再 `sudo ./install-daemon.sh`；完整安装、验证和卸载见 [README.md](README.md)。
- 2026-11-02 原 LLDB 实现的实测：
  - launchd 以 root 运行 patch 路径：attach pid 34611，报告 `PATCHED`，正常 detach，耗时 8 s。
  - 替身进程测试：退出即刻察觉，新 pid 满等待时间后被 patch；watcher 重启后不重复 patch。
  - 常驻 Rust watcher（pid 3270）：状态 `Ss`，CPU 0.0%，RSS 1.7 MB，累计 CPU 0.01 s，监听 WindowServer 34611。

原 LLDB 实现的补丁效果实测：

- 应用：pid 34611，slide `0x8d88000`，站点 `0x18fd1cb4c`，两处回读 ok，WindowServer 保持运行（同 pid）。
- 打补丁前：一次 `pmset displaysleepnow` 唤醒，`Packages Display N Shield` 12 → 15（+3）。
- 打补丁后：一次休眠唤醒，15 → 15（+0）；show→hide 约 105 ms
  （`PKGReconcileAutomaticAppearanceTheme` 20:21:31.404 → `complete_appearance_transition_on_session` 20:21:31.508）。
- 已泄漏的 15 个窗口保持到下次 WindowServer 重启时回收。

2026-11-02 Rust 原生实现的验证：独立子进程中修改可执行指令并执行，恢复 RX 权限；
加载真实 SkyLight 的子进程中修改同一补丁位置，父进程保持原始指令，验证代码页为进程私有。
该 shared cache 子进程的一次暂停约 11.4 ms。真实 WindowServer 上的新实现耗时待 root 验证。

源码级修复走 §7 的 Apple 路径；系统升级后 build/UUID 校验按支持的版本匹配，用 §6 复测确认。

## 6. 验证方法

修复生效的判据：休眠唤醒后 `Packages Display N Shield` 计数**不再增长**。

```
swift count.swift          # 记基线
pmset displaysleepnow      # 唤醒解锁
swift count.swift          # 应与基线相同（而非 +屏数）
```

当前临时释放：注销重登 / 重启 / `sudo killall -HUP WindowServer`（会结束图形会话）。

## 7. 根治路径：Apple Feedback

这是让它从源头被修、以后无需任何兜底的路径。报告要点（英文摘要，可直接提交）：

> **Title:** SkyLight leaks one "Packages Display N Shield" window per display on every display wake
>
> **Summary:** On macOS 26.5.2 (arm64e), each display sleep→wake cycle leaks one offscreen
> `Packages Display %x Shield` window per online display. They accumulate indefinitely (500+ after
> 33 days uptime on a 3-display setup), driving WindowServer to ~14 GB.
>
> **Root cause:** In `show_shield_windows(CGXSessionPackageData*)`, the success branch adds the newly
> created shield window to the PKGSpace but never records its window id into the release array at
> `CGXSessionPackageData+0x180`. The array therefore stays zero-filled for all real windows, so
> `hide_shield_windows`'s `CGXReleaseWindow(array[i])` loop frees nothing; teardown then relies solely
> on `PKGSpaceDestroy`, and the member windows are orphaned.
>
> **Fix:** record the window id (`*(uint32_t *)window`, CGXWindow+0x0) into the release-array slot on
> the success path (symmetric with the failure path which already writes 0). Verified with a live
> two-instruction patch: after it, a sleep/wake cycle adds 0 shield windows instead of +1 per display.
>
> **Repro:** `pmset displaysleepnow`, wake, count windows named "Packages Display % Shield" via
> `CGWindowListCopyWindowInfo(kCGWindowListOptionAll)`; +1 per online display each wake.

## 附录 A：count.swift

```swift
import CoreGraphics
let list = CGWindowListCopyWindowInfo(.optionAll, kCGNullWindowID) as! [[String: Any]]
let n = list.filter { ($0[kCGWindowName as String] as? String)?.hasPrefix("Packages Display") == true }.count
print("Packages shield windows: \(n)  (total \(list.count))")
```

## 附录 B：关键符号/地址（build 25F84，arm64e；OS 更新后会变）

```
show_shield_windows                       0x186f94978   __ZL19show_shield_windowsP21CGXSessionPackageData
hide_shield_windows                       0x186f948a4   __ZL19hide_shield_windowsP21CGXSessionPackageData
create_shield_window_common               0x186d34da0   __ZN2WSL27create_shield_window_commonE...
CGXDisplayConfigurationChanged            0x186f94848
PKGWindowUpdateAppearanceForDisplayWake   0x186f9f29c / 0x186f9f1bc  (block_invoke / _2)
格式串 "Packages Display %x Shield"        0x1871d9be7
缺陷指令（成功支路缺回写）                 0x186f94b4c
```
