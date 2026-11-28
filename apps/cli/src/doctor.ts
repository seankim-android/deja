import { spawn } from 'node:child_process'
import {
  MaestroEngineError,
  dejaHome,
  engineDirectory,
  resolveMaestroEngine,
  maestroChildEnv,
  withoutJvmNotices,
  MAESTRO_ENGINE_PIN,
} from '@deja-run/maestro'
import { TESTED_RANGE_TEXT } from './engine.ts'

export type DoctorStatus = 'ok' | 'warn' | 'fail'
export type DoctorCheck = { name: string; status: DoctorStatus; detail: string }
export type DoctorReport = { ok: boolean; checks: DoctorCheck[] }

/** Runs a read-only command and returns stdout and stderr together. Never throws. */
export type DoctorProbe = (command: string, args: string[], env?: NodeJS.ProcessEnv) => Promise<{ ok: boolean; output: string }>

export const TESTED_JAVA_MIN = 17
export const TESTED_XCODE_MAJOR = 27
export const TESTED_IOS_MAJOR = 27

export const defaultProbe: DoctorProbe = (command, args, env) => new Promise(resolveProbe => {
  let output = ''
  const child = spawn(command, args, { stdio: ['ignore', 'pipe', 'pipe'], ...(env ? { env } : {}) })
  const timer = setTimeout(() => child.kill('SIGKILL'), 20_000)
  child.stdout.setEncoding('utf8')
  child.stderr.setEncoding('utf8')
  child.stdout.on('data', (chunk: string) => { output += chunk })
  child.stderr.on('data', (chunk: string) => { output += chunk })
  child.once('error', error => { clearTimeout(timer); resolveProbe({ ok: false, output: error.message }) })
  child.once('close', code => { clearTimeout(timer); resolveProbe({ ok: code === 0, output }) })
})

export type DoctorOptions = {
  env?: NodeJS.ProcessEnv
  home?: string
  configExecutable?: string
  allowUntested?: boolean
  probe?: DoctorProbe
  platform?: NodeJS.Platform
}

async function engineCheck(options: DoctorOptions, probe: DoctorProbe): Promise<DoctorCheck> {
  const env = options.env ?? process.env
  const home = options.home ?? dejaHome(env)
  try {
    const engine = await resolveMaestroEngine({
      ...(options.configExecutable ? { configExecutable: options.configExecutable } : {}),
      env,
      home,
      probeVersion: async executable =>
        withoutJvmNotices((await probe(executable, ['--version'], maestroChildEnv(executable, { env, home }))).output),
    })
    const version = engine.version ? `Maestro ${engine.version}` : 'Maestro (version unreadable)'
    const origin = engine.source === 'pinned'
      ? 'pinned by Déjà'
      : `override via ${engine.overrideVia === 'config' ? 'maestro.executable' : 'DEJA_MAESTRO'}`
    const detail = `${version}, ${origin}, ${engine.executable}; tested range ${TESTED_RANGE_TEXT}`
    if (engine.range === 'in-range') return { name: 'engine', status: 'ok', detail }
    return {
      name: 'engine',
      status: options.allowUntested ? 'warn' : 'fail',
      detail: `${detail}; outside the tested range${options.allowUntested ? ' (allowed by maestro.allowUntested)' : ''}`,
    }
  } catch (error) {
    if (error instanceof MaestroEngineError) {
      return {
        name: 'engine',
        status: 'fail',
        detail: `${error.message} (expected in ${engineDirectory(home, MAESTRO_ENGINE_PIN.version)})`,
      }
    }
    throw error
  }
}

async function javaCheck(env: NodeJS.ProcessEnv, probe: DoctorProbe): Promise<DoctorCheck> {
  const java = env.JAVA_HOME ? `${env.JAVA_HOME}/bin/java` : 'java'
  const result = await probe(java, ['-version'])
  const match = /version "(\d+)(?:\.(\d+))?[^"]*"/.exec(result.output) ?? /\b(?:openjdk|java) (\d+)/i.exec(result.output)
  if (!result.ok || !match) return { name: 'java', status: 'fail', detail: `Java not found (Maestro needs ${TESTED_JAVA_MIN} or newer)` }
  // Java 8 prints "1.8.0": the major is the second number.
  const major = match[1] === '1' && match[2] ? Number(match[2]) : Number(match[1])
  return major >= TESTED_JAVA_MIN
    ? { name: 'java', status: 'ok', detail: `Java ${major} (needs ${TESTED_JAVA_MIN} or newer)` }
    : { name: 'java', status: 'fail', detail: `Java ${major} is too old (needs ${TESTED_JAVA_MIN} or newer)` }
}

async function xcodeChecks(probe: DoctorProbe): Promise<DoctorCheck[]> {
  const checks: DoctorCheck[] = []
  const xcode = await probe('xcodebuild', ['-version'])
  const version = /Xcode (\d+(?:\.\d+)*)/.exec(xcode.output)
  if (!xcode.ok || !version) {
    checks.push({ name: 'xcode', status: 'warn', detail: 'Xcode not found (needed for iOS only)' })
  } else {
    const major = Number(version[1].split('.')[0])
    checks.push({
      name: 'xcode',
      status: major === TESTED_XCODE_MAJOR ? 'ok' : 'warn',
      detail: `Xcode ${version[1]} (tested ${TESTED_XCODE_MAJOR}.x)`,
    })
  }
  // Read-only: lists installed runtimes. It never boots or touches a simulator.
  const runtimes = await probe('xcrun', ['simctl', 'list', 'runtimes', '--json'])
  let versions: string[] = []
  try {
    const parsed = JSON.parse(runtimes.output) as { runtimes?: { name?: string; version?: string; isAvailable?: boolean }[] }
    versions = (parsed.runtimes ?? [])
      .filter(runtime => runtime.isAvailable !== false && /^iOS/.test(runtime.name ?? ''))
      .map(runtime => String(runtime.version))
  } catch {
    versions = []
  }
  checks.push(versions.length === 0
    ? { name: 'ios-runtime', status: 'warn', detail: 'No iOS simulator runtime found (needed for iOS only)' }
    : {
      name: 'ios-runtime',
      status: versions.some(value => Number(value.split('.')[0]) === TESTED_IOS_MAJOR) ? 'ok' : 'warn',
      detail: `iOS ${versions.join(', ')} (tested ${TESTED_IOS_MAJOR}.x)`,
    })
  return checks
}

export async function runDoctor(options: DoctorOptions = {}): Promise<DoctorReport> {
  const env = options.env ?? process.env
  const probe = options.probe ?? defaultProbe
  const checks: DoctorCheck[] = [
    await engineCheck(options, probe),
    await javaCheck(env, probe),
  ]
  if ((options.platform ?? process.platform) === 'darwin') checks.push(...await xcodeChecks(probe))
  return { ok: checks.every(check => check.status !== 'fail'), checks }
}

export function renderDoctor(report: DoctorReport) {
  const lines = report.checks.map(check => `${check.status.toUpperCase().padEnd(4)} ${check.name}: ${check.detail}`)
  lines.push(report.ok ? 'Doctor: ready' : 'Doctor: not ready (see FAIL lines)')
  return `${lines.join('\n')}\n`
}
