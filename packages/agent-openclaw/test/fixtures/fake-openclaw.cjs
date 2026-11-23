#!/usr/bin/env bun
const { appendFileSync, mkdirSync, readFileSync, writeFileSync } = require('node:fs')
const { dirname } = require('node:path')

const args = process.argv.slice(2)
const configPath = args[args.indexOf('--config') + 1]
const config = JSON.parse(readFileSync(configPath, 'utf8'))
if (process.env.DEJA_FAKE_OPENCLAW_CAPTURE) {
  writeFileSync(process.env.DEJA_FAKE_OPENCLAW_CAPTURE, JSON.stringify({ args, config }, null, 2))
}
const logPath = config.mcp.servers.maestro.env.DEJA_MCP_LOG
if (process.env.DEJA_FAKE_OPENCLAW_SKIP_MCP_LOG !== '1') {
  mkdirSync(dirname(logPath), { recursive: true })
  appendFileSync(logPath, '')
  const mcpCalls = Number(process.env.DEJA_FAKE_OPENCLAW_MCP_CALLS ?? '0')
  for (let index = 0; index < mcpCalls; index += 1) {
    appendFileSync(logPath, `${JSON.stringify({ direction: 'request', method: 'tools/call', id: index })}\n`)
  }
}

const invalid = process.env.DEJA_FAKE_OPENCLAW_MODE === 'invalid'
const final = invalid
  ? `appId: dev.deja.checkout
---
- launchApp:
    appId: dev.deja.checkout
    clearState: true
- assertVisible: Déjà Shop
- assertVisible: "Order #WC-9999"
`
  : `appId: dev.deja.checkout
---
- launchApp:
    appId: dev.deja.checkout
    clearState: true
- assertVisible: Déjà Shop
- assertVisible: Order confirmed
`
const envelope = {
  ok: true,
  status: 'ok',
  final: process.env.DEJA_FAKE_OPENCLAW_FENCED === '1' ? `\`\`\`yaml\n${final}\`\`\`` : final,
  model: 'fixture-kimi',
  provider: 'fixture',
  sessionId: 'fixture-session',
}
if (process.env.DEJA_FAKE_OPENCLAW_NO_TURNS !== '1') {
  envelope.assistantTurns = Number(process.env.DEJA_FAKE_OPENCLAW_TURNS ?? '2')
}
console.log(JSON.stringify(envelope))
