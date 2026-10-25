//! DSH screen-control 常驻引擎入口。
//!
//! 进程模型：单线程 tokio 运行时（current_thread）。理由有两个：
//! 1. UIA 是 COM，必须线程亲和；多线程运行时会让"哪个线程初始化的 COM"
//!    变成必须推理的问题，而单线程时它天然正确；
//! 2. Win32 句柄本来就允许跨线程用，但**消息队列**有线程归属，
//!    投递与枚举放在同一条线程上，行为更容易预测。
//!
//! 启动顺序刻意固定：DPI 声明 -> 运行时 -> 管道。DPI 感知一旦被窗口创建
//! 触发就无法再改，所以它必须排在所有窗口 API 之前。

// release 版隐藏控制台窗口，debug 版保留（`--pipe` 手测时看得到日志）。
// 内层属性必须排在所有 item 之前；上面的 `//!` 文档注释本身也是内层属性，
// 所以放在它们后面是合法的。原先由 build.rs 生成再用 include! 注入的做法
// 会把内层属性插到文件中部，rustc 直接报 E0752。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod capture;
mod cdp;
mod cli;
mod dpi;
mod error;
mod find;
mod input;
mod ipc;
mod launch;
mod ocr;
mod ocr_worker;
mod path;
mod protocol;
mod record;
mod snapshot;
mod shot;
mod trace;
mod uia;
mod verify;
mod wait;
mod window;
mod window_state;

use std::sync::OnceLock;
use std::time::Instant;

use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use error::{EngineError, EngineResult};
use protocol::{
    CaptureParams, CdpParams, CdpResult, ClickParams, Delivery, DragParams, DragResult, FindParams,
    FocusProbeParams, FocusProbeResult, Frame, ImageFormat, LaunchParams, LogLevel, OcrParams,
    ObserveParams, ObserveResult, PathParams, PathResult, PathShape, PingResult, Point, PressParams,
    Rect, RecordParams, ScrollParams, TraceRequest, TypeParams, WaitParams, WindowFilter,
    WindowListResult, WindowStateParams, defaults,
};

/// 引擎版本号。TS 侧用它判断协议兼容性，改动协议时要一起动。
const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 进程启动时刻，用于 engine_ping.uptime_ms。
static STARTED: OnceLock<Instant> = OnceLock::new();

/// 当前日志级别，由 --log-level 设置。
static LOG_LEVEL: OnceLock<LogLevel> = OnceLock::new();

/// 启动时 DPI 感知声明是否成功。engine_ping 报的是这一次的结果，
/// 而不是"现在再声明一次"——重复声明只会失败，把真实状态报成 unknown。
static DPI_AWARE: OnceLock<bool> = OnceLock::new();

/// 发出一个事件帧。
///
/// 走 stderr 而不是 stdout：stdout 可能被管道/父进程的日志系统占用，
/// 而且事件帧的正式通道是命名管道（见 ipc::send_event），这里只是本地留痕。
pub fn log_event(level: LogLevel, message: impl Into<String>) {
    let configured = LOG_LEVEL.get().copied().unwrap_or(LogLevel::Info);
    let msg = message.into();
    if level <= configured {
        eprintln!("[screen-control][{}] {msg}", level.as_str());
    }
}

// 命令行解析（含常驻 `--pipe` 与一次性 find/capture 两种模式）全部在 cli.rs；
// main() 只做两件事：**先声明 DPI 感知**，然后把服务模式交给 tokio、
// 或把一次性模式执行完按契约退出码退出。
fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // 解析失败时要知道"是不是一次性模式"：契约要求 find/capture 的错误也必须在 stdout
    // 留一行 {"ok":false,"error":"..."}，而常驻模式的参数错误只需要人话 + 用法。
    let one_shot = matches!(argv.get(1).map(String::as_str), Some("find") | Some("capture"));
    let parsed = match cli::parse(argv) {
        Ok(parsed) => parsed,
        Err(msg) => {
            if one_shot {
                // stdout 只放一行 JSON：错误字段取第一行（人话），完整用法走 stderr——
                // 契约要求"stdout 永远只有一行 JSON"，别把多行用法塞进 JSON 字符串。
                let brief = msg.lines().next().unwrap_or("参数错误").to_string();
                println!("{}", json!({"ok": false, "error": brief}));
            }
            // 参数错误：stderr 打人话（含用法），退出码固定 2。
            eprintln!("{msg}");
            std::process::exit(cli::EXIT_ERROR);
        }
    };
    let _ = LOG_LEVEL.set(parsed.log_level);
    let _ = STARTED.set(Instant::now());

    // DPI 感知必须在**任何窗口 API 之前**声明（一旦被窗口创建触发就无法再改）。
    // 一次性 find/capture 同样要求坐标是屏幕物理像素，所以这一步不分模式。
    let dpi_aware = dpi::declare_awareness();
    let _ = DPI_AWARE.set(dpi_aware);

    let args = match parsed.invocation {
        cli::Invocation::Server(args) => args,
        other => {
            // 一次性模式：不开 tokio、不建管道、不抢任何锁，跑完按约定退出码退出。
            let code = cli::execute(other);
            std::process::exit(code);
        }
    };

    log_event(
        LogLevel::Info,
        format!(
            "引擎启动 pid={} version={} dpi_aware={} pipe={} once={}",
            std::process::id(),
            ENGINE_VERSION,
            dpi_aware,
            args.pipe,
            args.once
        ),
    );

    // current_thread 运行时：见文件头说明。enable_all 让 net/time 都可用。
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("创建 tokio 运行时失败: {e}");
            std::process::exit(1);
        }
    };

    let result = runtime.block_on(async move {
        if args.once {
            ipc::serve_once(&args.pipe, handle_request).await
        } else {
            ipc::serve_forever(&args.pipe, handle_request).await
        }
    });

    match result {
        Ok(()) => log_event(LogLevel::Info, "引擎正常退出"),
        Err(e) => {
            log_event(LogLevel::Error, format!("引擎退出：{e}"));
            std::process::exit(1);
        }
    }
    // 兜底清理：把还活着的快照表清空。
    // panic = "abort" 下析构函数不保证执行，所以这里显式做一次，别指望 Drop。
    snapshot::clear_all();
}

// ---------------------------------------------------------------------------
// 请求分发
// ---------------------------------------------------------------------------

/// 处理一条请求，返回要写回的响应帧。
///
/// 这里刻意返回 `Frame` 而不是 `Result`：任何错误都要变成一条正常写回的
/// 错误响应（而不是断开连接），否则 TS 侧只会看到管道意外关闭，
/// 拿不到 error.code，也没法决定要不要重试。
pub fn handle_request(id: u64, method: &str, params: &Map<String, Value>) -> Frame {
    let started = Instant::now();
    let outcome = dispatch(method, params);
    let elapsed = started.elapsed().as_millis();

    // 每个请求处理完顺手清理过期快照：常驻进程里如果只在退出时清理，
    // 长时间运行会让快照表一直涨。
    snapshot::purge();

    match outcome {
        Ok(value) => {
            log_event(LogLevel::Debug, format!("{method} ok ({elapsed}ms)"));
            Frame::Ok { id, ok: true, result: value }
        }
        Err(e) => {
            log_event(
                LogLevel::Warn,
                format!("{method} 失败 [{}] {} ({elapsed}ms)", e.code(), e),
            );
            Frame::Err { id, ok: false, error: protocol::RpcError::new(e.code(), e.to_string()) }
        }
    }
}

/// 方法名 -> 处理函数。IPC.md 方法表里的每一项都必须在这里有分支。
fn dispatch(method: &str, params: &Map<String, Value>) -> EngineResult<Value> {
    match method {
        "engine_ping" => engine_ping(),
        "win_list_windows" => win_list_windows(params),
        "win_capture" => win_capture(params),
        "win_observe" => win_observe(params),
        "win_click" => win_click(params),
        "win_type" => win_type(params),
        "win_press" => win_press(params),
        "win_scroll" => win_scroll(params),
        "win_launch" => win_launch(params),
        "win_window_state" => win_window_state(params),
        "win_drag" => win_drag(params),
        "win_drag_path" => win_drag_path(params),
        "win_wait" => win_wait(params),
        "win_find" => win_find(params),
        "win_image_trace" => win_image_trace(params),
        "win_record" => win_record(params),
        "win_ocr" => win_ocr(params),
        "win_web_cdp" => win_web_cdp(params),
        "win_focus_guard_probe" => win_focus_guard_probe(params),
        other => Err(EngineError::invalid(format!(
            "未知方法 {other:?}；可用方法见 docs/IPC.md 的方法表"
        ))),
    }
}

/// 把自由 JSON 参数转成强类型结构体。
fn parse_params<T: DeserializeOwned>(params: &Map<String, Value>) -> EngineResult<T> {
    // 先转 Value::Object 再反序列化，这样 serde 的 `#[serde(default)]`
    // 与字段名不匹配的报错信息都能直接透出去。
    let value = Value::Object(params.clone());
    serde_json::from_value(value).map_err(|e| {
        EngineError::invalid(format!("参数不合法: {e}"))
    })
}

// ---------------------------------------------------------------------------
// engine_ping
// ---------------------------------------------------------------------------

fn engine_ping() -> EngineResult<Value> {
    let monitors = dpi::enumerate_monitors()?;
    let uptime = STARTED.get().map(|s| s.elapsed().as_millis() as u64).unwrap_or(0);
    let monitor_count = monitors.len() as u32;

    let result = PingResult {
        ok: true,
        version: ENGINE_VERSION.to_string(),
        pid: std::process::id(),
        dpi_aware: awareness_label(),
        monitor_count,
        monitors,
        uptime_ms: uptime,
    };
    to_value(result)
}

/// 启动时那次 DPI 声明结果的标签形式（IPC.md 规定 `dpi_aware` 是字符串）。
fn awareness_label() -> String {
    if DPI_AWARE.get().copied().unwrap_or(false) {
        "per_monitor_aware_v2".to_string()
    } else {
        "unknown".to_string()
    }
}

// ---------------------------------------------------------------------------
// win_list_windows
// ---------------------------------------------------------------------------

fn win_list_windows(params: &Map<String, Value>) -> EngineResult<Value> {
    // filter 是可选的，缺省即空过滤器。
    let filter: WindowFilter = match params.get("filter") {
        Some(Value::Null) | None => WindowFilter::default(),
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| EngineError::invalid(format!("filter 不合法: {e}")))?,
    };

    let mut windows = window::list_windows(&filter)?;
    // limit 是给模型上下文上的保险丝：夹到 [1, 500]，防止一次列出几百个窗口把 Token 烧光。
    if let Some(limit) = params.get("limit").and_then(protocol::value_as_u64) {
        windows.truncate(limit.clamp(1, 500) as usize);
    }
    to_value(WindowListResult { windows })
}

// ---------------------------------------------------------------------------
// win_capture
// ---------------------------------------------------------------------------

fn win_capture(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: CaptureParams = parse_params(params)?;

    let hwnd = match p.hwnd {
        Some(raw) => Some(window::require_window(raw, "hwnd")?),
        None => None,
    };

    // region 给了就裁到窗口内；没给就让 capture 自己决定（窗口矩形或整屏）。
    let region = match (p.region, hwnd) {
        (Some(r), Some(h)) => Some(capture::clamp_region_to_window(h, r)?),
        (Some(r), None) => {
            // 整屏捕获时裁到虚拟桌面，避免 BitBlt 到屏幕外。
            let clamped = window::intersect(dpi::virtual_screen_rect(), r);
            if clamped.is_empty() {
                return Err(EngineError::invalid("region 完全落在虚拟桌面之外"));
            }
            Some(clamped)
        }
        (None, _) => None,
    };

    let max_dim = p.max_dim.unwrap_or(defaults::CAPTURE_MAX_DIM);
    let format = p.format.unwrap_or(ImageFormat::Png);
    let backend = p.backend.unwrap_or(protocol::CaptureBackend::Auto);

    // 实际截到的屏幕矩形：优先级 region > 窗口矩形 > 虚拟桌面（和 capture::capture 一致）。
    let shot_rect = match (region, hwnd) {
        (Some(r), _) => r,
        (None, Some(h)) => dpi::window_rect(h)?,
        (None, None) => dpi::virtual_screen_rect(),
    };

    // ocr=true 时先在**未缩放**的原始像素上识别：缩放会直接吃掉小字的识别率，
    // 而落盘那张图仍然按调用方给的 max_dim 处理（两者互不影响）。
    if p.ocr.unwrap_or(false) {
        let timer = capture::Timer::start();
        let raw = capture::capture(hwnd, region, backend)?;
        let capture_ms = timer.elapsed_ms();
        let outcome = ocr::recognize(&raw.rgba, raw.width, raw.height, p.ocr_language.as_deref())?;
        let result = capture::finalize(raw, max_dim, format, capture_ms)?;
        let (w, h) = (result.width, result.height);
        let mut value = to_value(result)?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "ocr".to_string(),
                json!({
                    "text": outcome.text,
                    "lines": outcome.lines,
                    "language": outcome.language,
                    "available_languages": outcome.available_languages,
                    "ocr_ms": outcome.ocr_ms,
                }),
            );
        }
        attach_shot(&mut value, hwnd.map(window::to_u64), shot_rect, w, h);
        return Ok(value);
    }

    let result = capture::capture_to_file(hwnd, region, max_dim, format, backend)?;
    let (w, h) = (result.width, result.height);
    let mut value = to_value(result)?;
    attach_shot(&mut value, hwnd.map(window::to_u64), shot_rect, w, h);
    Ok(value)
}

/// 把截图坐标换算信息（`coord`）挂到 capture 结果上。
///
/// 有了它，模型在返回图上量到的坐标可以直接交给动作工具的 `shot_id` 路径，
/// 不用再自己乘"截图 1400 宽 ×1.8286"这种系数——那类手算错一位就整块点歪。
fn attach_shot(value: &mut Value, hwnd: Option<u64>, rect: Rect, width: u32, height: u32) {
    let info = shot::register(hwnd, rect, width, height);
    if let Some(obj) = value.as_object_mut() {
        match to_value(&info) {
            Ok(v) => {
                obj.insert("coord".to_string(), v);
            }
            Err(e) => log_event(LogLevel::Warn, format!("截图坐标登记序列化失败：{e}")),
        }
    }
}

/// 给了 `shot_id` 就把图片坐标换成屏幕坐标，否则原样返回。
fn shot_point(s: Option<&shot::Shot>, x: i32, y: i32) -> (i32, i32) {
    match s {
        Some(s) => s.to_screen(x as f64, y as f64),
        None => (x, y),
    }
}

// ---------------------------------------------------------------------------
// win_observe
// ---------------------------------------------------------------------------

fn win_observe(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: ObserveParams = parse_params(params)?;
    let hwnd = window::require_window(p.hwnd, "hwnd")?;

    let ttl_ms = p.snapshot_ttl_ms.unwrap_or(defaults::SNAPSHOT_TTL_MS);
    if ttl_ms == 0 {
        return Err(EngineError::invalid("snapshot_ttl_ms 必须大于 0"));
    }

    let tree = uia::observe(hwnd, &p)?;

    // 先截图再入表：截图要花几十毫秒，如果先入表，TS 侧可能在图还没写完时
    // 就拿着 snapshot_id 发动作请求，命中的是"图还没生成"的中间状态。
    let screenshot = uia::maybe_screenshot(
        hwnd,
        p.with_screenshot.unwrap_or(true),
        defaults::CAPTURE_MAX_DIM,
    );

    let snap = snapshot::insert(
        window::to_u64(hwnd),
        tree.elements.clone(),
        std::time::Duration::from_millis(ttl_ms),
        tree.truncated,
    );

    let result = ObserveResult {
        snapshot_id: snap.id,
        created_at_ms: snap.created_at_ms,
        expires_at_ms: snap.expires_at_ms,
        hwnd: window::to_u64(hwnd),
        element_count: tree.elements.len() as u32,
        truncated: tree.truncated,
        elements: tree.elements,
        screenshot,
    };
    to_value(result)
}

// ---------------------------------------------------------------------------
// 动作类
// ---------------------------------------------------------------------------

fn win_click(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: ClickParams = parse_params(params)?;

    // 没给坐标时兜底到客户区中心：这是"只给 hwnd 想点一下窗口"的常见用法。
    let shot = match p.shot_id {
        Some(id) => Some(shot::get_valid(id)?),
        None => None,
    };
    let point = match (p.x, p.y) {
        (Some(x), Some(y)) => Some(shot_point(shot.as_ref(), x, y)),
        (None, None) => None,
        _ => return Err(EngineError::invalid("x 和 y 必须同时提供")),
    };
    // 给了 shot_id 但没给 hwnd 时，用截图时那个窗口当目标。
    let target_hwnd = p.hwnd.or_else(|| shot.as_ref().and_then(|s| s.info.hwnd));

    let mut ctx = input::resolve_target(p.snapshot_id, p.uid, target_hwnd, point)?;
    // 2026-10-16 起 click/type/press/scroll 默认都走 SendInput 真注入，现场（光标、
    // 前台）由 input.rs 的 InjectionGuard 在返回前还回去，审计结果在 warnings 里。
    // 这里再留一个"记账"守卫：万一还现场失败，日志里能看到前台确实被动过。
    let guard = input::NoActivateGuard::new();

    // 动作生效验证：基准必须在动作**之前**抓，否则"变化"无从谈起。
    let baseline = p.verify.as_ref().map(|v| verify::begin(ctx.root, v, point));

    let (delivered, client, elapsed) = input::click(
        &mut ctx,
        p.button.unwrap_or(protocol::MouseButton::Left),
        p.clicks.unwrap_or(1),
        p.force_sendinput.unwrap_or(false),
    )?;

    log_event(LogLevel::Info, format!("win_click 投递方式={}", delivered.as_str()));
    if guard.foreground_changed() {
        log_event(
            LogLevel::Warn,
            "win_click 返回后前台窗口与调用前不一致，请检查 InjectionGuard 的还现场是否失败",
        );
    }

    let mut result = ctx.finish(delivered, elapsed, client);
    if let Some(b) = baseline {
        result.effect = Some(verify::finish(b));
    }
    to_value(result)
}

fn win_type(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: TypeParams = parse_params(params)?;
    if p.text.is_empty() {
        return Err(EngineError::invalid("text 不能为空"));
    }

    let mut ctx = input::resolve_target(p.snapshot_id, p.uid, p.hwnd, None)?;
    let guard = input::NoActivateGuard::new();
    let (delivered, client, elapsed) =
        input::type_text(&mut ctx, &p.text, p.method.unwrap_or(Delivery::Auto))?;

    log_event(LogLevel::Info, format!("win_type 投递方式={}", delivered.as_str()));
    if delivered == Delivery::Clipboard {
        // 0.1.9 起 auto 模式遇到非 ASCII 会先自己写剪贴板再 Ctrl+V；显式 clipboard 模式
        // 仍然只发 Ctrl+V。两种情况都会在 warnings 里写清剪贴板被动过。
        log_event(
            LogLevel::Warn,
            "win_type 走剪贴板路径：剪贴板内容已被引擎覆盖（auto 模式）或由调用方事先写入（显式 clipboard）",
        );
    }
    if guard.foreground_changed() {
        log_event(
            LogLevel::Warn,
            "win_type 返回后前台窗口与调用前不一致，请检查 InjectionGuard 的还现场是否失败",
        );
    }

    to_value(ctx.finish(delivered, elapsed, client))
}

fn win_press(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: PressParams = parse_params(params)?;
    let mut ctx = input::resolve_target(p.snapshot_id, p.uid, p.hwnd, None)?;
    let guard = input::NoActivateGuard::new();

    // press 没有坐标，默认比对整窗（慢）：调用方最好显式给 verify.region。
    let baseline = p.verify.as_ref().map(|v| verify::begin(ctx.root, v, None));

    let (delivered, client, elapsed) =
        input::press(&mut ctx, &p.key, &p.modifiers, p.repeat.unwrap_or(1))?;

    log_event(LogLevel::Info, format!("win_press 投递方式={}", delivered.as_str()));
    if guard.foreground_changed() {
        log_event(
            LogLevel::Warn,
            "win_press 返回后前台窗口与调用前不一致，请检查 InjectionGuard 的还现场是否失败",
        );
    }
    let mut result = ctx.finish(delivered, elapsed, client);
    if let Some(b) = baseline {
        result.effect = Some(verify::finish(b));
    }
    to_value(result)
}

fn win_scroll(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: ScrollParams = parse_params(params)?;
    let shot = match p.shot_id {
        Some(id) => Some(shot::get_valid(id)?),
        None => None,
    };
    let point = match (p.x, p.y) {
        (Some(x), Some(y)) => Some(shot_point(shot.as_ref(), x, y)),
        (None, None) => None,
        _ => return Err(EngineError::invalid("x 和 y 必须同时提供")),
    };
    let target_hwnd = p.hwnd.or_else(|| shot.as_ref().and_then(|s| s.info.hwnd));

    let mut ctx = input::resolve_target(p.snapshot_id, p.uid, target_hwnd, point)?;
    let baseline = p.verify.as_ref().map(|v| verify::begin(ctx.root, v, point));
    let (delivered, client, elapsed) = input::scroll(&mut ctx, p.delta)?;
    log_event(LogLevel::Info, format!("win_scroll 投递方式={}", delivered.as_str()));
    let mut result = ctx.finish(delivered, elapsed, client);
    if let Some(b) = baseline {
        result.effect = Some(verify::finish(b));
    }
    to_value(result)
}

// ---------------------------------------------------------------------------
// win_launch / win_window_state / win_drag / win_wait / win_ocr / win_web_cdp
// ---------------------------------------------------------------------------

fn win_launch(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: LaunchParams = parse_params(params)?;
    let result = launch::launch(&p)?;
    log_event(
        LogLevel::Info,
        format!(
            "win_launch pid={} still_running={} path={}",
            result.pid, result.still_running, result.path
        ),
    );
    to_value(result)
}

fn win_window_state(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: WindowStateParams = parse_params(params)?;
    let result = window_state::apply(&p)?;
    log_event(
        LogLevel::Info,
        format!(
            "win_window_state {} hwnd={} fullscreen={} topmost={} minimized={} maximized={}",
            result.action,
            result.hwnd,
            result.is_fullscreen,
            result.is_topmost,
            result.is_minimized,
            result.is_maximized
        ),
    );
    to_value(result)
}

fn win_drag(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: DragParams = parse_params(params)?;

    // 起点两种给法：显式屏幕坐标，或者 snapshot_id + uid（用元素中心）。
    let shot = match p.shot_id {
        Some(id) => Some(shot::get_valid(id)?),
        None => None,
    };
    let point = match (p.from_x, p.from_y) {
        (Some(x), Some(y)) => Some(shot_point(shot.as_ref(), x, y)),
        (None, None) => None,
        _ => return Err(EngineError::invalid("from_x 和 from_y 必须同时提供")),
    };
    if point.is_none() && p.uid.is_none() {
        return Err(EngineError::invalid(
            "必须给 from_x/from_y（屏幕物理像素），或者给 snapshot_id + uid 用元素中心当起点",
        ));
    }
    let (to_x, to_y) = shot_point(shot.as_ref(), p.to_x, p.to_y);
    let target_hwnd = p.hwnd.or_else(|| shot.as_ref().and_then(|s| s.info.hwnd));

    let mut ctx = input::resolve_target(p.snapshot_id, p.uid, target_hwnd, point)?;
    let guard = input::NoActivateGuard::new();
    let baseline = p
        .verify
        .as_ref()
        .map(|v| verify::begin(ctx.root, v, Some((to_x, to_y))));
    let (delivered, from, to, steps, elapsed) = input::drag(
        &mut ctx,
        to_x,
        to_y,
        p.button.unwrap_or(protocol::MouseButton::Left),
        p.steps.unwrap_or(defaults::DRAG_STEPS),
        p.duration_ms.unwrap_or(defaults::DRAG_DURATION_MS),
        p.wobble.unwrap_or(true),
    )?;

    log_event(
        LogLevel::Info,
        format!(
            "win_drag 投递方式={} steps={steps} 用时={elapsed}ms",
            delivered.as_str()
        ),
    );
    if guard.foreground_changed() {
        log_event(
            LogLevel::Warn,
            "win_drag 返回后前台窗口与调用前不一致，请检查 InjectionGuard 的还现场是否失败",
        );
    }

    let distance = (((to.0 - from.0) as f64).powi(2) + ((to.1 - from.1) as f64).powi(2)).sqrt();
    let mut result = DragResult {
        delivered: delivered.as_str().to_string(),
        target_hwnd: window::to_u64(ctx.root),
        from_screen: Point { x: from.0, y: from.1 },
        to_screen: Point { x: to.0, y: to.1 },
        steps,
        duration_ms: elapsed,
        distance_px: distance,
        effect: None,
        warnings: std::mem::take(&mut ctx.warnings),
    };
    if let Some(b) = baseline {
        result.effect = Some(verify::finish(b));
    }
    to_value(result)
}

/// `win_drag_path`：一次按下沿整条路径走完（圆 / 圆弧 / 贝塞尔 / 任意折线）。
fn win_drag_path(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: PathParams = parse_params(params)?;

    let origin = match (p.from_x, p.from_y) {
        (Some(x), Some(y)) => Some((x, y)),
        (None, None) => None,
        _ => return Err(EngineError::invalid("from_x 和 from_y 必须同时提供")),
    };

    // 三种给法，优先级 strokes > points > shape。
    let mut strokes: Vec<Vec<(f64, f64)>> = if let Some(list) = p.strokes.as_ref() {
        if list.is_empty() {
            return Err(EngineError::invalid("strokes 不能是空数组"));
        }
        list.iter()
            .map(|s| s.iter().map(|q| (q[0], q[1])).collect())
            .collect()
    } else if let Some(pts) = p.points.as_ref() {
        vec![pts.iter().map(|q| (q[0], q[1])).collect()]
    } else if let Some(shape) = p.shape.as_ref() {
        match shape {
            PathShape::Circle { cx, cy, r, start_deg, ccw } => vec![path::circle(
                *cx,
                *cy,
                *r,
                start_deg.unwrap_or(-90.0),
                ccw.unwrap_or(false),
            )],
            PathShape::Arc { cx, cy, r, start_deg, end_deg, ccw } => vec![path::arc(
                *cx,
                *cy,
                *r,
                *start_deg,
                *end_deg,
                ccw.unwrap_or(false),
            )],
            PathShape::Bezier { points } => {
                if points.len() < 2 || points.len() > 4 {
                    return Err(EngineError::invalid(format!(
                        "bezier 需要 2~4 个控制点，收到 {}",
                        points.len()
                    )));
                }
                let ctrl: Vec<(f64, f64)> = points.iter().map(|q| (q[0], q[1])).collect();
                vec![path::bezier(&ctrl)]
            }
        }
    } else {
        return Err(EngineError::invalid(
            "必须给 points（单笔点列）、strokes（多笔）或 shape（circle/arc/bezier）之一",
        ));
    };

    if let Some((x, y)) = origin {
        let pt = (x as f64, y as f64);
        match strokes.first_mut() {
            Some(first) => first.insert(0, pt),
            None => strokes.push(vec![pt]),
        }
    }

    // 截图坐标：给了 shot_id 就把整条路径（含 shape 展开出来的点）一起换算成屏幕坐标。
    // 形状先展开再换算而不是先换算再展开，是为了让"半径按两轴平均缩放"这件事
    // 只有一处逻辑，且多笔之间不会混用两套坐标系。
    let shot = match p.shot_id {
        Some(id) => Some(shot::get_valid(id)?),
        None => None,
    };
    if let Some(s) = shot.as_ref() {
        for stroke in strokes.iter_mut() {
            for pt in stroke.iter_mut() {
                let (sx, sy) = s.to_screen(pt.0, pt.1);
                *pt = (sx as f64, sy as f64);
            }
        }
    }
    if strokes.iter().all(|s| s.is_empty()) {
        return Err(EngineError::invalid("路径为空：所有笔画都没有点"));
    }

    // 目标判定用首点做命中测试：给了 hwnd 就以它为根，没给就靠点找窗口。
    let first = strokes.iter().find(|s| !s.is_empty()).unwrap()[0];
    let probe = Some((first.0.round() as i32, first.1.round() as i32));
    let target_hwnd = p.hwnd.or_else(|| shot.as_ref().and_then(|s| s.info.hwnd));
    let mut ctx = input::resolve_target(p.snapshot_id, p.uid, target_hwnd, probe)?;
    let guard = input::NoActivateGuard::new();
    let baseline = p.verify.as_ref().map(|v| verify::begin(ctx.root, v, probe));
    let out = input::drag_path(
        &mut ctx,
        &strokes,
        p.button.unwrap_or(protocol::MouseButton::Left),
        p.step_px.unwrap_or(defaults::PATH_STEP_PX),
        p.duration_ms,
        p.wobble.unwrap_or(false),
        p.times_ms.as_deref(),
    )?;

    log_event(
        LogLevel::Info,
        format!(
            "win_drag_path {}笔/{}点 计划={}ms 实测={}ms 路径={:.1}px 最大步长={:.2}px timer_1ms={}",
            out.strokes,
            out.points,
            out.planned_ms,
            out.elapsed_ms,
            out.path_px,
            out.max_step_px,
            out.timer_1ms
        ),
    );
    if guard.foreground_changed() {
        log_event(
            LogLevel::Warn,
            "win_drag_path 返回后前台窗口与调用前不一致，请检查 InjectionGuard 的还现场是否失败",
        );
    }

    let mut result = PathResult {
        delivered: Delivery::SendInput.as_str().to_string(),
        target_hwnd: window::to_u64(ctx.root),
        strokes: out.strokes,
        points: out.points,
        planned_ms: out.planned_ms,
        duration_ms: out.elapsed_ms,
        path_px: out.path_px,
        max_step_px: out.max_step_px,
        start_screen: Point { x: out.start.0, y: out.start.1 },
        end_screen: Point { x: out.end.0, y: out.end.1 },
        timer_1ms: out.timer_1ms,
        timed: out.timed,
        effect: None,
        warnings: std::mem::take(&mut ctx.warnings),
    };
    if let Some(b) = baseline {
        result.effect = Some(verify::finish(b));
    }
    to_value(result)
}

/// `win_record`：录一段人手的鼠标轨迹（按住左键的那些段），用于原速回放。
fn win_record(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: RecordParams = parse_params(params)?;
    let result = record::record(&p)?;
    log_event(
        LogLevel::Info,
        format!(
            "win_record {}笔/{}点 采样={}Hz 用时={}ms",
            result.strokes.len(),
            result.strokes.iter().map(|s| s.points.len()).sum::<usize>(),
            result.hz,
            result.duration_ms
        ),
    );
    to_value(result)
}

/// 把浮点坐标压到 2 位小数：笔顺动辄上千个点，全精度 JSON 会让结果膨胀好几倍，
/// 而 0.01 像素的精度对落点毫无意义。
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// `win_image_trace`：图（文件或自截）→ 可直接画的笔顺。
fn win_image_trace(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: TraceRequest = parse_params(params)?;
    let started = Instant::now();

    // 1) 图源二选一：文件 或 自截（自截会顺带登记 shot，坐标可直接喂给 win_drag_path）
    let (rgba, iw, ih, shot_info) = match p.path.as_deref() {
        Some(path) => {
            let bytes = std::fs::read(path)
                .map_err(|e| EngineError::invalid(format!("读不到图片 {path}：{e}")))?;
            let img = image::load_from_memory(&bytes)
                .map_err(|e| EngineError::invalid(format!("图片解码失败 {path}：{e}")))?;
            let buf = img.to_rgba8();
            let (w, h) = (buf.width(), buf.height());
            (buf.into_raw(), w, h, None)
        }
        None => {
            let hwnd = match p.hwnd {
                Some(raw) => Some(window::require_window(raw, "hwnd")?),
                None => None,
            };
            let region = match (p.region, hwnd) {
                (Some(r), Some(h)) => Some(capture::clamp_region_to_window(h, r)?),
                (Some(r), None) => Some(window::intersect(dpi::virtual_screen_rect(), r)),
                (None, _) => None,
            };
            let rect = match (region, hwnd) {
                (Some(r), _) => r,
                (None, Some(h)) => dpi::window_rect(h)?,
                (None, None) => dpi::virtual_screen_rect(),
            };
            let raw = capture::capture(hwnd, region, protocol::CaptureBackend::Auto)?;
            let (mut w, mut h) = (raw.width, raw.height);
            let mut rgba = raw.rgba;
            let max_dim = p.max_dim.unwrap_or(defaults::CAPTURE_MAX_DIM);
            if max_dim > 0 && w.max(h) > max_dim {
                let scale = max_dim as f64 / w.max(h) as f64;
                let tw = ((w as f64) * scale).round().max(1.0) as u32;
                let th = ((h as f64) * scale).round().max(1.0) as u32;
                let img = image::RgbaImage::from_raw(w, h, rgba)
                    .ok_or_else(|| EngineError::other("截图缓冲尺寸与像素数不匹配"))?;
                let resized = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
                rgba = resized.into_raw();
                w = tw;
                h = th;
            }
            let info = shot::register(hwnd.map(window::to_u64), rect, w, h);
            (rgba, w, h, Some(info))
        }
    };

    // 2) 追踪
    let traced = trace::trace_rgba(&rgba, iw, ih, &p.trace);

    let strokes: Vec<Value> = traced
        .strokes
        .iter()
        .map(|s| {
            json!({
                "points": s
                    .points
                    .iter()
                    .map(|pt| json!([round2(pt[0]), round2(pt[1])]))
                    .collect::<Vec<_>>(),
                "length_px": round2(s.length_px),
                "closed": s.closed,
            })
        })
        .collect();

    let mut value = json!({
        "trace_ms": traced.trace_ms,
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "image_w": traced.image_w,
        "image_h": traced.image_h,
        "threshold": traced.threshold,
        "ink_px": traced.ink_px,
        "skeleton_px": traced.skeleton_px,
        "dropped_blobs": traced.dropped_blobs,
        "stroke_count": traced.strokes.len(),
        "total_points": traced.strokes.iter().map(|s| s.points.len()).sum::<usize>(),
        "total_px": round2(traced.strokes.iter().map(|s| s.length_px).sum()),
        "strokes": strokes,
        "warnings": traced.warnings,
    });
    if let Some(info) = shot_info {
        if let Some(obj) = value.as_object_mut() {
            match to_value(&info) {
                Ok(v) => {
                    obj.insert("coord".to_string(), v);
                }
                Err(e) => log_event(LogLevel::Warn, format!("坐标登记序列化失败：{e}")),
            }
        }
    }
    log_event(
        LogLevel::Info,
        format!(
            "win_image_trace {}笔/{}点 阈值={} 骨架={}px 用时={}ms",
            traced.strokes.len(),
            traced.strokes.iter().map(|s| s.points.len()).sum::<usize>(),
            traced.threshold,
            traced.skeleton_px,
            traced.trace_ms
        ),
    );
    Ok(value)
}

fn win_wait(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: WaitParams = parse_params(params)?;
    let result = wait::wait(&p)?;
    log_event(
        LogLevel::Info,
        format!(
            "win_wait {} matched={} timed_out={} polls={} waited={}ms",
            result.condition, result.matched, result.timed_out, result.polls, result.waited_ms
        ),
    );
    to_value(result)
}

fn win_find(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: FindParams = parse_params(params)?;
    let result = find::find(&p)?;
    log_event(
        LogLevel::Info,
        format!(
            "win_find found={} score={:.4} at=({},{}) {}x{} 用时={}ms{}",
            result.found,
            result.score,
            result.x,
            result.y,
            result.w,
            result.h,
            result.elapsed_ms,
            if result.warnings.is_empty() {
                String::new()
            } else {
                format!(" warnings={:?}", result.warnings)
            }
        ),
    );
    to_value(result)
}

fn win_ocr(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: OcrParams = parse_params(params)?;
    let hwnd = match p.hwnd {
        Some(raw) => Some(window::require_window(raw, "hwnd")?),
        None => None,
    };
    let region = match (p.region, hwnd) {
        (Some(r), Some(h)) => Some(capture::clamp_region_to_window(h, r)?),
        (Some(r), None) => {
            let clamped = window::intersect(dpi::virtual_screen_rect(), r);
            if clamped.is_empty() {
                return Err(EngineError::invalid("region 完全落在虚拟桌面之外"));
            }
            Some(clamped)
        }
        (None, _) => None,
    };

    // 重点（0.1.9）：整条"截图 + Windows.Media.Ocr"路径放进短命子进程执行。
    // 真机记录过整屏 OCR 让引擎以 0xC0000005 退出；访问冲突不是 panic，
    // 进程内兜不住（见 ocr_worker.rs 文件头）。子进程崩了这里拿到结构化错误，
    // 引擎本体与当前会话继续活着——满足"要么成功、要么结构化错误"。
    // 落盘 PNG 仍然不缩放（keep_file=true），方便人肉核对识别的到底是哪张图。
    let result = ocr_worker::capture_and_recognize_isolated(
        hwnd.map(window::to_u64),
        region,
        p.language.as_deref(),
        true,
        ocr_worker::DEFAULT_TIMEOUT_MS,
    )?;
    log_event(
        LogLevel::Info,
        format!(
            "win_ocr {} 行 / {} 字 / 语言={} / ocr={}ms",
            result.lines.len(),
            result.text.chars().count(),
            result.language,
            result.ocr_ms
        ),
    );
    to_value(result)
}

fn win_web_cdp(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: CdpParams = parse_params(params)?;
    let host = p.host.clone().unwrap_or_else(|| "127.0.0.1".to_string());
    let port = p.port.unwrap_or(defaults::CDP_PORT);
    let timeout = std::time::Duration::from_millis(
        p.timeout_ms.unwrap_or(defaults::CDP_TIMEOUT_MS).clamp(100, 120_000),
    );

    // 三选一：发命令 / 列目标 / 新建目标。给零个或给多个都直接报参数错，
    // 不猜调用方想要哪个。
    let chosen = [p.command.is_some(), p.list_targets.unwrap_or(false), p.new_target.is_some()]
        .iter()
        .filter(|v| **v)
        .count();
    if chosen != 1 {
        return Err(EngineError::invalid(
            "command / list_targets / new_target 必须且只能给一个",
        ));
    }

    if p.list_targets.unwrap_or(false) {
        let targets = cdp::list_targets(&host, port, timeout)?;
        let elapsed = 0;
        return to_value(CdpResult {
            host,
            port,
            ws_url: None,
            target: None,
            id: None,
            result: None,
            raw: None,
            events: Vec::new(),
            targets: Some(targets),
            elapsed_ms: elapsed,
        });
    }

    if let Some(url) = &p.new_target {
        let target = cdp::new_target(&host, port, url, timeout)?;
        let ws_url = target
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        log_event(LogLevel::Info, format!("win_web_cdp 新建目标 {url}"));
        return to_value(CdpResult {
            host,
            port,
            ws_url,
            target: Some(target),
            id: None,
            result: None,
            raw: None,
            events: Vec::new(),
            targets: None,
            elapsed_ms: 0,
        });
    }

    // chosen == 1 且前两个分支都没进，command 必然存在。
    let command = p.command.clone().unwrap_or_default();
    let outcome = cdp::send_command(
        &host,
        port,
        p.ws_url.as_deref(),
        &command,
        p.command_params.as_ref(),
        timeout,
        p.include_events.unwrap_or(false),
    )?;
    log_event(
        LogLevel::Info,
        format!(
            "win_web_cdp {command} ok ({}ms, events={})",
            outcome.elapsed_ms,
            outcome.events.len()
        ),
    );
    to_value(CdpResult {
        host: outcome.host,
        port: outcome.port,
        ws_url: Some(outcome.ws_url),
        target: outcome.target,
        id: Some(outcome.id),
        result: outcome.result,
        raw: Some(outcome.raw),
        events: outcome.events,
        targets: None,
        elapsed_ms: outcome.elapsed_ms,
    })
}

// ---------------------------------------------------------------------------
// win_focus_guard_probe（调试用）
// ---------------------------------------------------------------------------

/// 探针：报告一次"守卫生命周期"前后的前台窗口是否变化。
///
/// 用途：TS 侧在做危险动作前先探一次，确认引擎的前台守卫逻辑符合预期，
/// 而不是等到真的把用户的焦点弄丢了才发现。
/// 这个接口**不修改**任何窗口状态，只读。
fn win_focus_guard_probe(params: &Map<String, Value>) -> EngineResult<Value> {
    let p: FocusProbeParams = parse_params(params)?;

    let before = dpi::foreground_window();

    if let Some(raw) = p.hwnd {
        // 校验句柄是否存在；这个探针**不会**去激活它，也不会改任何窗口状态。
        let hwnd = window::require_window(raw, "hwnd")?;
        log_event(
            LogLevel::Debug,
            format!(
                "focus probe: hwnd={} 存在，is_foreground={}",
                raw,
                window::same_window(before, hwnd)
            ),
        );
    }

    // 重读一次：如果调用方在这中间动了焦点（或者别的什么抢了前台），
    // 两个值就会不同，TS 侧据此判断环境是否干净。
    let after = dpi::foreground_window();

    let result = FocusProbeResult {
        foreground_before: window::to_u64(before),
        foreground_after: window::to_u64(after),
    };
    to_value(result)
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn to_value<T: serde::Serialize>(value: T) -> EngineResult<Value> {
    serde_json::to_value(value).map_err(EngineError::from)
}

/// 让 `json!` 宏在本文件里被用到（避免 unused import 警告），
/// 同时给将来的调试分支留一个统一出口。
#[allow(dead_code)]
fn empty_object() -> Value {
    json!({})
}
