import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { delimiter, relative, resolve } from 'node:path'
import {
  loadExploreConfiguration,
  runColdExploration,
  type ColdExploreReport,
  type ExplorationMetrics,
  type PersistenceDetails,
  type VerificationDetails,
} from '../../../packages/agent-openclaw/src/index.ts'
import {
  classifyMaestroFailure,
  runMaestro,
  type MaestroRunResult,
} from '../../../packages/maestro/src/index.ts'

type ExploreDependencies = {
  root: string
  maestro: string
  openClawExecutable: string
  androidSdkRoot?: string
  javaHome?: string
  persistVerified: (
    goal: string,
    candidatePath: string,
    buildFingerprint: string,
    verification: VerificationDetails,
    metrics: ExplorationMetrics,
  ) => Promise<PersistenceDetails>
}

function executionDetails(result: MaestroRunResult, root: string): VerificationDetails {
  const outputPath = result.artifacts ? relative(root, result.artifacts.outputPath) : null
  return {
    runner: 'WALK-002 runMaestro',
    freshClearStateLaunch: true,
    startedAt: result.startedAt,
    endedAt: result.endedAt,
    durationMs: result.durationMs,
    monotonicDurationMs: result.monotonicDurationMs,
    exitCode: result.exitCode,
    signal: result.signal,
    timedOut: result.timedOut,
    passed: result.passed,
    errors: result.errors,
    stdoutPath: result.artifacts ? relative(root, result.artifacts.stdoutPath) : null,
    stderrPath: result.artifacts ? relative(root, result.artifacts.stderrPath) : null,
    outputPath,
    failureClassification: classifyMaestroFailure({
      passed: result.passed,
      timedOut: result.timedOut,
      output: result.output,
      errors: result.errors,
      rawArtifactPath: outputPath,
    }),
  }
}

async function clearAndroidApp(appId: string, device: string, root: string, androidSdkRoot?: string) {
  const adb = androidSdkRoot ? resolve(androidSdkRoot, 'platform-tools', 'adb') : 'adb'
  const child = spawn(adb, ['-s', device, 'shell', 'pm', 'clear', appId], {
    cwd: root,
    stdio: ['ignore', 'pipe', 'pipe'],
    detached: process.platform !== 'win32',
    windowsHide: true,
  })
  const stdout: string[] = []
  const stderr: string[] = []
  child.stdout.setEncoding('utf8')
  child.stderr.setEncoding('utf8')
  child.stdout.on('data', chunk => stdout.push(chunk))
  child.stderr.on('data', chunk => stderr.push(chunk))
  let timedOut = false
  const timer = setTimeout(() => {
    timedOut = true
    if (child.pid) {
      try {
        process.kill(process.platform === 'win32' ? child.pid : -child.pid, 'SIGTERM')
      } catch {
        // The bounded adb process may already have exited.
      }
    }
  }, 30_000)
  const exitCode = await new Promise<number | null>(resolveExit => {
    child.once('error', () => resolveExit(null))
    child.once('close', code => resolveExit(code))
  })
  clearTimeout(timer)
  if (timedOut || exitCode !== 0) {
    throw new Error(`adb pm clear failed for ${appId} on ${device}: exit ${String(exitCode)}${timedOut ? ' (timeout)' : ''}`)
  }
}

export async function exploreGoal(
  goal: string,
  configPath: string,
  dependencies: ExploreDependencies,
): Promise<ColdExploreReport> {
  const configuration = loadExploreConfiguration(configPath)
  if (resolve(configuration.workspaceRoot) !== resolve(dependencies.root)) {
    throw new Error('Explore config workspaceRoot must match the explicit command workspace')
  }
  const buildFile = Bun.file(resolve(configuration.workspaceRoot, configuration.buildPath))
  const buildFingerprint = createHash('sha256').update(Buffer.from(await buildFile.arrayBuffer())).digest('hex')
  const attemptDirectory = resolve(
    dependencies.root,
    `.walkcache/attempts/explore-${Date.now()}-${process.pid}`,
  )
  const maestroMcpEnvironment: Record<string, string> = {
    WALKCACHE_MAESTRO: dependencies.maestro,
    WALKCACHE_DEVICE: configuration.device,
    MAESTRO_CLI_NO_ANALYTICS: '1',
    MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED: 'true',
  }
  const pathPrefixes: string[] = []
  if (dependencies.androidSdkRoot) {
    maestroMcpEnvironment.ANDROID_HOME = dependencies.androidSdkRoot
    maestroMcpEnvironment.ANDROID_SDK_ROOT = dependencies.androidSdkRoot
    pathPrefixes.push(resolve(dependencies.androidSdkRoot, 'platform-tools'))
  }
  if (dependencies.javaHome) {
    maestroMcpEnvironment.JAVA_HOME = dependencies.javaHome
    pathPrefixes.push(resolve(dependencies.javaHome, 'bin'))
  }
  maestroMcpEnvironment.PATH = [...pathPrefixes, process.env.PATH ?? ''].filter(Boolean).join(delimiter)

  return runColdExploration({
    goal,
    configuration,
    buildFingerprint,
    attemptDirectory,
    openClawExecutable: dependencies.openClawExecutable,
    maestroMcpProxy: resolve(dependencies.root, 'scripts/maestro-mcp-proxy.ts'),
    maestroMcpEnvironment,
  }, {
    clearAppState: async () => {
      await clearAndroidApp(
        configuration.appId,
        configuration.device,
        configuration.workspaceRoot,
        dependencies.androidSdkRoot,
      )
    },
    verifyCandidate: async (candidatePath, attempt) => {
      const result = await runMaestro({
        executable: dependencies.maestro,
        device: configuration.device,
        platform: 'android',
        workspaceRoot: configuration.workspaceRoot,
        flowPath: candidatePath,
        timeoutMs: configuration.verificationTimeoutMs,
        androidSdkRoot: dependencies.androidSdkRoot,
        javaHome: dependencies.javaHome,
        artifacts: { directory: resolve(attempt, 'verification') },
      })
      if (result.stdout) console.log(result.stdout.trimEnd())
      if (result.stderr) console.error(result.stderr.trimEnd())
      for (const error of result.errors) {
        console.error(`Maestro ${error.kind} error: ${error.message}`)
      }
      return executionDetails(result, dependencies.root)
    },
    persistVerified: async (candidatePath, verification, metrics) => {
      return dependencies.persistVerified(goal, candidatePath, buildFingerprint, verification, metrics)
    },
  })
}
