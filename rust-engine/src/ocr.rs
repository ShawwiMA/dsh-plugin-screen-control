//! `win_ocr`：调用 **Windows.Media.Ocr**（WinRT 原生 OCR）识别截图里的文字。
//!
//! 为什么不自己搓 OCR 引擎：太重、效果还差。系统自带的 OCR 语言包就是为这个场景
//! 准备的，识别质量由微软负责；本机没装对应语言包时**直接返回 UNSUPPORTED**，
//! 让上层模型用视觉能力兜底，而不是硬编一个更烂的结果出来。
//!
//! 两个必须这样做的实现细节：
//!
//! 1. **每次识别都在自己的 MTA 线程里跑**（`RoInitialize(RO_INIT_MULTITHREADED)`）。
//!    引擎主线程在开 `uia` feature 时可能已经是 STA，而 STA 线程上阻塞等
//!    `IAsyncOperation::get()` 会死锁（那需要消息泵）。独立线程把这个问题从
//!    "取决于 feature 组合"变成"永远不存在"。
//! 2. **RGBA -> BGRA 时把 alpha 强制写成 255**。GDI 的 32 位 DIB 不维护 alpha 通道，
//!    直接当 Bgra8 + Premultiplied 送进去，整张图会被当成全透明，OCR 什么都读不到。
//!
//! 编译开关：`--features ocr`（**默认开启**）。关掉后 win_ocr / win_wait(text)
//! 返回 UNSUPPORTED，其余功能不受影响。

use std::time::Instant;

use windows_sys::Win32::Foundation::HWND;

use crate::error::{EngineError, EngineResult};
use crate::protocol::{CaptureBackend, ImageFormat, OcrLine, OcrResult, Point, Rect};
use crate::{capture, dpi};

/// 本构建是否编译了 OCR 支持。
pub const COMPILED: bool = cfg!(feature = "ocr");

/// 一次识别得到的纯数据（不含 WinRT 类型，可跨线程传）。
pub struct OcrOutcome {
    pub text: String,
    pub lines: Vec<OcrLine>,
    pub language: String,
    pub available_languages: Vec<String>,
    pub ocr_ms: u64,
}

/// 识别一块 RGBA8 像素。`language` 为空时用系统语言。
///
/// 错误约定（上层按这个分流）：
/// - `UNSUPPORTED`：本构建没编 OCR / 系统没有可用的 OCR 引擎 / 语言包缺失 /
///   图片超过本机 OCR 引擎的尺寸上限（[`run_on_mta_thread`] 里的护栏）。
///   上层应当改用模型自己的视觉能力看图，或传 region 缩小识别区域，而不是重试。
/// - `ENGINE_ERROR`：WinRT 调用本身失败（内存不足等）。
pub fn recognize(
    rgba: &[u8],
    width: u32,
    height: u32,
    language: Option<&str>,
) -> EngineResult<OcrOutcome> {
    if !COMPILED {
        return Err(EngineError::unsupported(
            "本构建没有编译 Windows OCR（需要 --features ocr，默认开启）；请改用模型自己的视觉能力看图",
        ));
    }
    if width == 0 || height == 0 {
        return Err(EngineError::invalid("OCR 输入尺寸为 0"));
    }
    let need = width as usize * height as usize * 4;
    if rgba.len() < need {
        return Err(EngineError::other(format!(
            "OCR 输入像素不完整：期望 {need} 字节，实际 {}",
            rgba.len()
        )));
    }

    // RGBA -> BGRA 在**调用线程**就地做：先拷一份（OCR 线程要所有权），再原地交换
    // R/B、alpha 强制 255。2560x1600 的整屏图一份就是 16MB，能少一次整幅拷贝就少一次
    // （真机出问题的那次环境是提权 + 游戏全屏，内存本来就紧张）。
    let mut bgra = rgba[..need].to_vec();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }

    let lang = language.map(str::to_string);
    let handle = std::thread::Builder::new()
        .name("dsh-ocr".to_string())
        .spawn(move || run_on_mta_thread(&bgra, width, height, lang.as_deref()))
        .map_err(|e| EngineError::other(format!("创建 OCR 线程失败: {e}")))?;
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(EngineError::other("OCR 线程 panic（WinRT 调用异常）")),
    }
}

/// 截图 + OCR 一条龙。`win_ocr` 与 `win_wait(text)` 共用这一条路径。
///
/// 后端用 `Auto`（先 PrintWindow，拿到全黑才回退 Bitblt）。理由：GPU 渲染的窗口
/// （Chromium/Electron/微信 4.0）用窗口 DC 做 BitBlt 得到的是**纯黑图**，OCR 会
/// 一个字都读不到（2026-10-16 实测）。PrintWindow 拿得到这类窗口的真实内容，
/// 而且窗口被遮挡时照样能读。给了 `hwnd` 就让 PrintWindow 打印那个窗口，
/// 没给 hwnd 时 PrintWindow 不可用，自动走屏幕 Bitblt。
/// `keep_file` 为 true 时顺手落一张不缩放的 PNG，方便人肉核对识别源图。
pub fn capture_and_recognize(
    hwnd: Option<HWND>,
    region: Option<Rect>,
    language: Option<&str>,
    keep_file: bool,
) -> EngineResult<OcrResult> {
    let timer = capture::Timer::start();
    let raw = capture::capture(hwnd, region, CaptureBackend::Auto)?;
    let (width, height) = (raw.width, raw.height);
    let backend = raw.backend.as_str().to_string();
    let capture_ms = timer.elapsed_ms();

    let outcome = recognize(&raw.rgba, width, height, language)?;

    let path = if keep_file {
        Some(capture::finalize(raw, 0, ImageFormat::Png, capture_ms)?.path)
    } else {
        None
    };

    Ok(OcrResult {
        text: outcome.text,
        lines: outcome.lines,
        language: outcome.language,
        available_languages: outcome.available_languages,
        origin: origin_of(hwnd, region)?,
        width,
        height,
        ocr_ms: outcome.ocr_ms,
        capture_ms,
        backend,
        path,
    })
}

/// 截图区域左上角在屏幕坐标系里的位置。
///
/// 必须和 `capture::capture` 的区域选择规则完全一致（region > 窗口矩形 > 虚拟桌面），
/// 否则 OCR 行矩形换算出来的屏幕坐标会整体偏移。
pub fn origin_of(hwnd: Option<HWND>, region: Option<Rect>) -> EngineResult<Point> {
    if let Some(r) = region {
        return Ok(Point { x: r.x, y: r.y });
    }
    match hwnd {
        Some(h) => {
            let r = dpi::window_rect(h)?;
            Ok(Point { x: r.x, y: r.y })
        }
        None => {
            let v = dpi::virtual_screen_rect();
            Ok(Point { x: v.x, y: v.y })
        }
    }
}

// ---------------------------------------------------------------------------
// WinRT 实现（只在 --features ocr 下编译）
// ---------------------------------------------------------------------------

#[cfg(feature = "ocr")]
fn run_on_mta_thread(
    bgra: &[u8],
    width: u32,
    height: u32,
    language: Option<&str>,
) -> EngineResult<OcrOutcome> {
    use windows::Globalization::Language;
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Security::Cryptography::CryptographicBuffer;
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::core::HSTRING;

    // SAFETY: 本线程刚创建、只服务这一次调用，RoInitialize 只会在这里被调用一次。
    // windows 0.62 把 HRESULT 折成了 Result：失败（含 RPC_E_CHANGED_MODE）会进 Err。
    if let Err(e) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        return Err(EngineError::unsupported(format!(
            "WinRT 初始化失败（RoInitialize: {e}）：当前环境不支持 Windows.Media.Ocr"
        )));
    }

    // 尺寸护栏：先问 OCR 引擎自己的上限（`OcrEngine::MaxImageDimension`，本机 10000）。
    // 超限的图不送进 WinRT——它的行为在不同 Windows 版本/语言包上不一致
    // （有的返回 E_INVALIDARG，有的直接访问冲突），在**我们这一层**拒绝并给
    // 结构化错误，调用方拿到后可以改用 region 缩小识别区域。
    match OcrEngine::MaxImageDimension() {
        Ok(max) if max > 0 && (width > max || height > max) => {
            return Err(EngineError::unsupported(format!(
                "OCR 图片 {width}x{height} 超过本机 OCR 引擎上限 {max}px：请用 region 缩小识别区域，\
                 或先把截图缩放后再识别；不要用同一尺寸重试"
            )));
        }
        // 拿不到上限（老系统/WinRT 调用失败）就不拦，照常往下走。
        _ => {}
    }

    let available = available_languages();

    let engine = match language {
        Some(tag) => {
            let lang = Language::CreateLanguage(&HSTRING::from(tag)).map_err(|e| {
                EngineError::unsupported(format!(
                    "OCR 语言 {tag:?} 在本机不可用（{e}）；可用语言: {}",
                    list_or_none(&available)
                ))
            })?;
            OcrEngine::TryCreateFromLanguage(&lang)
        }
        None => OcrEngine::TryCreateFromUserProfileLanguages(),
    };
    let engine = match engine {
        Ok(e) => e,
        Err(e) => {
            return Err(EngineError::unsupported(format!(
                "本机没有可用的 Windows OCR 引擎（{e}）。可用识别语言: {}。\
                 请改用模型自己的视觉能力看图，不要重试本工具。",
                list_or_none(&available)
            )));
        }
    };

    let buffer = CryptographicBuffer::CreateFromByteArray(bgra)
        .map_err(|e| EngineError::other(format!("构造 WinRT 缓冲区失败: {e}")))?;
    // 注意：windows 0.62 的 CreateCopyFromBuffer 只有 4 个参数（没有 alpha 模式参数），
    // 所以 alpha 必须在像素里自己写对（上面已经强制 255）。
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        width as i32,
        height as i32,
    )
    .map_err(|e| {
        EngineError::other(format!("构造 SoftwareBitmap 失败（{width}x{height}）: {e}"))
    })?;

    let started = Instant::now();
    let result = engine
        .RecognizeAsync(&bitmap)
        .and_then(|op| op.join())
        .map_err(|e| EngineError::other(format!("Windows OCR 识别失败: {e}")))?;
    let ocr_ms = started.elapsed().as_millis() as u64;

    let text = result.Text().map(|h| h.to_string_lossy()).unwrap_or_default();

    let mut lines: Vec<OcrLine> = Vec::new();
    if let Ok(line_list) = result.Lines() {
        for line in &line_list {
            lines.push(convert_line(&line));
        }
    }

    let used_language = engine
        .RecognizerLanguage()
        .ok()
        .and_then(|l| l.LanguageTag().ok())
        .map(|h| h.to_string_lossy())
        .unwrap_or_else(|| language.unwrap_or("system").to_string());

    Ok(OcrOutcome { text, lines, language: used_language, available_languages: available, ocr_ms })
}

/// 一行文本 + 行矩形。行本身没有矩形 API，只能把该行所有词的 BoundingRect 并起来。
#[cfg(feature = "ocr")]
fn convert_line(line: &windows::Media::Ocr::OcrLine) -> OcrLine {
    let text = line.Text().map(|h| h.to_string_lossy()).unwrap_or_default();
    let mut rect: Option<Rect> = None;
    if let Ok(words) = line.Words() {
        for word in &words {
            if let Ok(r) = word.BoundingRect() {
                let cur = Rect {
                    x: r.X.round() as i32,
                    y: r.Y.round() as i32,
                    w: r.Width.round() as i32,
                    h: r.Height.round() as i32,
                };
                rect = Some(match rect {
                    None => cur,
                    Some(prev) => union(prev, cur),
                });
            }
        }
    }
    OcrLine { text, rect: rect.unwrap_or_default() }
}

#[cfg(feature = "ocr")]
fn union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.w).max(b.x + b.w);
    let bottom = (a.y + a.h).max(b.y + b.h);
    Rect { x, y, w: right - x, h: bottom - y }
}

/// 本机可识别的语言列表。失败返回空表（这里只用来把错误信息写清楚）。
#[cfg(feature = "ocr")]
fn available_languages() -> Vec<String> {
    use windows::Media::Ocr::OcrEngine;
    let mut out = Vec::new();
    if let Ok(list) = OcrEngine::AvailableRecognizerLanguages() {
        for lang in &list {
            if let Ok(tag) = lang.LanguageTag() {
                out.push(tag.to_string_lossy());
            }
        }
    }
    out
}

#[cfg(feature = "ocr")]
fn list_or_none(list: &[String]) -> String {
    if list.is_empty() {
        "（一个都没有：系统没装任何 OCR 语言包）".to_string()
    } else {
        list.join(", ")
    }
}

// ---------------------------------------------------------------------------
// 未编译 OCR 时的占位实现
// ---------------------------------------------------------------------------

#[cfg(not(feature = "ocr"))]
fn run_on_mta_thread(
    _bgra: &[u8],
    _width: u32,
    _height: u32,
    _language: Option<&str>,
) -> EngineResult<OcrOutcome> {
    Err(EngineError::unsupported(
        "本构建没有编译 Windows OCR（需要 --features ocr，默认开启）",
    ))
}

// ---------------------------------------------------------------------------
// 离线单元测试
// ---------------------------------------------------------------------------
//
// 识别质量的验证**不需要真实窗口**：用 GDI 在内存 DIB 上画一行字，把像素直接
// 喂给 `recognize()`（这条路径只是"RGBA 字节 -> WinRT SoftwareBitmap -> 文本"，
// 与像素从哪来无关）。这样既能测识别准不准，又不碰屏幕、不抢前台、不注入输入。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::protocol::Rect;

    /// `OcrOutcome` 没必要实现 Debug，所以这里不用 `unwrap_err()`。
    fn expect_err(r: EngineResult<OcrOutcome>) -> EngineError {
        match r {
            Err(e) => e,
            Ok(_) => panic!("这一步应当失败"),
        }
    }

    #[test]
    fn rejects_zero_size_and_short_buffer() {
        let e = expect_err(recognize(&[], 0, 0, None));
        assert_eq!(e.code(), ErrorCode::InvalidArgs);

        // 4x4 需要 64 字节，只给 16 字节：必须是 ENGINE_ERROR 且说明差多少。
        let e = expect_err(recognize(&[0u8; 16], 4, 4, None));
        assert_eq!(e.code(), ErrorCode::EngineError);
        assert!(e.to_string().contains("期望 64 字节"), "{e}");
    }

    /// OCR 行矩形是相对截图起点的，上层要靠 `origin_of` 换算成屏幕坐标。
    /// 这条规则必须和 `capture` 的区域选择完全一致，所以单独测。
    #[test]
    fn origin_of_region_is_region_topleft() {
        let r = Rect { x: 11, y: 22, w: 33, h: 44 };
        let o = origin_of(None, Some(r)).unwrap();
        assert_eq!((o.x, o.y), (11, 22), "给了 region 时原点就是 region 左上角");
    }

    /// 回归（0.1.8 真机 bug）：`win_ocr` 不带 region 时截整屏（本机 2560x1600），
    /// 引擎会以 0xC0000005 崩掉，由父进程重启。这条测试要求"整屏截图 + OCR"
    /// **要么成功，要么返回结构化错误，不许崩**。
    ///
    /// 分两步打印进度，崩在哪一步一眼可见：截图（capture）还是识别（recognize）。
    /// 只读：只截屏 + 识别，不点击、不注入输入。
    #[test]
    fn full_screen_ocr_does_not_crash() {
        // 引擎进程在 main() 里会把 DPI 感知声明成 PER_MONITOR_AWARE_V2；测试进程
        // 没有那一步，默认是 DPI 不感知的，GetSystemMetrics 返回的是**逻辑像素**
        // （本机 1707x1067 而不是物理 2560x1600），根本走不到真机崩溃的那条路上。
        // 所以这里必须先补上同一个声明，否则"测过了"是假的。
        let aware = dpi::declare_awareness();
        let screen = dpi::virtual_screen_rect();
        println!("DPI 感知={aware}，虚拟桌面 {}x{}", screen.w, screen.h);

        let raw = match capture::capture(None, None, CaptureBackend::Auto) {
            Ok(raw) => raw,
            Err(e) => {
                println!("整屏截图失败（允许，说明环境无桌面）：{e}");
                return;
            }
        };
        println!("整屏截图 OK：{}x{}，字节 {}", raw.width, raw.height, raw.rgba.len());
        match recognize(&raw.rgba, raw.width, raw.height, None) {
            Ok(out) => println!(
                "整屏 OCR OK：{} 行 / {} 字 / 语言 {} / ocr={}ms",
                out.lines.len(),
                out.text.chars().count(),
                out.language,
                out.ocr_ms
            ),
            Err(e) => println!("整屏 OCR 返回结构化错误（允许）：{} {e}", e.code()),
        }
    }

    /// 用 GDI 在内存 DIB 上画 "OCR TEST 12345"，然后直接 OCR 这块像素。
    /// 本机没装 en-US 语言包时跳过断言（打印实际可用语言），不算失败。
    #[cfg(feature = "ocr")]
    #[test]
    fn ocr_reads_gdi_rendered_text() {
        // RECT 在 Foundation 下，不在 Gdi 下。
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::Graphics::Gdi::*;

        let (w, h) = (640i32, 120i32);
        let bgra = unsafe {
            let screen_dc = GetDC(std::ptr::null_mut());
            let dc = CreateCompatibleDC(screen_dc);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = w;
            info.bmiHeader.biHeight = -h; // 负数 = 自上而下，和截图的行序一致
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;

            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            assert!(!dib.is_null(), "CreateDIBSection 失败");
            let old_bmp = SelectObject(dc, dib as _);

            let brush = CreateSolidBrush(0x00FF_FFFF); // 白底
            let full = RECT { left: 0, top: 0, right: w, bottom: h };
            FillRect(dc, &full, brush);
            DeleteObject(brush as _);

            SetBkMode(dc, TRANSPARENT as _);
            SetTextColor(dc, 0x0000_0000); // 黑字

            let face: Vec<u16> = "Consolas\0".encode_utf16().collect();
            let font = CreateFontW(
                72,
                0,
                0,
                0,
                FW_BOLD as _,
                0,
                0,
                0,
                DEFAULT_CHARSET as _,
                OUT_TT_PRECIS as _,
                CLIP_DEFAULT_PRECIS as _,
                CLEARTYPE_QUALITY as _,
                FF_DONTCARE as _,
                face.as_ptr(),
            );
            let old_font = SelectObject(dc, font as _);

            let mut line: Vec<u16> = "OCR TEST 12345".encode_utf16().collect();
            let mut tr = RECT { left: 12, top: 12, right: w - 12, bottom: h - 12 };
            DrawTextW(
                dc,
                line.as_mut_ptr(),
                line.len() as i32,
                &mut tr,
                (DT_LEFT | DT_TOP | DT_SINGLELINE) as _,
            );

            SelectObject(dc, old_font);
            DeleteObject(font as _);
            let n = (w * h * 4) as usize;
            let mut px = vec![0u8; n];
            std::ptr::copy_nonoverlapping(bits as *const u8, px.as_mut_ptr(), n);

            SelectObject(dc, old_bmp);
            DeleteObject(dib as _);
            DeleteDC(dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            px
        };

        // DIB 是 BGRA，转成引擎内部统一的 RGBA。
        let mut rgba = vec![0u8; bgra.len()];
        for (dst, src) in rgba.chunks_exact_mut(4).zip(bgra.chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = 255;
        }

        match recognize(&rgba, w as u32, h as u32, Some("en-US")) {
            Ok(out) => {
                let text = out.text.trim().to_string();
                println!("离线 OCR（内存 DIB）识别结果: {text:?}，语言 {}", out.language);
                if !out.available_languages.iter().any(|l| l.starts_with("en")) {
                    println!("本机没有英文 OCR 语言包，跳过断言：{:?}", out.available_languages);
                    return;
                }
                assert!(text.contains("12345"), "应当读出 12345，实际 {text:?}");
                assert!(text.to_uppercase().contains("OCR"), "应当读出 OCR，实际 {text:?}");
            }
            Err(e) if e.code() == ErrorCode::Unsupported => {
                println!("本机 Windows OCR 不可用，跳过（正是设计中的兜底路径）：{e}");
            }
            Err(e) => panic!("离线 OCR 失败: {e}"),
        }
    }
}
