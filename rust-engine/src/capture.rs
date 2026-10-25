//! 截图。
//!
//! 后端优先级：PrintWindow(PW_RENDERFULLCONTENT) 优先，BitBlt 回退。
//!
//! 为什么要这个顺序：BitBlt 从窗口 DC 拷贝，遇到 DirectComposition 合成
//! （浏览器、Electron、UWP、大部分现代 GUI）会拿到一片黑或者旧帧；
//! PrintWindow + PW_RENDERFULLCONTENT 是让窗口自己重绘一遍，能拿到真实内容。
//! 但 PrintWindow 对老式 GDI 程序偶发返回全黑，所以失败/全黑时要回退到 BitBlt。
//!
//! 所有 GDI 对象都用带 Drop 的包装结构管理，函数里见不到裸 DeleteObject。

use std::io::Cursor;
use std::path::PathBuf;
use std::time::Instant;

use image::{DynamicImage, ImageBuffer, Rgba, RgbaImage};
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC,
    GetWindowDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::PW_RENDERFULLCONTENT;

// windows-sys 0.61 把 PrintWindow 归进了 `Win32::Storage::Xps` 命名空间（win32metadata
// 的归类结果），要拿到官方绑定就得为这一个函数打开整个 XPS feature。
// 下面的声明是逐字抄自 windows-sys 0.61.2 里那份绑定的：
//   fn PrintWindow(hwnd: HWND, hdcblt: HDC, nflags: PRINT_WINDOW_FLAGS) -> BOOL
// 其中 PRINT_WINDOW_FLAGS 就是 u32（PW_RENDERFULLCONTENT = 2）。
#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(hwnd: HWND, hdc: HDC, nflags: u32) -> BOOL;
}

use crate::dpi;
use crate::error::{EngineError, EngineResult};
use crate::protocol::{CaptureBackend, CaptureResult, ImageFormat, Rect, Size};

/// 一次捕获得到的原始像素（RGBA8，行序自上而下）。
pub struct RawImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub backend: CaptureBackend,
}

// ---------------------------------------------------------------------------
// RAII 包装
// ---------------------------------------------------------------------------

/// 窗口/屏幕 DC。
///
/// `owned_by_window` 决定用 ReleaseDC 还是 DeleteDC 释放：
/// GetDC/GetWindowDC 借来的 DC 必须 ReleaseDC 还给窗口，CreateCompatibleDC
/// 造出来的才用 DeleteDC。搞混会泄漏 GDI 对象，所以把这条规则封在类型里。
pub struct ScopedDc {
    hdc: HDC,
    release_to: Option<HWND>,
}

impl ScopedDc {
    /// 从窗口借 DC（GetWindowDC 含非客户区）。
    pub fn from_window(hwnd: HWND) -> EngineResult<Self> {
        // SAFETY: hwnd 由调用方保证有效；GetWindowDC 失败返回空句柄，下面显式判空。
        let hdc = unsafe { GetWindowDC(hwnd) };
        if hdc.is_null() {
            return Err(EngineError::other("GetWindowDC 失败"));
        }
        Ok(Self { hdc, release_to: Some(hwnd) })
    }

    /// 借屏幕 DC。`hwnd` 传空表示整个虚拟桌面。
    pub fn from_screen(hwnd: HWND) -> EngineResult<Self> {
        // SAFETY: 传空 HWND 表示取整个屏幕的 DC，这是 GetDC 的文档行为；
        // 非空时必须是有效窗口（调用方保证）。
        let hdc = unsafe { GetDC(hwnd) };
        if hdc.is_null() {
            return Err(EngineError::other("GetDC 失败"));
        }
        Ok(Self { hdc, release_to: Some(hwnd) })
    }

    /// 造一个与 `compatible` 兼容的内存 DC。
    pub fn compatible(compatible: HDC) -> EngineResult<Self> {
        // SAFETY: compatible 必须是有效 DC（调用方从 ScopedDc 拿的）。
        let hdc = unsafe { CreateCompatibleDC(compatible) };
        if hdc.is_null() {
            return Err(EngineError::other("CreateCompatibleDC 失败"));
        }
        Ok(Self { hdc, release_to: None })
    }

    pub fn raw(&self) -> HDC {
        self.hdc
    }
}

impl Drop for ScopedDc {
    fn drop(&mut self) {
        if self.hdc.is_null() {
            return;
        }
        match self.release_to {
            // SAFETY: hdc 是本结构体独占的、由 GetDC/GetWindowDC 借来的 DC，
            // 且借出时用的就是同一个 hwnd；Drop 只跑一次；释放后不再使用。
            Some(hwnd) => unsafe { ReleaseDC(hwnd, self.hdc) },
            // SAFETY: hdc 由 CreateCompatibleDC 创建，所有权归本结构体，只用 DeleteDC 释放。
            None => unsafe { DeleteDC(self.hdc) },
        };
    }
}

/// 位图句柄。
pub struct ScopedBitmap(pub HBITMAP);

impl Drop for ScopedBitmap {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: 位图由 CreateDIBSection/CreateCompatibleBitmap 创建，
            // 本结构体独占所有权；DeleteObject 只调用一次；调用后不再使用该句柄。
            unsafe { DeleteObject(self.0 as HGDIOBJ) };
        }
    }
}

/// 被选进 DC 的 GDI 对象，Drop 时自动还原旧对象。
///
/// 为什么必须还原：CreateCompatibleDC 一开始选了一张 1x1 的单色位图，
/// 不还原就 DeleteDC，那张原始位图会跟着泄漏（Windows 会警告 GDI 对象泄漏）。
pub struct SelectedObject {
    dc: HDC,
    previous: HGDIOBJ,
}

impl SelectedObject {
    pub fn new(dc: HDC, obj: HGDIOBJ) -> EngineResult<Self> {
        // SAFETY: dc 有效（来自 ScopedDc），obj 是刚创建且未被选入其它 DC 的 GDI 对象。
        let previous = unsafe { SelectObject(dc, obj) };
        if previous.is_null() {
            return Err(EngineError::other("SelectObject 失败"));
        }
        Ok(Self { dc, previous })
    }
}

impl Drop for SelectedObject {
    fn drop(&mut self) {
        // SAFETY: dc 与选入时是同一个；previous 是 SelectObject 返回的原对象，
        // 属于该 DC 且仍然有效；还原是幂等且必须的操作。
        unsafe { SelectObject(self.dc, self.previous) };
    }
}

// ---------------------------------------------------------------------------
// 捕获入口
// ---------------------------------------------------------------------------

/// 截取窗口或屏幕区域。
///
/// `hwnd` 为空表示整屏；`region` 为空表示取 `hwnd` 的整窗（或整屏）。
/// 坐标一律屏幕物理像素。
pub fn capture(
    hwnd: Option<HWND>,
    region: Option<Rect>,
    backend: CaptureBackend,
) -> EngineResult<RawImage> {
    let target = region.unwrap_or(match hwnd {
        Some(h) => dpi::window_rect(h)?,
        None => dpi::virtual_screen_rect(),
    });
    if target.is_empty() {
        return Err(EngineError::invalid("捕获区域为空（宽或高 <= 0）"));
    }

    match backend {
        CaptureBackend::PrintWindow => match try_print_window(hwnd, target) {
            Some(img) => Ok(img),
            None => Err(EngineError::other("PrintWindow 后端不可用或返回空图")),
        },
        CaptureBackend::Bitblt => bitblt(hwnd, target),
        CaptureBackend::Auto => {
            // auto：先试 PrintWindow；只有拿到非全黑内容才采用，否则回退 BitBlt。
            if let Some(img) = try_print_window(hwnd, target) {
                if !looks_blank(&img) {
                    return Ok(img);
                }
            }
            bitblt(hwnd, target)
        }
    }
}

/// 尝试 PrintWindow。任何一步失败都返回 None，由调用方决定回退策略。
///
/// **必须先渲染整窗再裁切**：`PrintWindow` 只认窗口，永远把整窗画到 DC 的 (0,0)，
/// 没有"只渲染子矩形"的用法。0.2.0 及以前这里按 `target.w/h` 建 DIB，于是
/// 「hwnd + 非 (0,0) 起点 region」拿到的是**窗口左上角那一块**，不是请求的区域——
/// 表现为 `win_capture` 传 region 却截到工具栏、动作生效验证永远 diff=0
/// （2026-10-24 0.2.1 实测抓出来并修掉）。
fn try_print_window(hwnd: Option<HWND>, target: Rect) -> Option<RawImage> {
    let hwnd = hwnd?; // 整屏捕获没有窗口可打印
    let win = dpi::window_rect(hwnd).ok()?;
    if win.w <= 0 || win.h <= 0 {
        return None;
    }

    let (bitmap, pixels) = create_dib(win.w, win.h).ok()?;
    let _bitmap_guard = ScopedBitmap(bitmap);

    // 先拿窗口 DC 只是为了有个兼容 DC 的参照；真正绘制由 PrintWindow 完成。
    let window_dc = ScopedDc::from_window(hwnd).ok()?;
    let mem_dc = ScopedDc::compatible(window_dc.raw()).ok()?;
    let _selected = SelectedObject::new(mem_dc.raw(), bitmap as HGDIOBJ).ok()?;

    // SAFETY: mem_dc 有效且已选入与**整窗同尺寸**的位图；hwnd 有效；
    // PW_RENDERFULLCONTENT 让窗口把 DirectComposition 内容也画进来；
    // 返回值 0 表示失败，下面立刻回退，不会使用未初始化的位图内容。
    let ok = unsafe { PrintWindow(hwnd, mem_dc.raw(), PW_RENDERFULLCONTENT) };
    if ok == 0 {
        return None;
    }

    let full = bgra_to_rgba(pixels, (win.w * win.h * 4) as usize);

    // 请求区域 == 整窗时直接返回，省一次拷贝。
    if target.x == win.x && target.y == win.y && target.w == win.w && target.h == win.h {
        return Some(RawImage {
            width: win.w as u32,
            height: win.h as u32,
            rgba: full,
            backend: CaptureBackend::PrintWindow,
        });
    }

    // 换算成窗口内坐标再裁切；越界部分夹掉（调用方通常已经 clamp 过）。
    let x0 = (target.x - win.x).clamp(0, win.w);
    let y0 = (target.y - win.y).clamp(0, win.h);
    let w = target.w.min(win.w - x0);
    let h = target.h.min(win.h - y0);
    if w <= 0 || h <= 0 {
        return None;
    }
    let row_bytes = (w * 4) as usize;
    let mut out = Vec::with_capacity(row_bytes * h as usize);
    for row in 0..h {
        let start = (((y0 + row) * win.w + x0) * 4) as usize;
        out.extend_from_slice(&full[start..start + row_bytes]);
    }
    Some(RawImage {
        width: w as u32,
        height: h as u32,
        rgba: out,
        backend: CaptureBackend::PrintWindow,
    })
}

/// BitBlt 回退路径。
fn bitblt(hwnd: Option<HWND>, target: Rect) -> EngineResult<RawImage> {
    let (bitmap, pixels) = create_dib(target.w, target.h)?;
    let _bitmap_guard = ScopedBitmap(bitmap);

    // 源 DC 与源坐标一起决定，避免"先匹配 DC 再回头判断 hwnd"这种两步走。
    // 窗口 DC 的原点在窗口左上角（源点 = 目标点 - 窗口原点）；
    // 屏幕 DC 的原点在虚拟桌面左上角（源点 = 目标点本身）。
    let (src_dc, sx, sy) = match hwnd {
        Some(h) => {
            let origin = dpi::window_rect(h)?;
            (ScopedDc::from_window(h)?, target.x - origin.x, target.y - origin.y)
        }
        None => (ScopedDc::from_screen(std::ptr::null_mut())?, target.x, target.y),
    };
    let mem_dc = ScopedDc::compatible(src_dc.raw())?;
    let _selected = SelectedObject::new(mem_dc.raw(), bitmap as HGDIOBJ)?;

    // SAFETY: mem_dc 已选入同尺寸位图；src_dc 是有效的源 DC；
    // 源矩形与目标矩形尺寸一致（target.w/h），且都在对应 DC 的可见范围内
    // （调用方已用 window::intersect 裁剪过）；CAPTUREBLT 让分层窗口也参与拷贝。
    let ok = unsafe {
        BitBlt(
            mem_dc.raw(),
            0,
            0,
            target.w,
            target.h,
            src_dc.raw(),
            sx,
            sy,
            SRCCOPY | CAPTUREBLT,
        )
    };
    if ok == 0 {
        return Err(EngineError::other("BitBlt 失败"));
    }

    let rgba = bgra_to_rgba(pixels, (target.w * target.h * 4) as usize);
    Ok(RawImage {
        width: target.w as u32,
        height: target.h as u32,
        rgba,
        backend: CaptureBackend::Bitblt,
    })
}

/// 创建 32bpp 自顶向下 DIB，返回 (位图句柄, 像素缓冲裸指针)。
///
/// 用负的 biHeight 让位图自顶向下存储，这样像素直接就是 PNG 需要的行序，
/// 省掉一次整图翻转——4K 图翻转一次是几十毫秒。
///
/// 返回裸指针是刻意的：位图句柄必须活得比指针久，把两者一起交给调用方管理，
/// 由调用方用 ScopedBitmap 兜住生命周期。
fn create_dib(width: i32, height: i32) -> EngineResult<(HBITMAP, *mut u8)> {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        // 负高度 = 自顶向下。
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: (width * height * 4) as u32,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: info 是栈上已完整初始化的 BITMAPINFO；bits 是栈上指针，API 会写入它；
    // hsection 传空表示由系统分配内存；offset 必须为 0。得到的是本进程可写的 DIB 内存，
    // 生命周期与返回的 HBITMAP 绑定（DeleteObject 时才释放），因此不能超出它使用。
    let bitmap =
        unsafe { CreateDIBSection(std::ptr::null_mut(), &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0) };
    if bitmap.is_null() || bits.is_null() {
        return Err(EngineError::other("CreateDIBSection 失败"));
    }
    Ok((bitmap, bits as *mut u8))
}

/// 备用方案：用 CreateCompatibleBitmap（不返回像素指针，需要 GetDIBits 才能读回）。
/// 当前未使用，保留是因为某些显卡驱动上 CreateDIBSection 会失败。
#[allow(dead_code)]
fn create_compatible_bitmap(dc: HDC, width: i32, height: i32) -> EngineResult<HBITMAP> {
    // SAFETY: dc 有效（调用方从 ScopedDc 取）；失败返回空句柄，已判空。
    let bitmap = unsafe { CreateCompatibleBitmap(dc, width, height) };
    if bitmap.is_null() {
        return Err(EngineError::other("CreateCompatibleBitmap 失败"));
    }
    Ok(bitmap)
}

/// BGRA（DIB 原生字节序）转 RGBA（PNG 需要）。
fn bgra_to_rgba(src: *mut u8, len: usize) -> Vec<u8> {
    // SAFETY: src 指向 CreateDIBSection 分配的 width*height*4 字节可读内存，
    // len 正好是那个尺寸（调用方按同一公式算出）；位图句柄在本函数返回前不会被释放
    // （由调用方的 ScopedBitmap 持有）；本函数只读不写。
    let slice = unsafe { std::slice::from_raw_parts(src, len) };
    let mut out = Vec::with_capacity(len);
    for px in slice.chunks_exact(4) {
        out.push(px[2]); // R
        out.push(px[1]); // G
        out.push(px[0]); // B
        out.push(px[3]); // A
    }
    out
}

/// 粗略判断是否全黑。
///
/// 用途：PrintWindow 在部分程序上"成功返回但内容全黑"，此时必须回退 BitBlt。
/// 采样而不是全量比较，是大图下省时间的取舍——全黑图不会只有几个像素是亮的。
fn looks_blank(img: &RawImage) -> bool {
    if img.rgba.is_empty() {
        return true;
    }
    let pixels = img.rgba.len() / 4;
    let step = (pixels / 512).max(1);
    let mut checked = 0usize;
    let mut nonblack = 0usize;
    let mut idx = 0usize;
    while idx < pixels {
        let base = idx * 4;
        if img.rgba[base] > 8 || img.rgba[base + 1] > 8 || img.rgba[base + 2] > 8 {
            nonblack += 1;
        }
        checked += 1;
        idx += step;
    }
    checked > 0 && nonblack == 0
}

// ---------------------------------------------------------------------------
// 缩放 + 编码 + 落盘
// ---------------------------------------------------------------------------

/// 缩放、编码 PNG、写临时文件，返回协议里的 CaptureResult。
pub fn finalize(
    raw: RawImage,
    max_dim: u32,
    format: ImageFormat,
    capture_ms: u64,
) -> EngineResult<CaptureResult> {
    // backend 必须在这之前取出来：下一行 scale_if_needed 会按值吃掉 raw，
    // 之后再读 raw.backend 就是 use-after-move（E0382）。
    let backend = raw.backend.as_str().to_string();
    let (img, scaled_from) = scale_if_needed(raw, max_dim);

    let mut png = Vec::new();
    // 用 write_to + ImageFormat::Png：image 0.25 里这是唯一没有泛型参数的公开编码入口，
    // 不需要显式写 PngEncoder<W> 的类型标注。
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| EngineError::other(format!("PNG 编码失败: {e}")))?;

    let path = temp_file_path(format);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &png)?;

    Ok(CaptureResult {
        path: path.to_string_lossy().to_string(),
        width: img.width(),
        height: img.height(),
        bytes: png.len() as u64,
        backend,
        capture_ms,
        scaled_from,
    })
}

/// 把一张原始截图按**原尺寸**编码成 PNG，写到调用方指定的路径。
///
/// 与 [`finalize`] 的分工：`finalize` 负责"缩放 + 落临时文件"的 IPC 路径；
/// 这个函数给命令行 `capture --out <绝对路径>` 用——不缩放（命令行截图常常是
/// 拿来当模板的，缩放会让模板匹配的 scale 一直对不上），路径由调用方给。
pub fn save_png_to(
    raw: RawImage,
    path: &std::path::Path,
    capture_ms: u64,
) -> EngineResult<CaptureResult> {
    let backend = raw.backend.as_str().to_string();
    let (w, h) = (raw.width, raw.height);
    let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, raw.rgba)
        .ok_or_else(|| EngineError::other("截图像素缓冲尺寸与宽高不一致"))?;
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(buffer)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| EngineError::other(format!("PNG 编码失败: {e}")))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &png)?;

    Ok(CaptureResult {
        path: path.to_string_lossy().to_string(),
        width: w,
        height: h,
        bytes: png.len() as u64,
        backend,
        capture_ms,
        scaled_from: None,
    })
}

/// 按最长边缩放到 max_dim 以内。max_dim 为 0 表示不缩放。
fn scale_if_needed(raw: RawImage, max_dim: u32) -> (DynamicImage, Option<Size>) {
    let (w, h) = (raw.width, raw.height);
    let buffer: RgbaImage = match ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, raw.rgba) {
        Some(buf) => buf,
        // 尺寸与缓冲区对不上（理论上不会发生，走了也只是放弃缩放而已）。
        None => return (DynamicImage::new_rgba8(w.max(1), h.max(1)), None),
    };
    let img = DynamicImage::ImageRgba8(buffer);

    let longest = w.max(h);
    if max_dim == 0 || longest <= max_dim {
        return (img, None);
    }

    // 用 thumbnail 而不是 resize：thumbnail 用整数比例快速缩，
    // 截图场景对画质不敏感，但 4K 缩到 1600 时速度差一个数量级。
    let scaled = img.thumbnail(max_dim, max_dim);
    (scaled, Some(Size { w, h }))
}

/// 临时文件路径：`%TEMP%\dsh-screen-control\shot-<pid>-<毫秒>-<序号>.<ext>`。
///
/// 带 pid 和毫秒时间戳是为了多实例/多次调用时不会互相覆盖——
/// 覆盖会导致 TS 侧刚拿到路径图就被下一张替换，读到错的内容。
fn temp_file_path(format: ImageFormat) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut dir = std::env::temp_dir();
    dir.push("dsh-screen-control");
    dir.push(format!(
        "shot-{}-{}-{}.{}",
        std::process::id(),
        stamp,
        seq,
        format.as_str()
    ));
    dir
}

/// 计时辅助：把 Instant 差值转成毫秒。
pub struct Timer(Instant);

impl Timer {
    pub fn start() -> Self {
        Self(Instant::now())
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.0.elapsed().as_millis() as u64
    }
}

/// 便捷入口：捕获 + 缩放 + 落盘一条龙，main.rs 直接用这个。
pub fn capture_to_file(
    hwnd: Option<HWND>,
    region: Option<Rect>,
    max_dim: u32,
    format: ImageFormat,
    backend: CaptureBackend,
) -> EngineResult<CaptureResult> {
    let timer = Timer::start();
    let raw = capture(hwnd, region, backend)?;
    let ms = timer.elapsed_ms();
    finalize(raw, max_dim, format, ms)
}

/// 供 uia/observe 复用的裁剪辅助：把请求区域裁到窗口范围内。
pub fn clamp_region_to_window(hwnd: HWND, region: Rect) -> EngineResult<Rect> {
    let win = dpi::window_rect(hwnd)?;
    Ok(crate::window::intersect(win, region))
}

/// 空矩形常量，避免调用点到处写 `Rect::default()` 时看不出意图。
pub fn empty_rect() -> Rect {
    Rect::default()
}

/// 让 RECT 在本模块内也被用到（PrintWindow 路径将来若改用 rcPaint 会需要）。
#[allow(dead_code)]
fn rect_to_win32(r: Rect) -> RECT {
    RECT { left: r.x, top: r.y, right: r.x + r.w, bottom: r.y + r.h }
}
