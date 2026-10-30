//! `win_ocr` 的隔离子进程：把"截图 + Windows.Media.Ocr"整条 WinRT/GDI 路径
//! 放进一个短命子进程跑，引擎本体只做参数解析、等待与结果转发。
//!
//! 为什么必须这么做（0.1.8 → 0.1.9 的 C1 修复）：
//! 真机记录里 `win_ocr` 不带 region 打整屏（2560x1600）会让引擎进程以
//! `0xC0000005`（访问冲突）退出，由父进程重启。该崩溃发生在操作系统代码里
//! （GDI `BitBlt` / `Windows.Media.Ocr` 的 WinRT 调用），进程内**无法兜住**：
//! 访问冲突不是 Rust panic，`catch_unwind` 对它无效，而 release profile 是
//! `panic = "abort"`。把这条路径挪进子进程后：子进程崩了，父进程只是 wait 到一个
//! 非零退出码，转成结构化 `ENGINE_ERROR` 返回——引擎、当前会话和其它请求都不受影响，
//! 满足"要么成功、要么结构化错误，不许把引擎打崩"。
//!
//! 代价：每次 win_ocr 多一次自身进程 spawn（~30-80ms）+ 一次临时 spec/结果文件读写。
//! OCR 本来就是几百毫秒级的操作，这个开销可接受；换来的是引擎不会因为一次 OCR 消失。
//!
//! 子进程复用同一个 exe（`--ocr-worker --spec <json> --out <json>`），因此截图后端、
//! DPI 感知、OCR 实现与常驻模式完全同源，不存在第二份代码。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{EngineError, EngineResult};
use crate::protocol::{OcrResult, Point, Rect};
use crate::{dpi, ocr, window};

/// 内部子进程模式的命令行标记（不对外文档化）。
pub const OCR_WORKER_ARG: &str = "--ocr-worker";

/// 父进程等子进程的上限。OCR 整屏约 0.2-2s；20s 已远高于正常值，
/// 只用于兜住"WinRT 死锁/卡死"，超时就 kill 掉子进程并报 TIMEOUT。
pub const DEFAULT_TIMEOUT_MS: u64 = 20_000;

/// 直接把「已经在父进程里截好的图」交给子进程识别时用的那份数据。
///
/// 为什么需要：win_capture(ocr=true) 的图是父进程截的（还要按 max_dim 落盘），
/// 但 OCR 本身同样会因为在引擎进程里跑 WinRT 而 0xC0000005 崩掉 —— 和 win_ocr
/// 是同一条路径。这里用临时文件把 RGBA 原始像素递过去，让 OCR 继续留在子进程。
#[derive(Debug, Serialize, Deserialize)]
struct ImageSpec {
    path: String,
    width: u32,
    height: u32,
}

/// 子进程 spec（父写、子读）。
#[derive(Debug, Serialize, Deserialize)]
struct Spec {
    hwnd: Option<u64>,
    region: Option<Rect>,
    language: Option<String>,
    keep_file: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image: Option<ImageSpec>,
}

/// 子进程结果（子写、父读）。`ok=false` 时带结构化错误码 + 人话。
#[derive(Debug, Serialize, Deserialize)]
struct Outcome {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<OcrResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl Outcome {
    fn ok(result: OcrResult) -> Self {
        Self { ok: true, result: Some(result), code: None, message: None }
    }

    fn err(code: &str, message: String) -> Self {
        Self { ok: false, result: None, code: Some(code.to_string()), message: Some(message) }
    }
}

/// 父进程入口：在隔离子进程里跑一次 `ocr::capture_and_recognize`。
///
/// `hwnd` / `region` 由调用方（main.rs 的 win_ocr）先按原有规则解析/裁剪好，
/// 子进程不再重复解释语义，只负责执行。
pub fn capture_and_recognize_isolated(
    hwnd: Option<u64>,
    region: Option<Rect>,
    language: Option<&str>,
    keep_file: bool,
    timeout_ms: u64,
) -> EngineResult<OcrResult> {
    let spec_path = temp_path("ocr-spec");
    let out_path = temp_path("ocr-out");
    let spec = Spec {
        hwnd,
        region,
        language: language.map(str::to_string),
        keep_file,
        image: None,
    };
    std::fs::write(&spec_path, serde_json::to_vec(&spec)?)?;

    let result = run_worker_process(&spec_path, &out_path, timeout_ms);
    // 无论成败都清掉临时文件：spec 里可能带窗口内容相关参数，结果里可能带整屏文本。
    let _ = std::fs::remove_file(&spec_path);
    let _ = std::fs::remove_file(&out_path);
    result
}

/// 父进程入口：把「已经在父进程里截好的 RGBA」交给隔离子进程做 OCR。
///
/// win_capture(ocr=true) 走这条：图是父进程截的（还要按 max_dim 落盘/返回），
/// 但识别必须离开引擎进程——整屏 OCR 让引擎 0xC0000005 退出的事故就是这么来的。
pub fn recognize_rgba_isolated(
    rgba: &[u8],
    width: u32,
    height: u32,
    language: Option<&str>,
    timeout_ms: u64,
) -> EngineResult<OcrResult> {
    if width == 0 || height == 0 {
        return Err(EngineError::invalid("图像尺寸为 0，无法做 OCR"));
    }
    let expect = width as usize * height as usize * 4;
    if rgba.len() != expect {
        return Err(EngineError::invalid(format!(
            "RGBA 长度 {} 与尺寸 {width}x{height} 不符（期望 {expect}）",
            rgba.len()
        )));
    }

    let img_path = temp_path("ocr-image");
    std::fs::write(&img_path, rgba)?;
    let spec_path = temp_path("ocr-spec");
    let out_path = temp_path("ocr-out");
    let spec = Spec {
        hwnd: None,
        region: None,
        language: language.map(str::to_string),
        keep_file: false,
        image: Some(ImageSpec {
            path: img_path.to_string_lossy().to_string(),
            width,
            height,
        }),
    };
    std::fs::write(&spec_path, serde_json::to_vec(&spec)?)?;
    let result = run_worker_process(&spec_path, &out_path, timeout_ms);
    // 临时文件一律清掉：里面可能是一整屏像素或整屏识别文本。
    let _ = std::fs::remove_file(&spec_path);
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&img_path);
    result
}

fn run_worker_process(spec: &Path, out: &Path, timeout_ms: u64) -> EngineResult<OcrResult> {
    let exe = std::env::current_exe()
        .map_err(|e| EngineError::other(format!("取引擎自身路径失败，无法拉起 OCR 子进程: {e}")))?;
    let mut command = Command::new(exe);
    command
        .arg(OCR_WORKER_ARG)
        .arg("--spec")
        .arg(spec)
        .arg("--out")
        .arg(out)
        // 子进程的 stdio 全部丢弃：它的返回值只走结果文件，日志混进引擎 stderr 会污染
        // bridge 的 stderr_tail（那是判断引擎崩溃原因的地方）。
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 引擎是 GUI 子系统程序，本来不弹控制台；显式给 CREATE_NO_WINDOW 是防
        // 子进程被当成控制台程序时闪一下黑框。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|e| EngineError::other(format!("拉起 OCR 子进程失败: {e}")))?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms.clamp(1_000, 120_000));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(EngineError::timeout(format!(
                        "OCR 子进程超过 {timeout_ms}ms 没有返回，已强制结束；引擎本体未受影响"
                    )));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(EngineError::other(format!("等待 OCR 子进程失败: {e}")));
            }
        }
    };

    let text = std::fs::read_to_string(out).ok();
    let Some(text) = text else {
        // 子进程没写结果文件：最可能是它在 WinRT/GDI 里崩了（访问冲突）。
        return Err(EngineError::other(format!(
            "OCR 子进程没有产出结果（{}）；这次调用已在独立进程里降级为结构化错误，引擎本体未受影响。\
             可改用 region 缩小识别区域，或改用 win_capture + read_image",
            describe_exit(status)
        )));
    };

    let outcome: Outcome = serde_json::from_str(&text).map_err(|e| {
        EngineError::other(format!("OCR 子进程结果不是合法 JSON（退出码 {}）: {e}", describe_exit(status)))
    })?;
    if outcome.ok {
        outcome.result.ok_or_else(|| {
            EngineError::other("OCR 子进程报告成功但没有 result 字段（内部协议错误）")
        })
    } else {
        Err(rebuild_error(outcome.code.as_deref(), outcome.message))
    }
}

/// 把子进程回传的 `code` + `message` 还原成引擎错误类型。
fn rebuild_error(code: Option<&str>, message: Option<String>) -> EngineError {
    let message = message.unwrap_or_else(|| "OCR 子进程返回了错误但没有说明".to_string());
    match code.and_then(crate::error::ErrorCode::parse) {
        Some(crate::error::ErrorCode::InvalidArgs) => EngineError::invalid(message),
        Some(crate::error::ErrorCode::NotFound) => EngineError::not_found(message),
        Some(crate::error::ErrorCode::StaleSnapshot) => EngineError::stale(message),
        Some(crate::error::ErrorCode::PasswordField) => EngineError::PasswordField(message),
        Some(crate::error::ErrorCode::Unsupported) => EngineError::unsupported(message),
        Some(crate::error::ErrorCode::Timeout) => EngineError::timeout(message),
        _ => EngineError::other(message),
    }
}

/// 退出码的人话描述：0xC0000005 这类 NTSTATUS 直接换算成十六进制更好认。
fn describe_exit(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => {
            if code < 0 || (code as u32) >= 0x8000_0000 {
                format!("退出码 {code} / 0x{:08X}", code as u32)
            } else {
                format!("退出码 {code}")
            }
        }
        None => "被信号终止".to_string(),
    }
}

/// 临时文件路径：`%TEMP%\dsh-screen-control\ocr-<pid>-<毫秒>-<序号>.json`。
fn temp_path(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut dir = std::env::temp_dir();
    dir.push("dsh-screen-control");
    dir.push(format!("{tag}-{}-{}-{}.json", std::process::id(), stamp, seq));
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    dir
}

// ---------------------------------------------------------------------------
// 子进程入口
// ---------------------------------------------------------------------------

/// 子进程 main：读 spec -> 截图 + OCR -> 写结果 JSON -> 返回退出码。
///
/// 退出码约定：0 = 结果文件已写（成功或结构化错误都算）；2 = 连结果文件都写不出来。
pub fn worker_main(spec_path: &str, out_path: &str) -> i32 {
    let spec: Option<Spec> = std::fs::read_to_string(spec_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let Some(spec) = spec else {
        let _ = write_outcome(
            out_path,
            &Outcome::err(
                "INVALID_ARGS",
                format!("OCR 子进程读不到参数文件: {spec_path}"),
            ),
        );
        return 2;
    };

    let outcome = match worker_recognize(&spec) {
        Ok(result) => Outcome::ok(result),
        Err(e) => Outcome::err(e.code().as_str(), e.to_string()),
    };
    match write_outcome(out_path, &outcome) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("OCR 子进程写结果失败: {e}");
            2
        }
    }
}

fn worker_recognize(spec: &Spec) -> EngineResult<OcrResult> {
    // 子进程是独立进程，DPI 感知必须自己声明（main() 里已经按同一顺序声明过；
    // 这里再显式调用一次，是为了让"worker 被单测直接调用"时行为也正确）。
    let _ = dpi::declare_awareness();

    // 父进程已经截好图（win_capture(ocr=true)）：直接对这份 RGBA 识别，
    // 不再重复截图——重复截图还会让"识别的那一帧"和用户拿到的那张图对不上。
    if let Some(img) = &spec.image {
        let raw = std::fs::read(&img.path).map_err(|e| {
            EngineError::other(format!("OCR 子进程读不到父进程传过来的图像（{}）: {e}", img.path))
        })?;
        let expect = img.width as usize * img.height as usize * 4;
        if raw.len() != expect {
            return Err(EngineError::invalid(format!(
                "OCR 子进程收到的图像字节数不对：{}/{}（{}x{} RGBA8）",
                raw.len(),
                expect,
                img.width,
                img.height
            )));
        }
        let outcome = ocr::recognize(&raw, img.width, img.height, spec.language.as_deref())?;
        // 图是父进程截的：坐标已经是「图片像素」，origin 固定 (0,0)，没有二次截图可言。
        return Ok(OcrResult {
            text: outcome.text,
            lines: outcome.lines,
            language: outcome.language,
            available_languages: outcome.available_languages,
            origin: Point { x: 0, y: 0 },
            width: img.width,
            height: img.height,
            ocr_ms: outcome.ocr_ms,
            capture_ms: 0,
            backend: "parent-capture".to_string(),
            path: None,
        });
    }

    let hwnd = match spec.hwnd {
        Some(raw) => Some(window::require_window(raw, "hwnd")?),
        None => None,
    };
    ocr::capture_and_recognize(hwnd, spec.region, spec.language.as_deref(), spec.keep_file)
}

fn write_outcome(path: &str, outcome: &Outcome) -> EngineResult<()> {
    let json = serde_json::to_vec(outcome)?;
    std::fs::write(path, json)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 离线单元测试（不碰屏幕、不做 OCR：只测协议封装与退出码描述）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::protocol::{OcrLine, Point};

    fn sample_result() -> OcrResult {
        OcrResult {
            text: "hello 屏幕".to_string(),
            lines: vec![OcrLine { text: "hello".to_string(), rect: Rect { x: 1, y: 2, w: 3, h: 4 } }],
            language: "zh-Hans-CN".to_string(),
            available_languages: vec!["zh-Hans-CN".to_string()],
            origin: Point { x: 10, y: 20 },
            width: 100,
            height: 50,
            ocr_ms: 7,
            capture_ms: 3,
            backend: "bitblt".to_string(),
            path: None,
        }
    }

    /// spec/结果都是父子进程之间唯一的接口，序列化必须能原样往返。
    #[test]
    fn spec_and_outcome_roundtrip() {
        let spec = Spec {
            hwnd: Some(1234),
            region: Some(Rect { x: 1, y: 2, w: 3, h: 4 }),
            language: Some("en-US".to_string()),
            keep_file: true,
            image: None,
        };
        let text = serde_json::to_string(&spec).unwrap();
        let back: Spec = serde_json::from_str(&text).unwrap();
        assert_eq!(back.hwnd, Some(1234));
        assert_eq!(back.region.unwrap().w, 3);
        assert_eq!(back.language.as_deref(), Some("en-US"));
        assert!(back.keep_file);

        let outcome = Outcome::ok(sample_result());
        let text = serde_json::to_string(&outcome).unwrap();
        let back: Outcome = serde_json::from_str(&text).unwrap();
        assert!(back.ok);
        assert_eq!(back.result.unwrap().text, "hello 屏幕");
    }

    /// 错误码必须能从子进程原样还原，否则"OCR 语言包缺失"会被吞成通用 ENGINE_ERROR。
    #[test]
    fn error_code_is_restored() {
        for (code, expected) in [
            ("INVALID_ARGS", ErrorCode::InvalidArgs),
            ("NOT_FOUND", ErrorCode::NotFound),
            ("UNSUPPORTED", ErrorCode::Unsupported),
            ("TIMEOUT", ErrorCode::Timeout),
            ("PASSWORD_FIELD", ErrorCode::PasswordField),
            ("STALE_SNAPSHOT", ErrorCode::StaleSnapshot),
            ("ENGINE_ERROR", ErrorCode::EngineError),
        ] {
            let e = rebuild_error(Some(code), Some("测试消息".to_string()));
            assert_eq!(e.code(), expected, "code={code} 还原错了: {e}");
            assert!(e.to_string().contains("测试消息"), "{e}");
        }
        // 未知/缺失 code 兜底成 ENGINE_ERROR，不能 panic。
        let e = rebuild_error(None, None);
        assert_eq!(e.code(), ErrorCode::EngineError);
    }

    /// 退出码 0xC0000005（访问冲突）在 i32 里是负数；报错信息里要能一眼认出来。
    #[test]
    fn access_violation_exit_code_is_hex_printed() {
        let text = describe_exit_label(0xC000_0005u32 as i32);
        assert!(text.contains("0xC0000005"), "{text}");
        // 正常退出码不画蛇添足加十六进制。
        assert_eq!(describe_exit_label(0), "退出码 0");
        assert_eq!(describe_exit_label(3), "退出码 3");
    }

    /// crate 私有的 describe_exit 基于 ExitStatus 很难构造，这里把格式化逻辑抽出来测。
    fn describe_exit_label(code: i32) -> String {
        if code < 0 || (code as u32) >= 0x8000_0000 {
            format!("退出码 {code} / 0x{:08X}", code as u32)
        } else {
            format!("退出码 {code}")
        }
    }
}
