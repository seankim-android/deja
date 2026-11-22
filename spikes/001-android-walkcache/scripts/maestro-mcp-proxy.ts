#!/usr/bin/env bun
import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { appendFileSync, mkdirSync } from 'node:fs'
import { dirname } from 'node:path'
import { createInterface } from 'node:readline'

const maestro = process.env.WALKCACHE_MAESTRO
const device = process.env.WALKCACHE_DEVICE
const logPath = process.env.WALKCACHE_MCP_LOG
if (!maestro || !device || !logPath) {
  throw new Error('WALKCACHE_MAESTRO, WALKCACHE_DEVICE, and WALKCACHE_MCP_LOG are required')
}
mkdirSync(dirname(logPath), { recursive: true })
appendFileSync(logPath, '')

const pending = new Map<string, { tool: string; startedAt: number }>()
function log(entry: Record<string, unknown>) {
  appendFileSync(logPath, `${JSON.stringify({ timestamp: new Date().toISOString(), ...entry })}\n`)
}

const child = spawn(maestro, ['--no-ansi', '--device', device, 'mcp'], {
  cwd: process.cwd(),
  env: {
    ...process.env,
    MAESTRO_CLI_NO_ANALYTICS: '1',
    MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED: 'true',
  },
  stdio: ['pipe', 'pipe', 'inherit'],
})

const fromAgent = createInterface({ input: process.stdin })
fromAgent.on('line', line => {
  if (!line.trim()) return
  try {
    const message = JSON.parse(line) as {
      id?: string | number
      method?: string
      params?: { name?: string; arguments?: Record<string, unknown> }
    }
    if (message.method === 'tools/call') {
      const id = String(message.id)
      const args = message.params?.arguments ?? {}
      const yaml = typeof args.yaml === 'string' ? args.yaml : null
      const blocked = 'files' in args || 'dir' in args ||
        (yaml !== null && /^\s*-\s+(?:runScript|runFlow):/m.test(yaml))
      log({
        direction: 'request',
        id,
        method: message.method,
        tool: message.params?.name ?? 'unknown',
        argumentKeys: Object.keys(args).sort(),
        inlineYamlBytes: yaml ? Buffer.byteLength(yaml) : null,
        inlineYamlSha256: yaml ? createHash('sha256').update(yaml).digest('hex') : null,
        rejectedExternalFlowInput: blocked,
      })
      if (blocked) {
        process.stdout.write(`${JSON.stringify({
          jsonrpc: '2.0', id: message.id,
          error: { code: -32602, message: 'Only inline yaml is allowed; files and dir are denied' },
        })}\n`)
        return
      }
      pending.set(id, { tool: message.params?.name ?? 'unknown', startedAt: performance.now() })
    } else if (message.method) {
      log({ direction: 'request', id: message.id ?? null, method: message.method })
    }
  } catch {
    log({ direction: 'request', method: 'unparseable-jsonrpc' })
  }
  child.stdin.write(`${line}\n`)
})

const fromMaestro = createInterface({ input: child.stdout })
fromMaestro.on('line', line => {
  if (!line.trim()) return
  try {
    const message = JSON.parse(line) as {
      id?: string | number
      error?: { code?: number }
      result?: { isError?: boolean; content?: Array<{ type?: string }> }
    }
    const id = String(message.id)
    const request = pending.get(id)
    if (request) {
      pending.delete(id)
      log({
        direction: 'response',
        id,
        tool: request.tool,
        durationMs: Math.round(performance.now() - request.startedAt),
        isError: message.result?.isError ?? Boolean(message.error),
        errorCode: message.error?.code ?? null,
        contentTypes: message.result?.content?.map(item => item.type ?? 'unknown') ?? [],
      })
    } else {
      log({ direction: 'response', id: message.id ?? null })
    }
  } catch {
    log({ direction: 'response', kind: 'unparseable-jsonrpc' })
  }
  process.stdout.write(`${line}\n`)
})

fromAgent.on('close', () => child.stdin.end())
child.once('error', error => {
  log({ direction: 'transport', event: 'launch_error', code: (error as NodeJS.ErrnoException).code })
  process.exitCode = 1
})
child.once('close', code => {
  log({ direction: 'transport', event: 'closed', exitCode: code })
  process.exitCode = code ?? 1
})
process.on('SIGTERM', () => child.kill('SIGTERM'))
process.on('SIGINT', () => child.kill('SIGINT'))
