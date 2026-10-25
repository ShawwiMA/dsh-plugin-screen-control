//! 命令行入口解析：常驻服务模式 + 一次性 `find` / `capture` 模式（+ 内部 OCR 子进程模式）。
//!
//! 为什么一次性模式也放在同一个 exe 里：它和常驻模式必须**同一份**截图/匹配代码
//! （同 DPI 声明、同 PrintWindow/BitBlt 回退、同匹配器），单独维护第二个二进制
//! 迟早会变成"命令行找得到、IPC 找不到"这种分叉。
//!
//! 模式约定（与 `--pipe` 互斥）：
//!
//! ```text
//! screen-control-engine find --template <png> [--hwnd N] [--region x,y,w,h]
//!                            [--min-score 0.9] [--scale 1.0] [--log-level info]
//! screen-control-engine capture --out <绝对png路径> [--hwnd N] [--region x,y,w,h]
//! ```
//!
//! 退出码：`find` 0=找到 / 3=没找到（同样是合法的一行 JSON）/ 2=错误；
//! `capture` 0=成功 / 2=错误。

use serde_json::json;

use crate::error::{EngineError, EngineResult};
use crate::protocol::{CaptureBackend, FindParams, ImageFormat, LogLevel, Rect};
use crate::{capture, dpi, find, window};

/// 成功。
pub const EXIT_OK: i32 = 0;
/// 参数/运行错误（stdout 仍输出一行 `{"ok":false,...}`，stderr 给人话）。
pub const EXIT_ERROR: i32 = 2;
/// `find` 没找到（正常业务结果，stdout 是 `{"ok":true,"found":false,...}`）。
pub const EXIT_NOT_FOUND: i32 = 3;

/// 常驻服务参数（`--pipe` 模式）。
pub struct ServerArgs {
    pub pipe: String,
    pub once: bool,
}

/// 命令行 `capture` 参数。
pub struct CaptureCli {
    pub out: String,
    pub hwnd: Option<u64>,
    pub region: Option<Rect>,
}

/// 一次调用的模式。`OcrWorker` 是引擎内部用的（win_ocr 的隔离子进程），不对外。
pub enum Invocation {
    Server(ServerArgs),
    Find(FindParams),
    Capture(CaptureCli),
    OcrWorker { spec: String, out: String },
    Help,
}

/// 解析结果：模式 + 进程日志级别（`--log-level` 对所有模式都生效）。
pub struct Parsed {
    pub invocation: Invocation,
    pub log_level: LogLevel,
}

/// 解析完整 argv（包含 argv[0]）。任何错误都返回"人话 + 用法"。
pub fn parse(argv: Vec<String>) -> Result<Parsed, String> {
    let args: Vec<String> = argv.into_iter().skip(1).collect();
    let log_level = extract_log_level(&args)?;

    // `--help` 优先级最高：任何模式下都只打印用法，不执行动作。
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(Parsed { invocation: Invocation::Help, log_level });
    }

    let invocation = match args.first().map(String::as_str) {
        Some("find") => Invocation::Find(parse_find(&args[1..])?),
        Some("capture") => Invocation::Capture(parse_capture(&args[1..])?),
        Some(crate::ocr_worker::OCR_WORKER_ARG) => parse_ocr_worker(&args[1..])?,
        _ => parse_server(&args)?,
    };
    Ok(Parsed { invocation, log_level })
}

/// 执行非服务模式，返回进程退出码。`Server` 由 main() 自己走 tokio 运行时。
pub fn execute(invocation: Invocation) -> i32 {
    match invocation {
        Invocation::Help => {
            println!("{}", usage());
            EXIT_OK
        }
        Invocation::Find(p) => run_find(&p),
        Invocation::Capture(c) => run_capture(&c),
        Invocation::OcrWorker { spec, out } => crate::ocr_worker::worker_main(&spec, &out),
        Invocation::Server(_) => {
            eprintln!("内部错误：Server 模式不应走 CLI 执行路径");
            EXIT_ERROR
        }
    }
}

/// 用法。常驻与一次性模式的参数必须都在这里，`--help`/无参时打印的就是它。
pub fn usage() -> String {
    [
        "用法:",
        "  screen-control-engine --pipe <管道名> [--once] [--log-level error|warn|info|debug]",
        "      常驻引擎（由 DSH 插件壳拉起，走命名管道 NDJSON）。",
        "",
        "  screen-control-engine find --template <模板PNG绝对路径> [--hwnd N] [--region x,y,w,h]",
        "                             [--min-score 0.9] [--scale 1.0] [--log-level 级别]",
        "      一次性模板匹配：只搜一次、不轮询。退出码 0=找到 / 3=没找到 / 2=错误；",
        "      stdout 永远只有一行 JSON。",
        "",
        "  screen-control-engine capture --out <绝对PNG路径> [--hwnd N] [--region x,y,w,h]",
        "                             [--log-level 级别]",
        "      一次性截图存 PNG（原尺寸不缩放），成功时 stdout 一行 JSON。",
        "",
        "  screen-control-engine --help",
        "",
        "说明: find/capture 不走命名管道、不启动 server、与本进程的单实例无关；",
        "      两种模式和 --pipe 互斥（一次只能选一个）。坐标一律屏幕物理像素。",
    ]
    .join("\n")
}

// ---------------------------------------------------------------------------
// 服务模式
// ---------------------------------------------------------------------------

fn parse_server(args: &[String]) -> Result<Invocation, String> {
    let mut pipe: Option<String> = None;
    let mut once = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pipe" => pipe = Some(take_value(args, &mut i, "--pipe")?),
            "--once" => once = true,
            // --log-level 已在 parse() 里被 extract_log_level 取走（进程级设置），
            // 这里只需要跳过它的值和参数名。0.1.9 忘了这一步，导致
            // `--pipe X --log-level debug` 报"未知参数"直接退出，
            // scripts/dev-probe.mjs 因此一直起不来引擎（被误记成脚本的坑）。
            "--log-level" => {
                let _ = take_value(args, &mut i, "--log-level")?;
            }
            other => return Err(format!("未知参数 {other:?}\n\n{}", usage())),
        }
        i += 1;
    }
    let pipe = pipe.ok_or_else(|| format!("缺少必需参数 --pipe\n\n{}", usage()))?;
    // 管道名的完整形式由 TS 壳给（\\.\pipe\dsh-screen-control-xxxx），
    // 但为了手测方便，允许只给短名，这里补前缀。
    let pipe = if pipe.starts_with(r"\\.\pipe\") { pipe } else { format!(r"\\.\pipe\{pipe}") };
    Ok(Invocation::Server(ServerArgs { pipe, once }))
}

// ---------------------------------------------------------------------------
// find 模式
// ---------------------------------------------------------------------------

fn parse_find(args: &[String]) -> Result<FindParams, String> {
    let mut template: Option<String> = None;
    let mut hwnd: Option<u64> = None;
    let mut region: Option<Rect> = None;
    let mut min_score: Option<f64> = None;
    let mut scale: Option<f64> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--template" => template = Some(take_value(args, &mut i, "--template")?),
            "--hwnd" => hwnd = Some(parse_u64(&take_value(args, &mut i, "--hwnd")?, "--hwnd")?),
            "--region" => region = Some(parse_region(&take_value(args, &mut i, "--region")?)?),
            "--min-score" => {
                min_score = Some(parse_f64(&take_value(args, &mut i, "--min-score")?, "--min-score")?)
            }
            "--scale" => scale = Some(parse_f64(&take_value(args, &mut i, "--scale")?, "--scale")?),
            // 同上：--log-level 是进程级的，模式解析层跳过即可。
            "--log-level" => {
                let _ = take_value(args, &mut i, "--log-level")?;
            }
            other => return Err(format!("find 模式不认识的参数 {other:?}\n\n{}", usage())),
        }
        i += 1;
    }

    let template = template
        .ok_or_else(|| format!("find 模式必须给 --template <模板PNG绝对路径>\n\n{}", usage()))?;
    Ok(FindParams { template, hwnd, region, min_score, scale })
}

// ---------------------------------------------------------------------------
// capture 模式
// ---------------------------------------------------------------------------

fn parse_capture(args: &[String]) -> Result<CaptureCli, String> {
    let mut out: Option<String> = None;
    let mut hwnd: Option<u64> = None;
    let mut region: Option<Rect> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out = Some(take_value(args, &mut i, "--out")?),
            "--hwnd" => hwnd = Some(parse_u64(&take_value(args, &mut i, "--hwnd")?, "--hwnd")?),
            "--region" => region = Some(parse_region(&take_value(args, &mut i, "--region")?)?),
            // 同上：--log-level 是进程级的，模式解析层跳过即可。
            "--log-level" => {
                let _ = take_value(args, &mut i, "--log-level")?;
            }
            other => return Err(format!("capture 模式不认识的参数 {other:?}\n\n{}", usage())),
        }
        i += 1;
    }

    let out = out.ok_or_else(|| format!("capture 模式必须给 --out <绝对PNG路径>\n\n{}", usage()))?;
    // 必须是绝对路径：相对路径会以"当前工作目录"为基准，而命令行的 cwd 由调用方决定，
    // 报出来的 path 再被别的进程按自己的 cwd 解读就会找不到文件。
    if !std::path::Path::new(&out).is_absolute() {
        return Err(format!("--out 必须是绝对路径，收到 {out:?}\n\n{}", usage()));
    }
    Ok(CaptureCli { out, hwnd, region })
}

// ---------------------------------------------------------------------------
// 内部 OCR 子进程模式
// ---------------------------------------------------------------------------

fn parse_ocr_worker(args: &[String]) -> Result<Invocation, String> {
    let mut spec: Option<String> = None;
    let mut out: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--spec" => spec = Some(take_value(args, &mut i, "--spec")?),
            "--out" => out = Some(take_value(args, &mut i, "--out")?),
            other => return Err(format!("OCR 子进程模式不认识的参数 {other:?}")),
        }
        i += 1;
    }
    match (spec, out) {
        (Some(spec), Some(out)) => Ok(Invocation::OcrWorker { spec, out }),
        _ => Err("OCR 子进程模式需要 --spec 和 --out".to_string()),
    }
}

// ---------------------------------------------------------------------------
// 命令行执行
// ---------------------------------------------------------------------------

fn run_find(p: &FindParams) -> i32 {
    match find::find(p) {
        Ok(r) => {
            for warning in &r.warnings {
                eprintln!("[screen-control] warning: {warning}");
            }
            // 契约：stdout 只有一行 JSON，字段固定。警告走 stderr，不混进 JSON。
            println!(
                "{}",
                json!({
                    "ok": true,
                    "found": r.found,
                    "x": r.x,
                    "y": r.y,
                    "w": r.w,
                    "h": r.h,
                    "cx": r.cx,
                    "cy": r.cy,
                    "score": r.score,
                    "origin": {"x": r.origin.x, "y": r.origin.y},
                    "elapsed_ms": r.elapsed_ms,
                })
            );
            if r.found {
                EXIT_OK
            } else {
                EXIT_NOT_FOUND
            }
        }
        Err(e) => report_failure("find", &e),
    }
}

fn run_capture(c: &CaptureCli) -> i32 {
    match capture_cli(c) {
        Ok(result) => {
            println!(
                "{}",
                json!({
                    "ok": true,
                    "path": result.path,
                    "width": result.width,
                    "height": result.height,
                })
            );
            EXIT_OK
        }
        Err(e) => report_failure("capture", &e),
    }
}

fn capture_cli(c: &CaptureCli) -> EngineResult<crate::protocol::CaptureResult> {
    let hwnd = match c.hwnd {
        Some(raw) => Some(window::require_window(raw, "hwnd")?),
        None => None,
    };
    // region 裁剪规则与 win_capture 完全一致。
    let region = match (c.region, hwnd) {
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

    let timer = capture::Timer::start();
    let raw = capture::capture(hwnd, region, CaptureBackend::Auto)?;
    let capture_ms = timer.elapsed_ms();
    let out = std::path::Path::new(&c.out);
    let expected = ImageFormat::Png.as_str();
    if let Some(ext) = out.extension().and_then(|e| e.to_str()) {
        if !ext.eq_ignore_ascii_case(expected) {
            return Err(EngineError::invalid(format!(
                "--out 目前只支持 .png，收到 {out:?}"
            )));
        }
    }
    capture::save_png_to(raw, out, capture_ms)
}

/// 错误约定：stdout 仍输出一行 `{"ok":false,"error":"..."}`（方便调用方按 JSON 解析），
/// 人话解释走 stderr。退出码固定 2。
fn report_failure(op: &str, e: &EngineError) -> i32 {
    println!("{}", json!({"ok": false, "error": e.to_string()}));
    eprintln!("screen-control-engine {op} 失败 [{}]: {e}", e.code());
    EXIT_ERROR
}

// ---------------------------------------------------------------------------
// 参数小工具
// ---------------------------------------------------------------------------

fn take_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index).cloned().ok_or_else(|| format!("{flag} 后面缺少值\n\n{}", usage()))
}

fn parse_u64(raw: &str, flag: &str) -> Result<u64, String> {
    raw.trim()
        .parse::<u64>()
        .map_err(|_| format!("{flag} 需要十进制整数，收到 {raw:?}\n\n{}", usage()))
}

fn parse_f64(raw: &str, flag: &str) -> Result<f64, String> {
    raw.trim()
        .parse::<f64>()
        .map_err(|_| format!("{flag} 需要数字，收到 {raw:?}\n\n{}", usage()))
}

/// `x,y,w,h`（屏幕物理像素）。
fn parse_region(raw: &str) -> Result<Rect, String> {
    let bad = || format!("--region 需要 x,y,w,h 四个整数，收到 {raw:?}\n\n{}", usage());
    let parts: Vec<i32> = raw
        .split(',')
        .map(|p| p.trim().parse::<i32>().map_err(|_| bad()))
        .collect::<Result<Vec<_>, _>>()?;
    if parts.len() != 4 {
        return Err(bad());
    }
    let r = Rect { x: parts[0], y: parts[1], w: parts[2], h: parts[3] };
    if r.is_empty() {
        return Err(format!("--region 的宽或高必须大于 0，收到 {raw:?}\n\n{}", usage()));
    }
    Ok(r)
}

/// 扫出 `--log-level <级别>`。放在模式解析之前，因为常驻/CLI 都要用它。
fn extract_log_level(args: &[String]) -> Result<LogLevel, String> {
    let mut level = LogLevel::Info;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--log-level" {
            let value = take_value(args, &mut i, "--log-level")?;
            level = LogLevel::parse(&value).ok_or_else(|| {
                format!(
                    "无法识别的日志级别 {value:?}（可选 error/warn/info/debug）\n\n{}",
                    usage()
                )
            })?;
        }
        i += 1;
    }
    Ok(level)
}
