//! DPI 与坐标换算。
//!
//! 契约要求：所有对外坐标都是屏幕物理像素。这条只有在进程 DPI 感知为
//! PER_MONITOR_AWARE_V2 时才成立——否则 Windows 会替我们做 DPI 虚拟化，
//! GetWindowRect 返回的是被缩放过的逻辑坐标，投递坐标就会偏。
//! 所以 [`declare_awareness`] 必须在**任何**窗口 API 之前调用，
//! main.rs 也把它排在 tokio 运行时之前。

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    ClientToScreen, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST,
    MONITORINFO, MONITORINFOEXW, MonitorFromPoint, MonitorFromWindow, ScreenToClient,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetForegroundWindow, GetSystemMetrics, GetWindowRect, MONITORINFOF_PRIMARY,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::error::{EngineError, EngineResult, win32};
use crate::protocol::{MonitorInfo, Rect};

/// 声明 PER_MONITOR_AWARE_V2。
///
/// 返回是否成功。失败通常是两种情况：
/// 1. 进程清单（manifest）已经声明过 DPI 感知——那也没关系，效果一样，只是我们设不进去；
/// 2. 系统太老（Win10 1703 以前没有这个 API）。
///
/// 无论哪种都不该让引擎起不来，所以返回值交给调用方上报到 `engine_ping.dpi_aware`。
pub fn declare_awareness() -> bool {
    // SAFETY: 参数是常量句柄形式的 DPI_AWARENESS_CONTEXT，无所有权语义，
    // 不涉及任何指针；该调用在本进程任何线程任何时机都合法。
    let ok = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    ok != 0
}

/// 前台窗口句柄。没有前台窗口时返回空句柄。
pub fn foreground_window() -> HWND {
    // SAFETY: 无参数查询，任何线程可调用，返回值可能为空。
    unsafe { GetForegroundWindow() }
}

/// 取窗口所在显示器的 DPI。
///
/// 用 GetDpiForWindow 而不是 GetDpiForMonitor：后者的实现在 shcore.dll，
/// windows-sys 没在当前 feature 集合下导出它。语义上窗口 DPI 就等于其所在显示器 DPI，
/// 且本进程是 PER_MONITOR_AWARE_V2，取值不会被打折。
pub fn dpi_of_window(hwnd: HWND) -> Option<u32> {
    // SAFETY: 纯查询。GetDpiForWindow 对无效/已销毁句柄返回 0，不会崩溃，
    // 因此这里不需要调用方额外保证句柄存活。
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { None } else { Some(dpi) }
}

/// 枚举所有显示器。
pub fn enumerate_monitors() -> EngineResult<Vec<MonitorInfo>> {
    // 回调把 HMONITOR 攒进这个 Vec，用裸指针经 LPARAM 传进去。
    // EnumDisplayMonitors 的回调是同步的，返回前一定执行完，所以这段栈内存始终有效。
    let mut handles: Vec<HMONITOR> = Vec::new();
    let ctx = &mut handles as *mut Vec<HMONITOR> as LPARAM;

    // SAFETY: hdc 传 null 表示用当前线程的屏幕 DC；lprcclip 传 null 表示不裁剪；
    // 回调是 extern "system" 且只做 push；ctx 指向栈上 Vec，在同步回调期间
    // 既不会被移动也不会被释放，也没有第二个 &mut 别名。
    let ok = unsafe {
        EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(monitor_proc), ctx)
    };
    if ok == 0 {
        return Err(win32("EnumDisplayMonitors"));
    }

    let mut out = Vec::with_capacity(handles.len());
    for (i, h) in handles.into_iter().enumerate() {
        if let Some(info) = describe_monitor(h, i as u32) {
            out.push(info);
        }
    }
    if out.is_empty() {
        return Err(EngineError::other("没有枚举到任何显示器"));
    }
    Ok(out)
}

/// EnumDisplayMonitors 的回调，签名必须与 MONITORENUMPROC 完全一致。
/// MONITORENUMPROC = Option<unsafe extern "system" fn(HMONITOR, HDC, *mut RECT, LPARAM) -> BOOL>，
/// 所以这里也写成 unsafe fn（安全 fn 能自动强转，但写全了更好读）。
unsafe extern "system" fn monitor_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: data 是 enumerate_monitors 里 &mut Vec<HMONITOR> 的地址，类型一致；
    // 回调是同步的，Vec 在此期间不会被移动或释放；不存在其它 &mut 别名。
    unsafe {
        let vec = &mut *(data as *mut Vec<HMONITOR>);
        vec.push(monitor);
    }
    1 // 非 0 = 继续枚举
}

/// 读取单台显示器的矩形、工作区与设备名。`index` 是枚举顺序，回给 TS 侧做稳定标识。
fn describe_monitor(monitor: HMONITOR, index: u32) -> Option<MonitorInfo> {
    // 用 MONITORINFOEXW 而不是 MONITORINFO，是为了顺便拿到 szDevice 名称；
    // 传参时把指针转成内部的 MONITORINFO*（cbSize 也用 MONITORINFO 的大小，
    // 这正是 Win32 对 MONITORINFOEX 的既定用法：cbSize 填 MONITORINFO 的尺寸）。
    let mut ex = MONITORINFOEXW::default();
    ex.monitorInfo.cbSize = std::mem::size_of::<MONITORINFO>() as u32;

    // SAFETY: monitor 是 EnumDisplayMonitors 刚给出的有效 HMONITOR；
    // ex 是栈上已初始化结构体，cbSize 已按 API 要求填好，且 ex 布局以 MONITORINFO 开头，
    // 因此把内部字段的指针交给 API 写入是合法的。
    let ok = unsafe { GetMonitorInfoW(monitor, &mut ex.monitorInfo as *mut MONITORINFO) };
    if ok == 0 {
        return None;
    }

    let mi = ex.monitorInfo;
    Some(MonitorInfo {
        handle: monitor as u64,
        device_name: utf16_fixed_to_string(&ex.szDevice),
        index,
        rect: rect_from_win32(mi.rcMonitor),
        work_rect: rect_from_win32(mi.rcWork),
        is_primary: mi.dwFlags & MONITORINFOF_PRIMARY != 0,
        dpi: dpi_of_monitor(monitor),
    })
}

/// 显示器自身的 DPI（96 = 100%）。
///
/// 为什么不用给显示器建 DC 再查 GetDeviceCaps：GetDpiForMonitor 直接回答
/// "这块屏当前的有效 DPI"，在每显示器 DPI 感知的进程里就是渲染用的那个值，
/// 而且不需要 CreateDC/DeleteDC 这对资源。失败（老系统、参数不合法）返回 None，
/// 调用方按"未知"处理，不要拿 96 冒充。
fn dpi_of_monitor(monitor: HMONITOR) -> Option<u32> {
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    let mut x: u32 = 0;
    let mut y: u32 = 0;
    // SAFETY: monitor 由调用方保证来自 EnumDisplayMonitors/MonitorFrom*；
    // x/y 是栈上 u32，指针在同步调用内有效；HRESULT < 0 表示失败。
    let hr = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) };
    if hr < 0 || x == 0 { None } else { Some(x) }
}

/// 某窗口所在显示器上的 DPI（拿不到就是 None）。
pub fn monitor_dpi_for_window(hwnd: HWND) -> Option<u32> {
    // SAFETY: 纯查询；hwnd 无效时 MonitorFromWindow 返回空句柄，不会崩溃。
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    dpi_of_window(hwnd)
}

/// 屏幕点落在哪台显示器上。
pub fn monitor_from_point(x: i32, y: i32) -> Option<MonitorInfo> {
    let pt = POINT { x, y };
    // SAFETY: pt 是栈上值，指针在本次同步调用内有效；MONITOR_DEFAULTTONEAREST
    // 保证任何点都能拿到一台显示器，不会返回空。
    let monitor = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    // 单点查询没有"枚举顺序"可言，index 固定给 0；调用方只关心 rect/dpi。
    describe_monitor(monitor, 0)
}

/// 包含所有显示器在内的虚拟桌面矩形。
///
/// 为什么不用显示器并集：多显示器可能不是矩形排布（L 形），
/// SM_*VIRTUALSCREEN 才是 Windows 认可的坐标系原点与尺寸；
/// 从屏幕 DC 做 BitBlt 时必须用它算源坐标。
pub fn virtual_screen_rect() -> Rect {
    // SAFETY: 纯查询，无副作用。
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    Rect { x, y, w, h }
}

/// 屏幕坐标 -> 指定窗口的客户区坐标。
///
/// 这是 win_click 的核心换算：TS 侧给屏幕物理像素，窗口消息里要客户区坐标。
pub fn screen_to_client(hwnd: HWND, x: i32, y: i32) -> EngineResult<(i32, i32)> {
    let mut pt = POINT { x, y };
    // SAFETY: hwnd 由调用方保证是有效窗口（各调用点统一校验过 is_window）；
    // pt 是栈上局部变量，指针只在本同步调用内使用，API 只写这一个点。
    let ok = unsafe { ScreenToClient(hwnd, &mut pt) };
    if ok == 0 {
        return Err(win32("ScreenToClient"));
    }
    Ok((pt.x, pt.y))
}

/// 窗口客户区坐标 -> 屏幕坐标。
pub fn client_to_screen(hwnd: HWND, x: i32, y: i32) -> EngineResult<(i32, i32)> {
    let mut pt = POINT { x, y };
    // SAFETY: 同 screen_to_client，只在同步调用内使用栈上点。
    let ok = unsafe { ClientToScreen(hwnd, &mut pt) };
    if ok == 0 {
        return Err(win32("ClientToScreen"));
    }
    Ok((pt.x, pt.y))
}

/// 窗口矩形（含边框），屏幕物理像素。
pub fn window_rect(hwnd: HWND) -> EngineResult<Rect> {
    // SAFETY: RECT 是 POD，zeroed 得到合法初值；API 会完整写入它。
    let mut r: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: hwnd 由调用方保证有效；r 是栈上结构体，指针在同步调用内有效。
    let ok = unsafe { GetWindowRect(hwnd, &mut r) };
    if ok == 0 {
        return Err(win32("GetWindowRect"));
    }
    Ok(rect_from_win32(r))
}

/// 客户区矩形，已换算到屏幕物理像素。
///
/// GetClientRect 给的是 (0,0,w,h)，必须再 ClientToScreen 才是屏幕坐标——
/// IPC.md 里的 `client_rect` 要求屏幕坐标系，不要直接返回 (0,0,w,h)。
pub fn client_rect_on_screen(hwnd: HWND) -> EngineResult<Rect> {
    let (w, h) = client_size(hwnd)?;
    let (sx, sy) = client_to_screen(hwnd, 0, 0)?;
    Ok(Rect { x: sx, y: sy, w, h })
}

/// 客户区宽高。
pub fn client_size(hwnd: HWND) -> EngineResult<(i32, i32)> {
    // SAFETY: 同 window_rect。
    let mut r: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: hwnd 由调用方保证有效；r 是栈上结构体。
    let ok = unsafe { GetClientRect(hwnd, &mut r) };
    if ok == 0 {
        return Err(win32("GetClientRect"));
    }
    Ok((r.right - r.left, r.bottom - r.top))
}

pub fn rect_from_win32(r: RECT) -> Rect {
    Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top }
}

fn utf16_fixed_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
