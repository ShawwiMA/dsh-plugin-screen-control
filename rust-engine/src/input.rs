//! 输入投递。
//!
//! **默认路径 = SendInput 真注入（2026-10-16 切换）**。
//!
//! 原来的默认是 PostMessage，卖点是"不抢前台、不动鼠标"；实测在 Qt 自绘窗口
//! （微信 4.0 / `MMUIRenderSubWindowHW`）上这条路径完全无效：消息进了队列、
//! `PostMessageW` 返回非零、界面一个字节都没变（同一窗口同一时刻 SendInput
//! 立刻生效，见 SKILL.md 第九节）。报"成功"却什么都没发生比报错更贵，
//! 所以默认改成真注入。
//!
//! 真注入的两个副作用在引擎内部处理掉，不再甩给调用方：
//!
//! 1. **光标**：注入前 `GetCursorPos` 记现场，注入后 `SetCursorPos` 搬回去。
//!    用户看到的是"光标闪一下"，而不是"鼠标被抢走"（实测过一次 21 次真注入
//!    把用户鼠标抢了 8 分钟）。
//! 2. **前台**：键盘注入前必须让目标窗口真的在前台，否则按键会打进用户正在用的
//!    窗口。这里用 `AttachThreadInput` + `SetForegroundWindow` 顶上去，注入后把
//!    前台还给原窗口。**注意**：这条打破了原来"禁止 SetForegroundWindow"的边界，
//!    代价是操作期间焦点会闪一下，且"无需激活前台窗口"这个卖点对
//!    click / press / scroll 不再成立——换来的是一致生效。
//!
//! 仍保留的显式路径：
//! - `win_type(method="postmessage")`：WM_CHAR 逐字符投递，不碰前台不动鼠标，
//!   对普通 Win32 编辑控件有效，对 Qt 自绘窗口无效。
//! - `win_type(method="clipboard")`：只发 Ctrl+V（走 SendInput），
//!   要求调用方自己先把文本放进剪贴板。
//!
//! 2026-10-18（0.1.9）起多了一条自动路径：`method="auto"` 且文本含**非 ASCII**
//! 字符时，引擎自己写 `CF_UNICODETEXT` 再按 Ctrl+V，并在返回值 `warnings` 里
//! 明说"剪贴板内容已被覆盖"。原因：`KEYEVENTF_UNICODE` 逐字符注入中文长串会
//! 偶发串位（真机实测），剪贴板粘贴是同一个系统里唯一稳定的中文输入方式。
//! 显式 `method="sendinput"` 语义不变（永远是逐字符 Unicode 注入）。
//!
//! 密码框：任何路径在投递前都必须过 [`ensure_not_password`]，命中即返回
//! PASSWORD_FIELD，不得输入。

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, POINT, WPARAM};
// GlobalFree 在 win32metadata 里被归到 Foundation（不是 Memory），别按直觉 import。
use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSEINPUT,
    MapVirtualKeyW, SendInput, VK_CONTROL, VK_LWIN, VK_MENU, VK_SHIFT, VK_V,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetCursorPos, PostMessageW, SetCursorPos, SetForegroundWindow, WM_CHAR,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN,
    WM_RBUTTONUP,
};

use crate::error::{EngineError, EngineResult};
use crate::path;
use crate::protocol::{ActionResult, ClientPoint, Delivery, Modifier, MouseButton};
use crate::{dpi, snapshot, window};

/// 一轮鼠标动作的默认间隔。太快（0ms）时有些程序会把 down/up 合并掉。
const CLICK_HOLD_MS: u64 = 12;

/// 还光标之前的等待。Qt / Electron 这类程序在处理点击时会异步再读一次光标位置，
/// 立刻搬走会让它读到新位置（表现为"点这里却滚/选到别处"）。
const CURSOR_RESTORE_DELAY_MS: u64 = 40;

/// 连按同一个键之间的间隔。0ms 的连击会被某些程序合并成一次。
const KEY_GAP_MS: u64 = 10;

/// 按下到松开之间的保持时间。
///
/// 游戏（Unity / DirectInput / RawInput 的键盘层）多半是**逐帧轮询**键盘状态：
/// 按下和抬起塞进同一批 SendInput（间隔 0ms）时，两帧之间看到的永远是"没按"，
/// 整个按键凭空消失。50ms 跨 3~8 帧（常见 60~144Hz），任何帧率都看得到按下。
const KEY_HOLD_MS: u64 = 50;

/// 逐个字符注入之间的间隔。整串一次性注入时目标端会串位，见 send_input_text。
const CHAR_GAP_MS: u64 = 3;

// ---------------------------------------------------------------------------
// NoActivateGuard
// ---------------------------------------------------------------------------

/// 前台窗口守卫。
///
/// 为什么必须有：SendInput 路径下，如果操作过程中前台窗口被换掉（用户 Alt+Tab、
/// 或者我们的注入触发了焦点变化），那么后续的按键会打进别人的窗口——
/// 这是最危险的一类 bug（可能在别人的聊天框里打字）。守卫在构造时记下当前
/// 前台窗口，在 Drop 时如果发现它变了就还原回去。
///
/// 注意：还原用的是 AttachThreadInput + SetFocus 组合之外的标准做法——
/// 我们**不**调用 SetForegroundWindow（被明令禁止），而是靠
/// `window::root_window` 比对 + 允许失败的最小干预策略：
/// 只有"原本的前台窗口仍然存在"时才尝试把它拉回来，且失败只记日志不报错。
pub struct NoActivateGuard {
    original_foreground: HWND,
    /// 记录是否在 Drop 里尝试过还原，供调试日志使用。
    pub restored: bool,
}

impl NoActivateGuard {
    /// 记录当前前台窗口。
    pub fn new() -> Self {
        let fg = dpi::foreground_window();
        Self { original_foreground: fg, restored: false }
    }

    pub fn original(&self) -> HWND {
        self.original_foreground
    }

    /// 前台窗口是否已经偏离。
    pub fn foreground_changed(&self) -> bool {
        let now = dpi::foreground_window();
        !window::same_window(now, self.original_foreground)
    }
}

impl Default for NoActivateGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NoActivateGuard {
    fn drop(&mut self) {
        // 这里只做"如实记录"。真正的还原动作交给 restore_foreground，
        // 因为 Drop 不该有副作用顺序依赖（panic=abort 下 Drop 甚至可能不跑）。
        self.restored = !self.foreground_changed();
    }
}

/// AttachThreadInput 的配对解绑。
///
/// 公开给同 crate 的 window_state.rs 用：改窗口状态前也要把输入队列挂到当前
/// 前台线程上，否则前台锁可能让不相干的窗口抢先激活。
pub(crate) struct AttachedInput {
    self_thread: u32,
    target_thread: u32,
}

impl Drop for AttachedInput {
    fn drop(&mut self) {
        // SAFETY: 与 attach 时使用的是同一对线程 ID，且参数从 1 改回 0 正好解绑；
        // 若不调用会导致两个线程的输入队列永久合并（用户键盘行为异常），
        // 所以这里必须解绑，且失败也无补偿手段，直接忽略返回值。
        unsafe {
            windows_sys::Win32::System::Threading::AttachThreadInput(
                self.self_thread,
                self.target_thread,
                0,
            );
        }
    }
}

fn thread_of(hwnd: HWND) -> u32 {
    let mut pid: u32 = 0;
    // SAFETY: hwnd 由调用方保证有效；pid 是栈上 u32，指针在同步调用内有效。
    // windows-sys 0.61 的 GetWindowThreadProcessId 只有两个参数——线程 ID 是**返回值**，
    // 不是第三个输出参数（SDK 文档里的 lpdwThreadId 在这份绑定里被省略了）。
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, &mut pid)
    }
}

/// 把 `hwnd` 所属线程的输入队列挂到当前线程上，返回的守卫 Drop 即解绑。
///
/// 为什么用它而不是 SetForegroundWindow：AttachThreadInput 只是共享输入队列，
/// 不改变窗口的激活状态，用户的视觉焦点不会跳。代价是必须成对解开，
/// 所以这里用 RAII 包住；挂不上就返回 None，不报错。
pub(crate) fn attach_threads(hwnd: HWND) -> Option<AttachedInput> {
    if hwnd.is_null() || !window::is_window(hwnd) {
        return None;
    }
    let target_thread = thread_of(hwnd);
    let self_thread = current_thread_id();
    if target_thread == 0 || target_thread == self_thread {
        return None;
    }
    // SAFETY: 两个线程 ID 都是刚查询得到的真实值；AttachThreadInput 的语义是
    // 把两个输入队列临时合并，成功返回非 0，失败（例如目标已退出）返回 0，
    // 不会留下半挂载状态，因此这里可以直接忽略错误码。
    let ok = unsafe {
        windows_sys::Win32::System::Threading::AttachThreadInput(self_thread, target_thread, 1)
    };
    if ok == 0 {
        return None;
    }
    Some(AttachedInput { self_thread, target_thread })
}

// ---------------------------------------------------------------------------
// 真注入现场守卫（光标 / 前台）
// ---------------------------------------------------------------------------

/// 真注入的"现场"：注入前的光标位置与前台窗口。
///
/// SendInput 不认识目标窗口，只认"光标在哪、谁在前台"。所以每次真注入都必须先把
/// 光标搬到位、必要时把目标窗口顶到前台，做完再还回去。不还的后果有实测：
/// 2026-10-16 那次微信测试里 21 次真注入把用户的鼠标抢了整整 8 分钟。
pub struct InjectionGuard {
    cursor_before: Option<(i32, i32)>,
    foreground_before: HWND,
    /// 本次是否动过光标 / 前台。没动过就不做无谓的还原，也不虚报"已还原"。
    cursor_moved: bool,
    focus_stolen: bool,
    /// 是否已经还过现场。Drop 里靠它避免重复还原（正常路径会显式调用 restore）。
    restored: bool,
}

impl InjectionGuard {
    pub fn new() -> Self {
        Self {
            cursor_before: cursor_position(),
            foreground_before: dpi::foreground_window(),
            cursor_moved: false,
            focus_stolen: false,
            restored: false,
        }
    }

    /// 声明"这次要搬光标"。调用方在真正搬动之前调一次。
    pub fn will_move_cursor(&mut self) {
        self.cursor_moved = true;
    }

    /// 把 `root` 顶到前台并等它落地；返回是否成功。
    ///
    /// 为什么必须 AttachThreadInput：Windows 有前台锁，后台进程直接
    /// SetForegroundWindow 常常被静默拒绝（返回值看着像成功，窗口没上来）。
    /// 先把自己的输入队列临时挂到当前前台线程上，再调就放行了；挂完必须解开
    /// （AttachedInput 的 Drop 负责，不解开会让两个线程的键盘行为互相串）。
    ///
    /// 返回 false 时调用方**必须放弃注入**，不能硬着头皮打键盘——那时候按键会落到
    /// 用户正在用的窗口里（最坏是在别人的聊天框里打字）。
    pub fn focus(&mut self, root: HWND) -> bool {
        if !root.is_null() && foreground_serves(root) {
            return true;
        }
        let current = dpi::foreground_window();
        let attached = attach_threads(current);
        // SAFETY: root 由调用方保证是有效窗口；这两个 API 只改窗口的激活状态和
        // Z 序，不分配内存、不持有跨调用指针。
        unsafe {
            SetForegroundWindow(root);
            BringWindowToTop(root);
        }
        drop(attached);
        self.focus_stolen = true;

        // 激活是异步的：SetForegroundWindow 返回时前台往往还没换过来，直接注入会把
        // 按键打进旧前台。这里最多等 300ms，用 GetForegroundWindow 复核。
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if foreground_serves(root) {
                return true;
            }
        }
        false
    }

    /// 还现场：光标搬回原处、前台还给原窗口。返回 (光标已还原, 前台已还原)。
    ///
    /// 顺序不能反：先还光标、再还前台。还前台会让原窗口激活重绘，如果那时光标还
    /// 停在目标窗口上，原窗口会收到一次假的 hover，用户看到界面闪。
    pub fn restore(&mut self) -> (bool, bool) {
        self.restored = true;
        let cursor_ok = if self.cursor_moved {
            match self.cursor_before {
                Some((x, y)) => {
                    std::thread::sleep(std::time::Duration::from_millis(CURSOR_RESTORE_DELAY_MS));
                    set_cursor_position(x, y)
                }
                // 取不到注入前的位置就如实报失败，不假装还原过。
                None => false,
            }
        } else {
            true
        };

        let focus_ok = if self.focus_stolen {
            restore_foreground_to(self.foreground_before)
        } else {
            true
        };

        (cursor_ok, focus_ok)
    }

    /// 把现场处理结果写进 `warnings`：调用方在工具返回值里就能看到这次注入到底
    /// 动不动用户的光标和焦点，不必去翻引擎 stderr。
    pub fn audit(&self, warnings: &mut Vec<String>, cursor_ok: bool, focus_ok: bool) {
        if self.cursor_moved {
            let before = match self.cursor_before {
                Some((x, y)) => format!("{x},{y}"),
                None => "未知".to_string(),
            };
            let after = if cursor_ok { "已还原到原位" } else { "还原失败，请手动确认光标" };
            warnings.push(format!("SendInput 真注入：注入前光标 ({before})，注入后{after}"));
        }
        if self.focus_stolen {
            let after = if focus_ok { "结束后已把前台还给原窗口" } else { "结束后前台未能还给原窗口，请手动确认" };
            warnings.push(format!("SendInput 真注入：为让输入落位临时激活了目标窗口，{after}"));
        }
    }
}

impl Drop for InjectionGuard {
    /// 兜底还现场。
    ///
    /// 为什么需要：注入中途任何一步返回 Err（SendInput 只提交了一部分、目标被 UIPI
    /// 拦截、调用方提前 return）都会跳过显式的 restore，用户的光标和前台就留在被
    /// 改动过的状态。Drop 里补一次，最坏情况是还原失败——那也只是原来的行为，不会更糟。
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        let _ = self.restore();
    }
}

impl Default for InjectionGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// 当前光标位置（屏幕物理像素）。
fn cursor_position() -> Option<(i32, i32)> {
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: p 是栈上 POINT，指针只在本次同步调用内使用。
    let ok = unsafe { GetCursorPos(&mut p) };
    if ok == 0 { None } else { Some((p.x, p.y)) }
}

/// 把光标搬到指定物理像素位置。
fn set_cursor_position(x: i32, y: i32) -> bool {
    // SAFETY: 两个 i32 参数，不涉及指针。
    unsafe { SetCursorPos(x, y) != 0 }
}

/// `root` 是不是当前前台窗口（比较顶层窗口，无边框/子窗口不会误判）。
fn foreground_is(root: HWND) -> bool {
    let fg = dpi::foreground_window();
    if fg.is_null() {
        return false;
    }
    window::same_window(window::root_window(fg), window::root_window(root))
}

/// 当前前台窗口能不能代表 `root` 收下键盘输入。两种都算：
///
/// 1. 就是 `root` 本身；
/// 2. `root` 同一个进程的另一个顶层窗口——微信 4.0 的搜索下拉框是 Chromium 渲染的
///    `Chrome_WidgetWin_0`，它自己是独立顶层窗口。2026-10-16 实测踩过这个坑：
///    把同进程下拉框当成"前台被换走"会让引擎去激活主窗口，而激活父窗口正好把展开
///    中的下拉框收起来——表现就是"字打进了搜索框、回车却什么都没打开"。
fn foreground_serves(root: HWND) -> bool {
    let fg = dpi::foreground_window();
    if fg.is_null() {
        return false;
    }
    if window::same_window(window::root_window(fg), window::root_window(root)) {
        return true;
    }
    let root_pid = window::process_id(root);
    root_pid != 0 && window::process_id(window::root_window(fg)) == root_pid
}

/// 把前台还给指定窗口。失败返回 false，不报错——用户可能已经自己切到别处了，
/// 这时候硬抢回来才是更讨厌的行为。
fn restore_foreground_to(target: HWND) -> bool {
    if target.is_null() || !window::is_window(target) {
        return false;
    }
    let current = dpi::foreground_window();
    let attached = attach_threads(current);
    // SAFETY: target 上面验过是有效窗口；只改激活状态。
    unsafe {
        SetForegroundWindow(target);
    }
    drop(attached);
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        if foreground_is(target) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// SendInput 前置闸门
// ---------------------------------------------------------------------------
//
// 为什么必须有这两道闸门：SendInput 是"注入到系统输入流"，它不认识目标窗口，
// 只认当前前台/焦点和光标位置。也就是说目标窗口一旦不在前台，我们按下的键会
// 原封不动打进用户正在用的那个窗口里——最坏情况是在别人的聊天框里打出一串字。
// 处理方式是主动拒绝并返回 UNSUPPORTED，把"让窗口到前台"这件事交回调用方，
// 而不是替用户去抢焦点（那是明确越界）。

/// 键盘注入前的闸门：**焦点控件不能是密码框**，而且目标窗口必须就在前台。
pub fn ensure_keyboard_reaches(ctx: &ActionContext) -> EngineResult<()> {
    // 密码框判定排在最前面。这里必须查"真正的焦点窗口"而不是 ctx.target：
    // win_type(hwnd=...) 时 ctx.target 是客户区中心命中的窗口（常常是根窗口），
    // 而按键实际落在焦点控件上。2026-10-16 用自测靶窗实测过这个洞——
    // 焦点在密码框里时，按 hwnd 发 win_type 会绕过 ctx.target 的样式检查。
    if let Some(focused) = window::focused_child(ctx.root) {
        ensure_not_password(focused, false)?;
    }

    let fg = dpi::foreground_window();
    if fg.is_null() {
        return Err(EngineError::unsupported(
            "当前没有前台窗口（会话可能已锁定），拒绝注入键盘输入",
        ));
    }
    if window::same_window(window::root_window(fg), ctx.root) {
        return Ok(());
    }
    // 同进程的弹出窗口（微信 4.0 的搜索下拉框就是 Chrome_WidgetWin_0）也算输入能
    // 落到位：那时候激活主窗口反而会把下拉框收起来。
    if foreground_serves(ctx.root) {
        return Ok(());
    }
    Err(EngineError::unsupported(format!(
        "目标窗口不在前台，键盘注入会落进「{}」(pid {})；请先让目标窗口成为前台窗口，\
         或对普通 Win32 编辑控件改用 win_type(method=\"postmessage\")",
        window::window_title(fg),
        window::process_id(window::root_window(fg))
    )))
}

/// 鼠标注入前的闸门：目标屏幕点必须确实落在目标窗口上。
///
/// 用 WindowFromPoint 做命中测试：返回的顶层窗口不是目标顶层窗口时，说明这一点
/// 被别的窗口盖住了，真点下去会点到别人身上，直接拒绝。
pub fn ensure_point_reaches(root: HWND, screen_x: i32, screen_y: i32) -> EngineResult<()> {
    let hit = window::window_at_point(screen_x, screen_y);
    if hit.is_null() {
        return Err(EngineError::unsupported(format!(
            "屏幕点 ({screen_x}, {screen_y}) 上没有可见窗口，已拒绝用 SendInput 点击"
        )));
    }
    let hit_root = window::root_window(hit);
    if window::same_window(hit_root, root) {
        return Ok(());
    }
    // 同一个进程的另一个顶层窗口不算"被别人盖住"：下拉候选、菜单、tooltip 这类
    // 弹出窗都是独立顶层窗口，点它本来就是调用方的意图。
    // 2026-10-16 实测过误拒：点微信搜索结果下拉框（和主窗口同进程）时，
    // 这里报「被「Weixin」盖住，已拒绝」，把调用方逼去用裸 SetCursorPos+click
    // 脚本——而那次裸点击正好点到登录窗的 × 上，把微信关掉了。
    // 放行的代价只是失去"跨进程遮挡"的保护，跨进程仍然拒绝。
    if window::process_id(hit_root) == window::process_id(root) && window::process_id(root) != 0 {
        return Ok(());
    }
    Err(EngineError::unsupported(format!(
        "屏幕点 ({screen_x}, {screen_y}) 被「{}」(pid {}) 盖住，点下去会点到别的窗口，已拒绝",
        window::window_title(hit_root),
        window::process_id(hit_root)
    )))
}

fn current_thread_id() -> u32 {
    // SAFETY: 无参数查询，返回当前线程 ID。
    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
}

// ---------------------------------------------------------------------------
// 密码框拦截
// ---------------------------------------------------------------------------

/// 投递前的统一安全闸门。
///
/// 三处信号任一命中即拒绝：
/// 1. 元素在快照里被标成 is_password（阶段一按样式位、阶段二按 UIA IsPassword）；
/// 2. 目标窗口本身的样式含 ES_PASSWORD；
/// 3. 目标窗口路径上任意一层祖先含 ES_PASSWORD（有些程序把密码框嵌在容器里）。
///
/// 宁可误拒也不能误放——误拒只是让用户换个方式，误放是密码泄漏。
pub fn ensure_not_password(target: HWND, elem_password_flag: bool) -> EngineResult<()> {
    if elem_password_flag {
        return Err(EngineError::PasswordField(format!(
            "目标元素被标记为密码框（hwnd={}），引擎拒绝向密码框输入",
            window::to_u64(target)
        )));
    }
    if window::is_password_edit(target) {
        return Err(EngineError::PasswordField(format!(
            "目标窗口样式含 ES_PASSWORD（hwnd={}），引擎拒绝向密码框输入",
            window::to_u64(target)
        )));
    }
    // 向上查祖先：最多 8 层，防止异常窗口链死循环。
    let mut cur = target;
    for _ in 0..8 {
        match window::parent_of(cur) {
            Some(p) => {
                if window::is_password_edit(p) {
                    return Err(EngineError::PasswordField(
                        "目标窗口的祖先含 ES_PASSWORD，引擎拒绝输入".to_string(),
                    ));
                }
                cur = p;
            }
            None => break,
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 动作入口
// ---------------------------------------------------------------------------

/// 一次动作的上下文：解析出来的目标窗口、元素（可选）与投递方式。
pub struct ActionContext {
    pub root: HWND,
    /// 命中测试得到的最深子窗口；消息实际发给它。
    pub target: HWND,
    /// 快照里的元素（如果这次动作是按 uid 发起的）。
    pub element: Option<crate::protocol::ElementInfo>,
    /// 屏幕坐标；None 表示"用目标窗口客户区中心"。
    pub screen_point: Option<(i32, i32)>,
    pub warnings: Vec<String>,
}

impl ActionContext {
    /// 解析客户区坐标：有屏幕点就换算，没有就用客户区中心。
    pub fn client_point(&self) -> EngineResult<ClientPoint> {
        match self.screen_point {
            Some((x, y)) => {
                let (cx, cy) = dpi::screen_to_client(self.target, x, y)?;
                Ok(ClientPoint { x: cx, y: cy })
            }
            None => {
                let (w, h) = dpi::client_size(self.target)?;
                Ok(ClientPoint { x: w / 2, y: h / 2 })
            }
        }
    }

    /// 组装 ActionResult。
    pub fn finish(self, delivered: Delivery, elapsed_ms: u64, client: ClientPoint) -> ActionResult {
        ActionResult {
            delivered: delivered.as_str().to_string(),
            target_hwnd: window::to_u64(self.root),
            deepest_child: window::to_u64(self.target),
            coords_client: client,
            sequence_ms: elapsed_ms,
            // 生效验证由 main.rs 在还完现场后再补（它必须在动作完成后才轮询轮询）。
            effect: None,
            warnings: self.warnings,
        }
    }
}

/// 鼠标点击。默认走真注入（SendInput）。
///
/// `force_sendinput` 参数保留只为兼容旧调用方：默认已经是 SendInput，传不传都一样。
/// 原来的 PostMessage 点击路径按 2026-10-16 的实测结论移除了——Qt 自绘窗口收下消息
/// 但不处理，属于"报了成功却没效果"，比直接报错更难查。
pub fn click(
    ctx: &mut ActionContext,
    button: MouseButton,
    clicks: u32,
    _force_sendinput: bool,
) -> EngineResult<(Delivery, ClientPoint, u64)> {
    let started = Instant::now();
    let client = ctx.client_point()?;
    let clicks = clicks.max(1).min(3); // 超过三击没有语义，直接夹住

    ensure_not_password(ctx.target, ctx.element.as_ref().map(|e| e.is_password).unwrap_or(false))?;

    // 坐标必须落实：uid 路径下 ctx.screen_point 是 None，不补算的话 SendInput 会在
    // "光标当前所在位置"点一下——那不是目标元素。
    let (sx, sy) = match ctx.screen_point {
        Some(p) => p,
        None => dpi::client_to_screen(ctx.target, client.x, client.y)?,
    };
    // 该点必须真落在目标窗口上（跨进程遮挡仍然拒绝，见 ensure_point_reaches）。
    ensure_point_reaches(ctx.root, sx, sy)?;

    // 鼠标路径**不**主动抢前台：真点击本来就会激活被点的窗口，这是 Windows 的语义。
    // 点完立刻把前台还回去，用户会以为没点上。只有键盘注入才需要主动切前台。
    let mut guard = InjectionGuard::new();
    guard.will_move_cursor();
    send_input_click(sx, sy, button, clicks, jiggle_point(ctx, sx, sy))?;
    let (cursor_ok, focus_ok) = guard.restore();
    guard.audit(&mut ctx.warnings, cursor_ok, focus_ok);

    Ok((Delivery::SendInput, client, started.elapsed().as_millis() as u64))
}

/// 按下之前先抖 1px 的那个落点。
///
/// 为什么抖动必须在**按下之前**：不少自绘控件（含 Qt）靠 hover 状态决定这一下算
/// 不算数，而光标本来就在目标点上时系统不会再发 WM_MOUSEMOVE，hover 状态是陈旧的。
/// 先落到旁边 1px 再回到目标点，至少产生一次真实移动。
/// 反过来"按下之后再抖"是把单击变成微拖拽（触发框选/拖拽语义），是错的。
///
/// 抖动落点必须仍在目标窗口范围内，否则窗口边界上的点击会抖到窗口外面去
/// （按 uid 点击时有元素矩形就用元素矩形，更准）。
fn jiggle_point(ctx: &ActionContext, sx: i32, sy: i32) -> Option<(i32, i32)> {
    let rect = match ctx.element.as_ref().map(|e| e.rect) {
        Some(r) => r,
        // 按坐标点击时没有元素矩形：用根窗口客户区兜底，至少保证抖动不越出窗口。
        None => dpi::client_rect_on_screen(ctx.root).ok()?,
    };
    if sx - 1 >= rect.x {
        Some((sx - 1, sy))
    } else if sx + 1 < rect.x + rect.w {
        Some((sx + 1, sy))
    } else {
        None // 目标窄到 1px，抖不动，直接点
    }
}

/// 抢前台失败时的统一报错：绝不"硬着头皮注入"——那时候按键会落进用户正在用的窗口。
fn foreground_required_err(root: HWND) -> EngineError {
    let fg = dpi::foreground_window();
    let fg_title = if fg.is_null() {
        "（当前没有前台窗口）".to_string()
    } else {
        window::window_title(fg)
    };
    EngineError::unsupported(format!(
        "无法把目标窗口「{}」切到前台（被 Windows 前台锁拒绝），已拒绝注入键盘——\
         否则这些按键会打进「{}」。请先把目标窗口点到前台再重试。",
        window::window_title(root),
        fg_title
    ))
}

/// 用 PostMessage 投递一次点击。
///
/// 现在没有调用方：保留它作为 PostMessage 路径的参照实现，将来给 win_click 补上
/// `method` 参数时可以直接用。
#[allow(dead_code)]
fn post_click(target: HWND, client: ClientPoint, button: MouseButton) -> EngineResult<bool> {
    let lp = make_lparam(client.x, client.y);
    // SAFETY: target 由调用方保证是有效窗口；PostMessageW 只是把消息投进队列，
    // 不等待处理，因此不会阻塞在无响应的目标进程上；wParam/lParam 都是按
    // Win32 消息约定组装的纯数值，不含指针。
    let move_ok = unsafe { PostMessageW(target, WM_MOUSEMOVE, 0, lp) };
    let (down, up) = match button {
        MouseButton::Left => (WM_LBUTTONDOWN, WM_LBUTTONUP),
        MouseButton::Right => (WM_RBUTTONDOWN, WM_RBUTTONUP),
        MouseButton::Middle => (WM_LBUTTONDOWN, WM_LBUTTONUP), // 中键用左键消息是刻意的保守选择
    };
    // SAFETY: 同上传；MK_LBUTTON(0x0001) 表示按键按下状态，是文档约定的 wParam。
    let down_ok = unsafe { PostMessageW(target, down, 0x0001, lp) };
    std::thread::sleep(std::time::Duration::from_millis(CLICK_HOLD_MS));
    // SAFETY: 同上。
    let up_ok = unsafe { PostMessageW(target, up, 0, lp) };
    Ok(move_ok != 0 && down_ok != 0 && up_ok != 0)
}

/// 用 SendInput 注入一次点击。
///
/// 坐标：**绝对**屏幕物理像素 + MOUSEEVENTF_VIRTUALDESK，归一化到 0..65535 的分母
/// 必须是虚拟桌面尺寸而不是主屏，否则多显示器下副屏上的点会被映射到屏幕外。
///
/// 时序：抖一下 → 落到目标点 → 按下 → 保持 CLICK_HOLD_MS → 抬起。
/// 原来把 MOVE+DOWN+UP 塞进同一个批次（按下和抬起间隔 0ms），太快时有些程序会把
/// down/up 合并掉、或者判成拖拽而不是点击，所以这里分开提交并留出保持时间。
fn send_input_click(
    sx: i32,
    sy: i32,
    button: MouseButton,
    clicks: u32,
    jiggle: Option<(i32, i32)>,
) -> EngineResult<()> {
    let move_flags =
        MOUSEEVENTF_MOVE_FLAG | MOUSEEVENTF_ABSOLUTE_FLAG | MOUSEEVENTF_VIRTUALDESK_FLAG;

    // 1) 移动：先抖一下，再落到目标点。同一次提交，顺序由系统保证。
    let mut moves: Vec<INPUT> = Vec::new();
    if let Some((jx, jy)) = jiggle {
        let (bx, by) = absolute_coords(jx, jy);
        moves.push(mouse_input(bx, by, 0, move_flags));
    }
    let (ax, ay) = absolute_coords(sx, sy);
    moves.push(mouse_input(ax, ay, 0, move_flags));
    send_inputs(&moves)?;

    let (down_flag, up_flag) = match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN_FLAG, MOUSEEVENTF_LEFTUP_FLAG),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN_FLAG, MOUSEEVENTF_RIGHTUP_FLAG),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN_FLAG, MOUSEEVENTF_MIDDLEUP_FLAG),
    };

    // 2) 每一次点击：按下、保持、抬起。双击之间再留一次间隔，否则程序识别不出双击。
    for i in 0..clicks {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(CLICK_HOLD_MS));
        }
        // 用 ButtonHold 而不是裸的两次 send_inputs：抬起发出失败时 Drop 会补发一次，
        // 不会把用户的鼠标留在按下状态。
        let mut hold = ButtonHold::down((down_flag, up_flag))?;
        std::thread::sleep(std::time::Duration::from_millis(CLICK_HOLD_MS));
        hold.release()?;
    }
    Ok(())
}

/// 拖拽时的鼠标移动标志（绝对坐标 + 虚拟桌面归一化）。
const DRAG_MOVE_FLAGS: u32 =
    MOUSEEVENTF_MOVE_FLAG | MOUSEEVENTF_ABSOLUTE_FLAG | MOUSEEVENTF_VIRTUALDESK_FLAG;

/// 拖拽：按下 → **轨迹插值** → 抬起。默认走真注入（SendInput）。
///
/// 为什么不能"移动 + 按下 + 移动 + 松开"了事：控件判定"这是一次拖拽"靠的是按下之后
/// 有没有**连续的移动事件**。中间不插点、或者一次把终点发过去，很多控件直接当成
/// 没动过（Win32 列表视图的拖选、Qt 的拖放手势、浏览器的 HTML5 拖放都这样）。
/// 所以这里按 `steps` 做插值，每步一次 `MOUSEEVENTF_MOVE`，进度用 smoothstep
/// 做缓入缓出（起步/收尾慢、中间快，跟人手一致），轨迹上再叠加 1px 法向微动。
///
/// 起点和终点**都必须落在目标窗口上**：SendInput 不认窗口、只认光标位置，
/// 一次落在别处的拖拽可能把东西拖进别的程序里——比点错更糟，所以宁可报错。
pub fn drag(
    ctx: &mut ActionContext,
    to_x: i32,
    to_y: i32,
    button: MouseButton,
    steps: u32,
    duration_ms: u64,
    wobble: bool,
) -> EngineResult<(Delivery, (i32, i32), (i32, i32), u32, u64)> {
    use std::time::Duration;

    let started = Instant::now();
    let steps = steps.clamp(2, 200);
    let duration_ms = duration_ms.clamp(50, 10_000);

    ensure_not_password(ctx.target, ctx.element.as_ref().map(|e| e.is_password).unwrap_or(false))?;

    // 起点：uid 路径下 ctx.screen_point 是 None，必须补算成屏幕坐标。
    let (sx, sy) = match ctx.screen_point {
        Some(p) => p,
        None => {
            let client = ctx.client_point()?;
            dpi::client_to_screen(ctx.target, client.x, client.y)?
        }
    };
    ensure_point_reaches(ctx.root, sx, sy)?;
    ensure_point_reaches(ctx.root, to_x, to_y)?;

    let mut guard = InjectionGuard::new();
    guard.will_move_cursor();

    // 1) 落到起点：先抖 1px 再回起点，理由同 click（制造一次真实移动刷新 hover）。
    let mut moves: Vec<INPUT> = Vec::with_capacity(2);
    if let Some((jx, jy)) = jiggle_point(ctx, sx, sy) {
        let (bx, by) = absolute_coords(jx, jy);
        moves.push(mouse_input(bx, by, 0, DRAG_MOVE_FLAGS));
    }
    let (ax, ay) = absolute_coords(sx, sy);
    moves.push(mouse_input(ax, ay, 0, DRAG_MOVE_FLAGS));
    send_inputs(&moves)?;
    std::thread::sleep(Duration::from_millis(CLICK_HOLD_MS));

    let (down_flag, up_flag) = mouse_button_flags(button);

    // 2) 按下。用 ButtonHold 兜底：后面任何一步失败都会在 Drop 里补发抬起。
    let mut hold = ButtonHold::down((down_flag, up_flag))?;

    // 3) 轨迹插值。每一步单独提交：一次提交多个 MOVE 时，目标端可能把它们
    //    合并成一次跳跃，就失去了"连续移动"这个语义。
    let per_step = (duration_ms / steps.max(1) as u64).max(1);
    let dx = to_x - sx;
    let dy = to_y - sy;
    let (nx, ny) = normal_hint(dx, dy);
    for i in 1..=steps {
        if crate::cancel::is_cancelled() {
            // ButtonHold 的 Drop 会替我们补发抬起，所以在这里返回是安全的。
            return Err(crate::cancel::cancelled("win_drag"));
        }
        let t = i as f64 / steps as f64;
        let eased = smoothstep(t);
        let mut px = sx + (dx as f64 * eased).round() as i32;
        let mut py = sy + (dy as f64 * eased).round() as i32;
        if i == steps {
            // 最后一步精确落终点：浮点插值取整可能差 1px，而"拖到哪儿"是本次调用的核心语义。
            px = to_x;
            py = to_y;
        }
        // 中间点微动：不改变起点/终点，只在途中叠 1px，让事件流里看得见"在动"。
        let (ox, oy) = if wobble && i % 4 == 2 && i < steps { (nx, ny) } else { (0, 0) };
        let (bx, by) = absolute_coords(px + ox, py + oy);
        send_inputs(&[mouse_input(bx, by, 0, DRAG_MOVE_FLAGS)])?;
        std::thread::sleep(Duration::from_millis(per_step));
    }

    // 4) 松手。抬起前留一点保持时间，太快会被当成"点了一下"。
    std::thread::sleep(Duration::from_millis(CLICK_HOLD_MS));
    hold.release()?;

    let (cursor_ok, focus_ok) = guard.restore();
    guard.audit(&mut ctx.warnings, cursor_ok, focus_ok);

    Ok((
        Delivery::SendInput,
        (sx, sy),
        (to_x, to_y),
        steps,
        started.elapsed().as_millis() as u64,
    ))
}

// ---------------------------------------------------------------------------
// win_drag_path：一次按下走完整条路径（0.2.0）
// ---------------------------------------------------------------------------

/// 自动时长的系数：每像素 1.2ms（≈800px/s）。再快就要加大步长，否则见棱角。
const PATH_MS_PER_PX: f64 = 1.2;
/// 重采样之后的点数上限，防止一次注入跑几分钟。
const PATH_MAX_POINTS: usize = 20_000;
/// 重采样之前的原始点数上限。Json 层只限制了整行字节数（16MB），
/// 一份「几十万个点」的请求照样能塞进来；若不先卡住，重采样会在检查之前就把内存吃光。
const PATH_MAX_RAW_POINTS: usize = 200_000;
/// 落笔后到第一个移动之间的等待，让目标端先处理 down。
const PATH_DOWN_SETTLE_MS: u64 = 5;
/// 回放时间轴的硬上限（毫秒）。比 duration_ms 的上限宽，因为录制可以长很多，
/// 但再长就该拒绝——超过这个值的 times_ms 一般不是帧数据而是坏数据。
const PATH_MAX_REPLAY_MS: f64 = 600_000.0;

#[link(name = "winmm")]
#[allow(non_snake_case)]
unsafe extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

/// 1ms 定时器精度守卫。
///
/// 为什么必须有：Windows 默认定时器步长 15.6ms，`sleep(1)` 实际睡 15~16ms，
/// 一条 200 步的弧线要跑 3 秒，屏幕上就是"一顿一顿"。`timeBeginPeriod(1)`
/// 之后实测每步 1.0~1.3ms。这是"流畅"的硬前提，算法再对也绕不过去。
///
/// 系统按引用计数管理，配对调用 `timeEndPeriod` 即可，嵌套安全。
struct Timer1Ms {
    active: bool,
}

impl Timer1Ms {
    fn new() -> Self {
        // 返回 0 = 成功。失败不报错：只是时长会明显偏长，结果里如实报 timer_1ms=false。
        let active = unsafe { timeBeginPeriod(1) } == 0;
        Self { active }
    }
}

impl Drop for Timer1Ms {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

/// 睡到**绝对时刻**。
///
/// 不能裸 `sleep(remain)`：`thread::sleep` 会被系统调度量化（即便开了 1ms 定时器，
/// 单次仍可能多睡 1~3ms），几百步累积下来整条线就忽快忽慢。所以按绝对时刻追：
/// 剩余 >2ms 粗睡到差 1ms，再细睡 150µs，最后 300µs 自旋收尾。
fn sleep_until(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remain = deadline - now;
        if remain > Duration::from_millis(2) {
            std::thread::sleep(remain - Duration::from_millis(1));
        } else if remain > Duration::from_micros(300) {
            std::thread::sleep(Duration::from_micros(150));
        } else {
            std::hint::spin_loop();
        }
    }
}

fn dist(a: (i32, i32), b: (i32, i32)) -> f64 {
    (((b.0 - a.0) as f64).powi(2) + ((b.1 - a.1) as f64).powi(2)).sqrt()
}

/// 校验录制回放的时间戳：有限、非负、每笔单调不减、最长不超过硬上限。
///
/// 为什么在校验层拦而不是在执行层 clamp：执行层只能悄悄把坏数据改成 0ms，
/// 那会让"回放看起来正常但速度全错"；校验层返回 false 时调用方整段退回匀速，
/// 并在 warnings 里写明原因，至少让调用方知道这次没有按录制速度重放。
fn times_sane(times: &[Vec<f64>], strokes: &[Vec<(f64, f64)>]) -> bool {
    for (si, ts) in times.iter().enumerate() {
        // 长度对不上的那几笔本来就会被忽略（见 stroke_times 的 filter），不算不合法。
        let raw_len = strokes.get(si).map(|s| s.len()).unwrap_or(0);
        if ts.len() != raw_len && !ts.is_empty() {
            // 长度不匹配只是"不使用"，不进 false：调用方常见写法是给整段录制，
            // 其中有笔少于两点被丢弃，长度自然对不上。
            continue;
        }
        let mut last = 0.0_f64;
        for t in ts {
            if !t.is_finite() || *t < 0.0 || *t > PATH_MAX_REPLAY_MS {
                return false;
            }
            if *t < last {
                return false;
            }
            last = *t;
        }
    }
    true
}

/// 计划表里的一步。
#[derive(Clone, Copy)]
enum PathStep {
    /// 鼠标移动到该点。`wob` = 是否叠 1px 法向微动（弧线上默认不叠）；
    /// `travel` = 这是笔间抬笔移动，不算进"最大步长"指标。
    Move { x: i32, y: i32, wob: bool, travel: bool },
    Down,
    Up,
}

pub struct PathOutcome {
    pub strokes: usize,
    pub points: usize,
    pub planned_ms: u64,
    pub elapsed_ms: u64,
    pub path_px: f64,
    pub max_step_px: f64,
    pub start: (i32, i32),
    pub end: (i32, i32),
    pub timer_1ms: bool,
    /// 是否按录制时间表重放（false = 弧长匀速）。
    pub timed: bool,
}

/// 注入计划：每一步（Move/Down/Up）+ 累计弧长 + 可选的名义截止时刻。
struct PathPlan {
    plan: Vec<PathStep>,
    cum: Vec<f64>,
    nominal: Vec<Option<f64>>,
    total_px: f64,
}

/// 把笔顺展开成注入计划表（纯函数，不碰 Win32，便于单测）。
///
/// 时间语义：`times` 是**整段录制的绝对毫秒数**（win_record 的输出），不是每笔各自
/// 从 0 开始。所以这里直接把它当截止时刻用，**不能再累加**——旧实现把上一笔的末时间
/// 戳叠到下一笔上，多笔轨迹的播放时长会成倍拉长、笔间间隔也不对。
fn build_path_plan(
    strokes: &[Vec<(i32, i32)>],
    stroke_times: &[Option<Vec<f64>>],
    wobble: bool,
) -> PathPlan {
    let mut plan: Vec<PathStep> = Vec::new();
    let mut cum: Vec<f64> = Vec::new();
    let mut nominal: Vec<Option<f64>> = Vec::new();
    let mut total_px = 0.0_f64;
    let mut prev = strokes[0][0];
    let mut prev_ms = 0.0_f64;
    let mut bump = |t: f64| -> f64 {
        let t = if t.is_finite() { t.clamp(0.0, PATH_MAX_REPLAY_MS) } else { 0.0 };
        if t > prev_ms {
            prev_ms = t;
        }
        prev_ms
    };
    for (si, pts) in strokes.iter().enumerate() {
        let first = pts[0];
        if si > 0 {
            total_px += dist(prev, first); // 抬笔移动到下一笔起点
            prev = first;
        }
        let ts = stroke_times.get(si).and_then(|t| t.as_ref());
        let at = |i: usize| -> Option<f64> { ts.map(|v| v.get(i).copied().unwrap_or(0.0)) };
        plan.push(PathStep::Move { x: first.0, y: first.1, wob: false, travel: si > 0 });
        cum.push(total_px);
        nominal.push(at(0).map(&mut bump));
        plan.push(PathStep::Down);
        cum.push(total_px);
        nominal.push(at(0).map(&mut bump));
        let n = pts.len();
        for (i, p) in pts.iter().enumerate().skip(1) {
            total_px += dist(prev, *p);
            prev = *p;
            // 抖动只在"这一笔的中间点"上叠：末点要精确，不能偏。
            let wob = wobble && i + 1 < n && i % 4 == 2;
            plan.push(PathStep::Move { x: p.0, y: p.1, wob, travel: false });
            cum.push(total_px);
            nominal.push(at(i).map(&mut bump));
        }
        plan.push(PathStep::Up);
        cum.push(total_px);
        nominal.push(at(n - 1).map(&mut bump));
    }
    PathPlan { plan, cum, nominal, total_px }
}
/// 沿整条路径画：按下 → 按**弧长匀速**走完所有点 → 抬起；多笔之间自动抬笔/落笔。
///
/// 和 [`drag`] 的区别：drag 只走一段直线，且每次调用都是完整的按下-抬起，
/// 拼多段画弧会在每段之间断笔、且每段独立加减速（折角+速度突变）。
/// 这里一笔只按一次、只抬一次，点按弧长等距、时间按弧长等分，所以速度恒定。
///
/// `duration_ms` 给 None 时按 `PATH_MS_PER_PX` 自动算；给 Some 时以它为准
/// （精度受定时器与注入开销限制，结果里 `elapsed_ms` 是实测值）。
pub fn drag_path(
    ctx: &mut ActionContext,
    strokes_in: &[Vec<(f64, f64)>],
    button: MouseButton,
    step_px: f64,
    duration_ms: Option<u64>,
    wobble: bool,
    // 每笔每点的时间戳（毫秒，相对整段开始）。给了就按它重放，忽略 `duration_ms`。
    times: Option<&[Vec<f64>]>,
) -> EngineResult<PathOutcome> {
    use std::time::Duration;

    let started = Instant::now();
    let step_px = step_px.clamp(0.5, 64.0);

    // 0) 先卡输入规模，再谈重采样：重采样是 O(原始点数) 的展开，先展开后检查
    //    等于把内存交给调用方决定。
    let raw_points: usize = strokes_in.iter().map(|v| v.len()).sum();
    if raw_points > PATH_MAX_RAW_POINTS {
        return Err(EngineError::invalid(format!(
            "原始轨迹点 {raw_points} 个，超过上限 {PATH_MAX_RAW_POINTS}：减少笔数或先抽稀再传"
        )));
    }

    ensure_not_password(
        ctx.target,
        ctx.element.as_ref().map(|e| e.is_password).unwrap_or(false),
    )?;

    // 1) 等距重采样 + 取整 + 去重。形状到这一层只认点列。
    //
    // 例外：调用方给了 `times`（录制回放）时**不重采样**，因为时间戳是按原始点一一对应的，
    // 重采样会改变点数、时间戳就对不上了。回放录好的轨迹本来就是密的。
    // times_ms 只信任「有限、非负、单调不减」的那一份；不合法就整段退回匀速，
    // 并写进 warnings —— 静默改语义比直接报错更难查。
    let times_valid = match times {
        Some(t) => times_sane(t, strokes_in),
        None => true,
    };
    if !times_valid {
        ctx.warnings.push(
            "times_ms 里存在非有限值/负数/非单调的时间戳，已整段退回匀速重放".to_string(),
        );
    }
    let times = if times_valid { times } else { None };

    let mut strokes: Vec<Vec<(i32, i32)>> = Vec::with_capacity(strokes_in.len());
    let mut stroke_times: Vec<Option<Vec<f64>>> = Vec::with_capacity(strokes_in.len());
    let mut total_points = 0usize;
    for (si, raw) in strokes_in.iter().enumerate() {
        if raw.is_empty() {
            continue;
        }
        let ts_in: Option<Vec<f64>> = times
            .and_then(|t| t.get(si))
            .filter(|t| t.len() == raw.len())
            .cloned();
        let dense: Vec<(f64, f64)> = if ts_in.is_some() {
            raw.clone()
        } else {
            path::resample(raw, step_px)
        };
        let mut pts: Vec<(i32, i32)> = Vec::with_capacity(dense.len());
        let mut kept: Vec<f64> = Vec::new();
        for (k, (x, y)) in dense.iter().enumerate() {
            if !x.is_finite() || !y.is_finite() {
                return Err(EngineError::invalid(format!(
                    "轨迹里有非有限坐标（第 {} 笔第 {} 点：({x},{y})）",
                    si + 1,
                    k + 1
                )));
            }
            let p = (x.round() as i32, y.round() as i32);
            if pts.last() == Some(&p) {
                continue; // 同一个像素不重复发
            }
            pts.push(p);
            if let Some(t) = &ts_in {
                kept.push(t[k]);
            }
        }
        if pts.is_empty() {
            continue;
        }
        total_points += pts.len();
        if total_points > PATH_MAX_POINTS {
            return Err(EngineError::invalid(format!(
                "路径点数 {total_points} 超过上限 {PATH_MAX_POINTS}：调大 step_px 或减少笔数"
            )));
        }
        strokes.push(pts);
        stroke_times.push(if ts_in.is_some() { Some(kept) } else { None });
    }
    if strokes.is_empty() {
        return Err(EngineError::invalid("路径为空：至少要一条含点的笔画"));
    }

    // 2) 所有点必须落在目标窗口矩形内：SendInput 不认窗口，画出界=画到别的程序上。
    let rect = dpi::window_rect(ctx.root)?;
    for pts in &strokes {
        for (x, y) in pts {
            if *x < rect.x || *y < rect.y || *x > rect.x + rect.w || *y > rect.y + rect.h {
                return Err(EngineError::invalid(format!(
                    "路径点 ({x},{y}) 落在目标窗口 rect=({},{},{}x{}) 之外，拒绝执行",
                    rect.x, rect.y, rect.w, rect.h
                )));
            }
        }
    }

    // 3) 每笔的落笔点做遮挡检查。
    for pts in &strokes {
        ensure_point_reaches(ctx.root, pts[0].0, pts[0].1)?;
    }

    let (down_flag, up_flag) = mouse_button_flags(button);

    // 4) 计划表：Move / Down / Up，每个 Move 记累计弧长（时间按弧长分配 = 匀速）。
    //    纯函数，单测在文件尾（多笔时间轴曾经在这里被重复累加）。
    let PathPlan { plan, cum, nominal, total_px } = build_path_plan(&strokes, &stroke_times, wobble);

    // 时间模式：所有笔都有时间戳才启用；否则整段退回匀速。
    let timed = !nominal.is_empty() && nominal.iter().all(|v| v.is_some());
    let planned_ms = if timed {
        nominal.iter().flatten().copied().fold(0.0_f64, f64::max).ceil().clamp(1.0, PATH_MAX_REPLAY_MS) as u64
    } else {
        match duration_ms {
            Some(d) => d.clamp(20, 120_000),
            None => (total_px * PATH_MS_PER_PX).clamp(60.0, 120_000.0) as u64,
        }
    };

    // 每个计划步骤的截止时刻（毫秒，相对 clock），两种模式统一成一张表。
    let deadlines: Vec<f64> = if timed {
        nominal.iter().map(|v| v.unwrap_or(0.0)).collect()
    } else {
        cum.iter()
            .map(|c| {
                let frac = if total_px > 0.0 { (c / total_px).clamp(0.0, 1.0) } else { 0.0 };
                planned_ms as f64 * frac
            })
            .collect()
    };

    // 5) 注入。
    let timer = Timer1Ms::new();
    let mut guard = InjectionGuard::new();
    guard.will_move_cursor();

    let (sx, sy) = strokes[0][0];
    let mut moves: Vec<INPUT> = Vec::with_capacity(2);
    if let Some((jx, jy)) = jiggle_point(ctx, sx, sy) {
        let (bx, by) = absolute_coords(jx, jy);
        moves.push(mouse_input(bx, by, 0, DRAG_MOVE_FLAGS));
    }
    let (ax, ay) = absolute_coords(sx, sy);
    moves.push(mouse_input(ax, ay, 0, DRAG_MOVE_FLAGS));
    send_inputs(&moves)?;
    std::thread::sleep(Duration::from_millis(CLICK_HOLD_MS));

    let clock = Instant::now();
    let mut sent_points = 0usize;
    let mut max_step = 0.0_f64;
    let mut last_pos = strokes[0][0];
    let mut wobble_dir: i32 = 1;
    // 每一笔的按下-抬起都用一个 ButtonHold 兜底：中途任何一步返回 Err，
    // Drop 会补发抬起，绝不会把鼠标留在按下状态继续画下去。
    let mut hold: Option<ButtonHold> = None;

    for (idx, step) in plan.iter().enumerate() {
        if crate::cancel::is_cancelled() {
            // hold 的 Drop 会补发抬起，不会把鼠标留在按下状态。
            return Err(crate::cancel::cancelled("win_drag_path"));
        }
        let due = deadlines.get(idx).copied().unwrap_or(0.0);
        // from_secs_f64 对 NaN / 负值 / inf 会 panic（release 下 panic=abort，引擎当场消失），
        // 所以在这里最后兜一道，宁可当成 0ms。
        let due = if due.is_finite() { due.clamp(0.0, PATH_MAX_REPLAY_MS) } else { 0.0 };
        sleep_until(clock + Duration::from_secs_f64(due / 1000.0));
        match *step {
            PathStep::Move { x, y, wob, travel } => {
                let (mut px, mut py) = (x, y);
                if wob {
                    let (dx, dy) = (x - last_pos.0, y - last_pos.1);
                    let (nx, ny) = if dx.abs() >= dy.abs() {
                        (0, wobble_dir)
                    } else {
                        (wobble_dir, 0)
                    };
                    px += nx;
                    py += ny;
                    wobble_dir = -wobble_dir;
                }
                let (bx, by) = absolute_coords(px, py);
                send_inputs(&[mouse_input(bx, by, 0, DRAG_MOVE_FLAGS)])?;
                sent_points += 1;
                // 笔间抬笔移动是一段直线飞行，不能算进"落笔步长"指标，否则多笔图形
                // 的 max_step_px 会被笔间距（可达上百 px）污染，看起来像轨迹断点。
                if !travel {
                    max_step = max_step.max(dist(last_pos, (px, py)));
                }
                last_pos = (px, py);
            }
            PathStep::Down => {
                hold = Some(ButtonHold::down((down_flag, up_flag))?);
                std::thread::sleep(Duration::from_millis(PATH_DOWN_SETTLE_MS));
            }
            PathStep::Up => {
                std::thread::sleep(Duration::from_millis(CLICK_HOLD_MS));
                if let Some(mut h) = hold.take() {
                    h.release()?;
                }
            }
        }
    }

    let elapsed = started.elapsed().as_millis() as u64;
    let (cursor_ok, focus_ok) = guard.restore();
    guard.audit(&mut ctx.warnings, cursor_ok, focus_ok);

    let timer_1ms = timer.active;
    drop(timer);

    Ok(PathOutcome {
        strokes: strokes.len(),
        points: sent_points,
        planned_ms,
        elapsed_ms: elapsed,
        path_px: total_px,
        max_step_px: max_step,
        start: strokes[0][0],
        end: last_pos,
        timer_1ms,
        timed,
    })
}

/// 「鼠标按键按下、尚未抬起」的守卫。
///
/// 为什么要有它：按下和抬起是两次独立的 SendInput。中间任何一步报错（目标进程被杀、
/// UIPI 拦截、线程 panic）都会让调用方直接 return，抬起就漏发了 —— 用户的鼠标会一直
/// 卡在按下状态，整个桌面都在框选。Drop 里补发抬起是唯一可靠的兜底。
struct ButtonHold {
    up_flag: u32,
    released: bool,
}

impl ButtonHold {
    /// 发一次按下；成功了才返回守卫（失败了根本没按下，不需要抬）。
    fn down(flags: (u32, u32)) -> EngineResult<Self> {
        let (down_flag, up_flag) = flags;
        send_inputs(&[mouse_input(0, 0, 0, down_flag)])?;
        Ok(Self { up_flag, released: false })
    }

    /// 正常抬起。失败时保守地认为"没抬成"，Drop 会再试一次。
    fn release(&mut self) -> EngineResult<()> {
        if self.released {
            return Ok(());
        }
        send_inputs(&[mouse_input(0, 0, 0, self.up_flag)])?;
        self.released = true;
        Ok(())
    }
}

impl Drop for ButtonHold {
    fn drop(&mut self) {
        if !self.released {
            // 兜底抬起：这里已经没机会把错误交给调用方了，只能尽力而为。
            let _ = send_inputs(&[mouse_input(0, 0, 0, self.up_flag)]);
            self.released = true;
        }
    }
}
/// 鼠标按键对应的 (按下标志, 抬起标志)。
fn mouse_button_flags(button: MouseButton) -> (u32, u32) {
    match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN_FLAG, MOUSEEVENTF_LEFTUP_FLAG),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN_FLAG, MOUSEEVENTF_RIGHTUP_FLAG),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN_FLAG, MOUSEEVENTF_MIDDLEUP_FLAG),
    }
}

/// 轨迹法向的整数化方向，只用来做 1px 微动：横向拖动就上下抖，纵向拖动就左右抖。
fn normal_hint(dx: i32, dy: i32) -> (i32, i32) {
    if dx == 0 && dy == 0 {
        return (0, 0);
    }
    if dx.abs() >= dy.abs() { (0, 1) } else { (1, 0) }
}

/// 缓入缓出：t=0/1 时导数为 0，起步和收尾慢，中段快。
fn smoothstep(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 输入文本。默认走真注入（SendInput + KEYEVENTF_UNICODE）。
///
/// 为什么默认不再是 WM_CHAR：Qt 自绘窗口（微信 4.0）收下 WM_CHAR 但不处理，报
/// "成功"却一个字都进不去（2026-10-16 实测：投递后截图 PNG 字节完全一致）。
/// `method="postmessage"` 仍保留，给普通 Win32 编辑控件用（不碰前台、不动鼠标）。
///
/// 0.1.9 起 `Auto` 多一条自动分支：文本含非 ASCII 时先自己写剪贴板再 Ctrl+V
/// （见文件头）。写剪贴板失败则退回 Unicode 逐字符注入，并在 warnings 里说明。
pub fn type_text(
    ctx: &mut ActionContext,
    text: &str,
    method: Delivery,
) -> EngineResult<(Delivery, ClientPoint, u64)> {
    let started = Instant::now();
    let client = ctx.client_point()?;
    ensure_not_password(ctx.target, ctx.element.as_ref().map(|e| e.is_password).unwrap_or(false))?;

    match method {
        // Auto 与显式 SendInput 走同一条路：先抢前台，再真注入，完了还现场。
        Delivery::Auto | Delivery::SendInput => {
            let mut guard = InjectionGuard::new();
            if !guard.focus(ctx.root) {
                return Err(foreground_required_err(ctx.root));
            }
            // 焦点复核：刚切过去又被别的程序抢走的情况真实存在，复核一次再注入。
            ensure_keyboard_reaches(ctx)?;

            // 只有 Auto 才会自动改走剪贴板；显式 sendinput 的语义保持不变。
            if method == Delivery::Auto && needs_clipboard_for_auto(text) {
                match write_clipboard_text(text) {
                    Ok(()) => {
                        send_ctrl_v()?;
                        ctx.warnings.push(
                            "文本含非 ASCII 字符，auto 模式已自动改用剪贴板粘贴：剪贴板内容已被这段文本覆盖"
                                .to_string(),
                        );
                        let (cursor_ok, focus_ok) = guard.restore();
                        guard.audit(&mut ctx.warnings, cursor_ok, focus_ok);
                        return Ok((
                            Delivery::Clipboard,
                            client,
                            started.elapsed().as_millis() as u64,
                        ));
                    }
                    Err(e) => {
                        // 剪贴板被别的程序占着（或内存分配失败）时不能把功能整个废掉：
                        // 退回逐字符 Unicode 注入，并把风险写进 warnings（长中文串可能串位）。
                        ctx.warnings.push(format!(
                            "写剪贴板失败（{e}），已退回逐字符 Unicode 注入；中文长串可能串位，输入后请截图回读核对"
                        ));
                    }
                }
            }

            send_input_text(text)?;
            let (cursor_ok, focus_ok) = guard.restore();
            guard.audit(&mut ctx.warnings, cursor_ok, focus_ok);
            Ok((Delivery::SendInput, client, started.elapsed().as_millis() as u64))
        }
        Delivery::PostMessage => {
            if post_text(ctx.target, text)? {
                return Ok((Delivery::PostMessage, client, started.elapsed().as_millis() as u64));
            }
            Err(EngineError::other(
                "WM_CHAR 投递失败（目标窗口可能已销毁）；改用默认方式（真注入）重试",
            ))
        }
        Delivery::Clipboard => {
            // 显式剪贴板路径语义不变：引擎只发 Ctrl+V，剪贴板内容由调用方自己写。
            clipboard_paste(ctx)?;
            Ok((Delivery::Clipboard, client, started.elapsed().as_millis() as u64))
        }
    }
}

/// 用 WM_CHAR 逐字符投递。
fn post_text(target: HWND, text: &str) -> EngineResult<bool> {
    let mut all_ok = true;
    // text.encode_utf16() 会自动把非 BMP 字符拆成代理对，
    // 编辑控件收到两个连续的 WM_CHAR 就能正确合成 emoji。
    for unit in text.encode_utf16() {
        // SAFETY: target 有效（调用方保证）；WM_CHAR 的 wParam 就是 UTF-16 码元，
        // lParam 的重复计数/扫描码对 WM_CHAR 无语义，填 1 即可。
        let ok = unsafe { PostMessageW(target, WM_CHAR, unit as WPARAM, 1) };
        if ok == 0 {
            all_ok = false;
        }
        // 微小的间隔让目标消息队列保持顺序感知；太快时某些 RichEdit 会丢字符。
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    Ok(all_ok)
}

/// 用 SendInput 投递 Unicode 字符。
///
/// 用 KEYEVENTF_UNICODE 而不是"按虚拟键码"：后者要求知道键盘布局，
/// 中文/标点会错；UNICODE 模式直接把码元交给系统。
///
/// 字符之间必须留间隔。2026-10-16 实测：把整串（22 字 = 44 个事件）塞进同一个
/// SendInput 批次时，目标端的输入法/文本处理会串位——「【自测】SendInput 切换后
/// 自动发送」被写成「【【测】】endInput 切换后自动发送」，第 2、5 个字符变成了
/// 前一个字符。所以这里改成每字符一次 SendInput + [CHAR_GAP_MS] 间隔。
fn send_input_text(text: &str) -> EngineResult<()> {
    for unit in text.encode_utf16() {
        let pair = [
            keyboard_input(0, unit, KEYEVENTF_UNICODE),
            keyboard_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
        ];
        send_inputs(&pair)?;
        std::thread::sleep(std::time::Duration::from_millis(CHAR_GAP_MS));
    }
    Ok(())
}

/// 剪贴板文本格式（Win32 `CF_UNICODETEXT = 13`）。
///
/// 不引 `Win32_System_Ole` 只为拿这一个常量：它只是格式号，`SetClipboardData`
/// 收的就是 u32，把值写在这里并注明来源即可，少开一个 feature。
const CF_UNICODETEXT: u32 = 13;

/// auto 模式下是否需要改走剪贴板：文本里只要有一个非 ASCII 字符就走。
///
/// 为什么按"非 ASCII"而不是"非 BMP"：emoji 之外的普通中文、全角标点都是
/// `KEYEVENTF_UNICODE` 逐字符注入会串位的那一类（真机实测），统统交给剪贴板；
/// 纯 ASCII 继续用逐字符注入（不碰剪贴板，速度也更快）。
fn needs_clipboard_for_auto(text: &str) -> bool {
    text.chars().any(|c| !c.is_ascii())
}

/// 把文本以 `CF_UNICODETEXT` 写入系统剪贴板。
///
/// **会覆盖用户剪贴板内容**（调用方必须在 warnings 里如实说明）。
/// 实现要点：
/// - `OpenClipboard` 可能因为别的进程正占着剪贴板而失败，重试几次再放弃；
/// - `SetClipboardData` 成功后内存所有权归系统，**不能**再 `GlobalFree`；
///   只有写入失败（或 `GlobalLock` 失败）时才由我们自己释放。
fn write_clipboard_text(text: &str) -> EngineResult<()> {
    // 剪贴板只认 UTF-16（含结尾 NUL）。
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * 2;

    // 关闭剪贴板也要兜底执行：早期返回时不能把剪贴板一直开着（会让别的程序写不进去）。
    struct CloseOnDrop;
    impl Drop for CloseOnDrop {
        fn drop(&mut self) {
            // SAFETY: 只在 OpenClipboard 成功之后才会构造本守卫，配对 CloseClipboard；
            // 剪贴板一旦关闭，本函数不再访问它。
            unsafe { CloseClipboard() };
        }
    }

    let mut opened = false;
    for attempt in 0..5u64 {
        // SAFETY: 传空 HWND 表示剪贴板不与某个具体窗口关联，任何线程都可调用；
        // 成功返回非 0，失败返回 0 并设置 LastError。
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            opened = true;
            break;
        }
        // 别的程序可能正持有剪贴板（复制大文件时很常见），退避重试而不是立刻失败。
        std::thread::sleep(std::time::Duration::from_millis(10 * (attempt + 1)));
    }
    if !opened {
        return Err(crate::error::win32("OpenClipboard"));
    }
    let _close = CloseOnDrop;

    // SAFETY: 从这里开始剪贴板由本线程持有（_close 保证配对关闭）。
    // EmptyClipboard 清空旧内容后才能 SetClipboardData；GlobalAlloc 拿到一块
    // GMEM_MOVEABLE 内存，写满 UTF-16 字节后交给 SetClipboardData：
    // 成功后内存归系统，失败则由我们 GlobalFree，所有权转移路径唯一。
    unsafe {
        if EmptyClipboard() == 0 {
            return Err(crate::error::win32("EmptyClipboard"));
        }
        let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if hmem.is_null() {
            return Err(crate::error::win32("GlobalAlloc"));
        }
        let dst = GlobalLock(hmem);
        if dst.is_null() {
            GlobalFree(hmem);
            return Err(crate::error::win32("GlobalLock"));
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, dst as *mut u8, bytes);
        // GlobalUnlock 返回 0 时 LastError 可能是 0（表示成功），所以返回值不可靠；
        // 它不产生副作用，忽略返回值即可。
        let _ = GlobalUnlock(hmem);
        if SetClipboardData(CF_UNICODETEXT, hmem as HANDLE).is_null() {
            // 交给系统失败 → 所有权还在我们手里，必须自己释放，否则内存泄漏。
            GlobalFree(hmem);
            return Err(crate::error::win32("SetClipboardData"));
        }
    }
    Ok(())
}

/// 只发 Ctrl+V（不发别的、不改剪贴板）。要求调用方已经把目标窗口切到前台。
fn send_ctrl_v() -> EngineResult<()> {
    // Ctrl 按下 -> (留间隔) -> V 按下/抬起 -> (留间隔) -> Ctrl 抬起。
    // 不能把四个事件塞进同一个 SendInput 批次：目标进程是稍后处理消息的，
    // 那时 Ctrl 已经抬起，粘帖就变成了插入一个 'v'（同 MOD_HOLD_MS 的说明）。
    send_inputs(&[key_event(VK_CONTROL, false)])?;
    std::thread::sleep(std::time::Duration::from_millis(MOD_HOLD_MS));
    send_inputs(&[key_event(VK_V, false), key_event(VK_V, true)])?;
    std::thread::sleep(std::time::Duration::from_millis(MOD_HOLD_MS));
    send_inputs(&[key_event(VK_CONTROL, true)])?;
    Ok(())
}

/// 剪贴板粘贴（显式 `method="clipboard"` 时进入）。
///
/// 只发 Ctrl+V，**不**改剪贴板内容：这条路径要求调用方自己先把文本放进剪贴板。
/// （0.1.9 起 auto + 非 ASCII 的自动路径走的是 [`write_clipboard_text`] + [`send_ctrl_v`]。）
///
/// Ctrl+V 也必须真注入：原来的 PostMessage WM_KEYDOWN 对 Qt 窗口同样无效（实测
/// 微信搜索框按下去没有任何反应）。真注入要抢前台，所以同样走 InjectionGuard。
fn clipboard_paste(ctx: &mut ActionContext) -> EngineResult<()> {
    let mut guard = InjectionGuard::new();
    if !guard.focus(ctx.root) {
        return Err(foreground_required_err(ctx.root));
    }
    send_ctrl_v()?;
    let (cursor_ok, focus_ok) = guard.restore();
    guard.audit(&mut ctx.warnings, cursor_ok, focus_ok);
    Ok(())
}

/// 修饰键按下与松开之间必须留的时间。
///
/// 为什么不能 0ms：SendInput 是往系统输入流里塞事件，而目标进程是**稍后**才从自己的
/// 消息队列里取消息的。事件发得太快时，目标处理 WM_KEYDOWN('a') 那一刻 Ctrl 已经被
/// 抬起，它读 GetKeyState(VK_CONTROL) 得到 UP，于是一次 Ctrl+A 变成一个普通的 'a'。
/// 2026-10-16 自测靶窗实测：没有这个间隔时 Ctrl+A 的效果就是插入一个 "a"。
const MOD_HOLD_MS: u64 = 30;

/// 按键。默认走真注入（SendInput），原因同 type_text：PostMessage 的
/// WM_KEYDOWN/WM_KEYUP 对 Qt 自绘窗口无效（微信 4.0 里按 Enter 毫无反应）。
pub fn press(
    ctx: &mut ActionContext,
    key: &str,
    modifiers: &[Modifier],
    repeat: u32,
) -> EngineResult<(Delivery, ClientPoint, u64)> {
    let started = Instant::now();
    let client = ctx.client_point()?;
    ensure_not_password(ctx.target, ctx.element.as_ref().map(|e| e.is_password).unwrap_or(false))?;

    let vk = normalize_key(key)?;
    let repeat = repeat.max(1).min(64);
    // 修饰键顺序：Ctrl+C 必须是"先按 Ctrl 再按 C"，反了就是普通字符 c。
    let mods: Vec<u16> = modifiers.iter().map(|m| modifier_vk(*m)).collect();

    let mut guard = InjectionGuard::new();
    if !guard.focus(ctx.root) {
        return Err(foreground_required_err(ctx.root));
    }
    ensure_keyboard_reaches(ctx)?;

    // 修饰键和主键都各用一个守卫：修饰键按住期间/主键按下期间任何一步失败，
    // Drop 都会补发抬起。旧实现只在失败分支里 release_keys(mods)，主键本身会卡住
    // （审查第 08 条）。
    let mut mod_hold: Option<ModHold> = None;
    if !mods.is_empty() {
        mod_hold = Some(ModHold::down(&mods)?);
        // 留时间让目标进程真的"看到"修饰键按住，见 MOD_HOLD_MS 的说明。
        std::thread::sleep(std::time::Duration::from_millis(MOD_HOLD_MS));
    }
    for i in 0..repeat {
        if i > 0 {
            // 连按之间留一点间隔：0ms 的连击会被某些程序合并成一次。
            std::thread::sleep(std::time::Duration::from_millis(KEY_GAP_MS));
        }
        // 绝不能把 down/up 塞进同一批 SendInput：那是 0ms 的按下-抬起，
        // 逐帧轮询键盘状态的游戏两帧之间永远看到"没按"，整键消失。
        let mut key = KeyHold::down(vk)?;
        std::thread::sleep(std::time::Duration::from_millis(KEY_HOLD_MS));
        key.release()?;
    }
    if mod_hold.is_some() {
        // 松开修饰键之前也要留间隔，否则组合键的"先按后放"顺序在目标侧会错位。
        std::thread::sleep(std::time::Duration::from_millis(MOD_HOLD_MS));
    }
    if let Some(mut held) = mod_hold.take() {
        held.release()?;
    }

    let (cursor_ok, focus_ok) = guard.restore();
    guard.audit(&mut ctx.warnings, cursor_ok, focus_ok);
    Ok((Delivery::SendInput, client, started.elapsed().as_millis() as u64))
}

/// 「键盘修饰键按住、尚未抬起」的守卫。
///
/// 和 ButtonHold 同理：修饰键按住期间任何一步返回 Err，Blocking 场景下用户会碰到
/// “Ctrl 一直按着”的卡键状态。Drop 里补一次抬起，最坏情况是抬起失败（那也只是
/// 原来的行为）。
struct ModHold {
    vks: Vec<u16>,
    released: bool,
}

impl ModHold {
    fn down(vks: &[u16]) -> EngineResult<Self> {
        let downs: Vec<INPUT> = vks.iter().map(|vk| key_event_hardware(*vk, false)).collect();
        send_inputs(&downs)?;
        Ok(Self { vks: vks.to_vec(), released: false })
    }

    fn release(&mut self) -> EngineResult<()> {
        if self.released {
            return Ok(());
        }
        // 抬起按倒序：与按下顺序相反，组合键在目标侧才看到“先按后放”。
        let ups: Vec<INPUT> = self.vks.iter().rev().map(|vk| key_event_hardware(*vk, true)).collect();
        send_inputs(&ups)?;
        self.released = true;
        Ok(())
    }
}

impl Drop for ModHold {
    fn drop(&mut self) {
        if !self.released {
            let ups: Vec<INPUT> = self.vks.iter().rev().map(|vk| key_event_hardware(*vk, true)).collect();
            let _ = send_inputs(&ups);
            self.released = true;
        }
    }
}

/// 「主按键按下、尚未抬起」的守卫。
///
/// 审查第 08 条：旧实现里主键抬起失败只 release_keys(mods)，主键本身留在按下状态。
struct KeyHold {
    vk: u16,
    released: bool,
}

impl KeyHold {
    fn down(vk: u16) -> EngineResult<Self> {
        send_inputs(&[key_event_hardware(vk, false)])?;
        Ok(Self { vk, released: false })
    }

    fn release(&mut self) -> EngineResult<()> {
        if self.released {
            return Ok(());
        }
        send_inputs(&[key_event_hardware(self.vk, true)])?;
        self.released = true;
        Ok(())
    }
}

impl Drop for KeyHold {
    fn drop(&mut self) {
        if !self.released {
            let _ = send_inputs(&[key_event_hardware(self.vk, true)]);
            self.released = true;
        }
    }
}
/// 逐个抬起修饰键（倒序）。
fn release_keys(mods: &[u16]) -> EngineResult<()> {
    if mods.is_empty() {
        return Ok(());
    }
    let mut ups: Vec<INPUT> = Vec::new();
    for vk in mods.iter().rev() {
        ups.push(key_event_hardware(*vk, true));
    }
    send_inputs(&ups)
}

/// 用 PostMessage 投递一次按键。现在没有调用方，保留作为参照实现。
#[allow(dead_code)]
fn post_key(target: HWND, vk: u16, up: bool) -> EngineResult<bool> {
    // SAFETY: MapVirtualKeyW 是纯映射查询，任何线程可调用。
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as i32;
    let lp = make_key_lparam(scan, up, is_extended_key(vk));
    let msg = if up { WM_KEYUP } else { WM_KEYDOWN };
    // SAFETY: target 有效；wParam 是虚拟键码，lParam 按 WM_KEYDOWN 规范组装。
    let ok = unsafe { PostMessageW(target, msg, vk as WPARAM, lp) };
    std::thread::sleep(std::time::Duration::from_millis(2));
    Ok(ok != 0)
}

/// 滚动。默认走真注入（SendInput 滚轮），原因同 click：WM_MOUSEWHEEL 投给 Qt
/// 自绘窗口同样可能被丢弃（收下消息、返回成功、界面不动）。
///
/// 滚轮注入只认"光标在哪"，所以要先搬光标、完事再搬回去；滚轮本身不会激活窗口
/// （Windows 有"在非活动窗口上滚动"的语义），所以这里不动前台。
pub fn scroll(ctx: &mut ActionContext, delta: i32) -> EngineResult<(Delivery, ClientPoint, u64)> {
    let started = Instant::now();
    let client = ctx.client_point()?;

    // 屏幕坐标：给了就用给的，没给就把客户区中心换算回屏幕。
    let (sx, sy) = match ctx.screen_point {
        Some(p) => p,
        None => dpi::client_to_screen(ctx.target, client.x, client.y)?,
    };
    // 滚轮同样要确认这一点没被别的进程窗口盖住，否则滚的是别人的窗口。
    ensure_point_reaches(ctx.root, sx, sy)?;

    // 一格 = WHEEL_DELTA(120)，正数向上滚（与 IPC.md 约定一致）。
    // 一格里发一个事件而不是"一个大增量发一次"：部分程序只按 ±120 逐格处理，
    // 收到 ±480 这种合并增量会直接丢掉。
    let notches = delta.clamp(-100, 100);
    let unit = if notches >= 0 { 120 } else { -120 };

    let move_flags =
        MOUSEEVENTF_MOVE_FLAG | MOUSEEVENTF_ABSOLUTE_FLAG | MOUSEEVENTF_VIRTUALDESK_FLAG;
    let (ax, ay) = absolute_coords(sx, sy);
    let mut inputs: Vec<INPUT> = Vec::new();
    inputs.push(mouse_input(ax, ay, 0, move_flags));
    for _ in 0..notches.abs() {
        inputs.push(mouse_input(0, 0, unit, MOUSEEVENTF_WHEEL_FLAG));
    }

    let mut guard = InjectionGuard::new();
    guard.will_move_cursor();
    send_inputs(&inputs)?;
    let (cursor_ok, focus_ok) = guard.restore();
    guard.audit(&mut ctx.warnings, cursor_ok, focus_ok);

    Ok((Delivery::SendInput, client, started.elapsed().as_millis() as u64))
}

// ---------------------------------------------------------------------------
// 消息参数组装
// ---------------------------------------------------------------------------

/// 鼠标消息的 lParam：低 16 位 x，高 16 位 y（都是有符号 16 位）。
fn make_lparam(x: i32, y: i32) -> LPARAM {
    let lo = (x as i16 as u16) as u32;
    let hi = (y as i16 as u16) as u32;
    ((hi << 16) | lo) as isize as LPARAM
}

/// 键盘消息的 lParam 位域（按 Win32 文档）：
/// bit0-15 重复次数(1)，bit16-23 扫描码，bit24 扩展键，bit29 上下文码(0)，
/// bit30 前一个键状态，bit31 转换状态（1 = 正在释放）。
fn make_key_lparam(scan: i32, up: bool, extended: bool) -> LPARAM {
    let mut lp: u32 = 1 & 0xFFFF; // 重复计数 = 1
    lp |= ((scan as u32) & 0xFF) << 16;
    if extended {
        lp |= 1 << 24;
    }
    if up {
        lp |= 1 << 30; // 前一状态：按下
        lp |= 1 << 31; // 转换状态：释放
    }
    lp as isize as LPARAM
}

/// 方向键、Home/End、Insert/Delete 等在键盘消息里必须带扩展位，
/// 否则很多程序收不到（它们按扫描码区分小键盘和主键盘区）。
fn is_extended_key(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x2E // PageUp..Delete
        | 0x5B..=0x5D // LWin/RWin/Apps
        | 0x6F // 数字键盘 /
        | 0x90 // NumLock
    )
}

/// 把 "ctrl" / "alt" / "shift" / "win" 映射成虚拟键码。
fn modifier_vk(m: Modifier) -> u16 {
    match m {
        Modifier::Ctrl | Modifier::Control => VK_CONTROL,
        Modifier::Alt => VK_MENU,
        Modifier::Shift => VK_SHIFT,
        Modifier::Win => VK_LWIN,
    }
}

/// 可读键名 -> 虚拟键码。
///
/// 支持三类写法：
/// - 单个字符（"a"、"7"、"+"）：ASCII 直接映射；
/// - 功能键（"f1".."f24"）；
/// - 具名键（"enter"/"esc"/"tab"/"space"/"up"/...）。
pub fn normalize_key(raw: &str) -> EngineResult<u16> {
    let key = raw.trim();
    if key.is_empty() {
        return Err(EngineError::invalid("key 不能为空"));
    }
    let lower = key.to_ascii_lowercase();

    // 单个 ASCII 字符：字母大写即为其虚拟键码；数字直接用。
    if key.len() == 1 {
        let ch = key.chars().next().expect("len==1 保证有字符");
        if ch.is_ascii_alphanumeric() {
            return Ok(ch.to_ascii_uppercase() as u16);
        }
    }

    // 功能键 F1..F24。
    if let Some(rest) = lower.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u16>() {
            if (1..=24).contains(&n) {
                return Ok(0x70 + n - 1); // VK_F1 = 0x70
            }
        }
    }

    let vk = match lower.as_str() {
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "tab" => 0x09,
        "space" | "spacebar" => 0x20,
        "backspace" | "back" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "up" | "arrowup" => 0x26,
        "down" | "arrowdown" => 0x28,
        "left" | "arrowleft" => 0x25,
        "right" | "arrowright" => 0x27,
        "ctrl" | "control" => VK_CONTROL,
        "alt" | "menu" => VK_MENU,
        "shift" => VK_SHIFT,
        "win" | "lwin" | "meta" => VK_LWIN,
        "capslock" | "caps" => 0x14,
        "numlock" => 0x90,
        "scrolllock" => 0x91,
        "prtsc" | "printscreen" => 0x2C,
        "pause" => 0x13,
        "apps" | "menu_key" => 0x5D,
        "numpad0" => 0x60,
        "numpad1" => 0x61,
        "numpad2" => 0x62,
        "numpad3" => 0x63,
        "numpad4" => 0x64,
        "numpad5" => 0x65,
        "numpad6" => 0x66,
        "numpad7" => 0x67,
        "numpad8" => 0x68,
        "numpad9" => 0x69,
        "numpadadd" => 0x6B,
        "numpadsub" | "numpadsubtract" => 0x6D,
        "numpadmul" | "numpadmultiply" => 0x6A,
        "numpaddiv" | "numpaddivide" => 0x6F,
        "numpaddecimal" => 0x6E,
        "plus" => 0xBB,     // VK_OEM_PLUS
        "minus" => 0xBD,    // VK_OEM_MINUS
        "comma" => 0xBC,    // VK_OEM_COMMA
        "period" | "dot" => 0xBE, // VK_OEM_PERIOD
        "slash" => 0xBF,
        "backslash" => 0xDC,
        "semicolon" => 0xBA,
        "quote" => 0xDE,
        "backtick" | "grave" => 0xC0,
        "bracketleft" => 0xDB,
        "bracketright" => 0xDD,
        _ => {
            return Err(EngineError::invalid(format!(
                "无法识别的按键名 {raw:?}；支持单字符、f1..f24、enter/esc/tab/up/down/left/right 等"
            )));
        }
    };
    Ok(vk)
}

// ---------------------------------------------------------------------------
// SendInput 底层
// ---------------------------------------------------------------------------

// windows-sys 里这些标志是 u32 常量，这里给短名字，免得每处都写全路径。
const MOUSEEVENTF_MOVE_FLAG: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN_FLAG: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP_FLAG: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN_FLAG: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP_FLAG: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN_FLAG: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP_FLAG: u32 = 0x0040;
const MOUSEEVENTF_ABSOLUTE_FLAG: u32 = 0x8000;
const MOUSEEVENTF_VIRTUALDESK_FLAG: u32 = 0x4000;
const MOUSEEVENTF_WHEEL_FLAG: u32 = 0x0800;

/// 屏幕坐标 -> SendInput 需要的 0..65535 绝对坐标。
///
/// 分母必须是**虚拟桌面**尺寸而不是主屏：多显示器下用主屏尺寸算，
/// 副屏上的点会被映射到屏幕外。
fn absolute_coords(x: i32, y: i32) -> (i32, i32) {
    let v = dpi::virtual_screen_rect();
    let vw = v.w.max(1) as f64;
    let vh = v.h.max(1) as f64;
    // 乘 65536 而不是 65535：系统把归一化值反算回像素时按 65535 做截断，
    // 用 65535 会在屏幕右下边缘系统性少 1px（2026-10-16 自测实测：拖到 y=1100
    // 落在 1099）。用 65536 + 夹到 65535 之后，量化误差变成居中分布的 ±0.5px。
    let nx = ((x - v.x) as f64 / vw * 65536.0).round() as i32;
    let ny = ((y - v.y) as f64 / vh * 65536.0).round() as i32;
    (nx.clamp(0, 65535), ny.clamp(0, 65535))
}

fn mouse_input(dx: i32, dy: i32, mouse_data: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: mouse_data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn keyboard_input(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

/// 按虚拟键码构造键盘事件，扫描码用 MapVirtualKeyW 取真实值。
///
/// 扫描码填 0 是 SendInput 路径原来的写法，靠系统兜底；而 PostMessage 路径一直
/// 取真实扫描码。Qt 的键盘映射（QWindowsKeyMapper）会同时看扫描码，所以这里统一
/// 发真实值，避免"同一个 Enter 两条路行为不一样"。
///
/// 注意：KEYEVENTF_UNICODE 的文本注入仍然走 [keyboard_input]——那里 wScan 放的是
/// UTF-16 码元，不是扫描码，不能套用这个函数。
fn key_event(vk: u16, up: bool) -> INPUT {
    // SAFETY: MapVirtualKeyW 是纯映射查询，任何线程可调用。
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    let flags = if up { KEYEVENTF_KEYUP } else { 0 };
    keyboard_input(vk, scan, flags)
}

/// 硬件扫描码键盘事件（wVk = 0 + KEYEVENTF_SCANCODE）。
///
/// 游戏（DirectInput / RawInput / Unity 的键盘层）常常只认"带硬件扫描码"的
/// 事件：收到只有虚拟键码、没有 KEYEVENTF_SCANCODE 的注入时直接丢掉。
/// 普通 Win32 程序两种都收，所以 press 默认改走这条；MapVirtualKeyW 映射不出
/// 扫描码的键（极少见）回退到虚拟键路径。
///
/// 依据：游戏自动化社区通用做法是"发 scancode 而不是 VK"（pydirectinput 等
/// 库就是为此存在）。2026-10-17 绝区零实测：SendInput 的 VK 路径（含 150ms
/// 长按）全部无效、鼠标注入却有效，指向游戏键盘层只认扫描码。
fn key_event_hardware(vk: u16, up: bool) -> INPUT {
    // SAFETY: MapVirtualKeyW 是纯映射查询，任何线程可调用。
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    if scan == 0 {
        return key_event(vk, up);
    }
    keyboard_input(0, scan, scancode_flags(up, is_extended_key(vk)))
}

/// 扫描码注入的 dwFlags（单测覆盖）。
fn scancode_flags(up: bool, extended: bool) -> u32 {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    flags
}

/// 一次性提交一串输入事件。
fn send_inputs(inputs: &[INPUT]) -> EngineResult<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: inputs 是本次调用独占的切片，指针与长度匹配，且 INPUT 布局由
    // windows-sys 保证与 Win32 一致；cbSize 必须等于 sizeof(INPUT)，这是 API 硬要求。
    let sent = unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32)
    };
    if sent as usize != inputs.len() {
        // 常见原因：目标进程以更高完整性级别运行（UIPI 拦截），或会话已锁定。
        return Err(EngineError::other(format!(
            "SendInput 只提交了 {sent}/{} 个事件；目标可能以管理员权限运行（UIPI 拦截）",
            inputs.len()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 目标解析
// ---------------------------------------------------------------------------

/// 从参数里解析出动作目标。
///
/// 优先级（与 IPC.md 的"快照校验"一节一致）：
/// 1. 给了 snapshot_id + uid：查表，过期/不存在直接 STALE_SNAPSHOT；
/// 2. 只给 hwnd：直接用，坐标必须显式给（除非调用方允许默认中心）；
/// 3. 什么都没给：INVALID_ARGS。
pub fn resolve_target(
    snapshot_id: Option<u64>,
    uid: Option<u64>,
    hwnd: Option<u64>,
    screen_point: Option<(i32, i32)>,
) -> EngineResult<ActionContext> {
    let mut warnings = Vec::new();

    if let Some(sid) = snapshot_id {
        let uid = uid.ok_or_else(|| {
            EngineError::invalid("给了 snapshot_id 就必须同时给 uid（uid 只在所属快照内唯一）")
        })?;
        let (snap, elem) = snapshot::resolve_element(sid, uid)?;
        let root = window::require_window(snap.hwnd, "snapshot.hwnd")?;
        // 元素自带 hwnd 时优先用它：它是 observe 当时命中的那个子窗口，
        // 比重新做一遍命中测试更准（期间窗口可能已经重排）。
        let target = match elem.hwnd {
            Some(raw) => {
                let h = window::from_u64(raw);
                if window::is_window(h) {
                    h
                } else {
                    let (cx, cy) = crate::uia::element_center(&elem);
                    window::deepest_child_at(root, cx, cy)
                }
            }
            None => {
                let (cx, cy) = crate::uia::element_center(&elem);
                window::deepest_child_at(root, cx, cy)
            }
        };
        // 按 uid 动作时坐标一律以元素为准：屏幕点只用来在"元素的 hwnd 已失效"时
        // 重新做命中测试，绝不作为投递坐标，否则会出现"uid 说点 A、坐标说点 B"。
        if screen_point.is_some() {
            warnings.push("同时给了 uid 和 x/y，投递坐标以 uid 指向的元素为准".to_string());
        }
        return Ok(ActionContext { root, target, element: Some(elem), screen_point: None, warnings });
    }

    let raw_hwnd = hwnd.ok_or_else(|| {
        EngineError::invalid("必须提供 snapshot_id+uid，或者直接提供 hwnd")
    })?;
    let root = window::require_window(raw_hwnd, "hwnd")?;
    let target = match screen_point {
        Some((x, y)) => window::deepest_child_at(root, x, y),
        None => window::focus_target(root),
    };

    Ok(ActionContext { root, target, element: None, screen_point, warnings })
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_timeline_uses_absolute_timestamps() {
    // 两笔，第二笔从录制的 400ms 开始（绝对时间轴）。旧实现会在 400 之上再叠
    // 上一笔的末时间戳（20）和笔间飞行时间，第二笔被推后到 420+，回放越来越慢。
    let strokes = vec![vec![(0, 0), (10, 0)], vec![(50, 0), (60, 0)]];
    let times = vec![Some(vec![0.0, 20.0]), Some(vec![400.0, 420.0])];
    let p = build_path_plan(&strokes, &times, false);
    let downs: Vec<usize> = p
        .plan
        .iter()
        .enumerate()
        .filter(|(_, s)| matches!(s, PathStep::Down))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(downs.len(), 2, "两笔应该各有一个 Down");
    let d0 = p.nominal[downs[0]].expect("第一笔必须有时间戳");
    let d1 = p.nominal[downs[1]].expect("第二笔必须有时间戳");
    assert_eq!(d0, 0.0);
    assert_eq!(d1, 400.0, "第二笔必须从绝对 400ms 开始，不能累加上一笔的时长");
    // 名义时间表必须单调不减（注入循环按它 sleep_until）。
    let mut last = -1.0;
    for t in p.nominal.iter().flatten() {
        assert!(*t >= last, "时间轴倒退了: {last} -> {t}");
        last = *t;
    }
    // 计划表每一步都要有截止时刻，否则 timed 模式会被关掉。
    assert!(p.nominal.iter().all(|v| v.is_some()));
    assert_eq!(p.plan.len(), p.cum.len());
}

fn times_sane_accepts_recorded_shape() {
    // win_record 给出的形状：每笔内单调、整段递增，第一笔从 0 开始。
    let strokes = vec![vec![(0.0, 0.0), (1.0, 1.0)], vec![(5.0, 5.0), (6.0, 6.0)]];
    let times = vec![vec![0.0, 20.0], vec![400.0, 420.0]];
    assert!(times_sane(&times, &strokes));
}

fn times_sane_rejects_bad_values() {
    let strokes = vec![vec![(0.0, 0.0), (1.0, 1.0)]];
    assert!(!times_sane(&vec![vec![f64::NAN, 20.0]], &strokes), "NaN 必须拒绝");
    assert!(!times_sane(&vec![vec![0.0, f64::INFINITY]], &strokes), "inf 必须拒绝");
    assert!(!times_sane(&vec![vec![-1.0, 20.0]], &strokes), "负数必须拒绝");
    assert!(!times_sane(&vec![vec![50.0, 20.0]], &strokes), "时间倒退必须拒绝");
    assert!(!times_sane(&vec![vec![0.0, 1e9]], &strokes), "超过硬上限必须拒绝");
}

fn scancode_flags_plain_down() {
        // KEYEVENTF_SCANCODE = 0x0008
        assert_eq!(scancode_flags(false, false), 0x0008);
    }

    #[test]
    fn scancode_flags_extended_up() {
        assert_eq!(scancode_flags(true, true), 0x0008 | 0x0001 | 0x0002);
    }

    #[test]
    fn extended_keys_detected() {
        assert!(is_extended_key(0x25)); // Left
        assert!(is_extended_key(0x5B)); // LWin
        assert!(!is_extended_key(0x71)); // F2
        assert!(!is_extended_key(0x1B)); // Esc
    }

    #[test]
    fn f2_maps_to_hardware_scancode() {
        // 依赖系统键位映射；F2 在所有常见布局的扫描码都是 0x3C。
        let scan = unsafe { MapVirtualKeyW(0x71, MAPVK_VK_TO_VSC) } as u16;
        assert_eq!(scan, 0x3C);
    }

    /// auto 模式的剪贴板分流规则（纯函数，不碰系统剪贴板——碰了会覆盖用户的剪贴板）。
    #[test]
    fn ascii_text_keeps_unicode_injection() {
        for text in ["abc123", "hello world! +-*/", "a\tb\nc", ""] {
            assert!(!needs_clipboard_for_auto(text), "{text:?} 不该走剪贴板");
        }
    }

    #[test]
    fn non_ascii_text_uses_clipboard() {
        // 中文、全角标点、emoji、带重音的拉丁字母都算非 ASCII。
        for text in ["中文", "你好，世界！", "ok😀", "café", "【自测】"] {
            assert!(needs_clipboard_for_auto(text), "{text:?} 应当走剪贴板");
        }
    }
}

/// 还原前台窗口。
///
/// 这是 SendInput 路径的收尾动作，由 main.rs 在整批动作完成后调用。
/// 实现上刻意保守：
/// - 原前台窗口已经不存在 → 什么都不做；
/// - 前台没变 → 什么都不做；
/// - 否则只尝试把它拉回来一次，失败只记日志。
///
/// **禁止**任何"顺便激活目标窗口"的行为，那是越界。
///
/// 2026-10-16 起这条"还前台"的职责已经移到 [`InjectionGuard::restore`]（它同时也
/// 负责还光标），main.rs 不再调用本函数，只保留 NoActivateGuard 做记账。
/// 留着它是为了在需要"只还前台、不碰光标"的场景下还有现成实现。
#[allow(dead_code)]
pub fn restore_foreground(guard: &NoActivateGuard) -> bool {
    let original = guard.original();
    if original.is_null() || !window::is_window(original) {
        return false;
    }
    if !guard.foreground_changed() {
        return false;
    }
    // 这里只用 AttachThreadInput 把原窗口线程的输入队列接回来，再让它自己
    // 通过 WM_SETFOCUS 恢复——不调用 SetForegroundWindow。
    match attach_threads(original) {
        Some(attached) => {
            // SAFETY: 在输入队列已合并的前提下把焦点还给原窗口。
            // 这里用 SetFocus 而不是 SetForegroundWindow：SetFocus 只改本线程
            // 输入队列内的焦点窗口，不会激活/提升窗口层级。
            // 返回值不看：目标原本就没焦点时它合法地返回 NULL，无法据此判断成败。
            unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(original) };
            // 显式解绑，不指望作用域结束——Drop 里做的就是 AttachThreadInput(...,0)。
            drop(attached);
            true
        }
        None => false,
    }
}
