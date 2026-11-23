import { spawn } from 'node:child_process'
import type { MaestroExecutionAdapter, MaestroPlatform } from './platform-adapter.ts'
import { realpathSync, statSync } from 'node:fs'
import { mkdir, realpath, writeFile } from 'node:fs/promises'
import { delimiter, dirname, resolve, sep } from 'node:path'

/** The Maestro flow schema that walk identities and stored flows are keyed on. */
export const MAESTRO_FLOW_SCHEMA_VERSION = 'maestro/v1'

export {
  MaestroWalkRunner,
  countFlowAssertions,
  type MaestroWalkRunnerOptions,
} from './walk-runner.ts'

export {
  COMPOSITION_MODEL_METRICS,
  MAESTRO_COMPOSITE_FLOW_SCHEMA,
  MaestroLoginComposer,
  composeMaestroLoginFlow,
  countMaestroAssertions,
  countMaestroSteps,
  readMaestroContinuation,
  validateMaestroFlowReference,
  type LoginCompositionResult,
  type MaestroFlowContainment,
  type MaestroLoginComposerOptions,
  type SegmentVerificationResult,
} from './subflow-composition.ts'

export {
  readCompositionRunnerTrace,
  type CompositionRunnerObservation,
  type CompositionRunnerTrace,
} from './execution-observations.ts'

export {
  androidMaestroAdapter,
  iosMaestroAdapter,
  webMaestroAdapter,
  type AdapterCacheIdentity,
  type AdapterIdentityInput,
  type AndroidAdapterOptions,
  type IosAdapterOptions,
  type MaestroCommandOptions,
  type MaestroExecutionAdapter,
  type MaestroPlatform,
  type WebAdapterOptions,
} from './platform-adapter.ts'

export {
  classifyMaestroFailure,
  type MaestroFailureCategory,
  type MaestroFailureClassification,
  type MaestroFailureInput,
} from './failure-classification.ts'

export type MaestroRunnerErrorKind =
  | 'configuration'
  | 'flow-path'
  | 'launch'
  | 'timeout'
  | 'execution'
  | 'output-artifact'

export type MaestroRunnerError = {
  kind: MaestroRunnerErrorKind
  message: string
  code?: string
  path?: string
}

export type MaestroArtifactPaths = {
  directory: string
  stdoutPath: string
  stderrPath: string
  outputPath: string
}

export type RunMaestroOptions = {
  executable: string
  device: string
  platform: MaestroPlatform
  adapter?: MaestroExecutionAdapter
  workspaceRoot: string
  flowPath: string
  timeoutMs: number
  androidSdkRoot?: string
  javaHome?: string
  /** Absolute path for Maestro's own JUnit report (`--format=JUNIT --output=`). Opt-in. */
  junitPath?: string
  artifacts?: {
    directory: string
  }
}

export type MaestroRunResult = {
  platform: MaestroPlatform
  device: string
  flowPath: string
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
  errors: MaestroRunnerError[]
  artifacts?: MaestroArtifactPaths
}

function insideRoot(root: string, candidate: string) {
  return candidate === root || candidate.startsWith(`${root}${sep}`)
}

function errorCode(error: unknown) {
  return (error as NodeJS.ErrnoException).code
}

function structuredError(
  kind: MaestroRunnerErrorKind,
  message: string,
  error?: unknown,
  path?: string,
): MaestroRunnerError {
  const code = errorCode(error)
  return {
    kind,
    message,
    ...(code ? { code } : {}),
    ...(path ? { path } : {}),
  }
}

export function resolveFlowPath(workspaceRoot: string, flowPath: string) {
  const root = realpathSync(resolve(workspaceRoot))
  const candidate = resolve(root, flowPath)
  if (!insideRoot(root, candidate) || candidate === root) {
    throw new Error(`Flow must be inside the workspace: ${flowPath}`)
  }

  const canonical = realpathSync(candidate)
  if (!insideRoot(root, canonical) || !statSync(canonical).isFile()) {
    throw new Error(`Flow must resolve to a file inside the workspace: ${flowPath}`)
  }
  return canonical
}

function resolveArtifactDirectory(workspaceRoot: string, directory: string) {
  const root = realpathSync(resolve(workspaceRoot))
  const candidate = resolve(root, directory)
  if (!insideRoot(root, candidate) || candidate === root) {
    throw new Error(`Artifact directory must be inside the workspace: ${directory}`)
  }

  let ancestor = candidate
  while (!insideRoot(root, ancestor)) {
    ancestor = dirname(ancestor)
  }
  while (true) {
    try {
      const canonicalAncestor = realpathSync(ancestor)
      if (!insideRoot(root, canonicalAncestor)) {
        throw new Error(`Artifact directory must stay inside the workspace: ${directory}`)
      }
      break
    } catch (error) {
      if (errorCode(error) !== 'ENOENT') throw error
      const parent = dirname(ancestor)
      if (parent === ancestor) throw error
      ancestor = parent
    }
  }
  return candidate
}

function validateOptions(options: RunMaestroOptions) {
  if (!options.executable.trim()) throw new TypeError('A Maestro executable is required')
  if (!options.device.trim()) throw new TypeError('A Maestro device is required')
  if (!Number.isInteger(options.timeoutMs) || options.timeoutMs <= 0 || options.timeoutMs > 2_147_483_647) {
    throw new TypeError('timeoutMs must be a positive integer no greater than 2147483647')
  }
}

function maestroEnvironment(options: RunMaestroOptions) {
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    MAESTRO_CLI_NO_ANALYTICS: '1',
    MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED: 'true',
  }
  const pathPrefixes: string[] = []
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

function terminateProcessTree(pid: number, signal: NodeJS.Signals) {
  try {
    if (process.platform === 'win32') {
      const killer = spawn('taskkill', ['/pid', String(pid), '/T', '/F'], {
        stdio: 'ignore',
        windowsHide: true,
      })
      killer.on('error', () => {})
      return
    }
    // The child is detached, so its process-group ID is its PID. A negative PID targets only
    // that group, including Maestro descendants, never the caller's group or an emulator.
    process.kill(-pid, signal)
  } catch (error) {
    if (errorCode(error) === 'ESRCH') return
    try {
      process.kill(pid, signal)
    } catch (fallbackError) {
      if (errorCode(fallbackError) !== 'ESRCH') throw fallbackError
    }
  }
}

function emptyResult(
  options: RunMaestroOptions,
  started: Date,
  startedMonotonic: number,
  error: MaestroRunnerError,
): MaestroRunResult {
  const ended = new Date()
  return {
    platform: options.platform,
    device: options.device,
    flowPath: options.flowPath,
    startedAt: started.toISOString(),
    endedAt: ended.toISOString(),
    durationMs: Math.max(0, ended.getTime() - started.getTime()),
    monotonicDurationMs: Math.round(performance.now() - startedMonotonic),
    exitCode: null,
    signal: null,
    timedOut: false,
    passed: false,
    stdout: '',
    stderr: '',
    output: '',
    errors: [error],
  }
}

async function writeArtifacts(
  workspaceRoot: string,
  configuration: RunMaestroOptions['artifacts'],
  stdout: string,
  stderr: string,
  output: string,
): Promise<MaestroArtifactPaths | undefined> {
  if (!configuration) return undefined
  const directory = resolveArtifactDirectory(workspaceRoot, configuration.directory)
  await mkdir(directory, { recursive: true })
  const canonicalDirectory = await realpath(directory)
  const root = await realpath(resolve(workspaceRoot))
  if (!insideRoot(root, canonicalDirectory)) {
    throw new Error(`Artifact directory must stay inside the workspace: ${configuration.directory}`)
  }

  const artifacts = {
    directory: canonicalDirectory,
    stdoutPath: resolve(canonicalDirectory, 'stdout.log'),
    stderrPath: resolve(canonicalDirectory, 'stderr.log'),
    outputPath: resolve(canonicalDirectory, 'output.log'),
  }
  await writeFile(artifacts.stdoutPath, stdout)
  await writeFile(artifacts.stderrPath, stderr)
  await writeFile(artifacts.outputPath, output)
  return artifacts
}

export async function runMaestro(options: RunMaestroOptions): Promise<MaestroRunResult> {
  validateOptions(options)
  const started = new Date()
  const startedMonotonic = performance.now()

  let flowPath: string
  try {
    flowPath = resolveFlowPath(options.workspaceRoot, options.flowPath)
  } catch (error) {
    return emptyResult(
      options,
      started,
      startedMonotonic,
      structuredError('flow-path', error instanceof Error ? error.message : String(error), error, options.flowPath),
    )
  }

  const child = spawn(
    options.executable,
    options.adapter?.commandArguments(flowPath, { junitPath: options.junitPath }) ??
      [
        '--no-ansi', '--device', options.device, 'test',
        ...(options.junitPath ? ['--format=JUNIT', `--output=${options.junitPath}`] : []),
        flowPath,
      ],
    {
      cwd: resolve(options.workspaceRoot),
      env: maestroEnvironment(options),
      detached: process.platform !== 'win32',
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    },
  )

  const stdoutChunks: string[] = []
  const stderrChunks: string[] = []
  child.stdout.setEncoding('utf8')
  child.stderr.setEncoding('utf8')
  child.stdout.on('data', (chunk: string) => stdoutChunks.push(chunk))
  child.stderr.on('data', (chunk: string) => stderrChunks.push(chunk))

  let spawned = false
  let settled = false
  let timedOut = false
  let forceTimer: NodeJS.Timeout | undefined
  child.once('spawn', () => {
    spawned = true
  })

  const completion = new Promise<{
    exitCode: number | null
    signal: NodeJS.Signals | null
    launchError?: unknown
  }>((resolveCompletion) => {
    child.once('error', (error) => {
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
  })

  const timeoutTimer = setTimeout(() => {
    if (settled || child.pid === undefined) return
    timedOut = true
    try {
      terminateProcessTree(child.pid, 'SIGTERM')
      if (process.platform !== 'win32') {
        forceTimer = setTimeout(() => {
          if (!settled && child.pid !== undefined) {
            try {
              terminateProcessTree(child.pid, 'SIGKILL')
            } catch {
              // The close event still determines the result; do not touch any other process.
            }
          }
        }, 250)
      }
    } catch {
      // If termination itself fails, the close event and timeout error still report the failure.
    }
  }, options.timeoutMs)

  const completed = await completion
  clearTimeout(timeoutTimer)
  if (forceTimer) clearTimeout(forceTimer)

  const ended = new Date()
  const stdout = stdoutChunks.join('')
  const stderr = stderrChunks.join('')
  const output = `${stdout}${stderr}`
  const errors: MaestroRunnerError[] = []

  if (completed.launchError) {
    errors.push(structuredError(
      'launch',
      `Unable to launch Maestro executable: ${options.executable}`,
      completed.launchError,
      options.executable,
    ))
  } else if (timedOut) {
    errors.push({
      kind: 'timeout',
      message: `Maestro exceeded the configured ${options.timeoutMs} ms timeout; its process tree was terminated`,
      code: 'ETIMEDOUT',
    })
  } else if (completed.exitCode !== 0) {
    errors.push({
      kind: 'execution',
      message: `Maestro exited with code ${String(completed.exitCode)}${completed.signal ? ` (${completed.signal})` : ''}`,
      code: 'MAESTRO_EXIT',
    })
  }

  let artifacts: MaestroArtifactPaths | undefined
  try {
    artifacts = await writeArtifacts(options.workspaceRoot, options.artifacts, stdout, stderr, output)
  } catch (error) {
    errors.push(structuredError(
      'output-artifact',
      error instanceof Error ? error.message : String(error),
      error,
      options.artifacts?.directory,
    ))
  }

  return {
    platform: options.platform,
    device: options.device,
    flowPath,
    startedAt: started.toISOString(),
    endedAt: ended.toISOString(),
    durationMs: Math.max(0, ended.getTime() - started.getTime()),
    monotonicDurationMs: Math.round(performance.now() - startedMonotonic),
    exitCode: completed.exitCode,
    signal: completed.signal,
    timedOut,
    passed: completed.exitCode === 0 && errors.length === 0,
    stdout,
    stderr,
    output,
    errors,
    ...(artifacts ? { artifacts } : {}),
  }
}
