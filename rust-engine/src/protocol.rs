//! IPC 协议的唯一真源：所有跨进程出现的 serde 类型都定义在这里。
//!
//! 契约文档是 `docs/IPC.md`。改本文件任何一个字段名之前，先改那份文档。
//! 其它模块只允许 `use crate::protocol::...`，不许自己另起一套结构体
//! （否则 TS 侧会收到两种形状的 JSON）。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// 帧
// ---------------------------------------------------------------------------

/// 请求帧：`{"id":42,"method":"win_click","params":{...}}`。
///
/// `params` 故意保持成自由 JSON，由各方法自己解析。
/// 如果这里定成强类型 enum，任何一个方法加字段都要改协议层，
/// 而且未知字段会直接让整条请求反序列化失败——对调试很不友好。
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    /// 正整数、单调递增；响应必须原样回填（包含 log 之外的任何分支）。
    pub id: u64,
    pub method: String,
    /// 缺省为 `{}`，因此用 `default` 兜住没有 params 字段的请求。
    #[serde(default)]
    pub params: Map<String, Value>,
}

/// 响应/事件三选一。`derive(Untagged)` 让序列化结果严格贴合 IPC.md 的三种样例，
/// 不会多出 `type` 之类的判别字段。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Frame {
    /// `{"id":42,"ok":true,"result":{...}}`
    Ok { id: u64, ok: bool, result: Value },
    /// `{"id":42,"ok":false,"error":{"code":"...","message":"..."}}`
    Err { id: u64, ok: bool, error: RpcError },
    /// `{"event":"log","level":"info","message":"..."}`，无 id，TS 侧只记日志。
    Event(EventFrame),
}

/// 错误载荷。`code` 必须是 [`ErrorCode`] 里那七个字符串之一。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

impl RpcError {
    pub fn new(code: crate::error::ErrorCode, message: impl Into<String>) -> Self {
        Self { code: code.as_str().to_string(), message: message.into() }
    }
}

/// 事件帧。字段顺序即序列化顺序，方便人肉比对日志。
#[derive(Debug, Clone, Serialize)]
pub struct EventFrame {
    pub event: String,
    pub level: String,
    pub message: String,
}

impl EventFrame {
    pub fn log(level: LogLevel, message: impl Into<String>) -> Self {
        Self { event: "log".to_string(), level: level.as_str().to_string(), message: message.into() }
    }
}

/// 事件级别。目前只有 info/warn/error 三档，够用即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
        }
    }

    /// `--log-level` 的解析入口。大小写不敏感，未知值返回 None 让调用方报错。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "error" => Some(LogLevel::Error),
            "warn" | "warning" => Some(LogLevel::Warn),
            "info" => Some(LogLevel::Info),
            "debug" | "trace" => Some(LogLevel::Debug),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 基础几何
// ---------------------------------------------------------------------------

/// 矩形。对外一律是**屏幕物理像素**（virtual-screen 坐标系）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
}

/// 缩放来源尺寸。只有真的缩过图才出现在 `CaptureResult.scaled_from` 里。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

// ---------------------------------------------------------------------------
// engine_ping
// ---------------------------------------------------------------------------

/// `engine_ping` 的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResult {
    pub ok: bool,
    pub version: String,
    pub pid: u32,
    /// 是否成功把进程设成 PER_MONITOR_AWARE_V2。false 说明坐标可能不是物理像素，
    /// TS 侧应当据此降级而不是当作正常值使用。
    pub dpi_aware: String,
    /// 显示器数量。省得 TS 侧再去数一遍 monitors 数组。
    pub monitor_count: u32,
    pub monitors: Vec<MonitorInfo>,
    pub uptime_ms: u64,
}

/// 一台显示器。`dpi` 来自 GetDpiForMonitor，拿不到时为 None。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// HMONITOR 的数值形式。TS 侧只用来做去重/标识，不要当句柄用。
    pub handle: u64,
    pub device_name: String,
    /// 枚举顺序（0 起），TS 侧的稳定标识。
    pub index: u32,
    pub rect: Rect,
    pub work_rect: Rect,
    #[serde(rename = "is_primary")]
    pub is_primary: bool,
    pub dpi: Option<u32>,
}

// ---------------------------------------------------------------------------
// win_list_windows / WinInfo
// ---------------------------------------------------------------------------

/// `win_list_windows` 的过滤条件。全部可选，缺省即不过滤。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WindowFilter {
    /// 子串匹配，大小写不敏感。JSON 字段名与工具参数一致（title_contains）。
    pub title_contains: Option<String>,
    /// 精确匹配类名（大小写敏感，类名本来就是精确标识）。
    pub class_name: Option<String>,
    pub pid: Option<u32>,
    /// 进程可执行文件名（不含路径），大小写不敏感；`"QQ"` 与 `"QQ.exe"` 视为同一个名字。
    /// 为什么要有：查进程名比查 pid 少一次 `tasklist` 往返（见 SKILL.md 第 0 节）。
    pub process_name: Option<String>,
    #[serde(default)]
    pub visible_only: Option<bool>,
    #[serde(default)]
    pub top_level_only: Option<bool>,
}

/// 窗口快照。字段与 IPC.md 的 WinInfo 样例一一对应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinInfo {
    pub hwnd: u64,
    pub title: String,
    pub class_name: String,
    pub pid: u32,
    /// 拿不到时为 None（进程已退出、权限不足、受保护进程等）。
    pub process_name: Option<String>,
    pub rect: Rect,
    /// 客户区在屏幕坐标系里的矩形。
    pub client_rect: Rect,
    /// 顶层窗口的 Z 序，0 表示最靠前；子窗口固定为 0。
    pub z_order: i32,
    pub is_visible: bool,
    pub is_minimized: bool,
    pub is_foreground: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowListResult {
    pub windows: Vec<WinInfo>,
}

// ---------------------------------------------------------------------------
// win_capture / CaptureResult
// ---------------------------------------------------------------------------

/// 截图后端。`auto` 只出现在请求里；结果里的 backend 一定是具体后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureBackend {
    Auto,
    PrintWindow,
    Bitblt,
}

impl CaptureBackend {
    /// 序列化到 JSON 时用的字面量。手写而不用 serde 的 rename，
    /// 是因为 `Bitblt` 想要的是 `"bitblt"` 而不是 `"bitblt"` 之外的任何变体。
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureBackend::Auto => "auto",
            CaptureBackend::PrintWindow => "printwindow",
            CaptureBackend::Bitblt => "bitblt",
        }
    }
}

/// 图片格式。阶段一只承诺 png。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
}

impl ImageFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
        }
    }
}

/// 截图结果。阶段一用临时 PNG 文件回传二进制，不走管道 Base64。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureResult {
    /// 绝对路径。
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub backend: String,
    pub capture_ms: u64,
    /// 只有发生缩放时才出现。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scaled_from: Option<Size>,
}

// ---------------------------------------------------------------------------
// win_observe / ObserveResult
// ---------------------------------------------------------------------------

/// 元素来源。`child_window` 是阶段一（EnumChildWindows），`uia` 需要 --features uia。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementSource {
    ChildWindow,
    Uia,
}

impl ElementSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ElementSource::ChildWindow => "child_window",
            ElementSource::Uia => "uia",
        }
    }
}

/// 单个可交互元素。`uid` 只在所属 snapshot 内有效。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementInfo {
    pub uid: u64,
    pub name: String,
    pub control_type: String,
    /// 屏幕物理像素。
    pub rect: Rect,
    pub is_enabled: bool,
    /// 为真时引擎硬性拒绝一切输入（返回 PASSWORD_FIELD）。
    pub is_password: bool,
    /// 相对根窗口（`hwnd`）的层级，根窗口的直属子窗口深度为 1。
    pub depth: u32,
    pub source: ElementSource,
    /// 元素自身窗口句柄。动作路径要靠它直接投递，避免重新做一遍命中测试。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hwnd: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveResult {
    pub snapshot_id: u64,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub hwnd: u64,
    pub element_count: u32,
    /// 因为 max_elements / max_depth 被截断时为 true。
    pub truncated: bool,
    pub elements: Vec<ElementInfo>,
    /// with_screenshot=false 时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<CaptureResult>,
}

// ---------------------------------------------------------------------------
// 动作类方法的参数与结果
// ---------------------------------------------------------------------------

/// 输入投递方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// 由引擎自动选择。2026-10-16 起等价于 SendInput（真注入）：
    /// 原来的"先 postmessage、需要时降级"会把 Qt 自绘窗口的静默失败当成成功
    /// （消息进了队列、返回非零、界面不动）。
    Auto,
    PostMessage,
    SendInput,
    Clipboard,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Auto => "auto",
            Delivery::PostMessage => "postmessage",
            Delivery::SendInput => "sendinput",
            Delivery::Clipboard => "clipboard",
        }
    }
}

/// 鼠标按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// 键盘修饰键。用于 win_press 的 modifiers 和组合键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modifier {
    Ctrl,
    Control,
    Alt,
    Shift,
    Win,
}

/// `win_click` 参数。
#[derive(Debug, Clone, Deserialize)]
pub struct ClickParams {
    #[serde(default)]
    pub snapshot_id: Option<u64>,
    #[serde(default)]
    pub uid: Option<u64>,
    /// 不带 snapshot 时必须给 hwnd。
    #[serde(default)]
    pub hwnd: Option<u64>,
    /// 屏幕物理像素。给了 hwnd 但没给 x/y 时，默认点窗口客户区中心。
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub button: Option<MouseButton>,
    #[serde(default)]
    pub clicks: Option<u32>,
    /// 兼容字段：2026-10-16 起 click 默认就是 SendInput，传不传都一样。
    #[serde(default)]
    pub force_sendinput: Option<bool>,
    /// 动作生效验证（0.2.1）。给了就在动作前抓基准、动作后轮询比对。
    #[serde(default)]
    pub verify: Option<EffectParams>,
    /// 截图坐标（0.2.2）：给了它，x/y 按这张截图的像素解释，引擎换算成屏幕坐标。
    #[serde(default)]
    pub shot_id: Option<u64>,
}

/// `win_type` 参数。
#[derive(Debug, Clone, Deserialize)]
pub struct TypeParams {
    #[serde(default)]
    pub snapshot_id: Option<u64>,
    #[serde(default)]
    pub uid: Option<u64>,
    #[serde(default)]
    pub hwnd: Option<u64>,
    pub text: String,
    #[serde(default)]
    pub method: Option<Delivery>,
}

/// `win_press` 参数。`key` 是 "enter" / "f5" / "a" 这类可读名，由 input.rs 归一化。
#[derive(Debug, Clone, Deserialize)]
pub struct PressParams {
    #[serde(default)]
    pub snapshot_id: Option<u64>,
    #[serde(default)]
    pub uid: Option<u64>,
    #[serde(default)]
    pub hwnd: Option<u64>,
    pub key: String,
    #[serde(default)]
    pub modifiers: Vec<Modifier>,
    #[serde(default)]
    pub repeat: Option<u32>,
    /// 动作生效验证（0.2.1）。press 没有坐标，默认比对整窗（慢），建议显式给 region。
    #[serde(default)]
    pub verify: Option<EffectParams>,
}

/// `win_scroll` 参数。`delta` 按“格”计，正数向上滚。
#[derive(Debug, Clone, Deserialize)]
pub struct ScrollParams {
    #[serde(default)]
    pub snapshot_id: Option<u64>,
    #[serde(default)]
    pub uid: Option<u64>,
    #[serde(default)]
    pub hwnd: Option<u64>,
    pub delta: i32,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    /// 截图坐标（0.2.2）：给了它，x/y 按这张截图的像素解释。
    #[serde(default)]
    pub shot_id: Option<u64>,
    /// 动作生效验证（0.2.1）。
    #[serde(default)]
    pub verify: Option<EffectParams>,
}

/// 动作结果。`delivered` 取值见 [`Delivery::as_str`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub delivered: String,
    pub target_hwnd: u64,
    /// 真正收到消息的最深子窗口。和 target_hwnd 相同说明没找到更深的子窗口。
    pub deepest_child: u64,
    /// 投递时使用的客户区坐标（屏幕坐标经 ScreenToClient 换算后）。
    pub coords_client: ClientPoint,
    pub sequence_ms: u64,
    /// 动作生效验证结果（0.2.1）。调用方没给 `verify` 时为 null。
    #[serde(default)]
    pub effect: Option<EffectResult>,
    /// 非致命问题，例如“窗口无响应，已改用 SendInput”。
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ClientPoint {
    pub x: i32,
    pub y: i32,
}

// ---------------------------------------------------------------------------
// win_capture / win_observe 参数
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct CaptureParams {
    /// 省略时截取整个虚拟桌面（所有显示器并集）。
    #[serde(default)]
    pub hwnd: Option<u64>,
    /// 屏幕物理像素。给了 region 就以它为准，忽略 hwnd 的位置（hwnd 仍用于 PrintWindow）。
    #[serde(default)]
    pub region: Option<Rect>,
    /// 最长边上限，默认 1600；给 0 表示不缩放。
    #[serde(default)]
    pub max_dim: Option<u32>,
    #[serde(default)]
    pub format: Option<ImageFormat>,
    #[serde(default)]
    pub backend: Option<CaptureBackend>,
    /// true 时在**未缩放**的原始像素上跑一次 Windows.Media.Ocr，把结果并进
    /// `CaptureResult.ocr`。默认 false：OCR 要几十上百毫秒，不该让每次截图都付这个钱。
    #[serde(default)]
    pub ocr: Option<bool>,
    /// OCR 语言（例如 zh-Hans-CN）；不给用系统语言。
    #[serde(default)]
    pub ocr_language: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObserveParams {
    pub hwnd: u64,
    #[serde(default)]
    pub max_depth: Option<u32>,
    #[serde(default)]
    pub max_elements: Option<u32>,
    #[serde(default)]
    pub with_screenshot: Option<bool>,
    #[serde(default)]
    pub snapshot_ttl_ms: Option<u64>,
}

/// `win_focus_guard_probe` 参数与结果（调试用）。
#[derive(Debug, Clone, Deserialize)]
pub struct FocusProbeParams {
    #[serde(default)]
    pub hwnd: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusProbeResult {
    pub foreground_before: u64,
    pub foreground_after: u64,
}

// ---------------------------------------------------------------------------
// Point：屏幕物理像素点
// ---------------------------------------------------------------------------

/// 屏幕物理像素点。和 [`ClientPoint`] 分开定义是刻意的：一个是屏幕坐标、
/// 一个是客户区坐标，混用是坐标类 bug 的主要来源，类型不同能让编译器帮忙看着。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

// ---------------------------------------------------------------------------
// win_launch
// ---------------------------------------------------------------------------

/// `win_launch` 参数。只负责把进程拉起来，**不**等窗口出现——
/// "等窗口"是 win_wait 的职责，两者分开才不会出现"启动工具卡 30 秒"。
#[derive(Debug, Clone, Deserialize)]
pub struct LaunchParams {
    /// 可执行文件路径，或 PATH 里的名字（`notepad.exe` / `notepad`）。
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// 控制台程序用：true 时加 CREATE_NO_WINDOW，不弹控制台黑框。
    #[serde(default)]
    pub no_window: Option<bool>,
    /// 拉起后等多久再检查进程是否还活着（默认 30ms；显式给 0 表示不检查）。
    #[serde(default)]
    pub check_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchResult {
    pub pid: u32,
    /// 调用方给的原样值。
    pub path: String,
    /// 引擎在文件系统 / PATH 里解析到的绝对路径；解析不到时是 None（**不代表**启动失败）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 检查时刻进程是否还在。false 说明它启动后立刻退出了（例如参数不对），
    /// 这时候窗口永远等不到，调用方应当据此放弃，而不是去 win_wait 干等。
    pub still_running: bool,
    pub checked_after_ms: u64,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// win_window_state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowStateAction {
    Maximize,
    Minimize,
    Restore,
    /// 无边框 + 置顶的伪全屏（`SetWindowPos` 到显示器矩形）。
    Fullscreen,
    /// 退出全屏：还原 fullscreen 之前记录的样式 / 位置 / 尺寸。
    #[serde(rename = "exit_fullscreen", alias = "exitfullscreen")]
    ExitFullscreen,
}

impl WindowStateAction {
    pub fn as_str(self) -> &'static str {
        match self {
            WindowStateAction::Maximize => "maximize",
            WindowStateAction::Minimize => "minimize",
            WindowStateAction::Restore => "restore",
            WindowStateAction::Fullscreen => "fullscreen",
            WindowStateAction::ExitFullscreen => "exit_fullscreen",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WindowStateParams {
    pub hwnd: u64,
    pub action: WindowStateAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowStateResult {
    pub hwnd: u64,
    pub action: String,
    pub is_visible: bool,
    pub is_minimized: bool,
    pub is_maximized: bool,
    /// 本次操作后引擎认定的全屏状态（有保存记录且样式已去掉边框 + 置顶）。
    pub is_fullscreen: bool,
    pub is_topmost: bool,
    pub rect: Rect,
    pub style: i32,
    pub ex_style: i32,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// win_drag
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct DragParams {
    #[serde(default)]
    pub hwnd: Option<u64>,
    #[serde(default)]
    pub snapshot_id: Option<u64>,
    #[serde(default)]
    pub uid: Option<u64>,
    /// 起点（屏幕物理像素）。给了 uid 时可以省略：用元素中心当起点。
    #[serde(default)]
    pub from_x: Option<i32>,
    #[serde(default)]
    pub from_y: Option<i32>,
    /// 终点（屏幕物理像素）。
    pub to_x: i32,
    pub to_y: i32,
    #[serde(default)]
    pub button: Option<MouseButton>,
    /// 轨迹点数（含终点），默认 24，夹在 2..=200。
    #[serde(default)]
    pub steps: Option<u32>,
    /// 从按下到抬起的总时长，默认 400ms，夹在 50..=10000。
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// 轨迹上叠加 1px 法向微动，默认 true。只做"移动+按下+移动+松开"时，
    /// 不少控件（含 Win32 列表视图的拖选、Qt 的拖放手势）认不出这是拖拽。
    #[serde(default)]
    pub wobble: Option<bool>,
    /// 截图坐标（0.2.2）：给了它，from_x/from_y/to_x/to_y 都按这张截图的像素解释。
    #[serde(default)]
    pub shot_id: Option<u64>,
    /// 动作生效验证（0.2.1）。
    #[serde(default)]
    pub verify: Option<EffectParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DragResult {
    pub delivered: String,
    pub target_hwnd: u64,
    pub from_screen: Point,
    pub to_screen: Point,
    /// 实际发出的轨迹点数（含终点）。
    pub steps: u32,
    pub duration_ms: u64,
    pub distance_px: f64,
    /// 动作生效验证结果（0.2.1）。调用方没给 `verify` 时为 null。
    #[serde(default)]
    pub effect: Option<EffectResult>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// win_drag_path（0.2.0）
// ---------------------------------------------------------------------------

/// 形状快捷方式：调用方给「圆心/半径/角度」或「控制点」，引擎展开成等距点列。
///
/// 为什么要快捷方式：让模型直接吐几百个点，成本高（每点几个 token）、慢、
/// 还容易越画越歪。给「画个半径 120 的圆」一句话，坐标由引擎算。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PathShape {
    /// 整圆。`start_deg` 可指定起笔角（0°=右侧，角度增大在屏幕上是顺时针）。
    Circle {
        cx: f64,
        cy: f64,
        r: f64,
        #[serde(default)]
        start_deg: Option<f64>,
        #[serde(default)]
        ccw: Option<bool>,
    },
    /// 圆弧：起止角（度）。
    Arc {
        cx: f64,
        cy: f64,
        r: f64,
        start_deg: f64,
        end_deg: f64,
        #[serde(default)]
        ccw: Option<bool>,
    },
    /// 贝塞尔：2~4 个控制点（1 阶=直线）。
    Bezier { points: Vec<[f64; 2]> },
}

#[derive(Debug, Clone, Deserialize)]
pub struct PathParams {
    #[serde(default)]
    pub hwnd: Option<u64>,
    #[serde(default)]
    pub snapshot_id: Option<u64>,
    #[serde(default)]
    pub uid: Option<u64>,
    /// 起点（屏幕物理像素）。给了它在路径前面插一个落笔点；不给就用首点。
    #[serde(default)]
    pub from_x: Option<i32>,
    #[serde(default)]
    pub from_y: Option<i32>,
    /// 单笔路径（屏幕物理像素）。
    #[serde(default)]
    pub points: Option<Vec<[f64; 2]>>,
    /// 多笔：每笔是一条折线，笔间自动抬笔再落笔。
    #[serde(default)]
    pub strokes: Option<Vec<Vec<[f64; 2]>>>,
    /// 形状快捷方式（circle / arc / bezier），与 points/strokes 三选一。
    #[serde(default)]
    pub shape: Option<PathShape>,
    #[serde(default)]
    pub button: Option<MouseButton>,
    /// 相邻落点最大间距（物理像素），默认 2，夹在 0.5..=64。
    /// 这是"圆不圆、线顺不顺"的唯一旋钮：步长越小越圆，但点数/耗时线性上升。
    #[serde(default)]
    pub step_px: Option<f64>,
    /// 总时长（所有笔合计，含笔间移动）。不给就按路径长度自动算（约 1.2ms/px）。
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// 轨迹微抖，默认 **false**（弧线上 1px 法向抖动就是毛刺）。
    /// 只有需要让控件认出"这是拖拽"时才开。
    #[serde(default)]
    pub wobble: Option<bool>,
    /// 截图坐标（0.2.2）：给了它，points/strokes/shape 里的坐标都按这张截图的像素解释。
    /// 圆/圆弧的半径也按两轴平均缩放换算；多笔之间不会混用两套坐标系。
    #[serde(default)]
    pub shot_id: Option<u64>,
    /// 录制回放（0.2.4）：每笔每点的时间戳（毫秒，相对整段开始），与点数一一对应。
    /// 给了就**跳过重采样**并按这个时间表重放（还原人手的快慢），忽略 duration_ms。
    /// 通常直接把 `win_record` 返回的 `strokes[].times_ms` 原样贴进来。
    #[serde(default)]
    pub times_ms: Option<Vec<Vec<f64>>>,
    /// 动作生效验证（0.2.1）。
    #[serde(default)]
    pub verify: Option<EffectParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathResult {
    pub delivered: String,
    pub target_hwnd: u64,
    /// 实际画的笔数（strokes 长度）。
    pub strokes: usize,
    /// 实际发出的移动点数（含每笔起点）。
    pub points: usize,
    /// 计划总时长。
    pub planned_ms: u64,
    /// 实测总时长（含注入开销），和 planned_ms 的差就是时间控制精度。
    pub duration_ms: u64,
    /// 路径总长（像素）。
    pub path_px: f64,
    /// 相邻两点的最大实际间距；应接近 step_px，突然变大说明有断点。
    pub max_step_px: f64,
    pub start_screen: Point,
    pub end_screen: Point,
    /// 是否成功把系统定时器精度提到 1ms（false 表示时长会明显偏长）。
    pub timer_1ms: bool,
    /// 是否按录制时间表重放（0.2.4）：true = 还原人手快慢，false = 弧长匀速。
    pub timed: bool,
    /// 动作生效验证结果（0.2.1）。调用方没给 `verify` 时为 null。
    #[serde(default)]
    pub effect: Option<EffectResult>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// 动作生效验证（0.2.1）
// ---------------------------------------------------------------------------

/// 动作后怎么判定"真的生效了"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    /// 目标区域画面变了（默认）。
    Pixels,
    /// 出现了**动作前不存在**的窗口（点开对话框 / 菜单 / 弹层用这个，比像素稳）。
    Window,
}

impl VerifyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyMode::Pixels => "pixels",
            VerifyMode::Window => "window",
        }
    }
}

/// 动作工具共用的 `verify` 参数。不传 = 不验证（零开销，行为与 0.2.0 一致）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EffectParams {
    /// 默认 `pixels`；`window` 模式必须给 title_contains / class_name / pid 之一。
    #[serde(default)]
    pub mode: Option<VerifyMode>,
    /// 比对区域（屏幕物理像素）。不给就取动作点周围 ±160px，再不给就整窗。
    #[serde(default)]
    pub region: Option<Rect>,
    /// `pixels` 模式：变化像素占比达到多少算生效，默认 0.002（0.2%）。
    /// 动画/视频类目标建议调到 0.01 以上，否则画面本来就在动，永远"生效"。
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub interval_ms: Option<u64>,
    #[serde(default)]
    pub title_contains: Option<String>,
    #[serde(default)]
    pub class_name: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectResult {
    /// 是否真的做了验证。`false` 表示参数不足/抓图失败，已跳过（看 warnings）。
    pub checked: bool,
    /// 是否检测到生效。`checked=false` 时这个值没有意义。
    pub changed: bool,
    pub mode: String,
    /// 轮询中出现的**最大**变化比例（pixels 模式）；window 模式下恒为 0。
    pub diff_ratio: f64,
    pub polls: u32,
    pub waited_ms: u64,
    /// 实际比对区域（pixels 模式）。
    pub region: Option<Rect>,
    /// window 模式的命中信息（标题/类名），或 pixels 模式的补充说明。
    pub detail: Option<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// 截图坐标换算（0.2.2）
// ---------------------------------------------------------------------------

/// `win_capture` 返回的换算信息。拿到它就能把返回图上的像素坐标换算成屏幕物理像素。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShotInfo {
    /// 传给动作工具的 `shot_id`：给了它，动作里的坐标字段一律按**这张截图的像素**解释。
    pub shot_id: u64,
    /// 截图用的窗口（整屏截图为 null）。
    pub hwnd: Option<u64>,
    /// 截图覆盖的屏幕矩形（物理像素）——图片左上角对应它的左上角。
    pub screen_rect: Rect,
    pub image_w: u32,
    pub image_h: u32,
    /// 图片 1 像素 = 多少屏幕像素（x / y 分开换算，避免宽高比不一致时累积偏移）。
    pub scale_x: f64,
    pub scale_y: f64,
    pub ttl_ms: u64,
}

// ---------------------------------------------------------------------------
// 图像 → 路径（0.2.3）
// ---------------------------------------------------------------------------

/// `win_image_trace` 的入参：图源二选一（`path` 文件 / `hwnd`+`region` 自截），
/// 其余处理参数展开在顶层（`flatten`），调用方不用多套一层。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TraceRequest {
    /// 图片文件路径（PNG）。给了它就不用 hwnd。
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub hwnd: Option<u64>,
    #[serde(default)]
    pub region: Option<Rect>,
    /// 自截图时的长边上限（默认 1600，0 = 不缩放）。
    #[serde(default)]
    pub max_dim: Option<u32>,
    #[serde(flatten)]
    pub trace: crate::trace::TraceParams,
}

// ---------------------------------------------------------------------------
// 录制回放（0.2.4）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecordParams {
    /// 录制时长（毫秒），默认 5000，夹 100..60000。
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// 采样频率，默认 120Hz，夹 20..500。
    #[serde(default)]
    pub hz: Option<u32>,
    /// 采样点数上限，默认 20000，防跑飞。
    #[serde(default)]
    pub max_points: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedStroke {
    /// 轨迹点（屏幕物理像素），已按拍摄顺序去重相邻重复点。
    pub points: Vec<[f64; 2]>,
    /// 每个点相对**整段录制开始**的毫秒时间戳，与 points 一一对应。
    /// 直接传给 `win_drag_path.times_ms` 就能按原速重放。
    pub times_ms: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordResult {
    pub duration_ms: u64,
    pub samples: u32,
    pub hz: u32,
    pub strokes: Vec<RecordedStroke>,
    pub total_px: f64,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// win_wait
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitCondition {
    Window,
    Image,
    Text,
}

impl WaitCondition {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitCondition::Window => "wait_for_window",
            WaitCondition::Image => "wait_for_image",
            WaitCondition::Text => "wait_for_text",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WaitParams {
    pub condition: WaitCondition,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub interval_ms: Option<u64>,

    // --- wait_for_window ---
    #[serde(default)]
    pub title_contains: Option<String>,
    #[serde(default)]
    pub class_name: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub visible_only: Option<bool>,

    // --- wait_for_image ---
    /// 模板 PNG 路径。必须是**未经缩放**的截图裁出来的，见 SKILL.md。
    #[serde(default)]
    pub template: Option<String>,
    /// 匹配阈值，0..1，默认 0.95。1.0 表示逐像素完全相同。
    #[serde(default)]
    pub min_score: Option<f64>,
    /// 模板相对屏幕的缩放系数，默认 1.0。模板若来自缩过图的截图，这里要填回去。
    #[serde(default)]
    pub template_scale: Option<f64>,

    // --- wait_for_text ---
    /// 要等到的子串（OCR 结果里找）。
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    /// OCR 语言，例如 zh-Hans-CN / en-US；不给则用系统语言。
    #[serde(default)]
    pub language: Option<String>,

    // --- 公共区域 ---
    #[serde(default)]
    pub hwnd: Option<u64>,
    #[serde(default)]
    pub region: Option<Rect>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitResult {
    pub condition: String,
    /// 是否等到。超时返回 `matched=false, timed_out=true`——**不是** RPC 错误：
    /// "没等到"是正常的业务结果，调用方拿它做分支，而不是当成失败重试。
    pub matched: bool,
    pub timed_out: bool,
    pub waited_ms: u64,
    pub polls: u32,
    pub timeout_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WinInfo>,
    /// wait_for_image 命中位置（屏幕物理像素，模板中心）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_point: Option<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_text: Option<String>,
    /// 最后一次 OCR 的全文（截断到 2000 字符），命不中时用它判断"到底识别到了什么"。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr_text: Option<String>,
    /// 轮询期间最后一次失败原因（截图失败、OCR 失败等）。超时结果里带上它，
    /// 否则"没等到"和"一直报错所以没等到"看起来一模一样。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// win_find
// ---------------------------------------------------------------------------

/// `win_find` 参数：在指定区域里做**一次性**模板匹配（不轮询）。
///
/// 与 `win_wait(condition="image")` 的区别：那个是"等到出现为止"，这个是
/// "现在就告诉我它在哪、有多像"。要等出现请用 win_wait，不要在 win_find 外面自己轮询。
#[derive(Debug, Clone, Deserialize)]
pub struct FindParams {
    /// 模板 PNG 绝对路径。必须是从**未经缩放**的截图裁出来的（否则用 scale 补回去）。
    pub template: String,
    /// 目标窗口。给了就用它的窗口矩形当默认搜索区域。
    #[serde(default)]
    pub hwnd: Option<u64>,
    /// 搜索区域（屏幕物理像素）。给了就以它为准，与捕获区域求交（越界会裁剪并给 warning）。
    #[serde(default)]
    pub region: Option<Rect>,
    /// 匹配阈值 0..1，默认沿用 `win_wait` 的 0.95。
    #[serde(default)]
    pub min_score: Option<f64>,
    /// 模板相对屏幕的缩放系数，默认 1.0（语义与 win_wait 的 template_scale 相同）。
    #[serde(default)]
    pub scale: Option<f64>,
}

/// `win_find` 结果。
///
/// `found=false` **不是** RPC 错误：它是正常业务结果，同样带最佳候选位置与得分，
/// 客户端必须先看 `found` 再决定用不用 x/y（`score` 会告诉你差多少）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindResult {
    pub found: bool,
    /// 最佳匹配得分，1.0 = 逐像素完全相同。
    pub score: f64,
    /// 匹配位置左上角（屏幕物理像素）= origin + 匹配偏移。
    pub x: i32,
    pub y: i32,
    /// 缩放后的模板尺寸。
    pub w: u32,
    pub h: u32,
    /// 匹配位置中心（整数取整）。
    pub cx: i32,
    pub cy: i32,
    /// 搜索区域左上角（屏幕物理像素）。x - origin.x 才是匹配在截图里的偏移。
    pub origin: Point,
    /// 实际搜索的区域（region 越界时是裁剪后的区域）。
    pub searched_region: Rect,
    pub elapsed_ms: u64,
    /// 非致命问题，例如 region 越界被裁剪。
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// win_ocr
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct OcrParams {
    #[serde(default)]
    pub hwnd: Option<u64>,
    #[serde(default)]
    pub region: Option<Rect>,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrLine {
    pub text: String,
    /// 相对截图左上角（见 [`OcrResult::origin`]）的像素矩形。
    pub rect: Rect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrResult {
    pub text: String,
    pub lines: Vec<OcrLine>,
    pub language: String,
    /// 本机可用的 OCR 语言列表。识别失败时靠它判断"是没装语言包"还是别的原因。
    pub available_languages: Vec<String>,
    /// 截图左上角在屏幕坐标系里的位置：lines 的 rect 加上它才是屏幕坐标。
    pub origin: Point,
    pub width: u32,
    pub height: u32,
    pub ocr_ms: u64,
    pub capture_ms: u64,
    pub backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

// ---------------------------------------------------------------------------
// win_web_cdp
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct CdpParams {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// 直接给页面级 WebSocket 地址；给了就跳过 /json/list 发现。
    #[serde(default)]
    pub ws_url: Option<String>,
    /// CDP 方法名，例如 Page.navigate / Runtime.evaluate。
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub command_params: Option<Value>,
    /// true 时只返回 /json/list 的目标列表，不发命令。
    #[serde(default)]
    pub list_targets: Option<bool>,
    /// 给一个 URL 时新建标签页（PUT /json/new?url=...）并返回它的目标信息。
    #[serde(default)]
    pub new_target: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// 是否把同一次连接里收到的 CDP 事件一并返回（默认 false，只回命令响应）。
    #[serde(default)]
    pub include_events: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdpResult {
    pub host: String,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ws_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// 命令响应的 `result` 字段，**原样**返回，引擎不做任何解析。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// 完整响应帧（含 id / result / error），同样原样返回。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub targets: Option<Value>,
    pub elapsed_ms: u64,
}

// ---------------------------------------------------------------------------
// 默认值（集中放这里，避免各模块各写一遍魔数）
// ---------------------------------------------------------------------------

pub mod defaults {
    /// IPC.md：`max_dim?=1600`
    pub const CAPTURE_MAX_DIM: u32 = 1600;
    /// IPC.md：`max_depth?=4`
    pub const OBSERVE_MAX_DEPTH: u32 = 4;
    /// IPC.md：`max_elements?=400`
    pub const OBSERVE_MAX_ELEMENTS: u32 = 400;
    /// IPC.md：`snapshot_ttl_ms?=5000`
    pub const SNAPSHOT_TTL_MS: u64 = 5_000;
    /// 单条 NDJSON 行的上限，防止对端灌爆内存。
    pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

    /// win_wait：默认超时 10s。
    pub const WAIT_TIMEOUT_MS: u64 = 10_000;
    /// win_wait：超时上限 5 分钟——再长调用方自己的工具超时先到，没有意义。
    pub const WAIT_MAX_TIMEOUT_MS: u64 = 300_000;
    /// win_wait：轮询间隔，下限 20ms（再密也只是烧 CPU）。
    pub const WAIT_INTERVAL_MS: u64 = 300;
    /// win_wait：模板匹配默认阈值。
    pub const WAIT_MIN_SCORE: f64 = 0.95;

    /// win_drag：默认轨迹点数与总时长。
    pub const DRAG_STEPS: u32 = 24;
    pub const DRAG_DURATION_MS: u64 = 400;

    /// win_drag_path：相邻落点默认间距（物理像素）。2px 是"圆看不出来棱角"的下限，
    /// 再大就明显；再小点数与耗时线性上升。
    pub const PATH_STEP_PX: f64 = 2.0;

    /// 动作生效验证：默认等多久、多久看一次、变化比例阈值、区域外扩、超时上限。
    pub const EFFECT_TIMEOUT_MS: u64 = 1_200;
    pub const EFFECT_INTERVAL_MS: u64 = 60;
    pub const EFFECT_THRESHOLD: f64 = 0.002;
    pub const EFFECT_REGION_PAD: i32 = 160;
    pub const EFFECT_MAX_TIMEOUT_MS: u64 = 10_000;

    /// win_launch：拉起后默认等多久再确认进程还活着。
    pub const LAUNCH_CHECK_MS: u64 = 30;

    /// win_web_cdp：默认调试端口与单次命令超时。
    pub const CDP_PORT: u16 = 9222;
    pub const CDP_TIMEOUT_MS: u64 = 10_000;
}

/// 把 `serde_json::Value` 转成 `u64` 句柄。
///
/// 为什么单独写：JS 的 number 到 JSON 再到 Rust 时可能是整数也可能是浮点，
/// 而且 TS 侧有时会把 hwnd 写成字符串（BigInt 序列化）。三种都接受，
/// 不能接受的返回 None 由调用方转成 INVALID_ARGS。
pub fn value_as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}
