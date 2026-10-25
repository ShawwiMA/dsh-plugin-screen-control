#!/usr/bin/env node
/**
 * 引擎直连探针(开发自测用)。
 *
 * 绕过 DSH 工具层,直接对 Rust 引擎的命名管道发 NDJSON 请求,一条请求一行 JSON 输出。
 * 存在的理由:插件是 TS 编译产物(lib/index.js),改完引擎后不重启 DSH 也能自测引擎;
 * 也方便一次提交多条相互独立的请求。
 *
 * 用法:
 *   node scripts/dev-probe.mjs --requests '<JSON 数组>'
 *   node scripts/dev-probe.mjs --file <requests.json>
 * 可选:
 *   --engine <exe 路径>   默认 bin/screen-control-engine.exe
 *   --timeout <ms>        单条响应超时,默认 340000
 *   --quiet               不转发引擎 stderr
 */

import { spawn } from 'node:child_process'
import net from 'node:net'
import fs from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const ROOT = path.resolve(HERE, '..')

function parseArgs(argv) {
  const out = { engine: path.join(ROOT, 'bin', 'screen-control-engine.exe'), timeout: 340000, quiet: false }
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i]
    if (a === '--requests') out.requests = argv[++i]
    else if (a === '--file') out.file = argv[++i]
    else if (a === '--engine') out.engine = argv[++i]
    else if (a === '--timeout') out.timeout = Number(argv[++i])
    else if (a === '--quiet') out.quiet = true
    else {
      console.error(`未知参数: ${a}`)
      process.exit(2)
    }
  }
  return out
}

const opt = parseArgs(process.argv.slice(2))
const list = opt.file ? JSON.parse(fs.readFileSync(opt.file, 'utf8')) : JSON.parse(opt.requests ?? '[]')
if (!Array.isArray(list) || list.length === 0) {
  console.error('没有请求:用 --requests 或 --file 给一个非空 JSON 数组')
  process.exit(2)
}
if (!fs.existsSync(opt.engine)) {
  console.error(`引擎不存在: ${opt.engine}`)
  process.exit(2)
}

const pipeName = `dsh-probe-${process.pid}-${Math.floor(Math.random() * 1e6)}`
const engine = spawn(opt.engine, ['--pipe', pipeName, '--log-level', 'debug'], {
  stdio: ['ignore', 'ignore', 'pipe'],
  windowsHide: true,
})
let stderrTail = []
engine.stderr.on('data', (chunk) => {
  const text = chunk.toString('utf8')
  stderrTail.push(text)
  if (stderrTail.length > 400) stderrTail = stderrTail.slice(-400)
  if (!opt.quiet) process.stderr.write(text)
})

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

/** 连一次管道,最多等 5s。 */
async function connect(deadlineMs = 5000) {
  const started = Date.now()
  const full = `\\\\.\\pipe\\${pipeName}`
  for (;;) {
    try {
      return await new Promise((resolve, reject) => {
        const socket = net.connect(full)
        socket.once('connect', () => resolve(socket))
        socket.once('error', reject)
      })
    } catch (error) {
      if (Date.now() - started > deadlineMs) throw new Error(`连不上管道 ${full}: ${error.message}`)
      await sleep(50)
    }
  }
}

function fail(message, extra) {
  const payload = { ok: false, harness_error: message }
  if (extra) payload.engine_stderr_tail = stderrTail.join('').split('\n').slice(-12).join('\n')
  console.log(JSON.stringify(payload))
  try {
    engine.kill()
  } catch {}
  process.exit(extra ? 3 : 0)
}

const socket = await connect().catch((error) => fail(error.message, true))
let buffer = ''
const pending = new Map()

socket.on('data', (chunk) => {
  buffer += chunk.toString('utf8')
  let index = buffer.indexOf('\n')
  while (index >= 0) {
    const line = buffer.slice(0, index).trim()
    buffer = buffer.slice(index + 1)
    index = buffer.indexOf('\n')
    if (!line) continue
    let frame
    try {
      frame = JSON.parse(line)
    } catch {
      console.log(JSON.stringify({ harness_error: '引擎输出不是 JSON', line }))
      continue
    }
    if (frame.event) continue // 事件帧(日志)不进结果
    const waiter = pending.get(frame.id)
    if (waiter) {
      pending.delete(frame.id)
      waiter(frame)
    }
  }
})

let id = 0
for (const item of list) {
  id += 1
  const request = { id, method: item.method, params: item.params ?? {} }
  const response = await new Promise((resolve) => {
    const timer = setTimeout(() => {
      pending.delete(id)
      resolve({ id, ok: false, error: { code: 'HARNESS_TIMEOUT', message: `等待 ${opt.timeout}ms 无响应` } })
    }, opt.timeout)
    pending.set(id, (frame) => {
      clearTimeout(timer)
      resolve(frame)
    })
    socket.write(`${JSON.stringify(request)}\n`)
  })
  console.log(JSON.stringify(response))
}

socket.end()
engine.kill()
await sleep(50)
process.exit(0)
