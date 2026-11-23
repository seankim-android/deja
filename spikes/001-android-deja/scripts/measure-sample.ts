#!/usr/bin/env bun
import { spawn, spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { loadExploreConfiguration } from '@deja-qa/explorer-openclaw'
import {
  LocalWalkStore,
  MAESTRO_FLOW_SCHEMA_VERSION,
} from '@deja-qa/store-local'
import type { MetricSample, SampleMode } from '@deja-qa/metrics'

const root = process.cwd()
const args = Bun.argv.slice(2)
function argument(name: string) {
  const index = args.indexOf(name)
  if (index < 0 || !args[index + 1]) throw new Error(`Missing ${name}`)
  return args[index + 1]
}
const sampleId = argument('--sample')
const pairId = argument('--pair')
const mode = argument('--mode') as SampleMode
if (!['cold', 'warm'].includes(mode) || !/^[a-z0-9-]+$/.test(sampleId) || !/^[a-z0-9-]+$/.test(pairId)) {
  throw new Error('Usage: bun scripts/measure-sample.ts --sample cold-01 --pair pair-1 --mode cold|warm')
}

const configuration = loadExploreConfiguration(resolve(root, 'explore.config.json'))
const buildPath = resolve(root, configuration.buildPath)
const buildFingerprint = createHash('sha256').update(Buffer.from(await Bun.file(buildPath).arrayBuffer())).digest('hex')
const adb = configuration.device === 'emulator-5554' && process.env.ANDROID_HOME
  ? resolve(process.env.ANDROID_HOME, 'platform-tools', 'adb')
  : 'adb'
function deviceCheck(suffix: string[]) {
  const result = spawnSync(adb, ['-s', configuration.device, ...suffix], {
    cwd: root,
    encoding: 'utf8',
    timeout: 30_000,
  })
  if (result.status !== 0) throw new Error(`Android precondition failed: ${suffix.join(' ')}`)
  return result.stdout.trim()
}
const deviceState = deviceCheck(['get-state'])
if (deviceState !== 'device') throw new Error(`Device is not ready: ${deviceState}`)
if (!deviceCheck(['shell', 'pm', 'path', configuration.appId]).includes('package:')) {
  throw new Error(`Fixture app is not installed: ${configuration.appId}`)
}
const fixtureSetting = deviceCheck(['shell', 'settings', 'get', 'global', 'walkcache_checkout_selector_stale'])
if (fixtureSetting !== 'null') throw new Error(`Fixture must be stable before measurement; got ${fixtureSetting}`)

const store = new LocalWalkStore({ workspaceRoot: root })
const cacheBefore = await store.lookup({
  goal: 'checkout',
  appId: configuration.appId,
  platform: 'android',
  flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION,
  buildFingerprint,
})
if (mode === 'cold' && cacheBefore.status === 'hit') {
  throw new Error('Cold sample requires successful cache memory to be absent before exploration')
}
if (mode === 'warm' && cacheBefore.status !== 'hit') {
  throw new Error(`Warm sample requires an exact hit; got ${cacheBefore.reason}`)
}

const sampleDirectory = resolve(root, '.deja/metrics/walk005', sampleId)
await mkdir(sampleDirectory, { recursive: true })
const command = mode === 'cold'
  ? [resolve(root, 'scripts/deja.ts'), 'explore', 'checkout', '--config', 'explore.config.json']
  : [resolve(root, 'scripts/deja.ts'), 'run', 'checkout']
const startedAt = new Date().toISOString()
const started = performance.now()
const child = spawn(process.execPath, command, {
  cwd: root,
  env: process.env,
  stdio: ['ignore', 'pipe', 'pipe'],
})
const stdout: string[] = []
const stderr: string[] = []
child.stdout.setEncoding('utf8')
child.stderr.setEncoding('utf8')
child.stdout.on('data', chunk => {
  stdout.push(chunk)
  process.stdout.write(chunk)
})
child.stderr.on('data', chunk => {
  stderr.push(chunk)
  process.stderr.write(chunk)
})
const commandExitCode = await new Promise<number | null>(resolveExit => {
  child.once('error', error => stderr.push(String(error)))
  child.once('close', code => resolveExit(code))
})
const commandWallDurationMs = Math.round(performance.now() - started)
const endedAt = new Date().toISOString()
const commandStdoutPath = resolve(sampleDirectory, 'command.stdout.log')
const commandStderrPath = resolve(sampleDirectory, 'command.stderr.log')
await writeFile(commandStdoutPath, stdout.join(''))
await writeFile(commandStderrPath, stderr.join(''))

const reportMatch = /Déjà report: (.+\.json)/.exec(stdout.join(''))
if (!reportMatch) throw new Error(`No lifecycle report path found for ${sampleId}`)
const lifecycleReportPath = resolve(root, reportMatch[1])
const runReport = JSON.parse(await readFile(lifecycleReportPath, 'utf8')) as Record<string, unknown>
const verification = runReport.verification && typeof runReport.verification === 'object'
  ? runReport.verification as Record<string, unknown>
  : null
function optionalNumber(value: unknown) {
  return typeof value === 'number' && Number.isFinite(value) ? value : null
}
const runnerDurationMs = mode === 'cold'
  ? optionalNumber(verification?.durationMs)
  : optionalNumber(runReport.durationMs)
const modelCalls = optionalNumber(runReport.modelCalls)
const mcpCalls = mode === 'cold' ? optionalNumber(runReport.mcpCalls) : 0
const flowPath = typeof runReport.flowPath === 'string' ? runReport.flowPath : null
const flowContents = flowPath ? await readFile(resolve(root, flowPath), 'utf8') : ''
const assertVisibleCount = (flowContents.match(/^\s*-\s+assertVisible:/gm) ?? []).length

let modelInstrumentation: MetricSample['modelInstrumentation']
if (mode === 'cold') {
  const sourcePath = typeof runReport.openClawResultPath === 'string' ? runReport.openClawResultPath : null
  if (!sourcePath) throw new Error('Cold sample is missing raw OpenClaw instrumentation')
  const rawInstrumentation = await Bun.file(resolve(root, sourcePath)).arrayBuffer()
  const parsed = JSON.parse(Buffer.from(rawInstrumentation).toString('utf8')) as Record<string, unknown>
  if (optionalNumber(parsed.assistantTurns) !== modelCalls) {
    throw new Error('Cold modelCalls does not match OpenClaw assistantTurns')
  }
  modelInstrumentation = {
    source: 'openclaw-result',
    sourcePath,
    sourceSha256: createHash('sha256').update(Buffer.from(rawInstrumentation)).digest('hex'),
    ...(modelCalls !== null ? { assistantTurns: modelCalls } : {}),
    ...(typeof parsed.model === 'string' ? { model: parsed.model } : {}),
    provider: typeof parsed.provider === 'string' ? parsed.provider : null,
  }
} else {
  if (modelCalls !== 0) throw new Error('Direct warm cache hit must report zero model calls')
  modelInstrumentation = { source: 'direct-cache-hit-contract' }
}

const sample: MetricSample = {
  schemaVersion: 1,
  sampleId,
  pairId,
  mode,
  startedAt,
  endedAt,
  commandWallDurationMs,
  commandExitCode,
  command: ['bun', 'scripts/deja.ts', ...command.slice(1)],
  conditions: {
    goal: 'checkout',
    appId: configuration.appId,
    platform: configuration.platform,
    device: configuration.device,
    deviceState,
    fixtureMode: 'stable',
    buildPath: configuration.buildPath,
    buildFingerprint,
    buildInstalledBeforeSeries: true,
    emulatorBootIncluded: false,
    buildOrInstallIncluded: false,
  },
  cacheBefore: { status: cacheBefore.status, ...(cacheBefore.status === 'miss' ? { reason: cacheBefore.reason } : {}) },
  metrics: {
    runnerDurationMs,
    lifecycleDurationMs: mode === 'cold' ? optionalNumber(runReport.durationMs) : null,
    explorationDurationMs: mode === 'cold' ? optionalNumber(runReport.explorationDurationMs) : null,
    verificationWrapperDurationMs: mode === 'cold' ? optionalNumber(runReport.verificationDurationMs) : null,
    mcpCalls,
    mcpCallBasis: mode === 'cold'
      ? String(runReport.mcpCallBasis ?? 'unknown')
      : 'direct cache-hit run spawns no Maestro MCP process',
    modelCalls,
    modelCallBasis: mode === 'cold'
      ? String(runReport.modelCallBasis ?? 'unknown')
      : 'direct cache-hit run spawns no OpenClaw/model process',
    cacheStatus: String(runReport.cacheStatus ?? 'unknown'),
    passed: runReport.passed === true,
  },
  modelInstrumentation,
  flow: {
    path: flowPath,
    sha256: typeof runReport.flowSha256 === 'string' ? runReport.flowSha256 : null,
    assertVisibleCount,
    finalProductOutcomeAssertion: flowContents.includes('Order #WC-1001'),
  },
  artifacts: {
    commandStdoutPath: `.deja/metrics/walk005/${sampleId}/command.stdout.log`,
    commandStderrPath: `.deja/metrics/walk005/${sampleId}/command.stderr.log`,
    lifecycleReportPath: reportMatch[1],
  },
  runReport,
}
const samplePath = resolve(root, '.deja/metrics/walk005/samples', `${sampleId}.json`)
await mkdir(resolve(root, '.deja/metrics/walk005/samples'), { recursive: true })
await writeFile(samplePath, `${JSON.stringify(sample, null, 2)}\n`)
console.log(JSON.stringify({
  samplePath: `.deja/metrics/walk005/samples/${sampleId}.json`,
  sampleId,
  commandExitCode,
  commandWallDurationMs,
  runnerDurationMs,
  modelCalls,
  mcpCalls,
  passed: sample.metrics.passed,
}, null, 2))
process.exitCode = commandExitCode ?? 1
