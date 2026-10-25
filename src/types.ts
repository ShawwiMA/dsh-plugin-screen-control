/**
 * IPC 协议类型 —— 与 `docs/IPC.md` 一一对应。
 *
 * 改这里的任何字段名,必须同步改 docs/IPC.md 和 rust-engine/src/protocol.rs。
 * 三处不一致时以 docs/IPC.md 为准。
 */

/** 无损 JSON 值(与 DSH 工具运行时的规范输出一致)。 */
export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue }

/** 引擎错误码,取值受 docs/IPC.md 约束。 */
export type ErrorCode =
  | 'INVALID_ARGS'
  | 'NOT_FOUND'
  | 'STALE_SNAPSHOT'
  | 'PASSWORD_FIELD'
  | 'UNSUPPORTED'
  | 'TIMEOUT'
  | 'ENGINE_ERROR'

/** 屏幕物理像素矩形。 */
export interface Rect {
  x: number
  y: number
  w: number
  h: number
}

/** 一个顶层窗口。 */
export interface WinInfo {
  hwnd: number
  title: string
  class_name: string
  pid: number
  process_name: string
  rect: Rect
  client_rect: Rect
  z_order: number
  is_visible: boolean
  is_minimized: boolean
  is_foreground: boolean
}

/** 一次屏幕捕获的结果。阶段一回传临时 PNG 文件路径,不走管道 Base64。 */
export interface CaptureResult {
  path: string
  width: number
  height: number
  bytes: number
  backend: string
  capture_ms: number
  scaled_from: { w: number; h: number } | null
}

/** 元素树里的一个可交互元素。 */
export interface ElementInfo {
  uid: number
  name: string
  control_type: string
  rect: Rect
  is_enabled: boolean
  is_password: boolean
  depth: number
  source: string
}

/** `win_observe` 的返回:快照 + 元素树 + 可选截图。 */
export interface ObserveResult {
  snapshot_id: number
  created_at_ms: number
  expires_at_ms: number
  hwnd: number
  element_count: number
  truncated: boolean
  elements: ElementInfo[]
  screenshot: CaptureResult | null
}

/** 一次输入注入的结果。 */
export interface ActionResult {
  delivered: string
  target_hwnd: number
  deepest_child: number
  coords_client: { x: number; y: number }
  sequence_ms: number
  warnings: string[]
}

/** 监视器信息。 */
export interface MonitorInfo {
  index: number
  rect: Rect
  is_primary: boolean
  dpi: number
}

/** `engine_ping` 的返回。 */
export interface EnginePing {
  ok: boolean
  version: string
  pid: number
  dpi_aware: string
  monitor_count: number
  monitors: MonitorInfo[]
  uptime_ms: number
}

/** 请求帧。 */
export interface RpcRequest {
  id: number
  method: string
  params: Record<string, JsonValue>
}

/** 错误体。 */
export interface RpcErrorBody {
  code: ErrorCode
  message: string
}

/** 响应帧:成功走 `result`,失败走 `error`。 */
export interface RpcResponse {
  id: number
  ok: boolean
  result?: JsonValue
  error?: RpcErrorBody
}

/** 无 id 的事件帧,只记日志,不参与请求配对。 */
export interface RpcEvent {
  event: 'log'
  level: string
  message: string
}

/** 引擎侧返回的错误,`code` 直接来自协议。 */
export class EngineError extends Error {
  readonly code: ErrorCode

  constructor(code: ErrorCode, message: string) {
    super(message)
    this.name = 'EngineError'
    this.code = code
  }
}
