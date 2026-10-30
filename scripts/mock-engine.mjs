#!/usr/bin/env node
/**
 * mock-engine.mjs —— 假的 Rust 引擎,只实现 IPC 协议,不做任何 Win32 调用。
 *
 * 用途:在本机没有 Rust 工具链、真引擎编译不出来的时候,把 **TS 桥层 + 协议帧**
 * 这一段真正跑通(不是"看着像对的",是真的收发了一轮 NDJSON)。
 *
 * 用法(由 bridge 自动拉起,也可以手工调):
 *   node scripts/mock-engine.mjs --pipe \\.\pipe\dsh-screen-control-test
 *   set DSH_SCREEN_CONTROL_ENGINE=<本文件绝对路径>
 *
 * 它**不能**替代真引擎:所有窗口数据都是编的。selfcheck 里单独标成"阶段二 A(mock)",
 * 真引擎那一档叫"阶段二 B",两者不混。
 */

import net from 'node:net'

const argv = process.argv.slice(2)
const pipeIndex = argv.indexOf('--pipe')
const pipePath = pipeIndex >= 0 ? argv[pipeIndex + 1] : null
const once = argv.includes('--once')

if (!pipePath) {
  console.error('mock-engine: 缺少 --pipe <完整管道路径>')
  process.exit(2)
}

const startedAt = Date.now()

const MOCK_WINDOWS = [
  {
    hwnd: 1001,
    title: 'mock-记事本.txt - 记事本',
    class_name: 'Notepad',
    pid: 4321,
    process_name: 'notepad.exe',
    rect: { x: 100, y: 120, w: 900, h: 640 },
    client_rect: { x: 108, y: 168, w: 884, h: 584 },
    z_order: 0,
    is_visible: true,
    is_minimized: false,
    is_foreground: true,
  },
  {
    hwnd: 1002,
    title: '计算器',
    class_name: 'ApplicationFrameWindow',
    pid: 4322,
    process_name: 'Calculator.exe',
    rect: { x: 1100, y: 200, w: 360, h: 560 },
    client_rect: { x: 1100, y: 232, w: 360, h: 528 },
    z_order: 1,
    is_visible: true,
    is_minimized: false,
    is_foreground: false,
  },
]

const MOCK_ELEMENTS = [
  { uid: 1, name: '七', control_type: 'Button', rect: { x: 1150, y: 400, w: 60, h: 60 }, is_enabled: true, is_password: false, depth: 2, source: 'child_window' },
  { uid: 2, name: '加', control_type: 'Button', rect: { x: 1215, y: 400, w: 60, h: 60 }, is_enabled: true, is_password: false, depth: 2, source: 'child_window' },
  { uid: 3, name: '一', control_type: 'Button', rect: { x: 1150, y: 465, w: 60, h: 60 }, is_enabled: true, is_password: false, depth: 2, source: 'child_window' },
  { uid: 4, name: '等于', control_type: 'Button', rect: { x: 1280, y: 465, w: 60, h: 60 }, is_enabled: true, is_password: false, depth: 2, source: 'child_window' },
  { uid: 5, name: '结果', control_type: 'Text', rect: { x: 1120, y: 300, w: 300, h: 40 }, is_enabled: true, is_password: false, depth: 1, source: 'child_window' },
]

function makeResult(method, params) {
  switch (method) {
    case 'engine_ping':
      return {
        ok: true,
        version: 'mock-0.1.0',
        pid: process.pid,
        dpi_aware: 'per_monitor_aware_v2',
        monitor_count: 1,
        monitors: [{ index: 0, rect: { x: 0, y: 0, w: 1920, h: 1080 }, is_primary: true, dpi: 96 }],
        uptime_ms: Date.now() - startedAt,
      }

    case 'win_list_windows': {
      const filter = params.filter ?? {}
      let windows = MOCK_WINDOWS
      if (typeof filter.title_contains === 'string') {
        const needle = filter.title_contains.toLowerCase()
        windows = windows.filter((w) => w.title.toLowerCase().includes(needle))
      }
      if (typeof filter.class_name === 'string') {
        windows = windows.filter((w) => w.class_name === filter.class_name)
      }
      if (typeof filter.pid === 'number') windows = windows.filter((w) => w.pid === filter.pid)
      if (typeof filter.process_name === 'string') {
        // 与 Rust 侧 name_matches 同义：脱掉 .exe 后整名比较，大小写不敏感，不做子串。
        const base = (s) => String(s).toLowerCase().replace(/\.exe$/, '')
        windows = windows.filter((w) => base(w.process_name) === base(filter.process_name))
      }
      const limit = typeof params.limit === 'number' ? params.limit : 50
      return { windows: windows.slice(0, limit) }
    }

    case 'win_capture':
      return {
        path: 'C:\\Users\\mock\\AppData\\Local\\Temp\\dsh-screen-control\\shot-1.png',
        width: 1600,
        height: 900,
        bytes: 123456,
        backend: 'mock',
        capture_ms: 12,
        scaled_from: { w: 2560, h: 1440 },
      }

    case 'win_observe': {
      const ttl = typeof params.snapshot_ttl_ms === 'number' ? params.snapshot_ttl_ms : 5000
      const now = Date.now()
      return {
        snapshot_id: 1,
        created_at_ms: now,
        expires_at_ms: now + ttl,
        hwnd: params.hwnd ?? 1002,
        element_count: MOCK_ELEMENTS.length,
        truncated: false,
        elements: MOCK_ELEMENTS,
        screenshot: {
          path: 'C:\\Users\\mock\\AppData\\Local\\Temp\\dsh-screen-control\\shot-2.png',
          width: 800,
          height: 600,
          bytes: 54321,
          backend: 'mock',
          capture_ms: 9,
          scaled_from: null,
        },
      }
    }

    case 'win_find': {
      // 0.1.9 的 win_find 形状：命中/未命中都是正常结果，都带最佳候选位置与得分。
      const region = params.region ?? { x: 0, y: 0, w: 1920, h: 1080 }
      const found = params.min_score === undefined || params.min_score <= 0.998
      return {
        found,
        score: found ? 0.998 : 0.412,
        x: region.x + 42,
        y: region.y + 24,
        w: 60,
        h: 40,
        cx: region.x + 72,
        cy: region.y + 44,
        origin: { x: region.x, y: region.y },
        searched_region: region,
        elapsed_ms: 23,
        warnings: [],
      }
    }

    case 'win_click':
      return {
        delivered: 'postmessage',
        target_hwnd: params.hwnd ?? 1002,
        deepest_child: 99887,
        coords_client: { x: 12, y: 30 },
        sequence_ms: 3,
        warnings: [],
      }

    case 'win_type':
      return {
        delivered: 'message',
        target_hwnd: params.hwnd ?? 1002,
        deepest_child: 99888,
        coords_client: { x: 0, y: 0 },
        sequence_ms: 7,
        warnings: [],
      }

    case 'win_launch':
      return {
        pid: 55221,
        path: params.path ?? 'notepad.exe',
        args: params.args ?? [],
        still_running: true,
        checked_after_ms: 30,
        warnings: [],
      }

    case 'win_window_state':
      return {
        hwnd: params.hwnd ?? 1002,
        action: params.action ?? 'maximize',
        is_visible: true,
        is_minimized: false,
        is_maximized: true,
        is_fullscreen: false,
        is_topmost: false,
        rect: { x: 1100, y: 200, w: 360, h: 560 },
        style: 0x00cf0000,
        ex_style: 0,
        warnings: [],
      }

    case 'win_drag':
      return {
        delivered: 'sendinput',
        target_hwnd: params.hwnd ?? 1002,
        from_screen: { x: params.from_x ?? 100, y: params.from_y ?? 120 },
        to_screen: { x: params.to_x ?? 200, y: params.to_y ?? 220 },
        steps: params.steps ?? 24,
        duration_ms: params.duration_ms ?? 400,
        distance_px: 141.4,
        effect: null,
        warnings: [],
      }

    case 'win_drag_path':
      return {
        delivered: 'sendinput',
        target_hwnd: params.hwnd ?? 1002,
        strokes: 1,
        points: 120,
        planned_ms: 300,
        duration_ms: 312,
        path_px: 250.5,
        max_step_px: 2.0,
        start_screen: { x: 380, y: 400 },
        end_screen: { x: 380, y: 400 },
        timer_1ms: true,
        timed: false,
        effect: null,
        warnings: [],
      }

    case 'win_record':
      return {
        duration_ms: 1000,
        samples: 120,
        hz: 120,
        strokes: [{ points: [[1, 2], [3, 4]], times_ms: [0, 20] }],
        total_px: 2.828,
        warnings: [],
      }

    case 'win_wait':
      return {
        condition: params.condition ?? 'window',
        matched: false,
        timed_out: true,
        waited_ms: 20,
        polls: 2,
        timeout_ms: 20,
        warnings: [],
      }

    case 'win_ocr':
      return {
        text: 'mock 识别文本',
        lines: [{ text: 'mock', rect: { x: 0, y: 0, w: 40, h: 20 } }],
        language: 'zh-Hans-CN',
        available_languages: ['zh-Hans-CN'],
        origin: { x: 0, y: 0 },
        width: 200,
        height: 100,
        ocr_ms: 5,
        capture_ms: 3,
        backend: 'mock',
      }

    case 'win_web_cdp':
      return {
        host: params.host ?? '127.0.0.1',
        port: params.port ?? 9222,
        ws_url: 'ws://127.0.0.1:9222/devtools/page/mock',
        id: 1,
        result: { title: 'mock' },
        raw: { id: 1, result: { title: 'mock' } },
        events: [],
        elapsed_ms: 6,
      }

    case 'win_image_trace':
      return {
        strokes: [{ points: [[0, 0], [10, 10]] }],
        strokes_total: 1,
        points_total: 2,
        source: { width: 100, height: 100 },
        elapsed_ms: 30,
        warnings: [],
      }

    case 'win_press':
    case 'win_scroll':
      return {
        delivered: 'postmessage',
        target_hwnd: params.hwnd ?? 1002,
        deepest_child: 99889,
        coords_client: { x: 0, y: 0 },
        sequence_ms: 2,
        warnings: [],
      }

    default:
      return { __error: { code: 'INVALID_ARGS', message: `mock 不支持的方法: ${method}` } }
  }
}

function handleLine(socket, line) {
  let request
  try {
    request = JSON.parse(line)
  } catch {
    socket.write(`${JSON.stringify({ id: 0, ok: false, error: { code: 'INVALID_ARGS', message: '不是合法 JSON' } })}\n`)
    return
  }
  const id = typeof request.id === 'number' ? request.id : 0
  const method = String(request.method ?? '')
  const params = request.params && typeof request.params === 'object' ? request.params : {}
  if (method === 'engine_ping') {
    // 事件帧:没有 id,TS 侧只记日志,不回。
    socket.write(`${JSON.stringify({ event: 'log', level: 'info', message: 'mock engine received ping' })}\n`)
  }
  const result = makeResult(method, params)
  if (result.__error) {
    socket.write(`${JSON.stringify({ id, ok: false, error: result.__error })}\n`)
    return
  }
  socket.write(`${JSON.stringify({ id, ok: true, result })}\n`)
}

const server = net.createServer((socket) => {
  socket.setEncoding('utf8')
  let buffer = ''
  socket.on('data', (chunk) => {
    buffer += chunk
    let index = buffer.indexOf('\n')
    while (index >= 0) {
      const line = buffer.slice(0, index).trim()
      buffer = buffer.slice(index + 1)
      if (line) handleLine(socket, line)
      index = buffer.indexOf('\n')
    }
  })
  socket.on('error', () => {})
  if (once) socket.on('close', () => server.close(() => process.exit(0)))
})

server.on('error', (error) => {
  console.error(`mock-engine: 管道错误 ${error.message}`)
  process.exit(1)
})

server.listen(pipePath, () => {
  console.log(`mock-engine: listening on ${pipePath} (pid=${process.pid})`)
})
