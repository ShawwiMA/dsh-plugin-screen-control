//! 窗口枚举与元信息。
//!
//! 这里集中所有 HWND 相关的查询，dpi.rs 只负责坐标换算，input/capture/uia
//! 都通过本模块拿窗口信息，避免同一件事写三遍。
//!
//! 关于"顶层窗口"的判定：不是简单地 EnumWindows 一把梭。托盘/输入法/桌面工具
//! 会造出大量碎片窗口，如果不过滤，`win_list_windows` 一屏能返回几百条噪音。
//! 规则见 [`is_top_level_candidate`]。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::OnceLock;

use parking_lot::RwLock;
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows_sys::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
// GetFocus 在 windows-sys 里归在 KeyboardAndMouse 模块（winuser.h 的输入那一组），
// 不在 WindowsAndMessaging 里——放错模块就是 E0432。
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CWP_ALL, CWP_SKIPINVISIBLE, ChildWindowFromPointEx, EnumWindows, GA_ROOT, GWL_EXSTYLE,
    GWL_STYLE, GW_CHILD, GW_HWNDNEXT, GetAncestor, GetClassNameW, GetParent, GetWindow,
    GetWindowLongW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindow, IsWindowVisible, WS_CHILD, WS_DISABLED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE, WindowFromPoint,
};

use crate::dpi;
use crate::error::{EngineError, EngineResult};
use crate::protocol::{Rect, WinInfo, WindowFilter};

// 这两个 ES_* 是 EDIT 控件样式，windows-sys 没有导出，按 WinUser.h 的真实取值写死。
// ES_PASSWORD 与 WS_VISIBLE 位重叠（控件样式的历史遗留），这是官方定义，不是笔误。
const ES_PASSWORD: i32 = 0x0020;
const ES_READONLY: i32 = 0x0800;

/// u64（JSON 里的 hwnd）转 HWND。
pub fn from_u64(raw: u64) -> HWND {
    raw as usize as HWND
}

/// HWND 转 u64（回填 JSON）。
pub fn to_u64(hwnd: HWND) -> u64 {
    hwnd as usize as u64
}

/// 句柄是否为空。集中在 errors 模块，这里转个手方便调用。
pub fn is_null(hwnd: HWND) -> bool {
    hwnd.is_null()
}

/// 句柄是否指向一个仍然存在的窗口。
///
/// 为什么必须查：TS 侧可能拿着几秒前的 hwnd 调过来，期间窗口已经被销毁；
/// 对已销毁的句柄调任何 API 都是未定义行为。
pub fn is_window(hwnd: HWND) -> bool {
    if is_null(hwnd) {
        return false;
    }
    // SAFETY: IsWindow 对任意句柄值都是安全的——它就是专门用来做这个判断的，
    // 不会解引用，对无效句柄只返回 0。
    unsafe { IsWindow(hwnd) != 0 }
}

/// 校验 TS 传来的 hwnd 字符串：非空 + 存在。
pub fn require_window(raw: u64, field: &str) -> EngineResult<HWND> {
    let hwnd = from_u64(raw);
    if is_null(hwnd) {
        return Err(EngineError::invalid(format!("{field} 不能为空")));
    }
    if !is_window(hwnd) {
        return Err(EngineError::not_found(format!("{field}={raw} 指向的窗口不存在")));
    }
    Ok(hwnd)
}

/// 窗口类名。失败返回空串而不是报错——类名拿不到不该让整个枚举挂掉。
pub fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: hwnd 由调用方保证有效（枚举回调给出的或校验过的）；
    // buf 是栈上定长数组，指针与长度匹配，API 只写入不超过 len 个 u16。
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

/// 窗口标题。
pub fn window_title(hwnd: HWND) -> String {
    // SAFETY: 纯查询，参数为空指针与 0 长度，API 只返回长度。
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    // 留一个位置给结尾 0，GetWindowTextW 最多写 len+1 个字符。
    let mut buf = vec![0u16; len as usize + 1];
    // SAFETY: buf 长度是 len+1，与传入的容量一致；hwnd 有效（同 class_name 的约定）。
    let written = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if written <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..written as usize])
}

/// 窗口所属进程 ID。
pub fn process_id(hwnd: HWND) -> u32 {
    let mut pid: u32 = 0;
    // SAFETY: hwnd 有效（调用方约定）；pid 是栈上 u32，指针在同步调用内有效。
    // windows-sys 0.61 的这个绑定只收两个参数，线程 ID 是返回值——我们不需要它，忽略即可。
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    pid
}

/// 标题 + 类名 + PID 三元组，用来给重复枚举做去重键。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WindowKey {
    pub title: String,
    pub class_name: String,
    pub pid: u32,
}

pub fn key_of(hwnd: HWND) -> WindowKey {
    WindowKey { title: window_title(hwnd), class_name: class_name(hwnd), pid: process_id(hwnd) }
}

/// 顶层窗口是否值得出现在 win_list_windows 里。
///
/// 过滤理由：
/// - 不可见：`visible_only=false` 时仍要保留，但默认调用方会要求可见；
/// - WS_EX_TOOLWINDOW：输入法候选框、拖拽提示这类浮动工具窗，用户不认为它们是"窗口"；
/// - 无标题 + WS_POPUP 且无 WS_CHILD：大量隐形 message-only 窗口（类名常为
///   `...MessageWindow`）属于此类，留在列表里只会干扰模型选择。
///
/// 注意：这里**不**按类名黑名单过滤。黑名单永远跟不上实际软件，
/// 让 TS 侧用 filter 自己挑更可靠。
pub fn is_top_level_candidate(hwnd: HWND) -> bool {
    let ex = ex_style(hwnd);
    if ex & (WS_EX_TOOLWINDOW as i32) != 0 {
        return false;
    }
    let style = style_of(hwnd);
    // message-only 窗口没有标题、没有 WS_VISIBLE，通常是纯消息泵。
    let visible = style & (WS_VISIBLE as i32) != 0;
    let popup = style & (WS_POPUP as i32) != 0;
    let child = style & (WS_CHILD as i32) != 0;
    let titled = !window_title(hwnd).is_empty();
    if !visible && !titled {
        return false;
    }
    if popup && !child && !visible {
        return false;
    }
    true
}

/// `GetWindowLongW(GWL_STYLE)`。
pub fn style_of(hwnd: HWND) -> i32 {
    // SAFETY: README 记录的风险点——windows-sys 0.61.2 把 GetWindowLongW 的
    // nindex 声明为 WINDOW_LONG_PTR_INDEX 枚举；这里传常量 GWL_STYLE。
    // 若该枚举不含 GWL_STYLE 会编译报错，改用 `GWL_STYLE as WINDOW_LONG_PTR_INDEX` 即可。
    unsafe { GetWindowLongW(hwnd, GWL_STYLE) }
}

/// `GetWindowLongW(GWL_EXSTYLE)`。
pub fn ex_style(hwnd: HWND) -> i32 {
    // SAFETY: 同 style_of，这里传 GWL_EXSTYLE。
    unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) }
}

pub fn is_visible(hwnd: HWND) -> bool {
    // SAFETY: 纯查询，对任何句柄值都安全。
    unsafe { IsWindowVisible(hwnd) != 0 }
}

pub fn is_minimized(hwnd: HWND) -> bool {
    // SAFETY: 纯查询。
    unsafe { IsIconic(hwnd) != 0 }
}

pub fn is_enabled(hwnd: HWND) -> bool {
    style_of(hwnd) & (WS_DISABLED as i32) == 0
}

/// 是否是密码输入框。
///
/// 两条判定路径（IPC.md 要求 `GetWindowLongW(hwnd, GWL_STYLE) & ES_PASSWORD`，
/// 开 uia 后还可以由 UIA 的 IsPassword 属性补充，见 uia.rs）：
/// 命中即返回 true，调用方据此返回 PASSWORD_FIELD 并**拒绝输入**。
pub fn is_password_edit(hwnd: HWND) -> bool {
    style_of(hwnd) & ES_PASSWORD != 0
}

/// 是否只读（不作为拒绝理由，只用来给警告）。
pub fn is_readonly_edit(hwnd: HWND) -> bool {
    style_of(hwnd) & ES_READONLY != 0
}

pub fn is_topmost(hwnd: HWND) -> bool {
    ex_style(hwnd) & (WS_EX_TOPMOST as i32) != 0
}

pub fn is_noactivate(hwnd: HWND) -> bool {
    ex_style(hwnd) & (WS_EX_NOACTIVATE as i32) != 0
}

/// 顶层窗口（跳过 WS_CHILD 链，回到根）。
pub fn root_window(hwnd: HWND) -> HWND {
    // SAFETY: 纯查询，GA_ROOT 沿 parent 链上溯到根窗口，不修改任何状态。
    unsafe { GetAncestor(hwnd, GA_ROOT) }
}

/// 屏幕点命中的最上层可见窗口。
///
/// 和 [`deepest_child_at`] 的区别：那个是在**指定窗口内部**按客户区坐标下钻，
/// 回答"这个窗口里哪个子控件在这个位置"；这个是从整个桌面自上而下做命中测试，
/// 回答"这一点到底归谁"——用来判断目标点有没有被别的窗口盖住。
/// WindowFromPoint 会跳过隐藏窗口和 disabled 窗口，正是这里需要的语义。
pub fn window_at_point(screen_x: i32, screen_y: i32) -> HWND {
    let pt = windows_sys::Win32::Foundation::POINT { x: screen_x, y: screen_y };
    // SAFETY: 纯查询，point 按值传入，无副作用。
    unsafe { WindowFromPoint(pt) }
}

/// 直接父窗口。
pub fn parent_of(hwnd: HWND) -> Option<HWND> {
    // SAFETY: 纯查询；GetParent 对顶层窗口返回空句柄。
    let p = unsafe { GetParent(hwnd) };
    if p.is_null() { None } else { Some(p) }
}

/// 枚举所有顶层窗口，返回按 Z 序（最前 -> 最后）排列的句柄。
///
/// EnumWindows 本身就从最前开始枚举，所以返回顺序天然就是 Z 序，
/// 下标直接可以当 `z_order` 用。
pub fn enum_top_level() -> EngineResult<Vec<HWND>> {
    let mut out: Vec<HWND> = Vec::new();
    let ctx = &mut out as *mut Vec<HWND> as LPARAM;

    // SAFETY: 回调是 extern "system" 且只做 push；ctx 指向栈上 Vec，
    // EnumWindows 同步执行完毕前它一直有效且无别名。
    let ok = unsafe { EnumWindows(Some(collect_enum_proc), ctx) };
    if ok == 0 {
        return Err(EngineError::other("EnumWindows 失败"));
    }
    Ok(out)
}

extern "system" fn collect_enum_proc(hwnd: HWND, data: LPARAM) -> BOOL {
    // SAFETY: data 是 enum_top_level 里 &mut Vec<HWND> 的地址，类型一致；
    // 同步回调期间 Vec 不会被移动或释放，也没有其它 &mut 别名。
    unsafe {
        let vec = &mut *(data as *mut Vec<HWND>);
        vec.push(hwnd);
    }
    1
}

/// 递归枚举子窗口。
///
/// 为什么不用 EnumChildWindows 一把抓平：它给不出层级，而我们要给元素标 `depth`，
/// 而且 max_depth 需要在遍历时就能提前收手，扁平枚举做不到。
/// 这里每层单独调一次 EnumChildWindows，再用队列逐层下钻。
pub struct ChildWalk {
    /// 发现的子窗口，按 (深度, Z 序) 排列。
    pub windows: Vec<(HWND, u32)>,
    /// 是否因为 max_depth 或 max_elements 提前停止。
    pub truncated: bool,
}

pub fn enum_children_bfs(root: HWND, max_depth: u32, max_elements: usize) -> ChildWalk {
    let mut result: Vec<(HWND, u32)> = Vec::new();
    let mut truncated = false;
    // 真正的 BFS 用 VecDeque + pop_front：用 Vec::pop 是 LIFO（深度优先），
    // 那样 result 里会先出现一整条深链再回头，既不符合「(深度, Z 序)」的返回契约，
    // 也会让 max_elements 先被某个深层子树吃光。
    let mut queue: VecDeque<(HWND, u32)> = VecDeque::new();
    queue.push_back((root, 0));
    // 去重：被 hook 过的窗口可能给出「同一个子窗口挂在两个父节点下」的假树，
    // 或者 sibling 链成环。按 hwnd 去重后，同一元素不会被重复返回、也不会死循环。
    let mut seen: HashSet<usize> = HashSet::new();
    seen.insert(to_u64(root) as usize);

    while let Some((parent, depth)) = queue.pop_front() {
        let children = direct_children(parent);
        if children.is_empty() {
            continue;
        }
        if depth >= max_depth {
            // 还能再往下但被 max_depth 挡住，如实标记截断。
            truncated = true;
            continue;
        }
        for child in children {
            if result.len() >= max_elements {
                truncated = true;
                break;
            }
            if !seen.insert(to_u64(child) as usize) {
                continue;
            }
            result.push((child, depth + 1));
            queue.push_back((child, depth + 1));
        }
    }

    ChildWalk { windows: result, truncated }
}

/// 直接子窗口列表（只有一层）。
///
/// 为什么不用 EnumChildWindows：它枚举的是整棵子树（含孙窗口），拿来做「下钻一层」
/// 会把孙辈当成子辈，再被子窗口遍历重复枚举一遍 —— 结果是控件重复、depth 错乱、
/// 节点预算被同一批窗口吃掉两遍。GetWindow(GW_CHILD) + GW_HWNDNEXT 才是 Win32
/// 里「直接子窗口」的正解，顺序同样是 Z 序（最前 -> 最后）。
pub fn direct_children(parent: HWND) -> Vec<HWND> {
    // 防御性上限：被 hook 过的窗口可能给出环形 sibling 链，宁可截断也不死循环。
    const MAX_SIBLINGS: usize = 65_536;
    let mut out: Vec<HWND> = Vec::new();
    if parent.is_null() {
        return out;
    }
    // SAFETY: GetWindow 是纯查询；parent 由调用方保证是有效窗口。
    let mut child = unsafe { GetWindow(parent, GW_CHILD) };
    while !child.is_null() {
        out.push(child);
        if out.len() >= MAX_SIBLINGS {
            break;
        }
        // SAFETY: 同上；GW_HWNDNEXT 沿同级兄弟链前进。
        child = unsafe { GetWindow(child, GW_HWNDNEXT) };
    }
    out
}

/// 统计整棵子窗口树的节点数（受 cap 限制，避免病态窗口把内存吃光）。
pub fn count_descendants(root: HWND, cap: usize) -> usize {
    let mut count = 0usize;
    let mut queue = vec![root];
    while let Some(node) = queue.pop() {
        for child in direct_children(node) {
            count += 1;
            if count >= cap {
                return count;
            }
            queue.push(child);
        }
    }
    count
}

/// 命中测试：屏幕坐标下最深的可见子窗口。
///
/// 为什么要找"最深"：投递鼠标消息时，若只发给顶层窗口，很多程序（浏览器、
/// 各类自绘控件容器）不会处理，因为真正接收点击的是内部子窗口。
///
/// `CWP_SKIPINVISIBLE` 不能省——隐藏的子窗口往往覆盖在可见控件之上，
/// 命中了它们等于把消息投进了黑洞。
pub fn deepest_child_at(root: HWND, screen_x: i32, screen_y: i32) -> HWND {
    let mut current = root;
    // 逐层下钻。设一个硬上限，防止自引用/环形父子关系导致死循环
    // （Win32 理论上不该有环，但被 hook 过的窗口不保证）。
    for _ in 0..64 {
        // 关键：ChildWindowFromPointEx 收的是「current 自己的客户区坐标」。
        // 每下钻一层，坐标系就换一次父窗口（子窗口的客户区原点相对父窗口有偏移），
        // 必须用当前层的 current 重新换算；沿用第一层的换算结果会在第二层起歪掉，
        // 命中错控件。
        let (cx, cy) = match dpi::screen_to_client(current, screen_x, screen_y) {
            Ok(v) => v,
            Err(_) => break,
        };
        // SAFETY: current 是有效窗口（起点 root 由调用方校验，后续来自
        // 上一轮 ChildWindowFromPointEx 的返回值）；point 是栈上 POINT；
        // CWP_ALL 表示不做额外过滤，可见性由我们自己在下一轮判断。
        let child = unsafe {
            ChildWindowFromPointEx(
                current,
                windows_sys::Win32::Foundation::POINT { x: cx, y: cy },
                CWP_ALL | CWP_SKIPINVISIBLE,
            )
        };
        if child.is_null() || child == current {
            break;
        }
        current = child;
    }
    current
}

/// 目标窗口的"最深可投递子窗口"。
///
/// 和 [`deepest_child_at`] 的区别：这里没有具体坐标，用客户区中心做探测，
/// 适合 win_type / win_press 这类只需要一个焦点窗口的场景。
pub fn focus_target(root: HWND) -> HWND {
    let rect = match dpi::client_rect_on_screen(root) {
        Ok(r) => r,
        Err(_) => return root,
    };
    if rect.is_empty() {
        return root;
    }
    deepest_child_at(root, rect.x + rect.w / 2, rect.y + rect.h / 2)
}

/// 目标窗口所在线程**真正拥有键盘焦点**的子窗口。
///
/// 为什么必须 AttachThreadInput：`GetFocus` 只能查"本线程"的焦点窗口，跨线程一律
/// 返回空句柄。把我们的输入队列临时挂到目标线程上才读得到它的真实焦点。
///
/// 用途：密码框拦截。`win_type(hwnd=...)` 的 ctx.target 是客户区中心命中的窗口
/// （常常是根窗口），而按键实际落在焦点控件上——2026-10-16 用自测靶窗实测过：
/// 焦点在密码框里时按 hwnd 发 win_type 能绕过 ctx.target 的样式检查。
pub fn focused_child(root: HWND) -> Option<HWND> {
    if root.is_null() || !is_window(root) {
        return None;
    }
    // SAFETY: 纯查询，hwnd 已验过有效。
    let target_thread = unsafe { GetWindowThreadProcessId(root, std::ptr::null_mut()) };
    // SAFETY: 无参数查询。
    let self_thread = unsafe { GetCurrentThreadId() };

    if target_thread == 0 || target_thread == self_thread {
        // SAFETY: 同线程时 GetFocus 直接可用。
        let focus = unsafe { GetFocus() };
        return if focus.is_null() { None } else { Some(focus) };
    }

    // SAFETY: 两个线程 ID 都是刚查询到的真实值；附加失败返回 0，下面判掉。
    let attached = unsafe { AttachThreadInput(self_thread, target_thread, 1) };
    if attached == 0 {
        return None;
    }
    // SAFETY: 已附加到目标线程，GetFocus 现在能读到它的焦点窗口。
    let focus = unsafe { GetFocus() };
    // SAFETY: 配对解绑，参数与上面一致；附加期间没有别的线程改动本线程的队列。
    unsafe { AttachThreadInput(self_thread, target_thread, 0) };
    if focus.is_null() { None } else { Some(focus) }
}

/// 组装 WinInfo。
///
/// `z_order` 由调用方给（顶层窗口枚举时的下标）；子窗口传 0。
pub fn describe(hwnd: HWND, z_order: i32) -> WinInfo {
    let rect = dpi::window_rect(hwnd).unwrap_or_default();
    let client_rect = dpi::client_rect_on_screen(hwnd).unwrap_or_default();
    let pid = process_id(hwnd);
    let fg = dpi::foreground_window();
    WinInfo {
        hwnd: to_u64(hwnd),
        title: window_title(hwnd),
        class_name: class_name(hwnd),
        pid,
        process_name: process_name(pid),
        rect,
        client_rect,
        z_order,
        is_visible: is_visible(hwnd),
        is_minimized: is_minimized(hwnd),
        is_foreground: !fg.is_null() && fg == hwnd,
    }
}

/// 列出窗口，按 filter 过滤。
pub fn list_windows(filter: &WindowFilter) -> EngineResult<Vec<WinInfo>> {
    let visible_only = filter.visible_only.unwrap_or(true);
    let top_level_only = filter.top_level_only.unwrap_or(true);
    let title_needle = filter.title_contains.as_ref().map(|s| s.to_lowercase());

    let mut out = Vec::new();

    if top_level_only {
        let all = enum_top_level()?;
        for (idx, hwnd) in all.iter().copied().enumerate() {
            if !is_top_level_candidate(hwnd) {
                continue;
            }
            if visible_only && !is_visible(hwnd) {
                continue;
            }
            if !matches_filter(hwnd, filter, title_needle.as_deref()) {
                continue;
            }
            out.push(describe(hwnd, idx as i32));
        }
    } else {
        // 非顶层模式：把所有顶层窗口的子树也拉进来，但只到 4 层，
        // 深度再大就变成"列出进程里所有控件"，那不是这个方法的用途。
        let all = enum_top_level()?;
        for (idx, hwnd) in all.iter().copied().enumerate() {
            if visible_only && !is_visible(hwnd) {
                continue;
            }
            if matches_filter(hwnd, filter, title_needle.as_deref()) {
                out.push(describe(hwnd, idx as i32));
            }
            for (child, _depth) in enum_children_bfs(hwnd, 4, 2_000).windows {
                if visible_only && !is_visible(child) {
                    continue;
                }
                if matches_filter(child, filter, title_needle.as_deref()) {
                    out.push(describe(child, 0));
                }
            }
        }
    }

    Ok(out)
}

fn matches_filter(hwnd: HWND, filter: &WindowFilter, title_needle: Option<&str>) -> bool {
    if let Some(needle) = title_needle {
        let title = window_title(hwnd).to_lowercase();
        if !title.contains(needle) {
            return false;
        }
    }
    if let Some(class) = filter.class_name.as_ref() {
        if class_name(hwnd) != *class {
            return false;
        }
    }
    if let Some(pid) = filter.pid {
        if process_id(hwnd) != pid {
            return false;
        }
    }
    if let Some(want) = filter.process_name.as_ref() {
        // 拿不到进程名（受保护进程、权限不足）时按"不匹配"处理：过滤要的是确定性，
        // 把 None 当匹配会让"按名字找窗口"返回一堆来路不明的窗口。
        let matched = process_name(process_id(hwnd))
            .as_deref()
            .map(|actual| name_matches(actual, want))
            .unwrap_or(false);
        if !matched {
            return false;
        }
    }
    true
}

/// 进程名匹配：只认"同一个名字"，不做子串匹配。
///
/// 为什么要脱掉 `.exe` 再比：调用方写 `"QQ"` 和写 `"QQ.exe"` 是同一个意思。
/// 但**不能**用子串——子串会让 `"QQ"` 命中 `"QQBrowser.exe"`，那是"查错窗口"的典型来源。
fn name_matches(actual: &str, want: &str) -> bool {
    if actual.eq_ignore_ascii_case(want) {
        return true;
    }
    strip_exe(actual).eq_ignore_ascii_case(strip_exe(want))
}

/// 脱掉末尾的 `.exe`（大小写都认）。
///
/// 写成嵌套 `fn` 而不是闭包：闭包同时拿 `&str` 参数又返回 `&str` 时生命周期推不出来
/// （实测报 `lifetime may not live long enough`），`fn` 的省略规则直接就对。
fn strip_exe(s: &str) -> &str {
    s.strip_suffix(".exe")
        .or_else(|| s.strip_suffix(".EXE"))
        .unwrap_or(s)
}

#[cfg(test)]
mod name_match_tests {
    use super::name_matches;

    #[test]
    fn both_spellings_are_the_same_name() {
        assert!(name_matches("QQ.exe", "QQ.exe"));
        assert!(name_matches("QQ.exe", "QQ"));
        assert!(name_matches("QQ", "QQ.exe"));
        // Windows 的可执行文件名本来就不区分大小写。
        assert!(name_matches("QQ.exe", "qq"));
        assert!(name_matches("MSEDGE.EXE", "msedge.exe"));
        assert!(name_matches("QQ.EXE", "qq"));
    }

    #[test]
    fn substring_is_not_a_match() {
        // 这个函数存在的理由：写 "QQ" 绝不能命中 QQBrowser.exe。
        assert!(!name_matches("QQBrowser.exe", "QQ"));
        assert!(!name_matches("QQ.exe", "Q"));
        assert!(!name_matches("notepad.exe", "note"));
    }
}

// ---------------------------------------------------------------------------
// 进程名查询（带缓存）
// ---------------------------------------------------------------------------

/// PID -> 进程名缓存。
///
/// 为什么要缓存：win_list_windows 一次要描述上百个窗口，其中同一个进程可能占
/// 几十个；每个都 OpenProcess + QueryFullProcessImageNameW 是纯浪费——
/// 这两步加起来在机械盘上是毫秒级的。进程名基本不变，缓存到进程退出为止。
///
/// 缓存 0 个条目时不要写入：查不到的 PID 以后大概率还是查不到（权限问题），
/// 但也不能缓存 None，因为进程可能刚启动还没起来。这里用 Option<String> 存负结果，
/// 由上层的 TTL 清理（见 snapshot::purge，共用同一个清理时机）。
struct ProcessNameCache {
    map: RwLock<HashMap<u32, Option<String>>>,
}

fn process_cache() -> &'static ProcessNameCache {
    static CACHE: OnceLock<ProcessNameCache> = OnceLock::new();
    CACHE.get_or_init(|| ProcessNameCache { map: RwLock::new(HashMap::new()) })
}

/// 取进程可执行文件名（不含路径）。查不到返回 None。
pub fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    if let Some(hit) = process_cache().map.read().get(&pid) {
        return hit.clone();
    }
    let resolved = query_process_name(pid);
    process_cache().map.write().insert(pid, resolved.clone());
    resolved
}

/// 清空进程名缓存。快照清理时顺带调用，避免长时间运行时表无限膨胀。
pub fn clear_process_cache() {
    process_cache().map.write().clear();
}

fn query_process_name(pid: u32) -> Option<String> {
    // SAFETY: OpenProcess 是纯查询入口，PID 来自 GetWindowThreadProcessId；
    // QUERY_LIMITED_INFORMATION 是权限最低的查询权限，对受保护进程会失败，
    // 失败只返回空句柄，不会有副作用。
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    // RAII：无论如何都要关句柄，所以包一层作用域而不是散落的 CloseHandle。
    let guard = ScopedHandle(handle);

    let mut buf = vec![0u16; 1024];
    let mut size = buf.len() as u32;
    // SAFETY: guard 持有刚由 OpenProcess 得到的有效句柄，在本作用域内不会被关闭；
    // buf 是堆上定长 Vec，size 与容量一致，API 只写入不超过 size 个 u16。
    let ok = unsafe {
        QueryFullProcessImageNameW(
            guard.0,
            PROCESS_NAME_WIN32,
            buf.as_mut_ptr(),
            &mut size as *mut u32,
        )
    };
    if ok == 0 || size == 0 {
        return None;
    }

    let full = String::from_utf16_lossy(&buf[..size as usize]);
    // 只取文件名：TS 侧要的是 "Calculator.exe" 这种，不是完整路径。
    let name = full.rsplit(['\\', '/']).next().unwrap_or(full.as_str()).to_string();
    if name.is_empty() { None } else { Some(name) }
}

/// 句柄 RAII 包装。
///
/// 为什么必须有：句柄泄漏在常驻进程里是累积性故障——一次枚举泄漏几十个句柄，
/// 跑几小时就会因为句柄表耗尽而全盘失败。手写 CloseHandle 迟早会漏一条错误路径。
pub struct ScopedHandle(pub windows_sys::Win32::Foundation::HANDLE);

impl Drop for ScopedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 是本结构体独占的、由 OpenProcess/CreateEvent 之类
            // 成功调用返回的句柄；Drop 只执行一次，不会重复关闭；
            // 关闭后本结构体不再被使用（所有权语义保证）。
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// 便捷函数：把一个 0 表示失败的 Win32 返回值转成 Result。
pub fn check_bool(ok: BOOL, api: &'static str) -> EngineResult<()> {
    if ok == 0 { Err(crate::error::win32(api)) } else { Ok(()) }
}

/// 判断两个窗口是否同一个（句柄值比较，不做任何系统调用）。
pub fn same_window(a: HWND, b: HWND) -> bool {
    a == b
}

/// 把一个矩形裁剪到窗口客户区内，返回裁剪后的屏幕矩形。
///
/// 用于 win_capture 带 region 的情形：TS 侧给的区域可能超出窗口边界，
/// 直接把超出部分也 BitBlt 会得到黑边或失败。
pub fn intersect(a: Rect, b: Rect) -> Rect {
    let x1 = a.x.max(b.x);
    let y1 = a.y.max(b.y);
    let x2 = (a.x + a.w).min(b.x + b.w);
    let y2 = (a.y + a.h).min(b.y + b.h);
    Rect { x: x1, y: y1, w: (x2 - x1).max(0), h: (y2 - y1).max(0) }
}

/// 顶层窗口的 Z 序下标；找不到返回 -1。
pub fn z_order_of(hwnd: HWND) -> i32 {
    match enum_top_level() {
        Ok(all) => all.iter().position(|&h| h == hwnd).map(|i| i as i32).unwrap_or(-1),
        Err(_) => -1,
    }
}

#[cfg(test)]
mod child_tree_tests {
    //! 真窗口回归测试（审查第 02、03、11 条）：直接建一个父子窗口树，验证
    //! 「只取一层」「BFS 深度/Z 序」「去重」「逐级坐标换算」。
    //!
    //! 为什么不能只测纯函数：这几个缺陷的共同点就是"逻辑看着对、落到真实
    //! HWND 关系上才错"，所以这里用 CreateWindowEx 建真窗口来跑。

    use super::*;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HINSTANCE;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW,
        SW_SHOWNOACTIVATE, ShowWindow, WNDCLASSW, WS_CHILD, WS_OVERLAPPED, WS_VISIBLE,
    };

    static CLASS: OnceLock<bool> = OnceLock::new();

    extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
        // SAFETY: 默认窗口过程对任意消息都安全；参数由系统传入。
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn class_name() -> Vec<u16> {
        "DshScreenControlTestWindow\0".encode_utf16().collect()
    }

    /// 注册一次窗口类；返回类名（带结尾 NUL）。
    fn ensure_class() -> Vec<u16> {
        CLASS.get_or_init(|| {
            let name = class_name();
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: std::ptr::null_mut(),
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: name.as_ptr(),
            };
            // SAFETY: wc 里的类名指针在调用期间有效；注册失败返回 0（类可能已被别的
            // 测试注册过，此时同样可以创建窗口）。
            unsafe { RegisterClassW(&wc) };
            true
        });
        class_name()
    }

    fn create(parent: HWND, x: i32, y: i32, w: i32, h: i32, style: u32) -> HWND {
        let name = ensure_class();
        let title: Vec<u16> = "test\0".encode_utf16().collect();
        // SAFETY: 参数都是纯数值 / 有效指针；创建失败返回空句柄由调用方断言。
        unsafe {
            CreateWindowExW(
                0,
                name.as_ptr(),
                title.as_ptr(),
                style,
                x,
                y,
                w,
                h,
                parent,
                std::ptr::null_mut(),
                std::ptr::null_mut::<std::ffi::c_void>() as HINSTANCE,
                std::ptr::null(),
            )
        }
    }

    struct Tree {
        parent: HWND,
        a: HWND,
        b: HWND,
        a_child: HWND,
    }

    impl Tree {
        fn new() -> Self {
            let parent = create(
                std::ptr::null_mut(),
                100,
                100,
                400,
                300,
                WS_OVERLAPPED | WS_VISIBLE,
            );
            assert!(!parent.is_null(), "父窗口创建失败");
            // 子窗口 A、B：同一层；A 内部再挂一个 A1。
            let a = create(parent, 10, 10, 120, 120, WS_CHILD | WS_VISIBLE);
            let b = create(parent, 200, 10, 120, 120, WS_CHILD | WS_VISIBLE);
            let a_child = create(a, 5, 5, 60, 60, WS_CHILD | WS_VISIBLE);
            assert!(!a.is_null() && !b.is_null() && !a_child.is_null(), "子窗口创建失败");
            // SAFETY: 窗口刚创建、句柄有效；SW_SHOWNOACTIVATE 只改可见性不抢焦点。
            unsafe { ShowWindow(parent, SW_SHOWNOACTIVATE) };
            Self { parent, a, b, a_child }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            // SAFETY: DestroyWindow 对有效句柄安全；子窗口随父窗口一起销毁。
            unsafe {
                DestroyWindow(self.parent);
            }
        }
    }

    fn key(hwnd: HWND) -> usize {
        to_u64(hwnd) as usize
    }

    #[test]
    fn direct_children_only_one_layer() {
        with_window_station(|| {
            let t = Tree::new();
            let kids = direct_children(t.parent);
            let ks: Vec<usize> = kids.iter().map(|h| key(*h)).collect();
            assert!(ks.contains(&key(t.a)), "A 必须是直接子窗口");
            assert!(ks.contains(&key(t.b)), "B 必须是直接子窗口");
            assert!(!ks.contains(&key(t.a_child)), "A1 是孙窗口，不能出现在直接子窗口里");
            assert_eq!(kids.len(), 2, "父窗口只有 A、B 两个直接子窗口");
        });
    }

    #[test]
    fn bfs_returns_depth_and_no_duplicates() {
        with_window_station(|| {
            let t = Tree::new();
            let walk = enum_children_bfs(t.parent, 6, 100);
            let mut seen = std::collections::HashSet::new();
            for (hwnd, _depth) in &walk.windows {
                assert!(seen.insert(key(*hwnd)), "同一窗口被枚举了两次: {:?}", hwnd);
            }
            let depth_of = |h: HWND| walk.windows.iter().find(|(w, _)| key(*w) == key(h)).map(|(_, d)| *d);
            assert_eq!(depth_of(t.a), Some(1), "A 深度应为 1");
            assert_eq!(depth_of(t.b), Some(1), "B 深度应为 1");
            assert_eq!(depth_of(t.a_child), Some(2), "A1 深度应为 2");
            // BFS 顺序：深度不递减。
            let depths: Vec<u32> = walk.windows.iter().map(|(_, d)| *d).collect();
            assert!(depths.windows(2).all(|w| w[0] <= w[1]), "BFS 必须以深度递增顺序返回: {depths:?}");
        });
    }

    #[test]
    fn deepest_child_converts_coords_each_level() {
        with_window_station(|| {
            let t = Tree::new();
            // A1 客户区中心在父窗口客户区里的坐标：A(10,10) + A1(5,5) + 30
            let (px, py) = dpi::client_to_screen(t.parent, 10 + 5 + 30, 10 + 5 + 30)
                .expect("父窗口客户区坐标换算失败");
            let hit = deepest_child_at(t.parent, px, py);
            assert_eq!(
                key(hit),
                key(t.a_child),
                "坐标换算必须在每一层重新做，否则命中的是 A 而不是 A1"
            );
        });
    }

    /// 单元测试默认跑在没有交互桌面的线程里也能创建窗口，但 CI 偶尔会跑在
    /// 非交互 session；这时跳过而不是假红。
    fn with_window_station(f: impl FnOnce()) {
        let probe = create(std::ptr::null_mut(), 0, 0, 10, 10, WS_OVERLAPPED);
        if probe.is_null() {
            eprintln!("跳过：当前环境没有可用的窗口站（非交互 session）");
            return;
        }
        // SAFETY: 探测窗口句柄有效。
        unsafe { DestroyWindow(probe) };
        f();
    }
}