# screen-control-engine

DSH `screen-control` 插件的 Rust 常驻引擎。一个独立进程,通过 Windows 命名管道收发 NDJSON。

**协议以 `../docs/IPC.md` 为准**,本文件只讲怎么编译和排查。

## 编译

本机工具链已装好:rustc/cargo 1.99.0,默认 host 就是 `x86_64-pc-windows-gnu`
(没有 MSVC 链接器,用 `C:\mingw64` 的 gcc 8.1.0 当链接器)。

```powershell
# 在插件根目录
npm run build:engine

# 只想快一点、要带调试符号:
npm run build:engine -- -Profile debug
```

**日常不需要 `-Target`** —— 默认 host 已经是 GNU 了。只有真要交叉编译到别的 triple 时才传。

`scripts/build-engine.ps1` 自己会补 gcc、绕开 PS 5.1 把 cargo stderr 当致命错误的坑,
并在编完后独立回读产物是否真的存在(再同步一份到 `bin\`)。

产物:`target/<profile>/screen-control-engine.exe`(带 `-Target` 时多一层 triple 目录)。

> ⚠️ 二进制名由 `Cargo.toml` 的 `[[bin]] name = "screen-control-engine"` 决定,**不要删**。
> Cargo 默认拿包名当二进制名,而包名是 `dsh-screen-control-engine`,产物会变成
> `dsh-screen-control-engine.exe`,而 TS 桥的 `ENGINE_EXE` 常量找的是不带 `dsh-` 的那个 ——
> 结果就是引擎永远找不到。

编译开关:

| 开关 | 作用 |
| --- | --- |
| 默认 | 元素树走 `EnumChildWindows`(快、稳) |
| `--features uia` | 额外编译真 UI Automation 路径(依赖 `uiautomation`) |

release 档开了 `lto`、`codegen-units = 1`、`panic = "abort"`、`strip`。

## 手工调试

```powershell
# 直接跑,看启动日志(引擎是服务端,没客户端连它会一直等)
.\target\release\screen-control-engine.exe --pipe dsh-screen-control-test --log-level debug

# --once:处理完一个连接就退出,方便脚本化冒烟
.\target\release\screen-control-engine.exe --pipe dsh-screen-control-test --once
```

`--pipe` 可以给短名(引擎自己补 `\\.\pipe\` 前缀),也可以给完整路径。

用 Node 手工打一发(不走 DSH):

```powershell
$env:DSH_SCREEN_CONTROL_ENGINE = "<exe 绝对路径>"
npm run selfcheck     # 阶段二 B 会打 engine_ping + win_list_windows
```

## 模块职责

| 文件 | 职责 |
| --- | --- |
| `main.rs` | 参数解析、DPI 声明、tokio 运行时、方法分发 |
| `ipc.rs` | 命名管道服务端、NDJSON 收发、事件帧 |
| `protocol.rs` | 全部 serde 结构体与默认值(**协议的唯一代码副本**) |
| `window.rs` | 顶层/子窗口枚举、过滤、Z-order、进程名 |
| `capture.rs` | PrintWindow → GDI BitBlt 回退、缩放、PNG 编码 |
| `uia.rs` | 元素树(默认子窗口树;`--features uia` 走真 UIA) |
| `input.rs` | PostMessage 投递、SendInput 降级、剪贴板粘贴、NoActivateGuard |
| `dpi.rs` | DPI 感知、显示器枚举、屏幕/客户区坐标换算 |
| `snapshot.rs` | `snapshot_id` 表、TTL 校验、uid → 绝对坐标解析 |
| `error.rs` | 错误码枚举与 `EngineError` |

## 铁律

1. **不激活窗口**。禁止 `SetForegroundWindow` / `SetActiveWindow` / `BringWindowToTop`。
2. **坐标对外一律屏幕物理像素**。换算只在这两个函数里发生:`dpi::screen_to_client` / `dpi::client_to_screen`。
3. **所有句柄 RAII**。HDC/HBITMAP/HANDLE 的释放写在 `Drop` 里,不散落在业务代码。
4. **`panic = "abort"`**。不要在业务路径上用 `unwrap()`/`expect()` —— 一次 panic 就是整个引擎挂掉
   (桥层会重启它,但用户的这次操作就丢了)。
5. **密码框硬拒绝**。命中 `ES_PASSWORD` / UIA `IsPassword` 时返回 `PASSWORD_FIELD`,不输入任何字符。
