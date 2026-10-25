//! 元素树。
//!
//! 阶段一（默认）：用 EnumChildWindows 走真实子窗口树。能覆盖的范围正好是
//! 传统 Win32 控件——对话框、记事本、资源管理器、多数国产桌面软件。
//! 它拿不到的：浏览器内部 DOM、WPF/UWP 的虚拟化控件（那些不是独立 HWND）。
//!
//! 阶段二（`--features uia`）：真 UI Automation，能拿到上面那批。
//! 两阶段的对外产物完全一致（`ElementInfo` 列表），切换只影响 source 字段，
//! TS 侧不需要改代码。

use windows_sys::Win32::Foundation::HWND;

use crate::error::{EngineError, EngineResult};
use crate::protocol::{ElementInfo, ElementSource, ObserveParams, Rect};
use crate::{capture, dpi, window};

/// 观察结果（不含 snapshot_id —— 那是 snapshot 模块的事）。
pub struct ElementTree {
    pub elements: Vec<ElementInfo>,
    pub truncated: bool,
    pub source: ElementSource,
}

/// 观察入口：按当前编译特性选择后端。
pub fn observe(hwnd: HWND, params: &ObserveParams) -> EngineResult<ElementTree> {
    let max_depth = params.max_depth.unwrap_or(crate::protocol::defaults::OBSERVE_MAX_DEPTH);
    let max_elements =
        params.max_elements.unwrap_or(crate::protocol::defaults::OBSERVE_MAX_ELEMENTS) as usize;

    if max_elements == 0 {
        return Err(EngineError::invalid("max_elements 必须大于 0"));
    }

    #[cfg(feature = "uia")]
    {
        // UIA 失败（COM 未初始化、目标进程拒绝、超时）不能让整个 observe 挂掉，
        // 安静回退到 child_window 树：拿到的东西少一点，但总比报错强。
        //
        // catch_unwind 是必须的：uiautomation 0.25 内部有 unwrap，目标进程无响应时
        // 它会 panic，而引擎是常驻进程——一次 observe 把整个引擎带走不可接受。
        // 前提是 panic=unwind：所以 uia 版本要用 `--profile release-uia`
        // （见 Cargo.toml），默认的 release 是 panic="abort"，那时这里拦不住。
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            uia_tree(hwnd, max_depth, max_elements)
        }));
        match attempt {
            Ok(Ok(tree)) => return Ok(tree),
            Ok(Err(e)) => {
                crate::log_event(
                    crate::protocol::LogLevel::Warn,
                    format!("UIA 树构建失败，回退 child_window: {e}"),
                );
            }
            Err(_) => {
                crate::log_event(
                    crate::protocol::LogLevel::Warn,
                    "UIA 内部 panic，已夹住，回退 child_window".to_string(),
                );
            }
        }
    }

    Ok(child_window_tree(hwnd, max_depth, max_elements))
}

/// 阶段一实现：EnumChildWindows 生成的元素树。
///
/// 过滤规则（决定什么算"元素"）：
/// - 不可见窗口直接丢：用户点不到，模型也不该尝试点它；
/// - 名字和类名都为空、且没有任何文本的子窗口丢：它们通常是布局用的
///   透明容器（如 "Static" 占位），保留只会稀释 max_elements 预算；
/// - 零面积窗口丢。
///
/// 保留：禁用但可见的控件——模型需要知道"这个按钮存在但点不动"，
/// 这比"按钮凭空消失"更有信息量（is_enabled=false）。
pub fn child_window_tree(root: HWND, max_depth: u32, max_elements: usize) -> ElementTree {
    let walk = window::enum_children_bfs(root, max_depth, max_elements);
    let mut elements = Vec::with_capacity(walk.windows.len());
    let mut uid: u64 = 1;

    for (hwnd, depth) in walk.windows {
        if !window::is_visible(hwnd) {
            continue;
        }
        let rect = dpi::window_rect(hwnd).unwrap_or_default();
        if rect.is_empty() {
            continue;
        }

        let class = window::class_name(hwnd);
        let name = element_name(hwnd, &class);
        if name.is_empty() && class.is_empty() {
            continue;
        }

        let mut elem = ElementInfo {
            uid,
            name,
            control_type: control_type_from_class(&class),
            rect,
            is_enabled: window::is_enabled(hwnd),
            // 密码判定：阶段一只能靠样式位；UIA 路径还会叠加 IsPassword 属性。
            is_password: window::is_password_edit(hwnd),
            depth,
            source: ElementSource::ChildWindow,
            hwnd: Some(window::to_u64(hwnd)),
        };

        // 名字为空时退化成类名，避免 TS 侧看到一堆空字符串无从下手。
        if elem.name.is_empty() {
            elem.name = class.clone();
        }
        // 只读编辑框标成 "Edit(ReadOnly)"？不——type 是给筛选用的，
        // 掺进状态会让 TS 侧的字符串比较失效。只读信息走 warnings/属性，不走这里。

        elements.push(elem);
        uid += 1;
    }

    ElementTree {
        truncated: walk.truncated || elements.len() >= max_elements,
        elements,
        source: ElementSource::ChildWindow,
    }
}

/// 元素显示名。
///
/// 窗口文本优先；空的话尝试从类名推断——很多控件（按钮、静态文本）的
/// 语义名字就在窗口文本里，GetWindowTextW 已经覆盖；这里只是兜底。
fn element_name(hwnd: HWND, class: &str) -> String {
    let text = window::window_title(hwnd);
    if !text.is_empty() {
        return normalize_text(&text);
    }
    // 部分控件（尤其是 owner-draw）窗口文本为空但类名有意义，交给调用方处理。
    let _ = class;
    String::new()
}

/// 文本规整：把换行/制表替换成空格并压缩连续空白。
///
/// 为什么必须做：窗口文本里带 \r\n 的话，JSON 里虽然能转义，但模型看到的是
/// 多行字符串，做匹配时很别扭；统一成单行更稳定。
fn normalize_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_space = false;
    for ch in raw.chars() {
        let is_ws = ch.is_whitespace();
        if is_ws {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim_end().to_string()
}

/// 类名 -> 可读控件类型。
///
/// 这里只映射标准控件类名（Win32 里它们是固定的字符串字面量）。
/// 认不出来的原样返回类名——比返回 "Unknown" 有用得多，
/// 模型可以据此判断"这是个第三方自绘控件，点它得靠坐标"。
pub fn control_type_from_class(class: &str) -> String {
    match class {
        "Button" => "Button",
        "Edit" => "Edit",
        "Static" => "Text",
        "ComboBox" | "ComboBoxEx32" => "ComboBox",
        "ListBox" => "List",
        "SysListView32" => "List",
        "SysTreeView32" => "Tree",
        "SysTabControl32" => "Tab",
        "SysHeader32" => "Header",
        "msctls_trackbar32" => "Slider",
        "msctls_updown32" => "Spinner",
        "msctls_progress32" => "ProgressBar",
        "msctls_statusbar32" => "StatusBar",
        "msctls_hotkey32" => "HotKey",
        "ScrollBar" => "ScrollBar",
        "RichEdit" | "RichEdit20A" | "RichEdit20W" | "RICHEDIT50W" => "Document",
        "SysLink" => "Hyperlink",
        "ToolbarWindow32" => "ToolBar",
        "ReBarWindow32" => "ToolBar",
        "Internet Explorer_Server" | "Chrome_RenderWidgetHostHWND" => "Document",
        "ApplicationFrameWindow" => "Window",
        "Windows.UI.Core.CoreWindow" => "Window",
        "MDIClient" => "Pane",
        "SHELLDLL_DefView" => "Pane",
        "SysIPAddress32" => "Edit",
        "" => "Unknown",
        other => return other.to_string(),
    }
    .to_string()
}

/// 把元素矩形转成屏幕坐标下的矩形。
///
/// 阶段一里 `ElementInfo.rect` 已经是屏幕坐标（GetWindowRect 的语义），
/// 这个函数存在的意义是把"将来可能出现的相对坐标"统一收口，
/// 免得以后加 UIA（BoundingRectangle 也是屏幕坐标，但原生类型不同）时到处改。
pub fn element_rect_screen(rect: Rect) -> Rect {
    rect
}

/// 给定元素找它在屏幕上的中心点，供按 uid 点击时使用。
pub fn element_center(elem: &ElementInfo) -> (i32, i32) {
    let r = element_rect_screen(elem.rect);
    (r.x + r.w / 2, r.y + r.h / 2)
}

/// 元素是否只读（仅用于给调用方补 warning，不作为拒绝输入的依据）。
pub fn element_readonly(elem: &ElementInfo) -> bool {
    match elem.hwnd {
        Some(raw) => window::is_readonly_edit(window::from_u64(raw)),
        None => false,
    }
}

/// 观察时是否要顺带截图。截图失败不影响元素树——很多最小化窗口抓不到图，
/// 但元素树依然有意义（用户可以据此决定要不要还原窗口）。
pub fn maybe_screenshot(
    hwnd: HWND,
    with_screenshot: bool,
    max_dim: u32,
) -> Option<crate::protocol::CaptureResult> {
    if !with_screenshot {
        return None;
    }
    capture::capture_to_file(
        Some(hwnd),
        None,
        max_dim,
        crate::protocol::ImageFormat::Png,
        crate::protocol::CaptureBackend::Auto,
    )
    .ok()
}

// ---------------------------------------------------------------------------
// 阶段二：真 UIA
// ---------------------------------------------------------------------------

#[cfg(feature = "uia")]
mod uia_backend {
    use super::*;
    use uiautomation::UIAutomation;
    use uiautomation::core::UIElement;
    use uiautomation::types::ControlType;

    /// 用 UIA 构建元素树。
    ///
    /// 风险提示（见 README）：uiautomation 0.25 的内部实现会调用 `unwrap()`，
    /// 目标进程无响应时可能 panic。因此 main.rs 在 uia feature 下
    /// 应当用 catch_unwind 包住整个请求处理（见 dispatch 的说明）。
    pub fn build(root_hwnd: HWND, max_depth: u32, max_elements: usize) -> EngineResult<ElementTree> {
        let automation = UIAutomation::new()
            .map_err(|e| EngineError::unsupported(format!("UIAutomation 初始化失败: {e}")))?;
        let root = automation
            .element_from_handle((root_hwnd as isize).into())
            .map_err(|e| EngineError::not_found(format!("UIA 取根元素失败: {e}")))?;
        let walker = automation
            .get_control_view_walker()
            .map_err(|e| EngineError::unsupported(format!("取 ControlViewWalker 失败: {e}")))?;

        let mut elements = Vec::new();
        let mut truncated = false;
        let mut uid: u64 = 1;

        // 显式栈的深度优先遍历：UIA 树可能很深，递归会爆栈。
        // 每层记录 (元素, 深度)。
        let mut stack: Vec<(UIElement, u32)> = Vec::new();
        if let Ok(first) = walker.get_first_child(&root) {
            stack.push((first, 1));
        }

        while let Some((element, depth)) = stack.pop() {
            if elements.len() >= max_elements {
                truncated = true;
                break;
            }

            if let Some(info) = describe_element(&element, depth, uid) {
                elements.push(info);
                uid += 1;
            }

            if depth < max_depth {
                if let Ok(child) = walker.get_first_child(&element) {
                    stack.push((child, depth + 1));
                }
            } else if walker.get_first_child(&element).is_ok() {
                truncated = true;
            }

            if let Ok(sibling) = walker.get_next_sibling(&element) {
                stack.push((sibling, depth));
            }
        }

        Ok(ElementTree { elements, truncated, source: ElementSource::Uia })
    }

    /// 把一个 UIElement 转成 ElementInfo。任何属性取不到都不算致命，
    /// 用默认值兜住——UIA 属性查询失败是常态（跨进程、元素已消失）。
    fn describe_element(element: &UIElement, depth: u32, uid: u64) -> Option<ElementInfo> {
        let control_type = map_control_type(element.get_control_type().ok()?);

        let rect = match element.get_bounding_rectangle() {
            Ok(r) => Rect { x: r.get_left(), y: r.get_top(), w: r.get_width(), h: r.get_height() },
            Err(_) => return None, // 没有几何信息的元素无法点击，直接丢
        };
        if rect.is_empty() {
            return None;
        }

        // NativeWindowHandle：UIA 里非窗口元素是 0，不为 0 时才回填。
        // 先算出来，下面的密码判定也要用它。
        let native = match element.get_native_window_handle() {
            Ok(h) => {
                let raw: isize = h.into();
                if raw > 0 { Some(raw as u64) } else { None }
            }
            Err(_) => None,
        };

        // 密码判定：以 UIA 的 IsPassword 为准，取不到再用窗口样式兜底。
        //
        // **不要**按类名兜底。类名叫 "Edit" 的控件绝大多数不是密码框，
        // 一旦拿类名当判据，整棵树上每个编辑框都会被标成密码框，
        // 结果是"UIA 版本一个字都打不进去"——这是实测踩到的坑，别再改回去。
        let mut is_password = element.is_password().unwrap_or(false);
        if !is_password {
            if let Some(raw) = native {
                let hwnd = window::from_u64(raw);
                if window::is_window(hwnd) {
                    is_password = window::is_password_edit(hwnd);
                }
            }
        }

        Some(ElementInfo {
            uid,
            name: element.get_name().unwrap_or_default(),
            control_type,
            rect,
            is_enabled: element.is_enabled().unwrap_or(true),
            is_password,
            depth,
            source: ElementSource::Uia,
            hwnd: native,
        })
    }

    /// ControlType -> IPC 里的字符串。
    ///
    /// 用 `as i32` 取判别值再映射，而不是 match 枚举本身：
    /// uiautomation 的 ControlType 变体名随版本改过（例如 Calendar 系列），
    /// 按整数映射即使枚举加变体也不会编译失败，只是回落到 "Custom"。
    fn map_control_type(ct: ControlType) -> String {
        match ct as i32 {
            50000 => "Button",
            50001 => "Calendar",
            50002 => "CheckBox",
            50003 => "ComboBox",
            50004 => "Edit",
            50005 => "Hyperlink",
            50006 => "Image",
            50007 => "ListItem",
            50008 => "List",
            50009 => "Menu",
            50010 => "MenuBar",
            50011 => "MenuItem",
            50012 => "ProgressBar",
            50013 => "RadioButton",
            50014 => "ScrollBar",
            50015 => "Slider",
            50016 => "Spinner",
            50017 => "StatusBar",
            50018 => "Tab",
            50019 => "TabItem",
            50020 => "Text",
            50021 => "ToolBar",
            50022 => "ToolTip",
            50023 => "Tree",
            50024 => "TreeItem",
            50025 => "Custom",
            50026 => "Group",
            50027 => "Thumb",
            50028 => "DataGrid",
            50029 => "DataItem",
            50030 => "Document",
            50031 => "SplitButton",
            50032 => "Window",
            50033 => "Pane",
            50034 => "Header",
            50035 => "HeaderItem",
            50036 => "Table",
            50037 => "TitleBar",
            50038 => "Separator",
            _ => "Custom",
        }
        .to_string()
    }
}

#[cfg(feature = "uia")]
use uia_backend::build as uia_tree;
