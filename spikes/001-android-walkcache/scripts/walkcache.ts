#!/usr/bin/env bun
import { createHash } from 'node:crypto'
import { copyFile, mkdir, readFile, rename, writeFile } from 'node:fs/promises'
import { homedir } from 'node:os'
import { dirname, relative, resolve, sep } from 'node:path'

type Walk = {
  goalKey: string
  goal: string
  appId: string
  platform: 'android'
  buildFingerprint: string
  flowPath: string
  lastValidation: { passed: boolean; at: string }
}

type Memory = { schemaVersion: 1; walks: Walk[] }

const root = process.cwd()
const memoryPath = resolve(root, '.walkcache/memory/walks.json')
const apkPath = resolve(root, 'app/build/outputs/apk/debug/app-debug.apk')
const device = process.env.WALKCACHE_DEVICE ?? 'emulator-5554'
const appId = 'dev.walkcache.checkout'
const maestro = resolve(homedir(), '.maestro/bin/maestro')
const androidHome = '/opt/homebrew/share/android-commandlinetools'
const javaHome = '/opt/homebrew/opt/openjdk@21'

function normalize(value: string) {
  return value.trim().toLowerCase().replace(/\s+/g, ' ')
}

function slug(value: string) {
  return normalize(value).replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '')
}

function insideRoot(path: string) {
  const candidate = resolve(root, path)
  if (candidate !== root && !candidate.startsWith(`${root}${sep}`)) {
    throw new Error(`Flow must be inside the spike workspace: ${path}`)
  }
  return candidate
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

async function loadMemory(): Promise<Memory> {
  try {
    const parsed = JSON.parse(await readFile(memoryPath, 'utf8')) as Memory
    if (parsed.schemaVersion !== 1 || !Array.isArray(parsed.walks)) {
      throw new Error('Unsupported walk memory schema')
    }
    return parsed
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') {
      return { schemaVersion: 1, walks: [] }
    }
    throw error
  }
}

async function runMaestro(flowPath: string) {
  const started = new Date()
  const startedMs = performance.now()
  const processHandle = Bun.spawn(
    [maestro, '--no-ansi', '--device', device, 'test', flowPath],
    {
      cwd: root,
      env: {
        ...process.env,
        ANDROID_HOME: androidHome,
        ANDROID_SDK_ROOT: androidHome,
        JAVA_HOME: javaHome,
        MAESTRO_CLI_NO_ANALYTICS: '1',
        MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED: 'true',
        PATH: `${androidHome}/platform-tools:${javaHome}/bin:${process.env.PATH ?? ''}`,
      },
      stdout: 'pipe',
      stderr: 'pipe',
    },
  )
  const [exitCode, stdout, stderr] = await Promise.all([
    processHandle.exited,
    new Response(processHandle.stdout).text(),
    new Response(processHandle.stderr).text(),
  ])
  const ended = new Date()
  if (stdout) console.log(stdout.trimEnd())
  if (stderr) console.error(stderr.trimEnd())
  return {
    startedAt: started.toISOString(),
    endedAt: ended.toISOString(),
    durationMs: Math.round(ended.getTime() - started.getTime()),
    monotonicDurationMs: Math.round(performance.now() - startedMs),
    exitCode,
    passed: exitCode === 0,
    output: `${stdout}${stderr}`,
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

async function remember(goalKey: string, flowArgument: string | undefined) {
  if (!flowArgument) throw new Error('remember requires --flow <candidate.yaml>')
  const candidate = insideRoot(flowArgument)
  const buildFingerprint = await fingerprint()
  const result = await runMaestro(candidate)
  const outputPath = `.walkcache/attempts/remember-${Date.now()}.log`
  await atomicWrite(resolve(root, outputPath), result.output)

  if (!result.passed) {
    await saveReport('remember', {
      goalKey: normalize(goalKey), appId, platform: 'android', buildFingerprint,
      cacheStatus: 'rejected', modelCalls: 0, verificationOnly: true,
      passed: false, durationMs: result.durationMs, outputPath,
    })
    process.exitCode = 1
    return
  }

  const flowPath = `.walkcache/flows/${slug(goalKey)}/android.yaml`
  await mkdir(dirname(resolve(root, flowPath)), { recursive: true })
  const temporaryFlow = `${flowPath}.${process.pid}.tmp`
  await copyFile(candidate, resolve(root, temporaryFlow))
  await rename(resolve(root, temporaryFlow), resolve(root, flowPath))

  const memory = await loadMemory()
  const walk: Walk = {
    goalKey: normalize(goalKey),
    goal: goalKey,
    appId,
    platform: 'android',
    buildFingerprint,
    flowPath,
    lastValidation: { passed: true, at: result.endedAt },
  }
  const remaining = memory.walks.filter(item => !(
    item.goalKey === walk.goalKey && item.appId === appId && item.platform === 'android'
  ))
  await atomicWrite(memoryPath, `${JSON.stringify({ schemaVersion: 1, walks: [...remaining, walk] }, null, 2)}\n`)
  await saveReport('remember', {
    goalKey: walk.goalKey, appId, platform: 'android', buildFingerprint,
    flowPath, cacheStatus: 'recorded', modelCalls: 0, verificationOnly: true,
    passed: true, durationMs: result.durationMs, outputPath,
  })
}

async function run(goalKey: string) {
  const buildFingerprint = await fingerprint()
  const memory = await loadMemory()
  const walk = memory.walks.find(item =>
    item.goalKey === normalize(goalKey) &&
    item.appId === appId &&
    item.platform === 'android' &&
    item.buildFingerprint === buildFingerprint
  )

  if (!walk) {
    await saveReport('run', {
      goalKey: normalize(goalKey), appId, platform: 'android', buildFingerprint,
      cacheStatus: 'miss', modelCalls: 0, passed: false, durationMs: 0,
    })
    process.exitCode = 2
    return
  }

  const flowPath = insideRoot(walk.flowPath)
  const result = await runMaestro(flowPath)
  const outputPath = `.walkcache/reports/run-${Date.now()}.log`
  await atomicWrite(resolve(root, outputPath), result.output)
  walk.lastValidation = { passed: result.passed, at: result.endedAt }
  await atomicWrite(memoryPath, `${JSON.stringify(memory, null, 2)}\n`)
  await saveReport('run', {
    goalKey: walk.goalKey, appId, platform: 'android', buildFingerprint,
    flowPath: relative(root, flowPath), cacheStatus: 'hit', modelCalls: 0,
    passed: result.passed, durationMs: result.durationMs, outputPath,
  })
  if (!result.passed) process.exitCode = 1
}

try {
  const [command, goalKey, ...flags] = Bun.argv.slice(2)
  if (!command || !goalKey || !['remember', 'run'].includes(command)) {
    throw new Error('Usage: bun scripts/walkcache.ts remember <goal> --flow <candidate.yaml> | run <goal>')
  }
  const flowFlag = flags.indexOf('--flow')
  const flow = flowFlag >= 0 ? flags[flowFlag + 1] : undefined
  if (command === 'remember') await remember(goalKey, flow)
  else await run(goalKey)
} catch (error) {
  console.error(error instanceof Error ? error.message : error)
  process.exitCode = 1
}

