#!/usr/bin/env bun
import { appendFile, mkdir, writeFile } from 'node:fs/promises'
import { homedir } from 'node:os'
import { resolve } from 'node:path'
import { Client } from '@modelcontextprotocol/sdk/client/index.js'
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js'

const toolName = Bun.argv[2]
const args = toolName && toolName !== 'tools/list'
  ? JSON.parse(Bun.argv[3] ?? '{}') as Record<string, unknown>
  : {}

const javaHome = '/opt/homebrew/opt/openjdk@21'
const androidHome = '/opt/homebrew/share/android-commandlinetools'
const maestro = resolve(homedir(), '.maestro/bin/maestro')
const device = process.env.WALKCACHE_DEVICE ?? 'emulator-5554'
const path = [
  resolve(homedir(), '.maestro/bin'),
  `${androidHome}/platform-tools`,
  `${javaHome}/bin`,
  process.env.PATH ?? '',
].join(':')

const transport = new StdioClientTransport({
  command: maestro,
  args: ['--no-ansi', '--device', device, 'mcp'],
  stderr: 'inherit',
  env: {
    ...process.env,
    JAVA_HOME: javaHome,
    ANDROID_HOME: androidHome,
    ANDROID_SDK_ROOT: androidHome,
    MAESTRO_CLI_NO_ANALYTICS: '1',
    MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED: 'true',
    PATH: path,
  } as Record<string, string>,
})

const client = new Client({ name: 'walkcache-android-spike', version: '0.1.0' })
const startedAt = performance.now()

try {
  await client.connect(transport)
  if (toolName === 'tools/list' || !toolName) {
    const result = await client.listTools()
    console.log(JSON.stringify(result, null, 2))
  } else {
    const result = await client.callTool(
      { name: toolName, arguments: args },
      undefined,
      { timeout: 180_000 },
    )
    const rendered: unknown[] = []
    for (const item of result.content ?? []) {
      if (item.type === 'text') {
        console.log(item.text)
        rendered.push(item)
      } else if (item.type === 'image') {
        await mkdir('.walkcache/artifacts', { recursive: true })
        const filename = `.walkcache/artifacts/${Date.now()}-screenshot.png`
        await writeFile(filename, Buffer.from(item.data, 'base64'))
        console.log(JSON.stringify({ type: 'image', path: filename, mimeType: item.mimeType }))
        rendered.push({ type: 'image', path: filename, mimeType: item.mimeType })
      } else {
        console.log(JSON.stringify(item))
        rendered.push(item)
      }
    }

    await mkdir('.walkcache/attempts', { recursive: true })
    await appendFile(
      '.walkcache/attempts/mcp-calls.jsonl',
      `${JSON.stringify({
        timestamp: new Date().toISOString(),
        tool: toolName,
        arguments: args,
        durationMs: Math.round(performance.now() - startedAt),
        isError: result.isError ?? false,
        content: rendered,
      })}\n`,
    )
  }
} finally {
  await client.close()
}

