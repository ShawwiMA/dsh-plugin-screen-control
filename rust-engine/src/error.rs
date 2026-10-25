//! 错误类型与错误码。
//!
//! 设计约束（来自 IPC.md）：对外只能出现七个错误码，任何内部失败
//! （Win32 调用失败、IO 错误、序列化错误……）都必须折叠进这七个之一，
//! 不能把 `ERROR_ACCESS_DENIED` 这种东西原样丢给 TS 侧。

use std::fmt;
use std::io;

/// 对外错误码。字符串字面量必须与 IPC.md 第 42 行完全一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// 参数缺失/类型不对/自相矛盾。
    InvalidArgs,
    /// 窗口、元素、快照等目标不存在。
    NotFound,
    /// snapshot 过期或 uid 不属于该 snapshot，要求重新 observe。
    StaleSnapshot,
    /// 目标是密码框，引擎硬性拒绝输入。
    PasswordField,
    /// 当前平台/构建不支持该能力（例如未开 uia feature）。
    Unsupported,
    /// 操作超时（等待窗口响应、等待截图等）。
    Timeout,
    /// 其它内部错误。
    EngineError,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidArgs => "INVALID_ARGS",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::StaleSnapshot => "STALE_SNAPSHOT",
            ErrorCode::PasswordField => "PASSWORD_FIELD",
            ErrorCode::Unsupported => "UNSUPPORTED",
            ErrorCode::Timeout => "TIMEOUT",
            ErrorCode::EngineError => "ENGINE_ERROR",
        }
    }

    /// 反查。TS 侧如果要把错误码回传（例如日志归并），用得上。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "INVALID_ARGS" => Some(ErrorCode::InvalidArgs),
            "NOT_FOUND" => Some(ErrorCode::NotFound),
            "STALE_SNAPSHOT" => Some(ErrorCode::StaleSnapshot),
            "PASSWORD_FIELD" => Some(ErrorCode::PasswordField),
            "UNSUPPORTED" => Some(ErrorCode::Unsupported),
            "TIMEOUT" => Some(ErrorCode::Timeout),
            "ENGINE_ERROR" => Some(ErrorCode::EngineError),
            _ => None,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 引擎内部统一错误类型。
///
/// 每个变体都自带一个错误码，`code()` 就是唯一映射入口；
/// 这样上层分发器只需要 `Err(e) => Frame::err(id, e)`，不用 match 一堆分支。
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    InvalidArgs(String),

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    StaleSnapshot(String),

    #[error("{0}")]
    PasswordField(String),

    #[error("{0}")]
    Unsupported(String),

    #[error("{0}")]
    Timeout(String),

    /// Win32 调用失败。`api` 是函数名，`os_error` 是 GetLastError 的值。
    /// 保留原始信息只为了进日志，对外的 message 会带上它但 code 是 ENGINE_ERROR。
    #[error("{api} 失败 (Win32 错误码 {os_error})")]
    Win32 { api: &'static str, os_error: u32 },

    /// 参数解析失败，统一归到 INVALID_ARGS。
    #[error("JSON 参数解析失败: {0}")]
    Json(#[from] serde_json::Error),

    /// 文件读写失败（写临时 PNG、建目录等）。
    #[error("IO 失败: {0}")]
    Io(#[from] io::Error),

    /// 兜底。任何不想细分的内部失败都走这里。
    #[error("{0}")]
    Other(String),
}

impl EngineError {
    pub fn code(&self) -> ErrorCode {
        match self {
            EngineError::InvalidArgs(_) | EngineError::Json(_) => ErrorCode::InvalidArgs,
            EngineError::NotFound(_) => ErrorCode::NotFound,
            EngineError::StaleSnapshot(_) => ErrorCode::StaleSnapshot,
            EngineError::PasswordField(_) => ErrorCode::PasswordField,
            EngineError::Unsupported(_) => ErrorCode::Unsupported,
            EngineError::Timeout(_) => ErrorCode::Timeout,
            EngineError::Win32 { .. } | EngineError::Io(_) | EngineError::Other(_) => {
                ErrorCode::EngineError
            }
        }
    }

    /// 常用的构造快捷方式，省得每处都写 `EngineError::InvalidArgs(format!(...))`。
    pub fn invalid(msg: impl Into<String>) -> Self {
        EngineError::InvalidArgs(msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        EngineError::NotFound(msg.into())
    }

    pub fn unsupported(msg: impl Into<String>) -> Self {
        EngineError::Unsupported(msg.into())
    }

    pub fn stale(msg: impl Into<String>) -> Self {
        EngineError::StaleSnapshot(msg.into())
    }

    pub fn timeout(msg: impl Into<String>) -> Self {
        EngineError::Timeout(msg.into())
    }

    pub fn other(msg: impl Into<String>) -> Self {
        EngineError::Other(msg.into())
    }
}

/// 便捷别名：引擎里几乎每个函数都返回它。
pub type EngineResult<T> = Result<T, EngineError>;

/// 把一次 Win32 调用失败包成 `EngineError::Win32`。
///
/// 用法：`let ok = unsafe { SomeApi(...) }; if ok == 0 { return Err(win32("SomeApi")); }`
/// 内部自己取 GetLastError，调用点不用再写一遍。
pub fn win32(api: &'static str) -> EngineError {
    // SAFETY: GetLastError 无参数、无副作用，任何线程任何时刻调用都合法。
    // 必须在失败的 API 调用之后紧接着调用，中间不能插入别的 Win32 调用。
    let os_error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    EngineError::Win32 { api, os_error }
}

/// 把 HWND 的空值判断收在一处：windows-sys 里 HWND 是裸指针，
/// 用 `== std::ptr::null_mut()` 比较到处都是噪音，且容易写错类型。
pub fn is_null_handle(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    hwnd.is_null()
}
