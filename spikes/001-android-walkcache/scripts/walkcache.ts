#!/usr/bin/env bun
import { createHash } from 'node:crypto'
import { mkdir, rename, writeFile } from 'node:fs/promises'
import { homedir } from 'node:os'
import { dirname, relative, resolve } from 'node:path'
import { runMaestro, resolveFlowPath, type MaestroRunResult } from '../../../packages/maestro/src/index.ts'
import type { PersistenceDetails, VerificationDetails } from '../../../packages/agent-openclaw/src/index.ts'
import {
  InvalidWalkMemoryError,
  LOCAL_MEMORY_PATH,
  LocalWalkStore,
  MAESTRO_FLOW_SCHEMA_VERSION,
  type ExplorationDetails,
} from '../../../packages/store-local/src/index.ts'
import { exploreGoal } from './explore.ts'

const root = process.cwd()
const apkPath = resolve(root, 'app/build/outputs/apk/debug/app-debug.apk')
const device = process.env.WALKCACHE_DEVICE ?? 'emulator-5554'
const appId = 'dev.walkcache.checkout'
const maestro = process.env.WALKCACHE_MAESTRO ?? resolve(homedir(), '.maestro/bin/maestro')
const openClawExecutable = process.env.WALKCACHE_OPENCLAW ?? 'openclaw'
const androidHome = process.env.ANDROID_HOME ?? process.env.ANDROID_SDK_ROOT
const javaHome = process.env.JAVA_HOME
const timeoutMs = Number(process.env.WALKCACHE_TIMEOUT_MS ?? '120000')
const walkStore = new LocalWalkStore({ workspaceRoot: root })

function normalize(value: string) {
  return value.trim().toLowerCase().replace(/\s+/g, ' ')
}

async function atomicWrite(path: string, contents: string | Buffer) {
  await mkdir(dirname(path), { recursive: true })
  const temporary = `${path}.${process.pid}.tmp`
  await writeFile(temporary, contents)
  await rename(temporary, path)
}

async function fingerprint() {
  return createHash('sha256').update(Buffer.from(await Bun.file(apkPath).arrayBuffer())).digest('hex')
}

async function runFlow(flowPath: string, artifactDirectory: string) {
  const result = await runMaestro({
    executable: maestro,
    device,
    platform: 'android',
    workspaceRoot: root,
    flowPath,
    timeoutMs,
    androidSdkRoot: androidHome,
    javaHome,
    artifacts: { directory: artifactDirectory },
  })
  if (result.stdout) console.log(result.stdout.trimEnd())
  if (result.stderr) console.error(result.stderr.trimEnd())
  for (const error of result.errors) {
    console.error(`Maestro ${error.kind} error: ${error.message}`)
  }
  return result
}

function executionDetails(result: MaestroRunResult) {
  return {
    startedAt: result.startedAt,
    endedAt: result.endedAt,
    durationMs: result.durationMs,
    monotonicDurationMs: result.monotonicDurationMs,
    exitCode: result.exitCode,
    signal: result.signal,
    timedOut: result.timedOut,
    passed: result.passed,
    errors: result.errors,
    stdoutPath: result.artifacts ? relative(root, result.artifacts.stdoutPath) : undefined,
    stderrPath: result.artifacts ? relative(root, result.artifacts.stderrPath) : undefined,
    outputPath: result.artifacts ? relative(root, result.artifacts.outputPath) : undefined,
  }
}

async function saveReport(command: string, details: Record<string, unknown>) {
  await mkdir(resolve(root, '.walkcache/reports'), { recursive: true })
  const filename = `.walkcache/reports/${command}-${Date.now()}.json`
  const report = { schemaVersion: 1, command, ...details }
  await atomicWrite(resolve(root, filename), `${JSON.stringify(report, null, 2)}\n`)
  await atomicWrite(resolve(root, '.walkcache/reports/latest.json'), `${JSON.stringify(report, null, 2)}\n`)
  console.log(`\nWalkCache report: ${filename}`)
  console.log(JSON.stringify({
    cacheStatus: report.cacheStatus,
    modelCalls: report.modelCalls,
    passed: report.passed,
    durationMs: report.durationMs,
    buildFingerprint: report.buildFingerprint,
  }, null, 2))
}

async function persistVerifiedCandidate(
  goalKey: string,
  candidatePath: string,
  buildFingerprint: string,
  verification: VerificationDetails,
  exploration?: ExplorationDetails,
  origin: 'remember' | 'explore' = 'explore',
): Promise<PersistenceDetails> {
  if (!verification.passed || typeof verification.endedAt !== 'string') {
    throw new Error('Only an independently passing candidate can enter successful storage')
  }
  // The verified candidate gets a content-addressed final flow before the atomic metadata rename.
  // Existing good flow/metadata remain untouched if candidate verification or replacement fails.
  const persisted = await walkStore.persistVerifiedCandidate({
    identity: {
      goal: goalKey,
      appId,
      platform: 'android',
      flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION,
      buildFingerprint,
    },
    candidatePath,
    validation: { passed: verification.passed, at: verification.endedAt },
    origin,
    ...(exploration ? { exploration } : {}),
  })
  return {
    flowPath: persisted.flowPath,
    flowSha256: persisted.flowSha256,
    storedMetadataPath: LOCAL_MEMORY_PATH,
    atomicWrite: persisted.atomicWrite,
  }
}

async function remember(goalKey: string, flowArgument: string | undefined) {
  if (!flowArgument) throw new Error('remember requires --flow <candidate.yaml>')
  const candidate = resolveFlowPath(root, flowArgument)
  const buildFingerprint = await fingerprint()
  const result = await runFlow(candidate, `.walkcache/attempts/remember-${Date.now()}`)

  if (!result.passed) {
    await saveReport('remember', {
      goalKey: normalize(goalKey), appId, platform: 'android', buildFingerprint,
      cacheStatus: 'rejected', modelCalls: 0, verificationOnly: true,
      ...executionDetails(result),
    })
    process.exitCode = 1
    return
  }

  const persisted = await persistVerifiedCandidate(
    goalKey,
    candidate,
    buildFingerprint,
    executionDetails(result),
    undefined,
    'remember',
  )
  await saveReport('remember', {
    goalKey: normalize(goalKey), appId, platform: 'android', buildFingerprint,
    flowPath: persisted.flowPath, cacheStatus: 'recorded', modelCalls: 0, verificationOnly: true,
    ...executionDetails(result),
  })
}

async function explore(goalKey: string, configPath: string) {
  const report = await exploreGoal(goalKey, configPath, {
    root,
    maestro,
    openClawExecutable,
    androidSdkRoot: androidHome,
    javaHome,
    persistVerified: async (
      persistGoal,
      candidatePath,
      buildFingerprint,
      verification,
      metrics,
    ) => persistVerifiedCandidate(
      persistGoal,
      candidatePath,
      buildFingerprint,
      verification,
      metrics,
    ),
  })
  await saveReport('explore', { ...report })
  console.log(JSON.stringify({
    explorationDurationMs: report.explorationDurationMs,
    verificationDurationMs: report.verificationDurationMs,
    mcpCalls: report.mcpCalls,
    modelCallBasis: report.modelCallBasis,
    attemptDirectory: report.attemptDirectory,
    flowPath: report.flowPath,
  }, null, 2))
  if (!report.passed) process.exitCode = 1
}

async function run(goalKey: string) {
  const buildFingerprint = await fingerprint()
  const identity = {
    goal: goalKey,
    appId,
    platform: 'android',
    flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION,
    buildFingerprint,
  }
  const lookup = await walkStore.lookup(identity)

  if (lookup.status !== 'hit') {
    await saveReport('run', {
      goalKey: normalize(goalKey), appId, platform: 'android', buildFingerprint,
      cacheStatus: 'miss', cacheMissReason: lookup.reason, recovery: 'explore-required',
      modelCalls: 0, passed: false, durationMs: 0,
    })
    process.exitCode = 2
    return
  }

  const result = await runFlow(lookup.flowPath, `.walkcache/reports/run-${Date.now()}`)
  await walkStore.recordValidation(identity, { passed: result.passed, at: result.endedAt })
  await saveReport('run', {
    goalKey: lookup.walk.goalKey, appId, platform: 'android', buildFingerprint,
    flowPath: relative(root, lookup.flowPath), cacheStatus: 'hit', modelCalls: 0,
    ...executionDetails(result),
  })
  if (!result.passed) process.exitCode = 1
}

try {
  const [command, goalKey, ...flags] = Bun.argv.slice(2)
  if (!command || !goalKey || !['explore', 'remember', 'run'].includes(command)) {
    throw new Error('Usage: bun scripts/walkcache.ts explore <goal> [--config <explore.config.json>] | remember <goal> --flow <candidate.yaml> | run <goal>')
  }
  const flowFlag = flags.indexOf('--flow')
  const flow = flowFlag >= 0 ? flags[flowFlag + 1] : undefined
  const configFlag = flags.indexOf('--config')
  const configPath = resolve(root, configFlag >= 0 ? flags[configFlag + 1] : 'explore.config.json')
  if (command === 'explore') await explore(goalKey, configPath)
  else if (command === 'remember') await remember(goalKey, flow)
  else await run(goalKey)
} catch (error) {
  console.error(error instanceof Error ? error.message : error)
  if (error instanceof InvalidWalkMemoryError) {
    console.error('Recovery: preserve or quarantine the invalid ignored .walkcache data, then run explore <goal>; never treat it as a passing walk.')
  }
  process.exitCode = 1
}

