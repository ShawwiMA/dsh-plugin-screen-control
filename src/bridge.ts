/**
 * 命名管道客户端 —— TS 壳与 Rust 引擎之间唯一的通道。
 *
 * 职责刻意保持极薄:
 *   1. 定位并拉起 Rust 引擎进程(`--pipe <完整路径>`);
 *   2. NDJSON 逐行收发,按 `id` 配对请求与响应;
 *   3. 进程崩溃时拒绝所有在途请求,下一次调用自动重启(限流)。
 *
 * 这里不做任何图像处理、坐标换算、Win32 调用 —— 那些全在 Rust 侧。
 */

import { spawn, type ChildProcess } from 'node:child_process'
import { connect, type Socket } from 'node:net'
import { existsSync } from 'node:fs'
import { randomUUID } from 'node:crypto'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { EngineError, type JsonValue, type Rect, type RpcResponse } from './types.js'

const HERE = path.dirname(fileURLToPath(import.meta.url))
/** `lib/bridge.js` → 包根;`src/bridge.ts` 直接跑时也指向包根。 */
const PACKAGE_ROOT = path.resolve(HERE, '..')

const ENGINE_EXE = 'screen-control-engine.exe'
const PIPE_PREFIX = '\\\\.\\pipe\\'
const CONNECT_TIMEOUT_MS = 5_000
const CONNECT_RETRY_MS = 20
const DEFAULT_CALL_TIMEOUT_MS = 15_000
/** 60 秒窗口内最多自动重启次数,防止二进制缺失时无限拉起。 */
const MAX_RESTARTS_PER_WINDOW = 3
const RESTART_WINDOW_MS = 60_000
/** 保留的 stderr 尾部行数,用于报错时给出真实原因。 */
const STDERR_TAIL_LINES = 20

/** 引擎日志出口。故意只要求最小接口,便于测试时替换。 */
export interface BridgeLogger {
  info(message: string): void
  warn(message: string): void
}

/** 引擎可执行文件的候选位置,按优先级排列。 */
export function engineCandidates(): string[] {
  const list: string[] = []
  const fromEnv = process.env.DSH_SCREEN_CONTROL_ENGINE
  if (fromEnv && fromEnv.trim()) list.push(fromEnv.trim())
  list.push(path.join(PACKAGE_ROOT, 'rust-engine', 'target', 'release', ENGINE_EXE))
  list.push(path.join(PACKAGE_ROOT, 'rust-engine', 'target', 'debug', ENGINE_EXE))
  list.push(path.join(PACKAGE_ROOT, 'bin', ENGINE_EXE))
  return list
}

/** 返回第一个存在的引擎路径,找不到返回 `null`。 */
export function locateEngine(): string | null {
  for (const candidate of engineCandidates()) {
    if (existsSync(candidate)) return candidate
  }
  return null
}

/**
 * `win_find` 的参数:一次性模板匹配(不轮询)。
 * 字段名与 docs/IPC.md 的 `win_find` 完全一致,这里不做任何改名或平铺转换。
 */
export interface FindParams {
  /** 模板 PNG 绝对路径(必须是从未经缩放的截图裁出来的,否则用 scale 补回去)。 */
  template: string
  /** 目标窗口;不给就搜整个虚拟桌面。 */
  hwnd?: number
  /** 搜索区域(屏幕物理像素);越界会与捕获区域求交并在 warnings 里说明。 */
  region?: Rect
  /** 匹配阈值 0..1,默认沿用引擎侧 0.95。 */
  min_score?: number
  /** 模板相对屏幕的缩放系数,默认 1.0。 */
  scale?: number
}

/**
 * `win_find` 的返回。`found=false` 不是错误:位置/得分仍是最佳候选,
 * 调用方**必须先看 found** 再决定用不用 x/y。
 */
export interface FindResult {
  found: boolean
  /** 最佳匹配得分,1.0 = 逐像素完全相同。 */
  score: number
  x: number
  y: number
  /** 缩放后的模板尺寸。 */
  w: number
  h: number
  cx: number
  cy: number
  /** 搜索区域左上角(屏幕物理像素);x - origin.x 才是匹配在截图里的偏移。 */
  origin: { x: number; y: number }
  /** 实际搜索的区域(region 越界时是裁剪后的)。 */
  searched_region: Rect
  elapsed_ms: number
  warnings: string[]
}

interface Pending {
  resolve(value: JsonValue): void
  reject(error: Error): void
  timer: NodeJS.Timeout
}

/** 一次 RPC 调用的可选项。 */
export interface CallOptions {
  timeoutMs?: number
  signal?: AbortSignal
}

/**
 * 引擎桥。所有工具共用同一个实例,引擎进程也只有一个。
 */
export class EngineBridge {
  readonly #logger: BridgeLogger
  #child: ChildProcess | null = null
  #socket: Socket | null = null
  #pending = new Map<number, Pending>()
  #buffer = ''
  #nextId = 1
  #starting: Promise<void> | null = null
  #stderrTail: string[] = []
  #restartTimes: number[] = []
  #disposed = false
  #lastError: string | null = null

  constructor(logger: BridgeLogger) {
    this.#logger = logger
  }

  /** 当前是否有一条可用的管道连接。 */
  get running(): boolean {
    return this.#socket !== null && !this.#socket.destroyed
  }

  /** 诊断信息,给 `win_engine_status` 用。 */
  status(): JsonValue {
    return {
      running: this.running,
      engine_path: locateEngine(),
      searched: engineCandidates(),
      pid: this.#child?.pid ?? null,
      child_exited: this.#child ? this.#child.exitCode !== null : null,
      pending_calls: this.#pending.size,
      restarts_in_window: this.#restartTimes.length,
      last_error: this.#lastError,
      stderr_tail: this.#stderrTail.join('\n'),
    }
  }

  /**
   * 发一条 RPC 并等待响应。
   * @throws EngineError 引擎未启动、超时、被取消,或引擎返回了错误码。
   */
  async call(
    method: string,
    params: Record<string, JsonValue> = {},
    options: CallOptions = {},
  ): Promise<JsonValue> {
    if (this.#disposed) throw new EngineError('ENGINE_ERROR', '插件已卸载,引擎桥已关闭')
    const timeoutMs = options.timeoutMs ?? DEFAULT_CALL_TIMEOUT_MS
    await this.#ensureStarted()
    const socket = this.#socket
    if (!socket || socket.destroyed) throw new EngineError('ENGINE_ERROR', '引擎管道不可用')

    const id = this.#nextId++
    const frame = `${JSON.stringify({ id, method, params })}\n`

    return await new Promise<JsonValue>((resolve, reject) => {
      const settle = (fn: () => void) => {
        clearTimeout(timer)
        options.signal?.removeEventListener('abort', onAbort)
        this.#pending.delete(id)
        fn()
      }
      const timer = setTimeout(() => {
        settle(() => reject(new EngineError('TIMEOUT', `${method} 超时(${timeoutMs}ms)`)))
      }, timeoutMs)
      const onAbort = () => {
        settle(() => reject(new EngineError('TIMEOUT', `${method} 已被调用方取消`)))
      }

      if (options.signal) {
        if (options.signal.aborted) {
          settle(() => reject(new EngineError('TIMEOUT', `${method} 在派发前已取消`)))
          return
        }
        options.signal.addEventListener('abort', onAbort, { once: true })
      }

      this.#pending.set(id, {
        resolve: (value) => settle(() => resolve(value)),
        reject: (error) => settle(() => reject(error)),
        timer,
      })

      socket.write(frame, (error) => {
        if (!error) return
        settle(() => reject(new EngineError('ENGINE_ERROR', `写入引擎失败: ${error.message}`)))
      })
    })
  }

  /**
   * 一次性模板匹配(`win_find`)。与 `call` 的区别只是多了一层类型标注:
   * 桥层依然是"哑管道",不解释结果;命中/未命中都正常返回,失败才抛 EngineError。
   */
  async find(params: FindParams, options: CallOptions = {}): Promise<FindResult> {
    const result = await this.call(
      'win_find',
      params as unknown as Record<string, JsonValue>,
      options,
    )
    return result as unknown as FindResult
  }

  /** 关闭引擎进程与连接。可重复调用。 */
  dispose(): void {
    if (this.#disposed) return
    this.#disposed = true
    this.#failAllPending(new EngineError('ENGINE_ERROR', '插件卸载,在途调用被中断'))
    this.#socket?.destroy()
    this.#socket = null
    const child = this.#child
    this.#child = null
    if (child && child.exitCode === null) {
      // 先请它自己退,给 2 秒;不退就强杀,避免留下孤儿进程。
      child.kill()
      const hardKill = setTimeout(() => {
        if (child.exitCode === null) child.kill('SIGKILL')
      }, 2_000)
      hardKill.unref?.()
    }
  }

  async #ensureStarted(): Promise<void> {
    if (this.running) return
    if (!this.#starting) {
      this.#starting = this.#start().finally(() => {
        this.#starting = null
      })
    }
    await this.#starting
  }

  async #start(): Promise<void> {
    if (this.#disposed) throw new EngineError('ENGINE_ERROR', '插件已卸载')

    const exe = locateEngine()
    if (!exe) {
      const searched = engineCandidates().join('\n  ')
      this.#lastError = 'engine binary not found'
      throw new EngineError(
        'ENGINE_ERROR',
        `找不到 Rust 引擎可执行文件 ${ENGINE_EXE}。\n已查找:\n  ${searched}\n` +
          '先在插件目录执行 npm run build:engine(需要 Rust 工具链),或用 DSH_SCREEN_CONTROL_ENGINE 指定路径。',
      )
    }

    // 限流:短时间内反复重启说明二进制或环境有问题,直接报错而不是死循环拉起。
    const now = Date.now()
    this.#restartTimes = this.#restartTimes.filter((t) => now - t < RESTART_WINDOW_MS)
    if (this.#restartTimes.length >= MAX_RESTARTS_PER_WINDOW) {
      throw new EngineError(
        'ENGINE_ERROR',
        `引擎在 ${RESTART_WINDOW_MS / 1000}s 内已重启 ${this.#restartTimes.length} 次,暂停自动重启。stderr 尾部:\n${this.#stderrTail.join('\n')}`,
      )
    }
    this.#restartTimes.push(now)

    const pipePath = `${PIPE_PREFIX}dsh-screen-control-${process.pid}-${randomUUID().slice(0, 8)}`
    this.#stderrTail = []
    this.#logger.info(`[screen-control] 启动引擎: ${exe} --pipe ${pipePath}`)

    const child = spawnEngineProcess(exe, pipePath)
    this.#child = child

    child.stdout?.setEncoding('utf8')
    child.stdout?.on('data', (chunk: string) => {
      for (const line of chunk.split('\n')) {
        if (line.trim()) this.#logger.info(`[screen-control][engine] ${line.trim()}`)
      }
    })
    child.stderr?.setEncoding('utf8')
    child.stderr?.on('data', (chunk: string) => {
      for (const line of chunk.split('\n')) {
        const text = line.trim()
        if (!text) continue
        this.#pushStderr(text)
        this.#logger.warn(`[screen-control][engine:err] ${text}`)
      }
    })
    child.on('error', (error) => {
      this.#lastError = `spawn failed: ${error.message}`
      this.#logger.warn(`[screen-control] 引擎启动失败: ${error.message}`)
    })
    child.on('exit', (code, signal) => {
      const reason = `引擎进程退出(code=${code}, signal=${signal})`
      this.#lastError = reason
      this.#logger.warn(`[screen-control] ${reason}`)
      this.#child = null
      this.#socket?.destroy()
      this.#socket = null
      this.#failAllPending(new EngineError('ENGINE_ERROR', `${reason}。stderr 尾部:\n${this.#stderrTail.join('\n')}`))
    })

    const socket = await this.#connectWithRetry(pipePath, child)
    this.#socket = socket
    this.#attachSocket(socket)
    this.#logger.info('[screen-control] 引擎已连接')
  }

  async #connectWithRetry(pipePath: string, child: ChildProcess): Promise<Socket> {
    const deadline = Date.now() + CONNECT_TIMEOUT_MS
    let lastError: Error | null = null
    while (Date.now() < deadline) {
      if (child.exitCode !== null) {
        throw new EngineError(
          'ENGINE_ERROR',
          `引擎进程在管道就绪前退出(code=${child.exitCode})。stderr 尾部:\n${this.#stderrTail.join('\n')}`,
        )
      }
      try {
        return await connectOnce(pipePath, 1_000)
      } catch (error) {
        lastError = error as Error
        await delay(CONNECT_RETRY_MS)
      }
    }
    throw new EngineError(
      'ENGINE_ERROR',
      `连接命名管道超时(${CONNECT_TIMEOUT_MS}ms): ${lastError?.message ?? '未知原因'}`,
    )
  }

  #attachSocket(socket: Socket): void {
    socket.setEncoding('utf8')
    socket.on('data', (chunk: string) => {
      this.#buffer += chunk
      let index = this.#buffer.indexOf('\n')
      while (index >= 0) {
        const line = this.#buffer.slice(0, index).trim()
        this.#buffer = this.#buffer.slice(index + 1)
        if (line) this.#handleLine(line)
        index = this.#buffer.indexOf('\n')
      }
    })
    socket.on('error', (error) => {
      this.#lastError = `pipe error: ${error.message}`
      this.#logger.warn(`[screen-control] 管道错误: ${error.message}`)
    })
    socket.on('close', () => {
      if (this.#socket === socket) this.#socket = null
      this.#failAllPending(new EngineError('ENGINE_ERROR', '引擎管道已关闭'))
    })
  }

  #handleLine(line: string): void {
    let frame: unknown
    try {
      frame = JSON.parse(line)
    } catch {
      this.#logger.info(`[screen-control][engine] ${line}`)
      return
    }
    if (frame === null || typeof frame !== 'object') return
    const record = frame as Record<string, unknown>

    // 事件帧:无 id,只记日志。
    if (typeof record.event === 'string') {
      const message = typeof record.message === 'string' ? record.message : line
      this.#logger.info(`[screen-control][engine:${String(record.level ?? 'log')}] ${message}`)
      return
    }

    const id = typeof record.id === 'number' ? record.id : null
    if (id === null) return
    const pending = this.#pending.get(id)
    if (!pending) return

    const response = frame as RpcResponse
    if (response.ok) {
      pending.resolve(response.result ?? null)
      return
    }
    const code = response.error?.code ?? 'ENGINE_ERROR'
    const message = response.error?.message ?? '引擎返回了未知错误'
    pending.reject(new EngineError(code, message))
  }

  #failAllPending(error: Error): void {
    if (this.#pending.size === 0) return
    const pending = [...this.#pending.values()]
    this.#pending.clear()
    for (const entry of pending) {
      clearTimeout(entry.timer)
      entry.reject(error)
    }
  }

  #pushStderr(line: string): void {
    this.#stderrTail.push(line)
    if (this.#stderrTail.length > STDERR_TAIL_LINES) this.#stderrTail.shift()
  }
}

/**
 * 拉起引擎进程。
 *
 * 路径以 `.js` / `.mjs` 结尾时用当前 Node 解释器执行 —— 这样在没有 Rust 工具链的机器上
 * 也能用 `scripts/mock-engine.mjs` 把桥层和协议帧真正跑一遍(见 npm run selfcheck 阶段二 A)。
 * 正常安装时这里是一个 exe,直接 spawn。
 */
function spawnEngineProcess(exe: string, pipePath: string): ChildProcess {
  const lower = exe.toLowerCase()
  if (lower.endsWith('.mjs') || lower.endsWith('.js')) {
    return spawn(process.execPath, [exe, '--pipe', pipePath], {
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    })
  }
  return spawn(exe, ['--pipe', pipePath], {
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
  })
}

/** 连接一次命名管道,失败即抛,由调用方决定是否重试。 */function connectOnce(pipePath: string, timeoutMs: number): Promise<Socket> {
  return new Promise<Socket>((resolve, reject) => {
    const socket = connect(pipePath)
    const cleanup = () => {
      clearTimeout(timer)
      socket.removeListener('error', onError)
      socket.removeListener('connect', onConnect)
    }
    const onError = (error: Error) => {
      cleanup()
      socket.destroy()
      reject(error)
    }
    const onConnect = () => {
      cleanup()
      resolve(socket)
    }
    const timer = setTimeout(() => {
      cleanup()
      socket.destroy()
      reject(new Error('连接超时'))
    }, timeoutMs)
    socket.once('error', onError)
    socket.once('connect', onConnect)
  })
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}
