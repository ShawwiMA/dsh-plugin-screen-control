#!/usr/bin/env node
/**
 * win_web_cdp 真机自测（自动拉起浏览器、跑完自动收尾）。
 *
 * 为什么单独一个脚本：CDP 是唯一「浏览器必须在跑」才能验的工具，它的正确性完全
 * 取决于有没有真的连上浏览器。这里用**独立 user-data-dir** 新起一个 Edge 实例
 * （Chromium 的规则：user-data-dir 不同 = 另一个浏览器进程），所以不会碰你手动
 * 开着的 Edge；关的时候也只按「命令行里带本次临时目录」精确匹配，绝不按镜像名
 * 批量杀 msedge.exe。
 *
 * 流程：
 *   1. 找 msedge.exe
 *   2. 端口若已被占用 → 直接退出（避免连到你自己的调试实例上自欺欺人）
 *   3. 用 cmd 的 start 拉起 Edge：--remote-debugging-port + 独立 --user-data-dir
 *   4. 轮询 /json/version 就绪（带超时，不死等）
 *   5. 经 dev-probe 直连引擎，依次验 list_targets / Page.navigate / Runtime.evaluate
 *   6. 精确关掉本次实例并删临时目录；原始响应写进 logs/
 *
 * 用法：
 *   node scripts/cdp-selftest.mjs              # 无头，跑完自动关
 *   node scripts/cdp-selftest.mjs --headful    # 看得见窗口（会抢一下焦点）
 *   node scripts/cdp-selftest.mjs --keep       # 跑完留着浏览器，方便自己接着点
 * 参数：--port <默认 9222> | --edge <msedge.exe> | --timeout <就绪等待上限 ms，默认 20000>
 *
 * 退出码：0 = 全过；1 = 有失败项（明细照样打印）。
 */

import { execSync, spawnSync } from 'node:child_process'
import fs from 'node:fs'
import http from 'node:http'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const ROOT = path.resolve(HERE, '..')
const PROBE = path.join(HERE, 'dev-probe.mjs')
const ENGINE = path.join(ROOT, 'bin', 'screen-control-engine.exe')
const HOST = '127.0.0.1'

const opt = { port: 9222, timeout: 20000, headful: false, keep: false }
const argv = process.argv.slice(2)
for (let i = 0; i < argv.length; i += 1) {
  const a = argv[i]
  if (a === '--port') opt.port = Number(argv[++i])
  else if (a === '--edge') opt.edge = argv[++i]
  else if (a === '--timeout') opt.timeout = Number(argv[++i])
  else if (a === '--headful') opt.headful = true
  else if (a === '--keep') opt.keep = true
  else {
    console.error(`未知参数: ${a}`)
    process.exit(2)
  }
}
if (!Number.isInteger(opt.port) || opt.port < 1 || opt.port > 65535) {
  console.error(`端口不合法: ${opt.port}`)
  process.exit(2)
}

const TITLE = 'DshCdpTest'
const PAGE_URL = `data:text/html,<title>${TITLE}</title><h1>dsh cdp selftest</h1>`
const profileDir = path.join(os.tmpdir(), `dsh-cdp-selftest-${process.pid}`)
const evidence = { started_at: new Date().toISOString(), port: opt.port, profile_dir: profileDir, steps: [] }

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

function step(name, ok, detail) {
  evidence.steps.push({ name, ok, detail })
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? ` — ${detail}` : ''}`)
  return ok
}

function findEdge() {
  if (opt.edge) return fs.existsSync(opt.edge) ? opt.edge : null
  const roots = [process.env['ProgramFiles(x86)'], process.env.ProgramFiles, process.env.LOCALAPPDATA]
  for (const root of roots.filter(Boolean)) {
    const p = path.join(root, 'Microsoft', 'Edge', 'Application', 'msedge.exe')
    if (fs.existsSync(p)) return p
  }
  const which = spawnSync('where.exe', ['msedge.exe'], { encoding: 'utf8' })
  const first = String(which.stdout ?? '')
    .split(/\r?\n/)
    .map((s) => s.trim())
    .filter(Boolean)[0]
  return first && fs.existsSync(first) ? first : null
}

function portBusy(port, ms = 800) {
  return new Promise((resolve) => {
    const socket = net.connect({ host: HOST, port })
    const settle = (busy) => {
      socket.destroy()
      resolve(busy)
    }
    socket.setTimeout(ms)
    socket.once('connect', () => settle(true))
    socket.once('timeout', () => settle(false))
    socket.once('error', () => settle(false))
  })
}

function httpGetJson(port, urlPath, ms = 2000) {
  return new Promise((resolve) => {
    const req = http.get({ host: HOST, port, path: urlPath, timeout: ms }, (res) => {
      let body = ''
      res.setEncoding('utf8')
      res.on('data', (c) => {
        body += c
      })
      res.on('end', () => {
        try {
          resolve(JSON.parse(body))
        } catch {
          resolve(null)
        }
      })
    })
    req.on('timeout', () => {
      req.destroy()
      resolve(null)
    })
    req.on('error', () => resolve(null))
  })
}

async function waitVersion() {
  const deadline = Date.now() + opt.timeout
  let last = null
  for (;;) {
    last = await httpGetJson(opt.port, '/json/version')
    if (last && last.webSocketDebuggerUrl) return last
    if (Date.now() >= deadline) return last
    await sleep(300)
  }
}

/** 经 dev-probe 直连引擎跑一批请求，返回引擎原始响应帧。 */
function probe(requests, timeoutMs = 30000) {
  const run = spawnSync(
    process.execPath,
    [PROBE, '--quiet', '--engine', ENGINE, '--timeout', String(timeoutMs), '--requests', JSON.stringify(requests)],
    { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 },
  )
  const frames = []
  for (const line of String(run.stdout ?? '').split(/\r?\n/)) {
    const text = line.trim()
    if (!text) continue
    try {
      frames.push(JSON.parse(text))
    } catch {
      frames.push({ harness_error: '非 JSON 输出', line: text })
    }
  }
  if (frames.length === 0) {
    frames.push({ harness_error: '探针没有输出', status: run.status, stderr: String(run.stderr ?? '').slice(-600) })
  }
  return frames
}

const pickFrame = (frames) => frames.find((f) => f && f.id === 1) ?? frames[0]

/** 只杀「命令行里带本次临时目录」的 msedge.exe。 */
function killOurEdge() {
  const needle = profileDir.replace(/'/g, "''")
  const script =
    `$p = @(Get-CimInstance Win32_Process -Filter "Name='msedge.exe'" | ` +
    `Where-Object { $_.CommandLine -like '*${needle}*' }); ` +
    `if ($p.Count -gt 0) { $p | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue } }; ` +
    `Write-Output $p.Count`
  const run = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { encoding: 'utf8' })
  return Number(String(run.stdout ?? '').trim()) || 0
}

async function removeProfile() {
  for (let i = 0; i < 12; i += 1) {
    try {
      fs.rmSync(profileDir, { recursive: true, force: true })
      if (!fs.existsSync(profileDir)) return true
    } catch {
      /* 浏览器还没退干净，等一会儿再删 */
    }
    await sleep(250)
  }
  return !fs.existsSync(profileDir)
}

async function run() {
  const edge = findEdge()
  if (!edge) {
    return step('1. 找到 msedge.exe', false, opt.edge ? `路径不存在：${opt.edge}` : '常见安装位置和 where msedge.exe 都没找到')
  }
  step('1. 找到 msedge.exe', true, edge)

  if (await portBusy(opt.port)) {
    return step(`2. 端口 ${opt.port} 空闲`, false, '已被占用，拒绝继续（否则测的是别人的浏览器）')
  }
  step(`2. 端口 ${opt.port} 空闲`, true)

  fs.mkdirSync(profileDir, { recursive: true })
  const flags = [
    `--remote-debugging-port=${opt.port}`,
    `--user-data-dir=${profileDir}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-sync',
    '--disable-extensions',
    '--no-service-autorun',
    opt.headful ? '--new-window' : '--headless=new',
    'about:blank',
  ]
  const cmdline = ['start', '""', `"${edge}"`, ...flags.map((f) => `"${f}"`)].join(' ')
  evidence.launch_cmdline = cmdline
  try {
    execSync(cmdline, { stdio: 'ignore', windowsHide: true })
    step('3. start 拉起独立 Edge 实例', true, opt.headful ? '有窗口' : '无头（不抢焦点）')
  } catch (error) {
    return step('3. start 拉起独立 Edge 实例', false, error.message)
  }

  const version = await waitVersion()
  if (!version || !version.webSocketDebuggerUrl) {
    return step('4. /json/version 就绪', false, `${opt.timeout}ms 内没起来（拿到：${JSON.stringify(version)}）`)
  }
  const browser = String(version.Browser ?? '')
  step('4. /json/version 就绪', true, browser)
  if (!/Edg\//.test(browser)) {
    return step('5. 确认连的是 Edge', false, `Browser=${browser}，不是 Edge，停止`)
  }
  step('5. 确认连的是 Edge', true, browser)

  // ---- list_targets ----
  const listFrame = pickFrame(
    probe([{ method: 'win_web_cdp', params: { host: HOST, port: opt.port, timeout_ms: 10000, list_targets: true } }]),
  )
  evidence.list_targets = listFrame
  const targets = listFrame?.result?.targets
  const pages = Array.isArray(targets) ? targets.filter((t) => t && t.type === 'page') : []
  if (!listFrame?.ok) {
    step('6. list_targets', false, `错误：${JSON.stringify(listFrame?.error ?? listFrame).slice(0, 300)}`)
  } else {
    step('6. list_targets', pages.length > 0, `${Array.isArray(targets) ? targets.length : 0} 个目标，其中 page ${pages.length} 个`)
  }

  // ---- Page.navigate ----
  const navFrame = pickFrame(
    probe([
      {
        method: 'win_web_cdp',
        params: {
          host: HOST,
          port: opt.port,
          timeout_ms: 15000,
          command: 'Page.navigate',
          command_params: { url: PAGE_URL },
        },
      },
    ]),
  )
  evidence.page_navigate = navFrame
  if (!navFrame?.ok) {
    step('7. Page.navigate', false, `错误：${JSON.stringify(navFrame?.error ?? navFrame).slice(0, 300)}`)
  } else {
    const navResult = navFrame?.result?.result
    const errorText = navResult?.errorText
    step(
      '7. Page.navigate',
      !errorText,
      errorText ? `errorText=${errorText}` : `目标 ${navFrame?.result?.ws_url ?? '?'}`,
    )
  }

  // ---- Runtime.evaluate（导航刚发出，给几次重试） ----
  let title = null
  let evalFrame = null
  for (let i = 0; i < 6; i += 1) {
    evalFrame = pickFrame(
      probe([
        {
          method: 'win_web_cdp',
          params: {
            host: HOST,
            port: opt.port,
            timeout_ms: 10000,
            command: 'Runtime.evaluate',
            command_params: { expression: 'document.title', returnByValue: true },
          },
        },
      ]),
    )
    title = evalFrame?.result?.result?.result?.value ?? null
    if (title === TITLE) break
    await sleep(400)
  }
  evidence.runtime_evaluate = evalFrame
  step(
    '8. Runtime.evaluate 读回标题',
    title === TITLE,
    title === TITLE ? `document.title = ${title}` : `期望 ${TITLE}，实际 ${JSON.stringify(title ?? evalFrame?.error ?? null)}`,
  )

  return evidence.steps.every((s) => s.ok)
}

let ok = false
try {
  ok = await run()
} catch (error) {
  step('脚本自身异常', false, error.stack ?? String(error))
} finally {
  if (opt.keep) {
    console.log(`\n--keep：浏览器留着没关，profile = ${profileDir}`)
  } else {
    const killed = killOurEdge()
    await sleep(800)
    const stillBusy = await portBusy(opt.port)
    step('9. 关掉本次实例', !stillBusy, `按 user-data-dir 精确停止 ${killed} 个 msedge.exe，端口${stillBusy ? '仍被占用' : '已释放'}`)
    step('10. 清理临时 profile', await removeProfile(), profileDir)
  }

  const passed = evidence.steps.filter((s) => s.ok).length
  const failed = evidence.steps.filter((s) => !s.ok)
  evidence.finished_at = new Date().toISOString()
  evidence.ok = failed.length === 0
  try {
    const logDir = path.join(ROOT, 'logs')
    fs.mkdirSync(logDir, { recursive: true })
    const logPath = path.join(logDir, `cdp-selftest-${new Date().toISOString().replace(/[:.]/g, '-')}.json`)
    fs.writeFileSync(logPath, JSON.stringify(evidence, null, 2), 'utf8')
    console.log(`\n证据：${logPath}`)
  } catch (error) {
    console.log(`\n证据落盘失败：${error.message}`)
  }
  console.log(`${passed}/${evidence.steps.length} 项通过${failed.length ? `，失败项：${failed.map((s) => s.name).join('、')}` : ''}`)
  process.exitCode = ok && failed.length === 0 ? 0 : 1
}
