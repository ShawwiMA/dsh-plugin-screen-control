#!/usr/bin/env node
/**
 * 真机验收：靶窗 + 真引擎（不经过 TS 工具层，直接说 IPC）。
 *
 * 为什么要单独写一个：`npm run selfcheck` 验的是"协议通不通"，这里验的是
 * **动作有没有真的落到目标控件上**。判据不是引擎自己的回报，而是靶窗
 * (scripts/testbed-window.exe) 自己写出来的状态文件——引擎说"我发了"，
 * 靶窗说"我收到了"，两边对上才算过。
 *
 * 覆盖 6 项：
 *   1. 找到靶窗
 *   2. observe 出来的元素树里有普通编辑框/密码框/按钮
 *   3. PostMessage 打字真的进了普通编辑框
 *   4. 密码框被硬拒（PASSWORD_FIELD），且内容长度仍然是 0
 *   5. 键盘 SendInput 闸门：目标不在前台时必须拒绝（回归：以前会打进用户窗口）
 *   6. 鼠标 SendInput 闸门：点不在目标窗口上时必须拒绝（该路径根本不会真的点）
 *   7. 100 轮 observe/capture 之后引擎句柄数不增长
 *
 * 用法: node scripts/acceptance.mjs
 *       DSH_SCREEN_CONTROL_ENGINE=<exe> 可指定要验的引擎（默认 target/release）
 */

import { spawn, spawnSync, execFileSync } from 'node:child_process'
import { connect } from 'node:net'
import { existsSync, mkdirSync, readFileSync, rmSync } from 'node:fs'
import { randomUUID } from 'node:crypto'
import path from 'node:path'
import os from 'node:os'
import { fileURLToPath } from 'node:url'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const ROOT = path.resolve(HERE, '..')
const PIPE = `\\\\.\\pipe\\dsh-acceptance-${process.pid}-${randomUUID().slice(0, 8)}`
const TESTBED = path.join(HERE, 'testbed-window.exe')
const STATE_FILE = path.join(os.tmpdir(), `dsh-testbed-state-${process.pid}.txt`)
const WINDOW_CLASS = 'DshTestbedWindow'
const OBSERVE_ITERATIONS = 100
const CAPTURE_ITERATIONS = 20
const HANDLE_LEAK_TOLERANCE = 12

const results = []
const skips = []
let engineChild = null
let testbedChild = null

function check(name, ok, detail) {
  results.push({ name, ok: Boolean(ok), detail: detail ?? '' })
  console.log(`  ${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? ` — ${detail}` : ''}`)
}

const delay = (ms) => new Promise((r) => setTimeout(r, ms))

function locateEngine() {
  const fromEnv = process.env.DSH_SCREEN_CONTROL_ENGINE
  if (fromEnv && fromEnv.trim()) return fromEnv.trim()
  return path.join(ROOT, 'rust-engine', 'target', 'release', 'screen-control-engine.exe')
}

/** 极简 IPC 客户端：NDJSON，按 id 配对；事件帧单独收起来。 */
class EngineClient {
  #socket
  #buffer = ''
  #nextId = 1
  #pending = new Map()
  events = []

  constructor(socket) {
    this.#socket = socket
    socket.setEncoding('utf8')
    socket.on('data', (chunk) => {
      this.#buffer += chunk
      let index = this.#buffer.indexOf('\n')
      while (index >= 0) {
        const line = this.#buffer.slice(0, index).trim()
        this.#buffer = this.#buffer.slice(index + 1)
        if (line) this.#handleLine(line)
        index = this.#buffer.indexOf('\n')
      }
    })
  }

  #handleLine(line) {
    let frame
    try {
      frame = JSON.parse(line)
    } catch {
      return
    }
    if (frame && typeof frame.event === 'string') {
      this.events.push(frame)
      return
    }
    const pending = this.#pending.get(frame?.id)
    if (!pending) return
    this.#pending.delete(frame.id)
    if (frame.ok) pending.resolve(frame.result ?? null)
    else {
      const error = new Error(frame.error?.message ?? '引擎返回未知错误')
      error.code = frame.error?.code ?? 'ENGINE_ERROR'
      pending.reject(error)
    }
  }

  call(method, params = {}, timeoutMs = 20000) {
    const id = this.#nextId++
    this.#socket.write(`${JSON.stringify({ id, method, params })}\n`)
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id)
        reject(new Error(`${method} 超时`))
      }, timeoutMs)
      this.#pending.set(id, {
        resolve: (v) => {
          clearTimeout(timer)
          resolve(v)
        },
        reject: (e) => {
          clearTimeout(timer)
          reject(e)
        },
      })
    })
  }
}

async function connectPipe(pipePath, deadlineMs) {
  const deadline = Date.now() + deadlineMs
  let last = null
  while (Date.now() < deadline) {
    try {
      return await new Promise((resolve, reject) => {
        const socket = connect(pipePath)
        const timer = setTimeout(() => {
          socket.destroy()
          reject(new Error('连接超时'))
        }, 800)
        socket.once('connect', () => {
          clearTimeout(timer)
          resolve(socket)
        })
        socket.once('error', (error) => {
          clearTimeout(timer)
          socket.destroy()
          reject(error)
        })
      })
    } catch (error) {
      last = error
      await delay(50)
    }
  }
  throw new Error(`连不上管道: ${last?.message ?? '未知'}`)
}

function readState() {
  if (!existsSync(STATE_FILE)) return null
  const out = {}
  for (const line of readFileSync(STATE_FILE, 'utf8').split(/\r?\n/)) {
    const index = line.indexOf('=')
    if (index > 0) out[line.slice(0, index)] = line.slice(index + 1)
  }
  return out
}

async function waitForState(predicate, timeoutMs) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const state = readState()
    if (state && predicate(state)) return state
    await delay(60)
  }
  return readState()
}

function handleCount(pid) {
  try {
    const out = execFileSync(
      'powershell',
      ['-NoProfile', '-NonInteractive', '-Command', `(Get-Process -Id ${pid}).Handles`],
      { encoding: 'utf8', windowsHide: true },
    )
    const value = Number.parseInt(out.trim(), 10)
    return Number.isFinite(value) ? value : null
  } catch {
    return null
  }
}

function pick(elements, predicate) {
  return elements.find(predicate) ?? null
}

/** 当前会话是否已提权(High IL)。提权后引擎能真的把靶窗抢到前台，见第 5 节。 */
function isElevated() {
  try {
    const out = execFileSync(
      'powershell',
      [
        '-NoProfile',
        '-NonInteractive',
        '-Command',
        '([Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)',
      ],
      { encoding: 'utf8', windowsHide: true },
    )
    return out.trim().toLowerCase() === 'true'
  } catch {
    return false
  }
}

async function main() {
  const engineExe = locateEngine()
  console.log('=== 真机验收：靶窗 + 真引擎 ===')
  console.log(`  引擎: ${engineExe}`)
  console.log(`  靶窗: ${TESTBED}`)
  if (!existsSync(engineExe)) throw new Error(`找不到引擎: ${engineExe}`)
  if (!existsSync(TESTBED)) {
    throw new Error(`找不到靶窗，先编译: gcc -O2 -mwindows -o scripts/testbed-window.exe scripts/testbed-window.c -luser32 -lgdi32`)
  }

  // --- 起靶窗 -------------------------------------------------------------
  testbedChild = spawn(TESTBED, [STATE_FILE], { stdio: 'ignore', windowsHide: false })
  const state = await waitForState((s) => s.alive === '1', 5000)
  check('靶窗起来了并写出状态文件', state?.alive === '1', state ? `tick=${state.tick}` : '状态文件没出现')
  if (!state) return

  // --- 起引擎 -------------------------------------------------------------
  engineChild = spawn(engineExe, ['--pipe', PIPE], { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true })
  const stderrTail = []
  engineChild.stdout.setEncoding('utf8')
  engineChild.stdout.on('data', (c) => {
    for (const line of String(c).split('\n')) if (line.trim()) console.log(`    [engine] ${line.trim()}`)
  })
  engineChild.stderr.setEncoding('utf8')
  engineChild.stderr.on('data', (c) => {
    for (const line of String(c).split('\n')) {
      const text = line.trim()
      if (!text) continue
      stderrTail.push(text)
      if (stderrTail.length > 10) stderrTail.shift()
    }
  })

  const socket = await connectPipe(PIPE, 8000)
  const client = new EngineClient(socket)

  const ping = await client.call('engine_ping')
  check('引擎应答 ping', ping?.ok === true, `pid=${ping?.pid} dpi_aware=${ping?.dpi_aware}`)
  const monitor = ping?.monitors?.[0]
  check(
    'MonitorInfo.dpi 已填上真实值',
    monitor?.dpi === 96 || (typeof monitor?.dpi === 'number' && monitor.dpi > 0),
    `monitor.dpi=${monitor?.dpi} rect=${monitor?.rect?.w}x${monitor?.rect?.h}`,
  )

  // --- 1. 找到靶窗 --------------------------------------------------------
  const listed = await client.call('win_list_windows', {
    filter: { class_name: WINDOW_CLASS, visible_only: true, top_level_only: true },
    limit: 20,
  })
  const testbed = listed?.windows?.[0] ?? null
  check('win_list_windows 找到靶窗', testbed !== null, testbed ? `hwnd=${testbed.hwnd} pid=${testbed.pid}` : '没找到')
  if (!testbed) {
    const all = await client.call('win_list_windows', { limit: 12 })
    for (const w of all?.windows ?? []) console.log(`      候选: ${w.class_name} | ${JSON.stringify(w.title)} | pid=${w.pid}`)
    return
  }
  const hwnd = testbed.hwnd

  // --- 2. 元素树 ----------------------------------------------------------
  const observed = await client.call('win_observe', { hwnd, max_depth: 4, max_elements: 64 })
  const elements = observed?.elements ?? []
  const source = elements[0]?.source ?? '?'
  for (const e of elements) {
    console.log(
      `      uid=${e.uid} type=${e.control_type} pwd=${e.is_password} name=${JSON.stringify(e.name)}`,
    )
  }
  const normalEdit = pick(elements, (e) => e.control_type === 'Edit' && e.is_password === false)
  const passwordEdit = pick(elements, (e) => e.is_password === true)
  const button = pick(elements, (e) => e.control_type === 'Button')
  check('元素树里认出了普通编辑框', normalEdit !== null, `source=${source} 共 ${elements.length} 个元素`)
  check('元素树里认出了密码框(标记为 is_password)', passwordEdit !== null, passwordEdit ? `uid=${passwordEdit.uid}` : '没有')
  check('元素树里认出了按钮', button !== null, button ? `uid=${button.uid} name=${button.name}` : '没有')
  const snapshotId = observed?.snapshot_id

  // --- 2.5 win_find（0.1.9 新增：一次性模板匹配）--------------------------
  // 判据不靠"引擎说找到了"，而是拿刚截的靶窗整窗图当模板回搜同一个窗口：
  // 同一后端、同一时刻截的图，自匹配必须是 1.0，且位置必须是窗口左上角。
  const tplShot = await client.call('win_capture', { hwnd, max_dim: 0 })
  const selfFind = await client.call('win_find', { template: tplShot.path, hwnd })
  check(
    'win_find 自匹配命中（score≈1、位置=窗口左上角）',
    selfFind?.found === true &&
      selfFind.score > 0.99 &&
      Math.abs(selfFind.x - testbed.rect.x) <= 2 &&
      Math.abs(selfFind.y - testbed.rect.y) <= 2,
    `found=${selfFind?.found} score=${selfFind?.score} at=(${selfFind?.x},${selfFind?.y}) 窗口=(${testbed.rect.x},${testbed.rect.y}) ` +
      `模板=${selfFind?.w}x${selfFind?.h}`,
  )
  // 缩过图的模板 + 阈值 1.0：数学上不可能拿满分，所以一定走"没找到"分支。
  const scaledShot = await client.call('win_capture', { hwnd, max_dim: 300 })
  const missFind = await client.call('win_find', { template: scaledShot.path, hwnd, min_score: 1.0 })
  check(
    'win_find 没找到时返回 found=false + 最佳候选（不是 RPC 错误）',
    missFind?.found === false && typeof missFind.score === 'number' && missFind.score < 1,
    `found=${missFind?.found} score=${missFind?.score} at=(${missFind?.x},${missFind?.y})`,
  )
  // 模板比搜索区域还大：必须是结构化 INVALID_ARGS，而不是崩/挂。
  let bigTplError = null
  try {
    await client.call('win_find', {
      template: tplShot.path,
      region: { x: testbed.rect.x, y: testbed.rect.y, w: 32, h: 32 },
    })
  } catch (error) {
    bigTplError = error
  }
  check(
    'win_find 模板比区域大时返回 INVALID_ARGS',
    bigTplError?.code === 'INVALID_ARGS',
    `code=${bigTplError?.code} msg=${bigTplError?.message}`,
  )

  // --- 2.6 win_ocr（0.1.9：整条 OCR 路径在子进程里跑，引擎本体不许崩）------
  try {
    const ocr = await client.call('win_ocr', { hwnd }, 20000)
    const ocrText = String(ocr?.text ?? '').toUpperCase()
    check(
      'win_ocr 识别靶窗大字（走隔离子进程，引擎本体存活）',
      ocrText.includes('12345'),
      `language=${ocr?.language} ${ocr?.width}x${ocr?.height} text=${JSON.stringify(String(ocr?.text ?? '').trim().slice(0, 60))}`,
    )
    check('win_ocr 之后引擎进程仍然存活', engineChild.exitCode === null, `exitCode=${engineChild.exitCode}`)
  } catch (error) {
    if (error.code === 'UNSUPPORTED') {
      skips.push(`win_ocr 端到端（本机 Windows OCR 不可用：${error.message.slice(0, 80)}）`)
    } else {
      check('win_ocr 识别靶窗大字（走隔离子进程，引擎本体存活）', false, `code=${error.code} msg=${error.message}`)
    }
  }

  // --- 3. PostMessage 打字真的进控件 --------------------------------------
  const text = 'abc123中文'
  const typed = await client.call('win_type', {
    snapshot_id: snapshotId,
    uid: normalEdit?.uid,
    text,
    method: 'postmessage',
  })
  const landed = await waitForState((s) => s.edit1 === text, 2500)
  check(
    'WM_CHAR 打字真的进了普通编辑框',
    landed?.edit1 === text,
    `delivered=${typed?.delivered} 靶窗里读到 edit1=${JSON.stringify(landed?.edit1)}`,
  )

  // --- 4. 密码框硬拒 ------------------------------------------------------
  let passwordError = null
  try {
    await client.call('win_type', { snapshot_id: snapshotId, uid: passwordEdit?.uid, text: 'hunter2' })
  } catch (error) {
    passwordError = error
  }
  await delay(400)
  const afterPassword = readState()
  check('往密码框打字被拒(PASSWORD_FIELD)', passwordError?.code === 'PASSWORD_FIELD', `code=${passwordError?.code} msg=${passwordError?.message}`)
  check('被拒之后密码框内容仍然是空的', afterPassword?.edit2_len === '0', `edit2_len=${afterPassword?.edit2_len}`)

  // --- 5. 键盘 SendInput 闸门 --------------------------------------------
  // 提权会话里引擎能真的把靶窗抢到前台(AttachThreadInput + SetForegroundWindow),
  // 闸门按设计放行并注入——这时测的已经不是"抢不到前台时拒绝对别的窗口注入"，
  // 而是"能不能抢前台"，结论会随会话权限变化。只在非提权会话验这一条。
  // 2026-10-17 提权后实测：这里会成功注入靶窗；旧断言因此失效。
  const elevated = isElevated()
  const beforeGate = readState()?.edit1
  if (testbed.is_foreground) {
    skips.push('键盘 SendInput 闸门(靶窗此刻正好是前台窗口，只能等它退到后台再验)')
  } else if (elevated) {
    skips.push('键盘 SendInput 闸门(本会话已提权，引擎能抢到前台，闸门按设计放行)')
  } else {
    let keyboardError = null
    try {
      // 必须指名普通编辑框：只给 hwnd 时引擎会取"客户区中心那个最深子窗口"，
      // 而靶窗中心恰好落在密码框上，会被更靠前的密码闸门先拦掉，测不到键盘闸门。
      await client.call('win_type', {
        snapshot_id: snapshotId,
        uid: normalEdit?.uid,
        text: 'SHOULD_NOT_LAND',
        method: 'sendinput',
      })
    } catch (error) {
      keyboardError = error
    }
    await delay(400)
    const afterGate = readState()
    check(
      '目标不在前台时键盘 SendInput 被拒(UNSUPPORTED)',
      keyboardError?.code === 'UNSUPPORTED',
      `code=${keyboardError?.code} msg=${keyboardError?.message}`,
    )
    check(
      '被拒之后编辑框内容没被污染',
      afterGate?.edit1 === beforeGate && !String(afterGate?.edit1).includes('SHOULD_NOT_LAND'),
      `edit1=${JSON.stringify(afterGate?.edit1)}`,
    )
  }

  // --- 6. 鼠标 SendInput 闸门 --------------------------------------------
  // 取一个**确定在靶窗之外**的屏幕点：窗口右下角再往外 40 像素。
  // 出屏也没关系——WindowFromPoint 同样返回空，一样是拒绝。
  const rect = testbed.rect
  const outsideX = rect.x + rect.w + 40
  const outsideY = rect.y + rect.h + 40
  let mouseError = null
  try {
    await client.call('win_click', { hwnd, x: outsideX, y: outsideY, force_sendinput: true })
  } catch (error) {
    mouseError = error
  }
  check(
    '点不在目标窗口上时鼠标 SendInput 被拒(UNSUPPORTED)',
    mouseError?.code === 'UNSUPPORTED',
    `点=(${outsideX},${outsideY}) code=${mouseError?.code} msg=${mouseError?.message}`,
  )
  check('闸门拒绝时没有真的点下去', readState()?.clicks === '0', `clicks=${readState()?.clicks}`)

  // --- 6.5 键盘真注入(硬件扫描码 + 保持时间) ---------------------------------
  // 目的：证明 win_press 换用扫描码注入后，按键仍会变成目标窗口线程里的
  // WM_KEYDOWN。判据同样来自靶窗自己统计的 keys/last_vk，不看引擎自报。
  // 本机中文输入法可能把按键转成 VK_PROCESSKEY(229)：只要 keys 增长，就说明
  // 事件确实进了目标窗口的输入队列（target 自己数到的，不是我方猜的）。
  const keysBefore = Number.parseInt(readState()?.keys ?? '0', 10)
  let pressDelivered = '?'
  try {
    const pressed = await client.call('win_press', { hwnd, key: 'a' })
    pressDelivered = pressed?.delivered ?? '?'
  } catch (error) {
    pressDelivered = `ERROR ${error.code ?? ''} ${error.message ?? error}`
  }
  const afterPress = await waitForState((s) => Number.parseInt(s.keys ?? '0', 10) > keysBefore, 2500)
  check(
    'win_press 扫描码注入真的进了目标窗口(keys 增长)',
    Number.parseInt(afterPress?.keys ?? '0', 10) > keysBefore,
    `delivered=${pressDelivered} keys ${keysBefore} -> ${afterPress?.keys} last_vk=${afterPress?.last_vk}`,
  )

  // --- 7. 句柄泄漏 --------------------------------------------------------
  const enginePid = engineChild.pid
  const handlesBefore = handleCount(enginePid)
  if (handlesBefore === null) {
    check('能读到引擎进程句柄数', false, 'Get-Process 取不到')
  } else {
    let observeOk = 0
    for (let i = 0; i < OBSERVE_ITERATIONS; i++) {
      try {
        await client.call('win_observe', { hwnd, max_depth: 3, max_elements: 32 })
        observeOk++
      } catch {
        /* 单次失败不影响泄漏结论 */
      }
    }
    let captureOk = 0
    for (let i = 0; i < CAPTURE_ITERATIONS; i++) {
      try {
        await client.call('win_capture', { hwnd, max_dim: 800 })
        captureOk++
      } catch {
        /* 同上 */
      }
    }
    // 句柄回收不是同步的，给它一点时间再数。
    await delay(1500)
    const handlesAfter = handleCount(enginePid)
    const delta = handlesAfter === null ? null : handlesAfter - handlesBefore
    check(
      `${OBSERVE_ITERATIONS} 轮 observe + ${CAPTURE_ITERATIONS} 轮 capture 后引擎句柄没泄漏`,
      delta !== null && delta <= HANDLE_LEAK_TOLERANCE,
      `before=${handlesBefore} after=${handlesAfter} delta=${delta} (阈值 +${HANDLE_LEAK_TOLERANCE}) observe成功=${observeOk} capture成功=${captureOk}`,
    )
  }

  // --- 8. 命令行一次性模式（0.1.9）----------------------------------------
  // 同一个 exe 的 find/capture 子命令：不走命名管道、不启动 server、不抢单实例锁。
  const cliDir = path.join(os.tmpdir(), `dsh-cli-accept-${process.pid}`)
  try {
    rmSync(cliDir, { recursive: true, force: true })
    mkdirSync(cliDir, { recursive: true })
    const cliShot = path.join(cliDir, 'testbed.png')
    const capOut = execFileSync(engineExe, ['capture', '--out', cliShot, '--hwnd', String(hwnd)], {
      encoding: 'utf8',
      windowsHide: true,
      timeout: 20_000,
    })
    const capJson = JSON.parse(capOut.trim())
    check(
      'CLI capture：stdout 一行 JSON + PNG 真的落盘',
      capJson.ok === true &&
        capJson.path === cliShot &&
        existsSync(cliShot) &&
        capJson.width > 0 &&
        capJson.height > 0,
      `${capJson.width}x${capJson.height} path=${capJson.path} exists=${existsSync(cliShot)}`,
    )

    const findOut = execFileSync(engineExe, ['find', '--template', cliShot, '--hwnd', String(hwnd)], {
      encoding: 'utf8',
      windowsHide: true,
      timeout: 20_000,
    })
    const findJson = JSON.parse(findOut.trim())
    check(
      'CLI find：自匹配退出码 0 + JSON 命中窗口左上角',
      findJson.ok === true &&
        findJson.found === true &&
        findJson.score > 0.99 &&
        Math.abs(findJson.x - testbed.rect.x) <= 2 &&
        Math.abs(findJson.y - testbed.rect.y) <= 2,
      `score=${findJson.score} at=(${findJson.x},${findJson.y}) 期望≈(${testbed.rect.x},${testbed.rect.y})`,
    )

    // 没找到：用缩过图的模板 + --min-score 1.0（数学上不可能满分）→ 退出码必须是 3。
    const scaledForCli = await client.call('win_capture', { hwnd, max_dim: 300 })
    const missRun = spawnSync(
      engineExe,
      ['find', '--template', scaledForCli.path, '--hwnd', String(hwnd), '--min-score', '1.0'],
      { encoding: 'utf8', windowsHide: true, timeout: 20_000 },
    )
    const missJson = missRun.status === 3 ? JSON.parse(missRun.stdout.trim()) : null
    check(
      'CLI find：没找到时退出码 3 且 stdout 仍是合法 JSON',
      missRun.status === 3 && missJson?.ok === true && missJson.found === false,
      `status=${missRun.status} found=${missJson?.found} score=${missJson?.score}`,
    )
  } catch (error) {
    check('CLI find/capture 一次性模式', false, error.stack ?? String(error))
  } finally {
    rmSync(cliDir, { recursive: true, force: true })
  }

  // --- 收尾 --------------------------------------------------------------
  socket.destroy()
}

async function run() {
  let failure = null
  try {
    await main()
  } catch (error) {
    failure = error
  } finally {
    engineChild?.kill()
    testbedChild?.kill()
    await delay(300)
    try {
      rmSync(STATE_FILE, { force: true })
    } catch {
      /* 忽略 */
    }
  }

  const failed = results.filter((r) => !r.ok)
  console.log('')
  if (failure) console.log(`致命错误: ${failure.message}`)
  console.log(`结果:${failed.length} 项失败,${skips.length} 项跳过,${results.length} 项检查`)
  if (failed.length) for (const f of failed) console.log(`  - 失败 ${f.name}: ${f.detail}`)
  for (const s of skips) console.log(`  - 跳过 ${s}`)
  process.exit(failed.length === 0 && !failure ? 0 : 1)
}

await run()
