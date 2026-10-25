# 更新日志

本文件记录本插件的所有重要变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

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

### 质量

- `cargo test`：61 项通过，0 失败，1 项忽略
- `npm run selfcheck`：全项通过
- `npm run acceptance`：22 项验收，0 失败

[0.2.4]: https://github.com/ShawwiMA/dsh-plugin-screen-control/releases/tag/v0.2.4
