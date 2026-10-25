#!/usr/bin/env node
/**
 * selfcheck.mjs —— 独立验证脚本(不是"看一眼觉得没问题",是真的跑)。
 *
 * 分四个阶段:
 *   阶段一 插件契约   :用假 Context 加载 lib/index.js,确认 18 个工具全部注册、契约完整。
 *   阶段二 A 工具闭环 :把引擎指向 scripts/mock-engine.mjs,**走真实工具路径**
 *                      (tool.execute → bridge → 命名管道 → 引擎 → 回包 → 解析),
 *                      验证参数转换、协议帧、错误码透传。不需要 Rust。
 *   阶段二 B 真引擎   :spawn Rust 引擎打一次 engine_ping + win_list_windows。
 *                      引擎没编译出来时明确报 SKIP —— 不伪装成通过。
 *   阶段三 卸载清理   :调用所有 disposer,确认工具全部注销、不抛异常。
 *
 * 用法:node scripts/selfcheck.mjs
 * 退出码:0 全部通过(允许有 SKIP);1 有失败项。
 */

import { existsSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const ROOT = path.resolve(HERE, '..')
const MOCK_ENGINE = path.join(ROOT, 'scripts', 'mock-engine.mjs')

const EXPECTED_TOOLS = [
  'win_engine_status',
  'win_list_windows',
  'win_capture',
  'win_observe',
  'win_click',
  'win_type',
  'win_press',
  'win_scroll',
  // 0.1.6 加的 6 个：当时漏了同步这张表，于是 selfcheck 长期挂一条假红
  // （"注册了 8 个工具 — 实际 14"）。加/删工具时这里必须一起改。
  'win_launch',
  'win_window_state',
  'win_drag',
  'win_wait',
  'win_ocr',
  'win_web_cdp',
  // 0.1.9 新增：一次性屏幕定位（不轮询；要等出现用 win_wait）。
  'win_find',
  // 0.2.0 新增：一次按下走完整条路径（圆/弧/贝塞尔/任意折线），画弧线的唯一正确工具。
  'win_drag_path',
  // 0.2.3 新增：图片 → 笔顺（二值化/细化/提笔顺/简化/重采样），配 shot_id 可直接画。
  'win_image_trace',
  // 0.2.4 新增：录制人手鼠标轨迹（按住左键的段），配 win_drag_path.times_ms 原速回放。
  'win_record',
]

let failed = 0
let skipped = 0

const pass = (name, detail = '') => console.log(`  PASS  ${name}${detail ? ` — ${detail}` : ''}`)
const fail = (name, detail) => {
  failed += 1
  console.log(`  FAIL  ${name} — ${detail}`)
}
const skip = (name, detail) => {
  skipped += 1
  console.log(`  SKIP  ${name} — ${detail}`)
}
const check = (name, ok, detail = '') => (ok ? pass(name, detail) : fail(name, detail))

// ---------------------------------------------------------------------------
// 阶段一:插件契约
// ---------------------------------------------------------------------------

console.log('=== 阶段一:插件契约 ===')

const entry = path.join(ROOT, 'lib', 'index.js')
if (!existsSync(entry)) {
  fail('lib/index.js 存在', '先跑 npm run build')
  console.log(`\n结果:${failed} 项失败`)
  process.exit(1)
}
pass('lib/index.js 存在')

// 引擎指向 mock:阶段二 A 要打真实的工具→桥→管道链路,但这里没有 Rust 引擎。
const envBefore = process.env.DSH_SCREEN_CONTROL_ENGINE
process.env.DSH_SCREEN_CONTROL_ENGINE = MOCK_ENGINE

const mod = await import(pathToFileURL(entry).href)
check('插件 name = screen-control', mod.name === 'screen-control', `实际 ${String(mod.name)}`)
check(
  "inject = ['tools']",
  Array.isArray(mod.inject) && mod.inject.length === 1 && mod.inject[0] === 'tools',
  `实际 ${JSON.stringify(mod.inject)}`,
)

const registered = new Map()
const disposers = []
const pluginLogs = []
const fakeCtx = {
  logger: () => ({
    info: (message) => pluginLogs.push(String(message)),
    warn: (message) => pluginLogs.push(`WARN ${String(message)}`),
    debug: () => {},
  }),
  effect: (fn, label) => {
    const disposer = fn()
    disposers.push({ label, disposer })
    return disposer
  },
  tools: {
    register: (definition) => {
      registered.set(definition.name, definition)
      return () => registered.delete(definition.name)
    },
  },
}

try {
  mod.apply(fakeCtx)
  pass('apply(fakeCtx) 未抛异常')
} catch (error) {
  fail('apply(fakeCtx) 未抛异常', error.stack ?? String(error))
}

check(
  `注册了 ${EXPECTED_TOOLS.length} 个工具`,
  registered.size === EXPECTED_TOOLS.length,
  `实际 ${registered.size}: ${[...registered.keys()].join(', ')}`,
)

for (const toolName of EXPECTED_TOOLS) {
  const definition = registered.get(toolName)
  if (!definition) {
    fail(`工具 ${toolName} 已注册`, '缺失')
    continue
  }
  const problems = []
  if (typeof definition.description !== 'string' || definition.description.length < 20) {
    problems.push('description 太短')
  }
  if (definition.parameters === null || typeof definition.parameters !== 'object') problems.push('缺 parameters')
  if (!definition.output?.schema) problems.push('缺 output.schema')
  if (typeof definition.output?.render !== 'function') problems.push('缺 output.render')
  if (typeof definition.execute !== 'function') problems.push('缺 execute')

  // render 必须返回 ContentBlock[]:至少一个 type='text' 的块。
  if (typeof definition.output?.render === 'function') {
    const blocks = definition.output.render({}, { probe: true })
    if (!Array.isArray(blocks) || blocks.length === 0 || blocks[0]?.type !== 'text') {
      problems.push('render 没返回 text block')
    }
  }
  check(`工具 ${toolName} 契约完整`, problems.length === 0, problems.join('; '))
}

// ---------------------------------------------------------------------------
// 阶段二 A:走真实工具路径的闭环(mock 引擎)
// ---------------------------------------------------------------------------

console.log('\n=== 阶段二 A:工具闭环(mock 引擎,不需要 Rust)===')
console.log('  走的是 tool.execute → bridge → 命名管道 → 引擎 → 回包,和真引擎一模一样,只是引擎是假的。')

const abortSignal = new AbortController().signal
const runTool = async (toolName, args) => {
  const definition = registered.get(toolName)
  if (!definition) throw new Error(`工具 ${toolName} 没注册`)
  return await definition.execute(args, { signal: abortSignal })
}

try {
  const status = await runTool('win_engine_status', {})
  check('win_engine_status 打通引擎', status?.ok === true && status?.version === 'mock-0.1.0', `pid=${status?.pid} version=${status?.version}`)
  check('win_engine_status 附带桥诊断', status?.bridge?.running === true, `engine_path=${status?.bridge?.engine_path}`)

  const all = await runTool('win_list_windows', {})
  check('win_list_windows 不带过滤返回全部', all?.windows?.length === 2, `拿到 ${all?.windows?.length} 个`)

  const filtered = await runTool('win_list_windows', { title_contains: '计算器' })
  check(
    '工具的平铺参数被正确转成协议里的 filter 对象',
    filtered?.windows?.length === 1 && filtered.windows[0]?.hwnd === 1002,
    `命中 ${filtered?.windows?.length} 个`,
  )

  const byPid = await runTool('win_list_windows', { pid: 4321 })
  check('pid 过滤生效', byPid?.windows?.length === 1 && byPid.windows[0]?.hwnd === 1001, `命中 ${byPid?.windows?.length} 个`)

  const byName = await runTool('win_list_windows', { process_name: 'Calculator' })
  check(
    'process_name 过滤生效(写不带 .exe 的名字也认)',
    byName?.windows?.length === 1 && byName.windows[0]?.hwnd === 1002,
    `命中 ${byName?.windows?.length} 个`,
  )

  const noSubstring = await runTool('win_list_windows', { process_name: 'Calc' })
  check('process_name 不做子串匹配(Calc 不该命中 Calculator.exe)', noSubstring?.windows?.length === 0, `命中 ${noSubstring?.windows?.length} 个`)

  const observed = await runTool('win_observe', { hwnd: 1002 })
  check(
    'win_observe 返回快照 + 元素树',
    typeof observed?.snapshot_id === 'number' && Array.isArray(observed?.elements) && observed.elements.length === 5,
    `snapshot=${observed?.snapshot_id} 元素=${observed?.element_count} 过期=${observed?.expires_at_ms - observed?.created_at_ms}ms`,
  )
  check(
    'win_observe 元素带 uid/name/rect',
    observed?.elements?.every((el) => typeof el.uid === 'number' && typeof el.name === 'string' && typeof el.rect?.w === 'number'),
  )

  const captured = await runTool('win_capture', { hwnd: 1002, max_dim: 800 })
  check('win_capture 返回 PNG 路径而非 Base64', typeof captured?.path === 'string' && captured.path.endsWith('.png') && typeof captured.bytes === 'number', `${captured?.width}x${captured?.height} ${captured?.bytes} 字节`)

  // win_find（0.1.9）：工具层 → 桥层 → mock 引擎这条链要通，且 found=false 不能当错误抛。
  const found = await runTool('win_find', {
    template: 'C:\\Users\\mock\\templates\\button.png',
    hwnd: 1002,
    region: { x: 100, y: 200, w: 300, h: 200 },
  })
  check(
    'win_find 命中时返回 found/score/x/y/w/h/cx/cy/origin',
    found?.found === true &&
      typeof found?.score === 'number' &&
      found?.origin?.x === 100 &&
      found?.origin?.y === 200 &&
      found?.x === 142 &&
      found?.cx === 172 &&
      found?.w === 60,
    `found=${found?.found} score=${found?.score} at=(${found?.x},${found?.y})`,
  )

  const missed = await runTool('win_find', { template: 'C:\\Users\\mock\\templates\\button.png', min_score: 1.0 })
  check(
    'win_find 未命中时也返回结果（found=false 不是 RPC 错误）',
    missed?.found === false && typeof missed?.score === 'number' && missed.score < 1,
    `found=${missed?.found} score=${missed?.score}`,
  )

  const clicked = await runTool('win_click', { snapshot_id: 1, uid: 1 })
  check('win_click 走通并回报投递方式', clicked?.delivered === 'postmessage' && clicked?.target_hwnd === 1002, `delivered=${clicked?.delivered}`)

  const typed = await runTool('win_type', { hwnd: 1002, text: '1+1' })
  check('win_type 走通', typed?.delivered === 'message', `delivered=${typed?.delivered}`)

  const pressed = await runTool('win_press', { hwnd: 1002, key: 'Enter' })
  check('win_press 走通', typeof pressed?.delivered === 'string', `delivered=${pressed?.delivered}`)

  const scrolled = await runTool('win_scroll', { hwnd: 1002, delta: -360 })
  check('win_scroll 走通', typeof scrolled?.delivered === 'string', `delivered=${scrolled?.delivered}`)

  check('引擎 stdout/事件帧被转发到插件日志', pluginLogs.some((line) => line.includes('mock engine received ping') || line.includes('mock-engine: listening on')))

  // render 出来的文本必须是模型真能读到的 JSON,不能是空壳。
  const blocks = registered.get('win_observe').output.render({ hwnd: 1002 }, observed)
  check('render 把结果变成可读文本', blocks[0]?.type === 'text' && blocks[0].text.includes('snapshot_id'), `${blocks[0]?.text?.length} 字符`)
} catch (error) {
  fail('mock 引擎下的工具闭环', error.stack ?? String(error))
}

// 错误码透传:引擎返回的 code 必须原样变成 EngineError.code,不能被吞成通用错误。
try {
  const bridgeModule = await import(pathToFileURL(path.join(ROOT, 'lib', 'bridge.js')).href)
  const probe = new bridgeModule.EngineBridge({ info: () => {}, warn: () => {} })
  // 注意:这里必须显式构造协议形状(嵌套 filter),因为桥层是"哑管道",
  // 平铺→嵌套的转换属于工具层,已经在上面验过了。
  let errorCode = null
  let errorName = null
  try {
    await probe.call('no_such_method', {}, { timeoutMs: 5_000 })
  } catch (error) {
    errorCode = error.code ?? null
    errorName = error.name
  }
  check('错误码透传为 EngineError.code', errorCode === 'INVALID_ARGS' && errorName === 'EngineError', `实际 code=${errorCode} name=${errorName}`)
  probe.dispose()
} catch (error) {
  fail('错误码透传', error.stack ?? String(error))
}

// ---------------------------------------------------------------------------
// 阶段二 B:真 Rust 引擎
// ---------------------------------------------------------------------------

console.log('\n=== 阶段二 B:真 Rust 引擎 ===')

if (envBefore === undefined) delete process.env.DSH_SCREEN_CONTROL_ENGINE
else process.env.DSH_SCREEN_CONTROL_ENGINE = envBefore

const bridgeModule = await import(pathToFileURL(path.join(ROOT, 'lib', 'bridge.js')).href)
const enginePath = bridgeModule.locateEngine()

if (!enginePath) {
  skip('真引擎 IPC 闭环', '引擎未编译。已查找:')
  for (const candidate of bridgeModule.engineCandidates()) console.log(`          ${candidate}`)
  console.log('        编译方式:npm run build:engine(见 README「编译引擎」)')

  // 引擎缺失时必须给出可读错误,而不是崩溃或空等 —— 这一条现在就能验。
  const bare = new bridgeModule.EngineBridge({ info: () => {}, warn: () => {} })
  try {
    await bare.call('engine_ping', {}, { timeoutMs: 5_000 })
    fail('引擎缺失时给出可读错误', '居然成功了?说明环境里有别的引擎在应答')
  } catch (error) {
    check('引擎缺失时给出可读错误', error.name === 'EngineError' && /找不到 Rust 引擎/.test(error.message), error.message.split('\n')[0])
  } finally {
    bare.dispose()
  }
} else {
  pass('找到真引擎可执行文件', enginePath)
  const bridge = new bridgeModule.EngineBridge({
    info: (message) => console.log(`  [engine] ${message}`),
    warn: (message) => console.log(`  [engine:warn] ${message}`),
  })
  try {
    const ping = await bridge.call('engine_ping', {}, { timeoutMs: 15_000 })
    check('engine_ping 往返成功', ping?.ok === true, `pid=${ping?.pid} monitors=${ping?.monitor_count}`)

    const listed = await bridge.call('win_list_windows', { limit: 5 }, { timeoutMs: 15_000 })
    const windows = Array.isArray(listed?.windows) ? listed.windows : []
    check('win_list_windows 返回窗口列表', windows.length > 0, `拿到 ${windows.length} 个窗口`)
    for (const win of windows.slice(0, 3)) {
      console.log(`          hwnd=${win.hwnd} pid=${win.pid} "${win.title}" ${win.class_name}`)
    }
  } catch (error) {
    fail('真引擎 IPC 调用', error.message)
  } finally {
    bridge.dispose()
    pass('真引擎桥已关闭')
  }
}

// ---------------------------------------------------------------------------
// 阶段三:卸载清理
// ---------------------------------------------------------------------------

console.log('\n=== 阶段三:卸载清理 ===')
// disposer 抛异常会让整个 profile 卸载失败,所以必须真调一遍。
// 这一步同时会杀掉阶段二 A 留下的 mock 引擎子进程。
try {
  for (const { disposer } of [...disposers].reverse()) {
    if (typeof disposer === 'function') disposer()
  }
  pass('所有 disposer 调用未抛异常')
} catch (error) {
  fail('所有 disposer 调用未抛异常', error.stack ?? String(error))
}
check('卸载后工具全部注销', registered.size === 0, `残留 ${registered.size} 个`)

console.log(`\n结果:${failed} 项失败,${skipped} 项跳过`)
process.exit(failed === 0 ? 0 : 1)
