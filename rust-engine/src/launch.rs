//! `win_launch`：拉起一个进程。
//!
//! 用 `std::process::Command`，不引额外 crate。职责边界刻意画死：
//!
//! - **只负责把进程拉起来**。不等窗口出现（那是 `win_wait` 的活），
//!   也不接管子进程的生命周期——引擎退出不杀子进程，已拉起的程序继续跑。
//! - 不支持 shell 内建命令（`cd`、`dir`、`start`）。要 shell 语义就自己
//!   `path: "cmd.exe", args: ["/C", "..."]`，这样责任在调用方，引擎不替它猜。
//!
//! "起没起来"用 `GetExitCodeProcess == STILL_ACTIVE` 复核，而不是假定
//! `Command::spawn` 成功就万事大吉：路径写错时进程会瞬间退出，那种情况下
//! 调用方应该立刻放弃，而不是去 `win_wait` 干等一个永远不来的窗口。

use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::error::{EngineError, EngineResult};
use crate::protocol::{LaunchParams, LaunchResult, defaults};

/// `CREATE_NO_WINDOW`：控制台程序不弹黑框。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `STILL_ACTIVE`：GetExitCodeProcess 表示"还在跑"的约定值。
const STILL_ACTIVE: u32 = 259;

pub fn launch(p: &LaunchParams) -> EngineResult<LaunchResult> {
    if p.path.trim().is_empty() {
        return Err(EngineError::invalid("path 不能为空"));
    }

    let mut warnings = Vec::new();
    let resolved = resolve_program(&p.path, p.cwd.as_deref());

    // 裸名字（不含路径分隔符）在 PATH 里找不到时提前报错。理由：CreateProcess
    // 失败只会给一句"系统找不到指定的文件"，分不清是 exe 找不到还是工作目录不存在。
    if resolved.is_none() && p.cwd.is_none() && !p.path.contains(['\\', '/']) {
        return Err(EngineError::not_found(format!(
            "PATH 里找不到可执行文件 {:?}（win_launch 只接受 exe 或 PATH 里能搜到的名字，\
             不接受 cd/dir/start 这类 shell 内建命令；要用 shell 语义请显式启动 cmd.exe）",
            p.path
        )));
    }
    if let Some(found) = &resolved {
        // 只有调用方给的是相对路径时才提示解析结果；给绝对路径还回一句
        // "解析为 X" 属于纯噪音。
        if !Path::new(&p.path).is_absolute() {
            warnings.push(format!("相对路径解析为 {found}"));
        }
    }

    let mut cmd = Command::new(&p.path);
    if !p.args.is_empty() {
        cmd.args(&p.args);
    }
    if let Some(cwd) = &p.cwd {
        let dir = Path::new(cwd);
        if !dir.is_dir() {
            return Err(EngineError::not_found(format!("工作目录不存在或不是目录: {cwd}")));
        }
        cmd.current_dir(dir);
    }
    if p.no_window.unwrap_or(false) {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("启动 {:?} 失败: {e}", p.path);
            // 2 = ERROR_FILE_NOT_FOUND / 3 = ERROR_PATH_NOT_FOUND，这两种归 NOT_FOUND，
            // 其余（权限不足、不是有效的 Win32 程序）归 ENGINE_ERROR。
            return Err(match e.raw_os_error() {
                Some(2) | Some(3) => EngineError::not_found(msg),
                _ => EngineError::other(msg),
            });
        }
    };
    let pid = child.id();
    // 丢掉子进程句柄。Windows 上丢弃 Child **不会**终止进程（没有 kill-on-drop），
    // 子进程继续独立运行——引擎重启不影响已经拉起来的程序。
    drop(child);

    let check_ms = p.check_ms.unwrap_or(defaults::LAUNCH_CHECK_MS);
    let mut still_running = true;
    if check_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(check_ms));
        still_running = is_running(pid);
        if !still_running {
            warnings.push(format!(
                "进程 {pid} 在 {check_ms}ms 内已退出：命令行/参数大概率不对，窗口不会出现，\
                 别去 win_wait 干等"
            ));
        }
    }

    Ok(LaunchResult {
        pid,
        path: p.path.clone(),
        resolved_path: resolved,
        args: p.args.clone(),
        cwd: p.cwd.clone(),
        still_running,
        checked_after_ms: check_ms,
        warnings,
    })
}

/// 进程是否还活着。取不到句柄（已退出 / 权限不足）一律当"不在了"。
fn is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: OpenProcess 只返回句柄，失败给空句柄；下面按空句柄判并提前返回。
    let handle: HANDLE = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut code: u32 = 0;
    // SAFETY: handle 刚验过非空，code 是栈上 u32。
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
    // SAFETY: handle 是本函数自己的句柄，用完关掉，不会二次使用。
    unsafe { CloseHandle(handle) };
    ok != 0 && code == STILL_ACTIVE
}

/// 把 `path` 解析成绝对路径，解析不到返回 None（**不代表启动会失败**）。
///
/// 语义刻意与 Rust std 的 `Command` 查找规则对齐（std 自己就做 PATH + PATHEXT
/// 查找），否则会出现"我报找不到、std 其实能起来"这种自相矛盾的行为。
fn resolve_program(path: &str, cwd: Option<&str>) -> Option<String> {
    let p = Path::new(path);
    if p.is_absolute() || path.contains(['\\', '/']) {
        // 相对路径按 cwd 解析（给了 cwd 的话），否则按引擎自己的当前目录——
        // 这就是 CreateProcess 的真实语义，不自作聪明。
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else if let Some(dir) = cwd {
            Path::new(dir).join(p)
        } else {
            std::env::current_dir().ok()?.join(p)
        };
        return canonical_or_none(&candidate);
    }

    // 裸名字：PATH + PATHEXT。带扩展名时只按原名找，不再拼扩展名。
    let exts: Vec<String> = if p.extension().is_some() {
        vec![String::new()]
    } else {
        pathext()
    };
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let candidate = dir.join(format!("{path}{ext}"));
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }
    None
}

fn canonical_or_none(p: &Path) -> Option<String> {
    if !p.is_file() {
        return None;
    }
    match p.canonicalize() {
        // Windows 的 canonicalize 会加上 `\\?\`（长路径语法）前缀，对调用方没用还难看。
        Ok(abs) => Some(strip_verbatim(&abs.to_string_lossy())),
        Err(_) => Some(p.to_path_buf().to_string_lossy().to_string()),
    }
}

/// 去掉 `\\?\` 前缀：`\\?\C:\x` -> `C:\x`，`\\?\UNC\srv\share` -> `\\srv\share`。
fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}

/// PATHEXT 列表（小写化后的扩展名）。环境变量缺失时给一份 Windows 默认值。
fn pathext() -> Vec<String> {
    let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    raw.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// 让 `PathBuf` 在本模块里被用到（resolve_program 的返回值将来若要改成 PathBuf）。
#[allow(dead_code)]
fn as_path_buf(s: &str) -> PathBuf {
    PathBuf::from(s)
}
