# 更新日志

本文件记录本插件的所有重要变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [0.2.5] - 2026-10-30

按 GPT 的静态安全审查（21 + 8 项）逐条核对后做的本地加固。只改本地，未推送。

### 安全

- `win_web_cdp` 的 `host` 与 `ws_url` 里的 host 现在只接受回环地址（localhost / 127.0.0.0-8 / ::1）。调试端口等于浏览器控制权，之前模型或提示注入可以把 host 指向任意地址（审查第 01 条）。
- 命名管道首个实例改用 `first_pipe_instance(true)`：同名管道被别的进程抢先创建时直接失败，不再可能冒充引擎（第 25 条）。
- 图片来源加了硬上限：`win_image_trace` 的 `path`、`win_wait` 的模板图都先看文件大小（64MB）再解码，解码后要求不超过 1 亿像素。超大图片不再能把引擎内存吃光（第 28 条）。
- 半行请求 30s 没补齐就断开连接：单个卡死客户端不会再占死唯一的串行服务连接（第 26 条）。

### 崩溃与状态

- `win_capture(ocr=true)` 与 `win_wait(text)` 改走 OCR 隔离子进程。之前这两条路径在引擎进程里跑 WinRT OCR，整屏识别一崩就是 0xC0000005、整个引擎消失（第 22 条）。
- `InjectionGuard` 加了 Drop 兜底：注入中途返回 Err 也会还原光标和前台（第 04 条）。
- 鼠标按下-抬起用 ButtonHold 兜底：中途失败会补发抬起，不会把按键卡在按下状态；drag / drag_path / click 三处都接上了（第 05 条）。
- `times_ms` 回放校验有限值/非负/单调，并按"整段录制绝对时间轴"解释。旧实现把上一笔末时间戳再累加给下一笔，多笔回放时长会成倍拉长（第 09、24 条）。
- `duration` 计算前先卡原始点数（20 万）并拒绝非有限坐标，`PATH_MAX_REPLAY_MS` 兜住极端时间戳，不再有 `Duration::from_secs_f64` panic（第 23 条）。
- `exit_fullscreen` 先恢复、成功后才删恢复记录；扩展样式也回读校验（`WS_EX_TOPMOST` 除外，它只能由 SetWindowPos 设置）。恢复失败不再永久丢失原始样式（第 29 条）。
- 快照 TTL 上限 10 分钟，避免 expires 溢出与永久不过期（第 20 条）。

### 正确性

- 子窗口枚举改用 `GetWindow(GW_CHILD)` + `GW_HWNDNEXT`（只取一层）。旧实现用 `EnumChildWindows`（会递归整棵子树）当"直接子窗口"，再递归展开，导致控件重复、depth 错乱、预算被吃两遍；BFS 也改成真正的 `VecDeque` 队列并加了 hwnd 去重（第 02 条）。
- `deepest_child_at` 每下钻一层都重新做屏幕→客户区换算。旧实现沿用第一层的坐标，深层控件会命中错的目标（第 03 条）。
- `verify` 的 `threshold=0` 不再等于"永远生效"；非有限阈值退回默认值（第 10、14 条）。
- `win_record` 的 `max_points` 改成整次录制总点数上限，不再只在单笔内生效（第 19 条）。
- `package.json` 描述不再写"不激活前台窗口"：键盘注入会临时激活并还原（第 21 条）。

### 发布一致性

- CI 现在跑 `cargo test --release` + `cargo build --release`，并用 `sync-engine.mjs --check --require-built` 强制 bin/ 里的 exe 与现编产物逐字节一致。之前 CI 完全不碰 Rust，旧引擎顶包无人能发现（第 06、07 条）。
- publish workflow 也先现编引擎再发布。
- `npm run acceptance:strict`：有跳过项时返回非 0（默认仍允许跳过，CI 用 strict）（第 16 条）。
- selfcheck 的 mock 闭环用例补齐到 18/18 个工具，并加了一条"每个工具都必须有 execute 用例"的自检（第 15 条）。

### 取消

- 真取消（第 27 条）：引擎加了一条独立控制管道 `<pipe>-ctl`，由后台线程收 `{"cancel": id}`；`win_drag` / `win_drag_path` / `win_wait` / `win_record` 在循环里检查取消位，停在安全边界（鼠标/键盘先抬起）。TS 侧 abort 时发出取消帧。实测 `win_wait` abort 后 400ms 调用方返回、引擎 504ms 收尾且继续可用。

### 其它

- 键盘主按键释放失败也会补发抬起（KeyHold/ModHold 的 Drop，第 08 条）：旧实现只释放修饰键。
- 命名管道专用 DACL（第 25 条）：SDDL `D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)`，只允许 系统/管理员/创建者 连接；`D:` 表示 DACL 受保护。
- 新增真窗口回归测试（第 11-13 条）：测试里用 `CreateWindowExW` 建父子窗口树，验证「只取一层 / BFS 深度与去重 / 逐层坐标换算」；另加回放时间轴单测（多笔绝对时间戳）与取消状态机单测。
- 默认 feature 用测试钉住（第 18 条）：`default_build_has_ocr_but_not_uia`，谁改 Cargo.toml 默认值都会红。
- `docs/IPC.md` 补「取消控制管道」一节。

### 验证

- `cargo test --release`：68 通过，0 失败，1 忽略
- `npm run typecheck`：通过
- `npm run selfcheck`：0 失败 0 跳过（含 18/18 工具的 mock 闭环、真引擎取消链路）
- `npm run acceptance`：22 项检查 0 失败，1 项跳过（本会话已提权，键盘闸门按设计放行）
- `node scripts/sync-engine.mjs --check --require-built`：bin 引擎与现编产物逐字节一致

### 仍未处理

- CI 里的 TypeScript 类型检查：devDependencies 用的是本机 `file:` 链接，GitHub runner 上装不上，暂时仍只在本地跑（`npm run typecheck`）。
- 真机 `npm run acceptance:strict` 需要交互桌面，本地跑，不进 CI。

## [0.2.4] - 2026-10-25

首个对外发布版本。

### 工具（18 个）

- 观察：`win_engine_status`、`win_list_windows`、`win_capture`、`win_observe`、`win_ocr`、`win_find`、`win_wait`
- 操作：`win_click`、`win_type`、`win_press`、`win_scroll`、`win_drag`、`win_drag_path`、`win_record`
- 窗口：`win_launch`、`win_window_state`
- 网页：`win_web_cdp`（走 Chrome DevTools Protocol，不靠点像素）

### 引擎

- Rust 常驻进程，通过命名管道走 NDJSON 通信，一次启动长期复用，避免每次调用重新加载
- 观察走 `PrintWindow`，被遮挡或不在前台的窗口也能截图，不需要抢前台
- 动作走 `SendInput` 真注入，带生效验证（像素比对 / 新窗口检测）
- 高 DPI：按屏幕物理像素工作，自动换算缩放系数

### 修复

- `win_scroll` 的 `delta` 单位说明写错了：旧版描述写成「一格 120，`-360` 表示向下滚 3 格」，模型照此传 `-360` 会被引擎当成 360 格（截断到 100 格），长页面一滚到底、中间内容全跳过。实际单位是滚轮格数，实测 1 格约 3 行文字。描述已改为「单位是格，建议一次 1~5 格，不要传 120 的倍数」。

### 质量

- `cargo test`：61 项通过，0 失败，1 项忽略
- `npm run selfcheck`：全项通过
- `npm run acceptance`：22 项验收，0 失败

[0.2.4]: https://github.com/ShawwiMA/dsh-plugin-screen-control/releases/tag/v0.2.4
