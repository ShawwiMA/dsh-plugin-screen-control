# screen-control IPC 协议 (v1)

TS 壳与 Rust 引擎之间的唯一契约。任何一侧改动都必须同步改本文件。

## 传输层

- Windows 命名管道,**完整路径由 TS 壳**生成:`\\.\pipe\dsh-screen-control-<pid>-<random>`。
- Rust 引擎是**服务端**:启动参数 `--pipe <full-path>`,`--once` 表示处理完一个连接就退出。
- `--pipe` 传的是**完整路径**(已含 `\\.\pipe\` 前缀),引擎**原样使用**;为容错,若传入值不以 `\\.\pipe\` 开头则引擎自己补上前缀。
- TS 壳是**客户端**:spawn 引擎后按 20ms 间隔重试 `net.connect()`,最多 5s。
- 编码 UTF-8,**NDJSON**:一行一个 JSON 对象,`\n` 结尾,不允许内嵌换行。

## 请求帧 (TS -> Rust)

```json
{"id": 42, "method": "win_click", "params": {"hwnd": 1234567, "x": 100, "y": 200}}
```

- `id`: 正整数,单调递增,响应必须原样回填。
- `method`: 见下表。
- `params`: 方法参数对象,缺省为 `{}`。

## 响应帧 (Rust -> TS)

成功:

```json
{"id": 42, "ok": true, "result": { }}
```

失败:

```json
{"id": 42, "ok": false, "error": {"code": "STALE_SNAPSHOT", "message": "snapshot 7 expired 120ms ago"}}
```

事件(无 `id`,TS 侧只记日志,不回):

```json
{"event": "log", "level": "info", "message": "capture backend = printwindow"}
```

`error.code` 取值必须来自:`INVALID_ARGS` / `NOT_FOUND` / `STALE_SNAPSHOT` / `PASSWORD_FIELD` / `UNSUPPORTED` / `TIMEOUT` / `ENGINE_ERROR`。

## 字段宽容规则

引擎可以返回本文档没列出的**额外字段**(例如调试用的 `handle`、`device_name`),TS 侧忽略即可。
反过来不行:本文档列出的字段**一个都不能少、名字不能变**。少一个字段就是协议违约。

## mock 引擎

`scripts/mock-engine.mjs` 是协议的一份可执行副本(Node 写,不做任何 Win32 调用),
`npm run selfcheck` 的阶段二 A 用它验证 TS 桥层。**改协议时三处要一起改**:
docs/IPC.md、`src/types.ts`、`rust-engine/src/protocol.rs`(外加 mock 这份可执行副本)。

## 坐标约定

**全部对外坐标一律是屏幕物理像素**(virtual-screen 坐标,DPI 感知为 PER_MONITOR_AWARE_V2)。

- `win_observe` / `win_list_windows` 返回的 `rect` 是屏幕物理像素。
- `win_click` 的 `x` / `y` 也是屏幕物理像素;引擎内部用 `ScreenToClient` 换算成目标窗口客户区坐标再投递消息。
- 引擎启动时调用 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`,保证 1 逻辑单位 = 1 物理像素。

## 方法表

引擎版本 0.1.9。表里每一行在 `rust-engine/src/main.rs` 的 `dispatch()` 里都有对应分支
（`win_focus_guard_probe` 只给内部调试用，没有注册成模型工具）。

| method | params | result |
| --- | --- | --- |
| `engine_ping` | `{}` | `PingResult` |
| `win_list_windows` | `{filter?: {title_contains?, class_name?, pid?, process_name?, visible_only?, top_level_only?}, limit?}` | `{windows: WinInfo[]}` |
| `win_capture` | `{hwnd?, region?: {x,y,w,h}, max_dim?=1600, format?="png", backend?="auto", ocr?, ocr_language?}` | `CaptureResult`（`ocr=true` 时多一个 `ocr` 字段） |
| `win_observe` | `{hwnd, max_depth?=4, max_elements?=400, with_screenshot?=true, snapshot_ttl_ms?=5000}` | `ObserveResult` |
| `win_click` | `{snapshot_id?, uid?, hwnd?, x?, y?, button?="left", clicks?=1, force_sendinput?, shot_id?, verify?}` | `ActionResult` |
| `win_type` | `{snapshot_id?, uid?, hwnd?, text, method?="auto"}` | `ActionResult`（`delivered` 可能是 `sendinput` / `clipboard` / `postmessage`） |
| `win_press` | `{snapshot_id?, uid?, hwnd?, key, modifiers?=[], repeat?=1, verify?}` | `ActionResult` |
| `win_scroll` | `{snapshot_id?, uid?, hwnd?, delta, x?, y?, shot_id?, verify?}` | `ActionResult` |
| `win_launch` | `{path, args?=[], cwd?, no_window?=false, check_ms?=30}` | `LaunchResult` |
| `win_window_state` | `{hwnd, action: "maximize"\|"minimize"\|"restore"\|"fullscreen"\|"exit_fullscreen"}` | `WindowStateResult` |
| `win_drag` | `{hwnd?, snapshot_id?, uid?, from_x?, from_y?, to_x, to_y, button?="left", steps?=24, duration_ms?=400, wobble?=true, shot_id?, verify?}` | `DragResult` |
| `win_drag_path` | `{hwnd?, snapshot_id?, uid?, from_x?, from_y?, points?, strokes?, shape?, button?="left", step_px?=2, duration_ms?, wobble?=false, shot_id?, times_ms?, verify?}` | `PathResult` |
| `win_wait` | `{condition: "window"\|"image"\|"text", timeout_ms?=10000, interval_ms?=300, title_contains?, class_name?, pid?, visible_only?=true, template?, min_score?=0.95, template_scale?=1.0, text?, case_sensitive?=false, language?, hwnd?, region?}` | `WaitResult` |
| `win_find` | `{template, hwnd?, region?, min_score?=0.95, scale?=1.0}` | `FindResult` |
| `win_image_trace` | `{path?, hwnd?, region?, max_dim?=1600, threshold?, invert?=false, crop?, min_area?=16, smooth?=1.2, step_px?=2.0, max_strokes?=64, close_loops?=true, min_len_px?=8}` | `TraceResult`（自截时多一个 `coord`） |
| `win_record` | `{duration_ms?=5000, hz?=120, max_points?=20000}` | `RecordResult` |
| `win_ocr` | `{hwnd?, region?, language?}` | `OcrResult` |
| `win_web_cdp` | `{host?="127.0.0.1", port?=9222, ws_url?, command?, command_params?, list_targets?, new_target?, timeout_ms?=10000, include_events?=false}` | `CdpResult` |
| `win_focus_guard_probe` | `{hwnd?}` | `{foreground_before, foreground_after}`（调试用，只读，不激活窗口） |

`command` / `list_targets` / `new_target` 三选一，给零个或多个都返回 `INVALID_ARGS`。
`win_wait` 超时**不是** RPC 错误：返回 `matched=false, timed_out=true`。

### WinInfo

```json
{"hwnd": 65764, "title": "计算器", "class_name": "ApplicationFrameWindow",
 "pid": 1234, "process_name": "Calculator.exe",
 "rect": {"x": 100, "y": 200, "w": 800, "h": 600},
 "client_rect": {"x": 108, "y": 245, "w": 784, "h": 555},
 "z_order": 0, "is_visible": true, "is_minimized": false, "is_foreground": false}
```

### CaptureResult

```json
{"path": "C:\\Users\\x\\AppData\\Local\\Temp\\dsh-screen-control\\shot-7.png",
 "width": 1280, "height": 720, "bytes": 84321,
 "backend": "printwindow", "capture_ms": 41, "scaled_from": {"w": 2560, "h": 1440}}
```

用**临时 PNG 文件**回传二进制，不走管道 Base64。`max_dim=0` 表示不缩放（做模板时用）。

### ObserveResult

```json
{"snapshot_id": 7, "created_at_ms": 1730000000000, "expires_at_ms": 1730000005000,
 "hwnd": 65764, "element_count": 23, "truncated": false,
 "elements": [{"uid": 3, "name": "七", "control_type": "Button",
   "rect": {"x": 100, "y": 200, "w": 40, "h": 40},
   "is_enabled": true, "is_password": false, "depth": 2, "source": "child_window"}],
 "screenshot": CaptureResult}
```

`source` 取 `child_window`（默认）或 `uia`（开启 `--features uia` 后）。`is_password` 为真的元素被硬性拒绝输入。

### ActionResult

```json
{"delivered": "sendinput", "target_hwnd": 65764, "deepest_child": 99887,
 "coords_client": {"x": 12, "y": 30}, "sequence_ms": 51, "warnings": []}
```

`delivered` 取 `sendinput` / `clipboard` / `postmessage`。

### WaitResult

```json
{"condition": "wait_for_image", "matched": true, "timed_out": false,
 "waited_ms": 412, "polls": 3, "timeout_ms": 10000,
 "match_point": {"x": 640, "y": 480}, "score": 0.9982, "warnings": []}
```

没等到的超时结果仍带 `score`（最像的一次）与 `last_error`（轮询期间最后一次失败原因）。

### PathResult（0.2.0 新增）

`win_drag_path`：**一次按下走完整条路径**。和 `win_drag`（一段直线、每次调用都完整
按下-抬起）的本质区别：多段拼弧会断笔 + 每段独立加减速，折角一眼可见。

三种给法，优先级 `strokes` > `points` > `shape`：

```json
{"hwnd": 1509592, "shape": {"kind": "circle", "cx": 300, "cy": 980, "r": 70}, "step_px": 2}
{"hwnd": 1509592, "shape": {"kind": "arc", "cx": 800, "cy": 1000, "r": 156, "start_deg": 180, "end_deg": 360}}
{"hwnd": 1509592, "shape": {"kind": "bezier", "points": [[520,900],[700,1020],[520,1020],[610,1040]]}}
{"hwnd": 1509592, "points": [[520, 900], [700, 1020]]}
{"hwnd": 1509592, "strokes": [[[520,900],[700,1020]], [[700,900],[520,1020]]]}
```

```json
{"delivered": "sendinput", "target_hwnd": 1509592, "strokes": 1, "points": 221,
 "planned_ms": 538, "duration_ms": 565, "path_px": 448.99, "max_step_px": 2.83,
 "start_screen": {"x": 300, "y": 910}, "end_screen": {"x": 300, "y": 910},
 "timer_1ms": true, "warnings": []}
```

要点：

- **弧长匀速**：先按 `step_px` 做等弧长重采样，时间再按累计弧长分配，所以笔速恒定
  （按贝塞尔参数 t 均分会让曲率大处速度突变）。
- **1ms 定时器**：`timeBeginPeriod(1)` 在动作期间生效，`timer_1ms=false` 说明没拿到，
  时长会明显偏长（默认 15.6ms 一档，一条 200 步的弧要 3 秒）。
- **精度**：`planned_ms` 与 `duration_ms` 的差是注入开销（实测 246 点 / 601ms 计划
  → 627ms 实测，+4.3%）。
- **`max_step_px`** 只统计落笔步（不含笔间抬笔飞行），突然变大说明形状里真有断点。
- `wobble` 默认 **false**：弧线上叠 1px 法向抖动就是毛刺。
- 所有点必须落在目标窗口 rect 内，否则整条路径拒绝执行（画出界=画到别的程序上）。
- 落笔点逐笔做遮挡检查，被别的窗口盖住直接拒绝。

真机实测（mspaint 77% 缩放、4px 画笔，三条同题对比）：

| 画法 | 拟合半径 | 径向误差均值 | 最大 | 标准差 |
| --- | --- | --- | --- | --- |
| `win_drag` 6 段直弦 | 117.82 | 1.27 | 4.18 | 1.55 |
| `win_drag` 300 点拼接 | 120.06 | 1.09 | 3.39 | 1.29 |
| `win_drag_path` 圆弧（请求 r=156） | 156.19 | 1.03 | 3.04 | 1.23 |

`win_drag_path` 的圆心实测 (799.7, 998.6) vs 请求 (800, 1000)，半径偏差 +0.19px。

### 动作生效验证（0.2.1）：`verify` 参数

`win_click` / `win_press` / `win_scroll` / `win_drag` / `win_drag_path` 共用同一个可选对象。
**不传 = 完全不验证**（零开销，行为与 0.2.0 一致）；传了就在动作**之前**抓基准，
动作**之后**轮询比对，结果挂在返回值的 `effect` 字段上。

```json
{"hwnd": 1509592, "x": 57, "y": 146,
 "verify": {"mode": "pixels", "threshold": 0.002, "timeout_ms": 1200}}
{"hwnd": 1509592, "x": 44, "y": 66,
 "verify": {"mode": "window", "class_name": "Microsoft.UI.Content.PopupWindowSiteBridge"}}
```

```json
"effect": {"checked": true, "changed": false, "mode": "pixels",
           "diff_ratio": 0.0, "polls": 19, "waited_ms": 1208,
           "region": {"x": -9, "y": 0, "w": 320, "h": 320},
           "detail": "1200ms 内最大变化 0.00%，未达阈值 0.20%", "warnings": []}
```

两种判定：

- `pixels`（默认）：比对区域画面变化比例 ≥ `threshold` 才算生效。
  比对区域优先级 = `verify.region` > 动作点 ±160px（夹到窗口内）> 整窗。
  取样每 3 像素 1 个，三通道差值和 >24 才算"变了"（压掉合成噪声）。
  **动画/视频类目标必须把 threshold 调大**（默认 0.002 = 0.2%），否则画面本来就在动。
- `window`：动作后出现了**动作前不存在的**窗口（`title_contains` / `class_name` / `pid`
  至少给一个）。点开对话框/菜单/弹层用这个，比像素稳得多。动作前窗口已存在时
  结果固定 `changed=false`，`detail` 会说明原因，避免把"本来就在"当成"刚打开"。

`checked=false` 表示没验成（基准图抓取失败、window 模式没给过滤条件等），原因在
`warnings` 里；此时 `changed` 没有意义。

真机实测（2026-10-24，mspaint）：

| 场景 | mode | changed | 说明 |
| --- | --- | --- | --- |
| 点「选择」工具（工具高亮变化） | pixels | true | diff 1.397%，1 次轮询、4ms |
| **再点一次同一个已选中的工具** | pixels | **false** | diff 0%，19 次轮询 1208ms——静默无效被抓到 |
| 点「文件」菜单（弹出菜单窗） | window | true | 1 次轮询命中 `PopupWindowSiteBridge` |
| 点画布空白处（不弹窗） | window | false | 1200ms 内无新窗口 |

顺带修掉一个 0.2.0 之前就存在的 bug：`win_capture` 传 `hwnd` + 非 (0,0) 起点的 `region`
时，PrintWindow 后端返回的是**窗口左上角**那一块而不是请求区域（PrintWindow 只会整窗渲染，
旧实现按 region 尺寸建 DIB 就直接拿了左上角）。这会让区域截图和 `verify` 全部失真，
0.2.1 改成「整窗渲染后按窗口内坐标裁切」。

### 截图坐标换算（0.2.2）：`win_capture` 的 `coord` 与动作的 `shot_id`

`win_capture` 默认把长边缩到 1600，于是**返回图上的像素坐标 ≠ 屏幕坐标**。
以前这个换算系数靠调用方自己算（"截图 1400 宽 ×1.8286"），算错一位整块就点歪；
窗口一挪、区域一换，系数还得重算。

现在每次截图都会登记一条换算记录，结果里多一个 `coord`：

```json
"coord": {"shot_id": 1, "hwnd": 1509592,
          "screen_rect": {"x": -9, "y": 0, "w": 1298, "h": 1609},
          "image_w": 1291, "image_h": 1600,
          "scale_x": 1.00542, "scale_y": 1.00562, "ttl_ms": 600000}
```

动作工具（`win_click` / `win_scroll` / `win_drag` / `win_drag_path`）传 `shot_id` 之后，
**所有坐标字段都按那张截图的像素解释**，引擎负责换算：

```json
{"shot_id": 1, "x": 300, "y": 145}                       // 点截图里的 (300,145)
{"shot_id": 1, "from_x": 350, "from_y": 700, "to_x": 450, "to_y": 800}
{"shot_id": 1, "shape": {"kind": "circle", "cx": 300, "cy": 500, "r": 80}}  // 半径按两轴平均缩放
```

规则与边界：

- `shot_id` 只改变**坐标的解释方式**，不改别的语义；不传就还是屏幕物理像素（向后兼容）。
- 传了 `shot_id` 而没给 `hwnd`，就用截图时那个窗口当目标。
- 换算公式：`screen = screen_rect.origin + image_px * scale`，x/y 两轴分别换算
  （缩放按长边等比，两轴系数可能差千分之几，一个系数套两轴会在边缘累积偏移）。
- 图左上角对应 `screen_rect` 左上角；`image_w/h` 是**落盘那张图**的尺寸。
- 生命周期 10 分钟、最多 32 张（超了淘汰最旧）；过期/淘汰/引擎重启后 `shot_id` 失效，
  报 `INVALID_ARGS` 并提示重新截图。

真机实测（2026-10-24，mspaint 1298×1609 窗口、截图 1291×1600）：
`shot_id + (300,145)` → 点击落到屏幕 `(293,146)`；`points [[350,700],[450,800]]`
→ 画出的线起终点实测 `(343,704)`–`(443,805)`，与公式一致。

### TraceResult（0.2.3 新增）：图片 → 笔顺

把图（截图或 PNG 文件）里的线条变成**可直接画的笔顺**，坐标是图片像素：
`win_capture`（或本工具自截）拿到的 `coord.shot_id` 和 `strokes` 一起喂给
`win_drag_path`，引擎负责换算成屏幕坐标，模型完全不用碰缩放系数。

```json
{"hwnd": 1509592, "region": {"x": 66, "y": 450, "w": 1044, "h": 750}}
{"path": "C:/refs/lineart.png", "smooth": 1.5, "min_area": 24}
```

```json
{"stroke_count": 31, "total_points": 1575, "total_px": 3054.17,
 "image_w": 1044, "image_h": 750, "threshold": 140,
 "ink_px": 10855, "skeleton_px": 3186, "dropped_blobs": 4, "trace_ms": 12,
 "strokes": [{"points": [[634,449],[632.3,450.05], ...], "length_px": 447.78, "closed": false}, ...],
 "warnings": []}
```

流水线（全在引擎里，一次调用）：

1. **灰度 + Otsu 二值化**（可 `threshold` 手给；`invert` 处理"黑影白线"）。
   判据是 `v <= 阈值` 算墨——Otsu 在"白底少量纯黑线"的图上会算出 0，用 `<` 会漏掉整条线。
2. **去小连通块**（`min_area`，8 邻接）。
3. **Zhang-Suen 细化**成 1px 骨架，避免 4px 笔迹被抽成两条平行笔顺。
4. **提笔顺**：贪心 + 惯性走法（每步挑方向最接近前进方向的未访问邻居），
   交叉点直穿、环路绕一圈成一笔。
5. **断线接回**：端点距离 ≤3px 的笔顺按走向拼接（骨架在斜边台阶处会断）。
6. **丢冗余碎片**：与更长笔顺 80% 重叠的（45° 台阶残留）直接丢掉。
7. **Douglas-Peucker 简化**（`smooth`）→ **等距重采样**（`step_px`）→ 闭合判定。

要点：

- 输出点已压到 2 位小数（笔顺动辄上千点，全精度 JSON 白白膨胀）。
- `closed=true` 表示首尾已对齐（末点 = 首点），直接画就是闭合曲线。
- 笔顺按**长度从长到短**排序并限量（`max_strokes`）：先主干后细节。
- 真机实测（mspaint 1044×750 画布）：11 笔涂鸦 → 31 条笔顺 / 1575 点 / **12ms**；
  其中原来的手画圆被识别成 1 条 `closed=true`、`length_px=442.99` 的笔顺
  （理论周长 2π×70=439.8，误差 0.7%），圆心 (233,528) 与画布上的实际圆心一致。

### RecordResult（0.2.4 新增）：录制回放

录的是**人手的真实鼠标轨迹**：按住左键期间的点被采样成笔顺，抬键即断笔。
每个点带毫秒时间戳，把它交给 `win_drag_path.times_ms` 就能按原速重放。

```json
{"duration_ms": 2200, "hz": 120}          // win_record
```

```json
{"duration_ms": 2200, "samples": 256, "hz": 120, "total_px": 233.1,
 "strokes": [{"points": [[475,1268],[476,1267],...], "times_ms": [0,8.3,...]}],
 "warnings": []}
```

```json
{"hwnd": 1509592,
 "strokes": [[[475,1268],[476,1267], ...]],
 "times_ms": [[0, 8.3, ...]]}             // win_drag_path：按原速重放
```

要点：

- **实现是轮询**（`GetCursorPos` + `GetAsyncKeyState`，默认 120Hz），不装
  WH_MOUSE_LL 钩子：钩子要自己起消息泵线程、回调里不能做重活、超时会被系统摘掉。
  轮询对画线足够，且完全不改变系统状态。
- **调用会阻塞 `duration_ms`**，期间引擎不处理别的请求——故意的：录的时候不该有
  别的注入污染轨迹。
- 录制结束后时间轴整体平移，让第一个点为 `0ms`（录制前的等待不该让重放先干等）。
- `times_ms` 存在时 `win_drag_path` **跳过重采样**（时间戳按原始点一一对应，重采样会
  改变点数），并按时间表逐点发；`duration_ms` 被忽略。返回里 `timed=true`。
- 只有一笔有 `times_ms`、另一笔没有时会整段退回匀速（避免半程两套时间轴）。
- 实测（2026-10-25）：另一进程注入 1200ms 的圆弧，录制到 1 笔 / 109 点 /
  时间戳 0→1198ms，重放 `planned_ms=1198`、`elapsed_ms=1224`（+2.2% 注入开销）。

### FindResult（0.1.9 新增）

```json
{"found": true, "score": 0.9982,
 "x": 640, "y": 480, "w": 120, "h": 40, "cx": 700, "cy": 500,
 "origin": {"x": 600, "y": 460},
 "searched_region": {"x": 600, "y": 460, "w": 900, "h": 600},
 "elapsed_ms": 37, "warnings": []}
```

- `x/y` = `origin` + 模板匹配偏移，屏幕物理像素；`w/h` = **缩放后**的模板尺寸；
  `cx/cy` = 中心取整。
- `found=false` 是正常结果（**不是** RPC 错误）：`x/y/score` 仍是最佳候选，
  调用方必须先看 `found` 再用坐标。
- `region` 越界时与捕获区域求交（不是直接失败），裁剪结果写在 `warnings` 里；
  交完为空才返回 `INVALID_ARGS`。
- 模板文件不存在 → `NOT_FOUND`；模板解码失败 / 比搜索区域还大 / `min_score` 越界 → `INVALID_ARGS`。
- **一次性搜索、不轮询**：要等某样东西出现请用 `win_wait(condition="image")`。

### OcrResult

```json
{"text": "OCR TEST 12345", "lines": [{"text": "OCR TEST 12345",
   "rect": {"x": 20, "y": 145, "w": 420, "h": 60}}],
 "language": "en-US", "available_languages": ["en-US", "zh-Hans-CN"],
 "origin": {"x": 100, "y": 200}, "width": 640, "height": 440,
 "ocr_ms": 121, "capture_ms": 33, "backend": "printwindow", "path": "..."}
```

`lines[].rect` 相对截图左上角，加上 `origin` 才是屏幕坐标。本机没装语言包/引擎不可用 → `UNSUPPORTED`。

## 命令行一次性模式（0.1.9 新增）

同一个 exe 除了 `--pipe` 常驻模式，还支持两个**一次性**子命令。它们**不走命名管道、
不启动 server、不抢单实例锁**，与 `--pipe` 互斥；跑完按约定退出码退出。
坐标同样一律屏幕物理像素。

```text
screen-control-engine find --template <模板PNG绝对路径> [--hwnd N] [--region x,y,w,h]
                           [--min-score 0.9] [--scale 1.0] [--log-level 级别]

screen-control-engine capture --out <绝对PNG路径> [--hwnd N] [--region x,y,w,h]
                             [--log-level 级别]
```

`find` 的 stdout **只有一行 JSON**（参数与 `win_find` 相同，输出字段固定在下面这个形状）：

```json
{"ok":true,"found":true,"x":640,"y":480,"w":120,"h":40,"cx":700,"cy":500,
 "score":0.9982,"origin":{"x":600,"y":460},"elapsed_ms":37}
```

退出码：

| find | capture | 含义 |
| --- | --- | --- |
| 0 | 0 | 找到 / 截图成功 |
| 3 | — | 没找到（stdout 同样是上面那行 JSON，只是 `found=false`） |
| 2 | 2 | 错误：stdout 输出 `{"ok":false,"error":"..."}`，stderr 再给人话说明 |

`capture` 成功时 stdout：

```json
{"ok":true,"path":"C:\\Users\\x\\testbed.png","width":640,"height":440}
```

`capture` 存的是**原尺寸** PNG（不缩放，方便直接当模板用）；`--out` 必须是绝对路径。
`--help` 打印完整用法（含常驻模式）并以 0 退出；无参数时打印用法并以 2 退出。

## win_ocr 的子进程隔离（0.1.9）

真机记录过 `win_ocr` 不带 region 整屏识别时引擎以 `0xC0000005` 退出（提权 + 游戏全屏会话）。
该崩溃发生在操作系统代码里（GDI `BitBlt` / `Windows.Media.Ocr` 的 WinRT 调用），
进程内无法用 `catch_unwind` 兜住（访问冲突不是 panic），所以 0.1.9 起：

- 整条"截图 + OCR"路径在**短命子进程**里执行（同一个 exe 的内部 `--ocr-worker` 模式）；
- 子进程崩了，引擎只拿到一个非零退出码，转成结构化 `ENGINE_ERROR` 返回，
  引擎本体/当前会话/其它请求都不受影响；
- 子进程超过 20s 未返回 → 强制结束并返回 `TIMEOUT`；
- 图片超过本机 OCR 引擎上限（`OcrEngine::MaxImageDimension`）→ `UNSUPPORTED`（提示用 region 缩小），
  不会把超限图送进 OCR 引擎。

代价是每次 `win_ocr` 多一次进程 spawn（约 30-80ms）。`win_capture(ocr=true)` 与
`win_wait(condition="text")` 仍然在引擎内直接调用 OCR（同一条 `recognize()` 实现，
同样有尺寸护栏），不走子进程。

## win_type 的自动剪贴板（0.1.9）

`method="auto"`（默认）且文本含**非 ASCII**（中文/emoji/全角标点等）时：

1. 引擎自己把文本写成 `CF_UNICODETEXT`；
2. 发 Ctrl+V（真注入，走同一套前台守卫）；
3. 返回 `delivered="clipboard"`，并在 `warnings` 里明说"剪贴板内容已被这段文本覆盖"。

`KEYEVENTF_UNICODE` 逐字符注入中文长串真机实测会偶发串位，剪贴板是同一系统里更稳的路。
写剪贴板失败（被别的程序占着等）→ 自动退回逐字符 Unicode 注入 + warning。
显式 `method="sendinput"` / `method="postmessage"` / `method="clipboard"` 语义不变。

## 快照校验

- `win_observe` 生成的 `snapshot_id` 默认 TTL 5000ms。
- 带 `snapshot_id` + `uid` 的动作:引擎查表,过期返回 `STALE_SNAPSHOT`,并要求重新 observe。
- 不带 `snapshot_id` 时必须显式给 `hwnd` + 坐标/文本,否则 `INVALID_ARGS`。

## SendInput 的两道闸门(2026-10-16 起)

`SendInput` 是"注入到系统输入流",它不认识目标窗口 —— 只认当前前台窗口和光标位置。
真注入路径在真正注入之前必须过闸门,不合格一律回 `UNSUPPORTED`。

- **键盘**(`win_type` 的默认/显式 sendinput 路径、`win_press`、剪贴板 Ctrl+V):目标顶层窗口
  必须**就是当前前台窗口**(引擎会先 `AttachThreadInput` + `SetForegroundWindow` 把它顶上去,
  注入完再还现场)。目标不在前台又抢不过来时拒绝——否则按键会原封不动打进用户正在用的
  那个窗口。错误信息里会带上"会落进哪个窗口"。
- **鼠标**(`win_click`):投递坐标必须是**显式算出**的屏幕点
  (uid 路径下引擎自己把客户区坐标换算成屏幕坐标),而且该点经 `WindowFromPoint`
  命中测试后必须仍属于目标顶层窗口,否则拒绝(点被别的窗口盖住了)。

两道闸门都排在密码框判定**之后**:先看是不是密码框(`PASSWORD_FIELD`),再看能不能投递。
**默认 `method="auto"` 走真注入**（不是 postmessage）；只有显式 `method="postmessage"` 才走 WM_CHAR、不碰前台。


## 取消控制管道（0.2.5）

引擎是**单线程串行**处理请求的：主管道在处理一个长操作（`win_drag_path` / `win_wait` /
`win_record`）期间读不到第二条请求。所以取消走一条独立的控制管道：

- 名称：主管道名 + `-ctl`（例：`\\.\pipe\dsh-screen-control-1234-abcd-ctl`）；
- 方向：TS → 引擎，一行一个 JSON，只认 `{"cancel": <请求 id>}`；
- 引擎侧：独立线程读取，把 id 写进取消位；长操作在自己的循环里检查"当前请求是否被取消"；
- 取消命中后，长操作返回 `TIMEOUT`，message 以 `已取消：` 开头（IPC 契约只允许 7 个错误码，
  不新增第 8 个）。停在安全边界：已按下的鼠标 / 键盘一定先抬起（ButtonHold / KeyHold 的 Drop）；
- 控制管道连不上时，取消退化为旧行为（只取消调用方等待），不影响功能。

TS 侧在 `signal.abort` 或调用超时时会尽力发一条取消帧。
