import { spawn } from 'node:child_process'
import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import type { MaestroFlowResult, MaestroHierarchyResult, MaestroTransport } from './ios-exploration.ts'

type Spawned = { stdout: string; stderr: string; exitCode: number | null; timedOut: boolean; durationMs: number }

function run(
  executable: string,
  args: string[],
  timeoutMs: number,
  env: NodeJS.ProcessEnv,
): Promise<Spawned> {
  const started = performance.now()
  return new Promise(resolve => {
    const child = spawn(executable, args, { env, stdio: ['ignore', 'pipe', 'pipe'], detached: true })
    let stdout = ''
    let stderr = ''
    let timedOut = false
    child.stdout.setEncoding('utf8').on('data', (chunk: string) => { stdout += chunk })
    child.stderr.setEncoding('utf8').on('data', (chunk: string) => { stderr += chunk })
    const timer = setTimeout(() => {
      timedOut = true
      // The child leads its own process group, so this reaches only Maestro and its helpers.
      try { process.kill(-child.pid!, 'SIGKILL') } catch { /* already gone */ }
    }, timeoutMs)
    const finish = (exitCode: number | null) => {
      clearTimeout(timer)
      resolve({ stdout, stderr, exitCode, timedOut, durationMs: Math.round(performance.now() - started) })
    }
    child.once('error', () => finish(null))
    child.once('close', code => finish(code))
  })
}

function tail(text: string, lines = 12) {
  return text.trim().split('\n').slice(-lines).join('\n')
}

function maestroEnv(): NodeJS.ProcessEnv {
  return { ...process.env, MAESTRO_CLI_NO_ANALYTICS: '1', MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED: 'true' }
}

export type ProcessMaestroTransportOptions = {
  /** Path to the `maestro` executable. */
  executable: string
  /** Simulator UDID. */
  device: string
  timeoutMs?: number
}

/** A fresh `maestro` process per call: `maestro hierarchy` and `maestro test` of a one-command flow. */
export class ProcessMaestroTransport implements MaestroTransport {
  readonly kind = 'process' as const
  private readonly options: ProcessMaestroTransportOptions
  constructor(options: ProcessMaestroTransportOptions) {
    this.options = options
  }

  async hierarchy(): Promise<MaestroHierarchyResult> {
    const result = await run(
      this.options.executable,
      ['--no-ansi', '--platform', 'ios', '--device', this.options.device, 'hierarchy'],
      this.options.timeoutMs ?? 120_000,
      maestroEnv(),
    )
    if (result.exitCode !== 0) throw new Error(`maestro hierarchy failed (${result.timedOut ? 'timeout' : result.exitCode}): ${tail(result.stderr)}`)
    // Maestro may print log lines before the JSON object.
    const start = result.stdout.indexOf('{')
    return { raw: JSON.parse(result.stdout.slice(start)), durationMs: result.durationMs }
  }

  async runFlow(yaml: string, env: Record<string, string> = {}): Promise<MaestroFlowResult> {
    const directory = await mkdtemp(join(tmpdir(), 'deja-step-'))
    try {
      const flow = join(directory, 'step.yaml')
      await writeFile(flow, yaml)
      const variables = Object.entries(env).flatMap(([key, value]) => ['-e', `${key}=${value}`])
      const result = await run(
        this.options.executable,
        ['--no-ansi', '--platform', 'ios', '--device', this.options.device, 'test', ...variables, flow],
        this.options.timeoutMs ?? 120_000,
        maestroEnv(),
      )
      return {
        passed: result.exitCode === 0 && !result.timedOut,
        exitCode: result.exitCode,
        output: tail(`${result.stdout}\n${result.stderr}`),
        durationMs: result.durationMs,
        timedOut: result.timedOut,
      }
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  }

  async close() {}
}

export type McpMaestroTransportOptions = {
  executable: string
  device: string
  timeoutMs?: number
}

type Pending = { resolve: (value: any) => void; reject: (error: Error) => void }

/**
 * One long-running `maestro mcp` process spoken to over stdio JSON-RPC. The caller owns it:
 * `close()` stops the process, and nothing else may be killed.
 */
export class McpMaestroTransport implements MaestroTransport {
  readonly kind = 'mcp' as const
  private child: ReturnType<typeof spawn> | null = null
  private buffer = ''
  private nextId = 0
  private readonly pending = new Map<number, Pending>()
  private ready: Promise<void> | null = null
  /** Time `initialize` took, so a caller can count session start separately from per-step cost. */
  startupMs = 0

  private readonly options: McpMaestroTransportOptions
  constructor(options: McpMaestroTransportOptions) {
    this.options = options
  }

  private send(message: object) {
    this.child!.stdin!.write(`${JSON.stringify(message)}\n`)
  }

  private request(method: string, params: object): Promise<any> {
    const id = ++this.nextId
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id)
        reject(new Error(`MCP ${method} timed out`))
      }, this.options.timeoutMs ?? 120_000)
      this.pending.set(id, {
        resolve: value => { clearTimeout(timer); resolve(value) },
        reject: error => { clearTimeout(timer); reject(error) },
      })
      this.send({ jsonrpc: '2.0', id, method, params })
    })
  }

  private start(): Promise<void> {
    this.ready ??= (async () => {
      const started = performance.now()
      const child = spawn(this.options.executable, ['mcp'], { env: maestroEnv(), stdio: ['pipe', 'pipe', 'ignore'] })
      this.child = child
      child.stdout!.setEncoding('utf8').on('data', (chunk: string) => {
        this.buffer += chunk
        for (let end = this.buffer.indexOf('\n'); end >= 0; end = this.buffer.indexOf('\n')) {
          const line = this.buffer.slice(0, end)
          this.buffer = this.buffer.slice(end + 1)
          try {
            const message = JSON.parse(line)
            if (typeof message.id === 'number') {
              const waiting = this.pending.get(message.id)
              this.pending.delete(message.id)
              if (message.error) waiting?.reject(new Error(String(message.error.message ?? 'MCP error')))
              else waiting?.resolve(message.result)
            }
          } catch { /* not a JSON-RPC line */ }
        }
      })
      child.once('close', () => {
        for (const waiting of this.pending.values()) waiting.reject(new Error('maestro mcp exited'))
        this.pending.clear()
      })
      await this.request('initialize', {
        protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'deja', version: '0' },
      })
      this.send({ jsonrpc: '2.0', method: 'notifications/initialized' })
      this.startupMs = Math.round(performance.now() - started)
    })()
    return this.ready
  }

  private async call(name: string, args: object): Promise<any> {
    await this.start()
    return this.request('tools/call', { name, arguments: { device_id: this.options.device, ...args } })
  }

  async hierarchy(): Promise<MaestroHierarchyResult> {
    const started = performance.now()
    const result = await this.call('inspect_screen', {})
    const json = (result.content as { text: string }[]).map(part => part.text).find(text => text.trimStart().startsWith('{'))
    if (!json) throw new Error('inspect_screen returned no hierarchy')
    return { raw: JSON.parse(json), durationMs: Math.round(performance.now() - started) }
  }

  async runFlow(yaml: string, env?: Record<string, string>): Promise<MaestroFlowResult> {
    const started = performance.now()
    const result = await this.call('run', { yaml, ...(env ? { env } : {}) })
    const text = (result.content as { text: string }[]).map(part => part.text).join('\n')
    const failed = result.isError === true || /\bFAILED\b|"status"\s*:\s*"(ERROR|FAILED)"/i.test(text)
    return {
      passed: !failed,
      exitCode: null,
      output: tail(text),
      durationMs: Math.round(performance.now() - started),
      timedOut: false,
    }
  }

  async close() {
    const child = this.child
    this.child = null
    this.ready = null
    if (!child || child.exitCode !== null) return
    child.kill('SIGTERM')
    await new Promise<void>(resolve => {
      const timer = setTimeout(() => { child.kill('SIGKILL'); resolve() }, 3000)
      child.once('close', () => { clearTimeout(timer); resolve() })
    })
  }
}
