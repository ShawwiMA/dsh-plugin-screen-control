# dsh-plugin-screen-control

DSH（DeepSeek Harness）的 Windows 屏幕操控插件。给模型 18 个 `win_*` 工具，让它能看屏幕、点鼠标、打字、拖拽，也能操作后台窗口和网页。

TypeScript 薄壳负责注册工具、校验参数、转发请求；Rust 常驻引擎（`bin/screen-control-engine.exe`）负责窗口枚举、截图、输入注入，两者通过命名管道收发 NDJSON。
观察走 `PrintWindow`，被遮挡或不在前台的窗口照样能截图；操作走 `SendInput` 真注入，可选做像素比对或新窗口检测，确认动作真的生效。
引擎在第一次调用工具时才启动，之后长期复用，插件卸载时自动结束进程。
当前版本 0.2.4，变更见 [CHANGELOG.md](CHANGELOG.md)。

## 它解决什么问题

模型看不见屏幕，也没有鼠标键盘。遇到只有图形界面、没有 API 的软件，它需要一双眼睛和一只手：截图、元素树是眼睛，鼠标键盘注入是手。

普通桌面自动化通常要求目标窗口在前台。用户一切换窗口，自动化就断；纯消息投递去操作后台窗口，又经常没反应，Chromium、Electron 和部分自绘界面尤其常见。

这个插件把观察和操作分开处理：

- **观察不抢前台。** `PrintWindow` 直接向窗口要画面，被遮挡、不在前台也能截图；元素树同样按窗口句柄读取。
- **操作按 Windows 的真实输入语义走 `SendInput`。** 不需要你先手动切到目标窗口，但会移动光标、可能激活窗口；抢不到前台就返回 `UNSUPPORTED`，不会把输入落到别的窗口。
- **一次启动长期复用。** 引擎常驻，多步任务不用反复加载；异常退出后，桥层会在下一次调用时重启它，不牵连 DSH 主进程。

需要完全不动鼠标的网页操作，用 `win_web_cdp` 走 Chrome DevTools Protocol，而不是点像素。

## 能力清单

共 18 个工具，按用途分四组。这里只写用途，参数没有全列；完整签名以工具描述为准。

### 观察（7 个）

| 工具 | 用途 |
| --- | --- |
| `win_engine_status` | 查看引擎是否启动、可执行文件路径、重启次数和最近错误；任何 `win_*` 报错先看它。 |
| `win_list_windows` | 列出桌面顶级窗口，含 hwnd、标题、类名、PID、进程名、位置尺寸和 Z-order；可按标题、类名、PID 或进程名过滤。 |
| `win_capture` | 截窗口或屏幕区域，压缩后返回 PNG 文件路径，不走 Base64；默认把长边缩到 1600，可同时做 OCR。捕获优先 `PrintWindow`，失败回退 GDI `BitBlt`。 |
| `win_observe` | 返回 UI 元素树（uid、名称、控件类型、矩形）和高亮缩略图，并生成 `snapshot_id`；比整张截图省 token。快照默认 5 秒过期。 |
| `win_find` | 一次性模板匹配，返回模板位置和相似度；只搜一次，不轮询。找不到时返回 `found=false` 和最佳候选，不算报错。 |
| `win_wait` | 条件等待：等窗口出现、等图像出现、等 OCR 文字出现；不是 `sleep`，超时会正常返回 `matched=false`。 |
| `win_ocr` | 调用 Windows 原生 OCR 读窗口或区域，返回全文和逐行矩形；缺语言包时返回 `UNSUPPORTED`。 |

### 操作（8 个）

| 工具 | 用途 |
| --- | --- |
| `win_click` | 点击。可用 `snapshot_id` + `uid` 精确定位，也可直接给屏幕坐标；走真实输入注入。 |
| `win_type` | 输入文本。纯 ASCII 走 Unicode 注入；中文等非 ASCII 自动走剪贴板；密码框直接拒绝。 |
| `win_press` | 发送按键或组合键，例如 Enter、Tab、Esc、方向键、F1 到 F12，可带 Ctrl、Alt、Shift、Win。 |
| `win_scroll` | 在指定位置滚轮滚动。`delta` 单位是格（一格约 3 行文字），负数向下、正数向上，建议一次 1~5 格。 |
| `win_drag` | 单段拖拽：按下、插值移动、抬起，带 1px 微动；适合拖标题栏、拖选文本、拖滑块。 |
| `win_drag_path` | 一次按下走完整条路径，支持圆、圆弧、贝塞尔、折线和多笔；绘制曲线、签名用这个，不要用 `win_drag` 拼段。 |
| `win_record` | 录一段人手的鼠标轨迹，返回带毫秒时间戳的笔顺，交给 `win_drag_path` 按原速回放。 |
| `win_image_trace` | 把图片线条转成可直接画的笔顺：二值化、细化、提笔顺、简化、重采样；配 `shot_id` 可直接喂给 `win_drag_path`。 |

### 窗口（2 个）

| 工具 | 用途 |
| --- | --- |
| `win_launch` | 拉起进程，可带启动参数和工作目录；只负责启动，不等窗口出现。 |
| `win_window_state` | 最大化、最小化、还原、进入伪全屏、退出全屏；返回操作后回读的真实状态。 |

### 网页（1 个）

| 工具 | 用途 |
| --- | --- |
| `win_web_cdp` | 走 Chrome DevTools Protocol：发一条 CDP 命令、列出标签页、新建标签页；不解析 DOM，原始 JSON 原样返回。 |

`win_click`、`win_press`、`win_scroll`、`win_drag`、`win_drag_path` 支持可选的 `verify` 参数：动作前后自动比对画面或检测新窗口，把“命令发出去了”和“目标真的反应了”区分开。

`win_capture` 还会返回 `coord.shot_id`。把它传给动作工具的 `shot_id`，截图里量到的像素坐标就能直接当参数用，引擎会按那张截图的缩放自动换算。

典型顺序：`win_list_windows` 拿 hwnd → `win_observe` 拿 `snapshot_id` 和 uid → 调动作工具 → 再 `win_observe` 或 `win_capture` 回读确认。

## 安装

1. 安装插件。在能运行 `dsh` 的终端执行：

   ```bash
   dsh plugin --profile <你的profile> add dsh-plugin-screen-control
   ```

2. 重启 `dsh web`。先停掉正在运行的进程，再重新启动。运行中的 DSH 不会热加载新插件，重启后开一个新会话。

3. 验证。调用 `win_engine_status`，能返回引擎版本、可执行文件路径、运行状态和重启次数，就说明插件和引擎都正常。

从本地目录安装（例如调试自己的修改）：

```bash
dsh plugin --profile <你的profile> add link:/绝对路径/dsh-plugin-screen-control
```

把 `/绝对路径` 换成插件根目录的绝对路径。本地安装同样需要重启 `dsh web`。

## 环境要求

- **Windows 10 / 11，仅 x64。** 引擎是 x86_64 的 Windows 原生可执行文件，ARM64 不能运行。
- **Node.js `^22.19 || >=24`。**
- **DSH `^0.2.0-rc.2`。**
- **`@deepseek-ai/dsh-tools` 由 DSH 宿主提供。** 插件不自己安装它，用户不需要手动执行 `npm install`。
- **发布包已带引擎二进制。** 不需要安装 Rust 工具链。

## 安全声明

- **插件自身不联网、无遥测、不上传任何截图或数据。** 唯一的网络能力在 `win_web_cdp`：它连你指定的调试端口（默认 `127.0.0.1:9222`），host 和端口都由你传参决定，插件不会自己往任何地方连。
- **只调用 Windows 公开 API。** 屏幕观察和输入注入使用 Win32、`PrintWindow`、`SendInput`；OCR 使用系统自带的 `Windows.Media.Ocr`。
- **不读写其他进程的内存。** 唯一例外是 `win_web_cdp`：它通过 Chrome DevTools Protocol 与浏览器通信，需要你自己用 `--remote-debugging-port=9222` 启动浏览器。
- **`win_type` 输入中文等非 ASCII 字符时会走剪贴板，覆盖当前剪贴板内容。** 返回值 `warnings` 会注明；不想动剪贴板可以显式传 `method:"sendinput"`。
- **`win_click` / `win_type` 等动作会真的移动光标，也可能激活目标窗口。** 这是 Windows 的输入语义，不是无声模拟；默认注入路径抢不到前台时返回 `UNSUPPORTED`，不会硬输入到别的窗口。
- **`bin/screen-control-engine.exe` 未做代码签名。** Windows Defender 或 SmartScreen 可能提示「未知发布者」；确认来源可信后再放行。
- **密码框硬拒绝，但覆盖面取决于构建。** 命中 `ES_PASSWORD` 或 UIA 的 `IsPassword` 时，`win_type` 返回 `PASSWORD_FIELD`，不输入任何字符。默认构建走 `EnumChildWindows` 控件树、不编译 UIA（`--features uia` 才开），所以纯自绘界面的密码框识别不到，不要把它当安全边界。
- **`win_web_cdp` 只连本机。** `host` / `ws_url` 里的主机名必须是回环地址；连远端浏览器请自己用 SSH 端口转发到 127.0.0.1。

## 已知限制

- **仅 x64 Windows。** 没有 ARM64、macOS 或 Linux 版本。
- **`win_web_cdp` 需要浏览器开调试端口。** 目标浏览器或 Electron 必须用 `--remote-debugging-port=9222` 启动，否则连不上。目前验证过的是 Microsoft Edge + 9222，其他浏览器和端口没有逐一验证。
- **`win_ocr` 依赖系统 OCR 语言包。** 系统没有可用 OCR 引擎或缺少对应语言包时返回 `UNSUPPORTED`；这时改用 `win_capture` 加看图。
- **高 DPI 只在 150% 缩放下实际用过。** 125% 和 175% 还没有充分验证；引擎按屏幕物理像素工作，缩放换算的边界情况仍可能有偏差。
- **动作不是完全无声。** `SendInput` 需要目标窗口能被激活、光标落点没有被别的窗口盖住；不满足时返回 `UNSUPPORTED`，不会硬点。需要无干扰的网页操作，走 `win_web_cdp`。
- **模板匹配对周期性图案有多解。** `win_wait(condition:"image")` 遇到条纹、表格这类重复图案时，可能返回多个同分位置中的一个；用 `region` 缩小范围可以规避。
- **模板必须从未缩放的截图里裁剪。** `win_capture` 默认把长边缩到 1600；拿缩放图当模板会匹配不准，可以传 `max_dim=0` 重新截图，或用 `scale` 补回缩放系数。

## 许可证

MIT 许可证。见 [LICENSE](LICENSE)。

Copyright (c) 2026 ShawwiMA.
