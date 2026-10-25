//! `win_window_state`：窗口状态控制（maximize / minimize / restore / fullscreen）。
//!
//! 全屏用"无边框 + 置顶 + 铺满显示器"实现（`SetWindowPos`），不用
//! `SetWindowLongW` 切 `WS_POPUP` 那种激进做法：只清掉边框相关的位，
//! 退出时按**进入前记录的原始值**还原，不会把窗口搞成另一种样子。
//!
//! 两个必须做对的细节：
//!
//! 1. **样式只在第一次进入全屏时记录**。反复调 `fullscreen` 不能把"全屏后的样式"
//!    当成原始样式存下来，否则退出全屏会还原成无边框窗口。
//! 2. **不抢用户焦点**。`SetWindowPos` 带 `SWP_NOACTIVATE`，`ShowWindow` 之前用
//!    `AttachThreadInput` 把输入队列挂到当前前台线程上（否则前台锁可能让别的窗口
//!    抢先激活）。做完发现前台被换走了，就把原来的前台还回去并写进 warnings。

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GWL_STYLE, HWND_NOTOPMOST, HWND_TOPMOST, IsZoomed, SET_WINDOW_POS_FLAGS,
    SHOW_WINDOW_CMD, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SetWindowLongW, SetWindowPos, ShowWindow, WS_CAPTION, WS_EX_TOPMOST, WS_MAXIMIZEBOX,
    WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};

use crate::error::{EngineError, EngineResult, win32};
use crate::protocol::{Rect, WindowStateAction, WindowStateParams, WindowStateResult};
use crate::{dpi, input, window};

/// 全屏时要清掉的边框位。
const FRAME_MASK: i32 = (WS_CAPTION | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_SYSMENU) as i32;

/// 窗口没有全屏记录时的兜底：至少补回一套标准边框。
const DEFAULT_FRAME_MASK: i32 = FRAME_MASK;

/// 进入全屏前的现场。
#[derive(Debug, Clone, Copy)]
struct SavedWindow {
    style: i32,
    ex_style: i32,
    rect: Rect,
    was_maximized: bool,
}

/// 每个 hwnd 的全屏前现场。引擎重启后丢失——那时退出全屏只能做兜底恢复，
/// 并在 warnings 里如实说明"原样式细节还原不了"。
fn saved() -> &'static Mutex<HashMap<u64, SavedWindow>> {
    static SAVED: OnceLock<Mutex<HashMap<u64, SavedWindow>>> = OnceLock::new();
    SAVED.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn apply(p: &WindowStateParams) -> EngineResult<WindowStateResult> {
    let hwnd = window::require_window(p.hwnd, "hwnd")?;
    let raw = window::to_u64(hwnd);
    let mut warnings = Vec::new();

    // 记下调用前的前台窗口：窗口状态变化有可能把别的窗口顶到前台，做完要还回去。
    let guard = input::NoActivateGuard::new();
    // AttachThreadInput 挂到当前前台线程。不挂的话前台锁可能让不相干的窗口抢先激活，
    // 这正是"操作窗口状态却激活错窗口"的成因。
    let attached = input::attach_threads(dpi::foreground_window());

    let outcome = match p.action {
        WindowStateAction::Maximize => show(hwnd, SW_MAXIMIZE),
        WindowStateAction::Minimize => show(hwnd, SW_MINIMIZE),
        WindowStateAction::Restore => show(hwnd, SW_RESTORE),
        WindowStateAction::Fullscreen => enter_fullscreen(hwnd, raw, &mut warnings),
        WindowStateAction::ExitFullscreen => exit_fullscreen(hwnd, raw, &mut warnings),
    };

    drop(attached);
    outcome?;

    let minimized = p.action == WindowStateAction::Minimize;
    if guard.foreground_changed() {
        if minimized {
            warnings.push(
                "最小化会让目标窗口失去前台，这是系统语义（前台交给了下一个窗口），引擎不做还原"
                    .to_string(),
            );
        } else if input::restore_foreground(&guard) {
            warnings.push("操作过程中前台被切走，已把前台还原回原窗口".to_string());
        } else {
            warnings.push("操作过程中前台被切走，且未能还原，请手动确认当前前台窗口".to_string());
        }
    }

    // 复核：所有状态都回读一遍，不靠"我以为设置成功了"。
    let rect = dpi::window_rect(hwnd)?;
    let style = window::style_of(hwnd);
    let ex_style = window::ex_style(hwnd);
    let is_fullscreen = saved().lock().contains_key(&raw) && (style & FRAME_MASK) == 0;
    if p.action == WindowStateAction::Fullscreen && !is_fullscreen {
        warnings.push(format!(
            "进了全屏流程但复核不通过：记录={} style={style:#x}（边框位应为 0）",
            saved().lock().contains_key(&raw)
        ));
    }

    Ok(WindowStateResult {
        hwnd: raw,
        action: p.action.as_str().to_string(),
        is_visible: window::is_visible(hwnd),
        is_minimized: window::is_minimized(hwnd),
        // IsZoomed 对"最大化"的判定就是 Win32 的官方口径，不用自己比矩形。
        // SAFETY: 纯查询，对任何句柄值都安全。
        is_maximized: unsafe { IsZoomed(hwnd) } != 0,
        is_fullscreen,
        is_topmost: window::is_topmost(hwnd),
        rect,
        style,
        ex_style,
        warnings,
    })
}

/// ShowWindow。它的返回值是"调用前是否可见"，**不是**成功与否，所以不能拿来判失败。
fn show(hwnd: HWND, cmd: SHOW_WINDOW_CMD) -> EngineResult<()> {
    // SAFETY: 纯窗口状态调用，hwnd 已由 require_window 验过。
    unsafe { ShowWindow(hwnd, cmd) };
    Ok(())
}

fn enter_fullscreen(hwnd: HWND, raw: u64, warnings: &mut Vec<String>) -> EngineResult<()> {
    let style = window::style_of(hwnd);
    let ex_style = window::ex_style(hwnd);
    let rect = dpi::window_rect(hwnd)?;
    // SAFETY: 纯查询。
    let was_maximized = unsafe { IsZoomed(hwnd) } != 0;

    // 只在第一次进入时记录。重复进入全屏若覆盖记录，退出时会还原成"无边框窗口"。
    saved().lock().entry(raw).or_insert(SavedWindow { style, ex_style, rect, was_maximized });

    // 铺满窗口所在的显示器（不是虚拟桌面：多屏时全屏应该只占一块屏）。
    let monitor = dpi::monitor_from_point(rect.x + rect.w / 2, rect.y + rect.h / 2)
        .map(|m| m.rect)
        .unwrap_or_else(dpi::virtual_screen_rect);

    // 只改 GWL_STYLE，**不**在 GWL_EXSTYLE 里写 WS_EX_TOPMOST：那个位用 SetWindowLong
    // 写是无效的（文档明确不支持；2026-10-16 实测：写 8 进去回读还是 0）。
    // 置顶只能由 SetWindowPos(HWND_TOPMOST) 设置。
    set_style(hwnd, style & !FRAME_MASK, ex_style)?;
    // SWP_NOACTIVATE：全屏是"改外观"，不该顺手抢走用户焦点。
    set_pos(hwnd, HWND_TOPMOST, monitor, SWP_FRAMECHANGED | SWP_NOACTIVATE)?;

    // 回读 + 重试一次：改 GWL_STYLE 之后 Windows 会重算 ex-style，置顶位有被抹掉的
    // 可能。"全屏却不置顶"在调用方看来就是"没铺在最上面"，必须当场发现并如实报告。
    if window::ex_style(hwnd) & (WS_EX_TOPMOST as i32) == 0 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        set_pos(hwnd, HWND_TOPMOST, monitor, SWP_FRAMECHANGED | SWP_NOACTIVATE)?;
        if window::ex_style(hwnd) & (WS_EX_TOPMOST as i32) == 0 {
            warnings.push(
                "进入全屏后 WS_EX_TOPMOST 未生效（SetWindowPos 被拒）：窗口仍然无边框铺满，\
                 但盖不住置顶窗口"
                    .to_string(),
            );
        }
    }

    let after = window::style_of(hwnd);
    if after & FRAME_MASK != 0 {
        warnings.push(format!(
            "已请求无边框但复核时仍有边框位（style={after:#x}）：该窗口可能不接受外部改样式"
        ));
    }
    Ok(())
}

fn exit_fullscreen(hwnd: HWND, raw: u64, warnings: &mut Vec<String>) -> EngineResult<()> {
    let record = saved().lock().remove(&raw);
    let rect = dpi::window_rect(hwnd).unwrap_or_default();
    match record {
        Some(s) => {
            set_style(hwnd, s.style, s.ex_style)?;
            // 置顶位按记录还原：原来不置顶的必须用 HWND_NOTOPMOST 摘掉置顶，
            // 否则这个窗口会一直压着别人的窗口（很难被发现的那种后遗症）。
            let insert_after = if s.ex_style & (WS_EX_TOPMOST as i32) != 0 {
                HWND_TOPMOST
            } else {
                HWND_NOTOPMOST
            };
            set_pos(hwnd, insert_after, s.rect, SWP_FRAMECHANGED | SWP_NOACTIVATE)?;
            if s.was_maximized {
                show(hwnd, SW_MAXIMIZE)?;
            }
            Ok(())
        }
        None => {
            // 没有记录：引擎重启过，或这次全屏不是本引擎设的。不去猜原来的样式，
            // 只做"能让人继续用"的兜底恢复，并把损失写清楚。
            warnings.push(
                "引擎里没有该窗口全屏前的样式记录（引擎重启过，或全屏不是本引擎设的）：\
                 只能补回标准边框、取消置顶，并按当前显示器工作区放置；原样式细节还原不了"
                    .to_string(),
            );
            let style = window::style_of(hwnd);
            let ex_style = window::ex_style(hwnd);
            set_style(
                hwnd,
                style | DEFAULT_FRAME_MASK,
                ex_style & !(WS_EX_TOPMOST as i32),
            )?;
            let work = dpi::monitor_from_point(rect.x + rect.w / 2, rect.y + rect.h / 2)
                .map(|m| m.work_rect)
                .unwrap_or_else(dpi::virtual_screen_rect);
            set_pos(hwnd, HWND_NOTOPMOST, work, SWP_FRAMECHANGED | SWP_NOACTIVATE)?;
            Ok(())
        }
    }
}

/// 写样式并**回读复核**。
///
/// 不用 `SetWindowLongW` 的返回值判成败：它返回的是原值，原值恰好是 0 时和失败
/// 无法区分。回读一次 `GetWindowLongW` 是唯一能确定的做法。
fn set_style(hwnd: HWND, style: i32, ex_style: i32) -> EngineResult<()> {
    // SAFETY: 三个参数都是纯数值 + 已验过的 hwnd。
    unsafe {
        SetWindowLongW(hwnd, GWL_STYLE, style);
        SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style);
    }
    let got = window::style_of(hwnd);
    if got != style {
        return Err(EngineError::other(format!(
            "设置窗口样式无效：期望 {style:#x}，回读 {got:#x}（该窗口可能禁止外部改样式）"
        )));
    }
    Ok(())
}

fn set_pos(hwnd: HWND, insert_after: HWND, rect: Rect, flags: SET_WINDOW_POS_FLAGS) -> EngineResult<()> {
    // SAFETY: rect 是纯数值；insert_after 用的是 windows-sys 的 HWND_TOPMOST / HWND_NOTOPMOST 常量。
    let ok = unsafe {
        SetWindowPos(hwnd, insert_after, rect.x, rect.y, rect.w, rect.h, flags)
    };
    if ok == 0 {
        return Err(win32("SetWindowPos"));
    }
    Ok(())
}
