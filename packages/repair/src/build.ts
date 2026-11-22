import { spawn } from 'node:child_process'
import { mkdir, writeFile } from 'node:fs/promises'
import { delimiter, resolve } from 'node:path'

export type RepairBuildResult = {
  command: string[]
  startedAt: string
  endedAt: string
  durationMs: number
  monotonicDurationMs: number
  exitCode: number | null
  signal: NodeJS.Signals | null
  timedOut: boolean
  passed: boolean
  stdout: string
  stderr: string
  output: string
  errors: string[]
  installCommand?: string[]
  installExitCode?: number | null
}

export type RepairBuildOptions = {
  workspaceRoot: string
  timeoutMs: number
  javaHome?: string
  androidSdkRoot?: string
  gradleExecutable?: string
}

export type RepairBuildInstallOptions = RepairBuildOptions & {
  device: string
  apkPath: string
  adbExecutable?: string
}

type CommandOptions = {
  executable: string
  args: string[]
  cwd: string
  timeoutMs: number
  env: NodeJS.ProcessEnv
}

function errorCode(error: unknown) {
  return (error as NodeJS.ErrnoException).code
}

function terminateTree(pid: number, signal: NodeJS.Signals) {
  try {
    if (process.platform === 'win32') {
      spawn('taskkill', ['/pid', String(pid), '/T', '/F'], { stdio: 'ignore', windowsHide: true })
        .on('error', () => {})
      return
    }
    process.kill(-pid, signal)
  } catch (error) {
    if (errorCode(error) === 'ESRCH') return
    try {
      process.kill(pid, signal)
    } catch (fallback) {
      if (errorCode(fallback) !== 'ESRCH') throw fallback
    }
  }
}

async function runCommand(options: CommandOptions): Promise<RepairBuildResult> {
  if (!Number.isInteger(options.timeoutMs) || options.timeoutMs <= 0) {
    throw new TypeError('Each bounded build phase requires a positive remaining time budget')
  }
  const started = new Date()
  const monotonicStart = performance.now()
  const child = spawn(options.executable, options.args, {
    cwd: options.cwd,
    env: options.env,
    detached: process.platform !== 'win32',
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
  })
  const stdout: string[] = []
  const stderr: string[] = []
  child.stdout.setEncoding('utf8')
  child.stderr.setEncoding('utf8')
  child.stdout.on('data', chunk => stdout.push(chunk))
  child.stderr.on('data', chunk => stderr.push(chunk))

  let spawned = false
  let settled = false
  let timedOut = false
  let forceTimer: NodeJS.Timeout | undefined
  child.once('spawn', () => { spawned = true })
  const completion = new Promise<{ exitCode: number | null; signal: NodeJS.Signals | null; launchError?: unknown }>(
    resolveCompletion => {
      child.once('error', error => {
        if (!spawned && !settled) {
          settled = true
          resolveCompletion({ exitCode: null, signal: null, launchError: error })
        }
      })
      child.once('close', (exitCode, signal) => {
        if (!settled) {
          settled = true
          resolveCompletion({ exitCode, signal })
        }
      })
    },
  )
  const timer = setTimeout(() => {
    if (settled || child.pid === undefined) return
    timedOut = true
    terminateTree(child.pid, 'SIGTERM')
    if (process.platform !== 'win32') {
      forceTimer = setTimeout(() => {
        if (!settled && child.pid !== undefined) terminateTree(child.pid, 'SIGKILL')
      }, 250)
    }
  }, options.timeoutMs)
  const completed = await completion
  clearTimeout(timer)
  if (forceTimer) clearTimeout(forceTimer)

  const ended = new Date()
  const errors: string[] = []
  if (completed.launchError) errors.push(`Unable to launch bounded command: ${options.executable}`)
  else if (timedOut) errors.push(`Bounded command exceeded its remaining ${options.timeoutMs} ms budget`)
  else if (completed.exitCode !== 0) errors.push(`Command exited with code ${String(completed.exitCode)}`)
  return {
    command: [options.executable, ...options.args],
    startedAt: started.toISOString(),
    endedAt: ended.toISOString(),
    durationMs: Math.max(0, ended.getTime() - started.getTime()),
    monotonicDurationMs: Math.round(performance.now() - monotonicStart),
    exitCode: completed.exitCode,
    signal: completed.signal,
    timedOut,
    passed: completed.exitCode === 0 && errors.length === 0,
    stdout: stdout.join(''),
    stderr: stderr.join(''),
    output: `${stdout.join('')}${stderr.join('')}`,
    errors,
  }
}

function buildEnvironment(options: RepairBuildOptions) {
  const pathPrefixes: string[] = []
  const env: NodeJS.ProcessEnv = { ...process.env }
  if (options.androidSdkRoot) {
    env.ANDROID_HOME = options.androidSdkRoot
    env.ANDROID_SDK_ROOT = options.androidSdkRoot
    pathPrefixes.push(resolve(options.androidSdkRoot, 'platform-tools'))
  }
  if (options.javaHome) {
    env.JAVA_HOME = options.javaHome
    pathPrefixes.push(resolve(options.javaHome, 'bin'))
  }
  env.PATH = [...pathPrefixes, process.env.PATH ?? ''].filter(Boolean).join(delimiter)
  return env
}

export async function runCheckoutGradleBuild(options: RepairBuildOptions): Promise<RepairBuildResult> {
  const executable = options.gradleExecutable ?? resolve(options.workspaceRoot, 'gradlew')
  return runCommand({
    executable,
    args: ['clean', ':app:assembleDebug'],
    cwd: options.workspaceRoot,
    timeoutMs: options.timeoutMs,
    env: buildEnvironment(options),
  })
}

export async function runCheckoutBuildAndInstall(
  options: RepairBuildInstallOptions,
): Promise<RepairBuildResult> {
  const started = performance.now()
  const build = await runCheckoutGradleBuild(options)
  if (!build.passed) return build
  const remainingMs = options.timeoutMs - Math.round(performance.now() - started)
  if (remainingMs <= 0) {
    return {
      ...build,
      passed: false,
      timedOut: true,
      errors: [...build.errors, 'The rebuild consumed the entire budget before adb install'],
    }
  }
  const adbExecutable = options.adbExecutable ??
    (options.androidSdkRoot ? resolve(options.androidSdkRoot, 'platform-tools/adb') : 'adb')
  const install = await runCommand({
    executable: adbExecutable,
    args: ['-s', options.device, 'install', '-r', '-g', options.apkPath],
    cwd: options.workspaceRoot,
    timeoutMs: remainingMs,
    env: buildEnvironment(options),
  })
  return {
    command: build.command,
    startedAt: build.startedAt,
    endedAt: install.endedAt,
    durationMs: Math.max(0, Date.parse(install.endedAt) - Date.parse(build.startedAt)),
    monotonicDurationMs: Math.round(performance.now() - started),
    exitCode: install.exitCode,
    signal: install.signal,
    timedOut: build.timedOut || install.timedOut,
    passed: build.passed && install.passed,
    stdout: `${build.stdout}${install.stdout}`,
    stderr: `${build.stderr}${install.stderr}`,
    output: `${build.output}${install.output}`,
    errors: [...build.errors, ...install.errors],
    installCommand: install.command,
    installExitCode: install.exitCode,
  }
}

export async function writeRepairProcessArtifacts(
  directory: string,
  result: Pick<RepairBuildResult, 'stdout' | 'stderr' | 'output'>,
) {
  await mkdir(directory, { recursive: true })
  await Promise.all([
    writeFile(resolve(directory, 'stdout.log'), result.stdout),
    writeFile(resolve(directory, 'stderr.log'), result.stderr),
    writeFile(resolve(directory, 'output.log'), result.output),
  ])
}
