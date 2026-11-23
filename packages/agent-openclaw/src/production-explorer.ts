import { spawn } from 'node:child_process'
import { delimiter, relative, resolve, sep } from 'node:path'
import type {
  ExplorationResult,
  ExploreRequest,
  RunArtifact,
  WalkExplorer,
  WalkFailure,
  WalkStore,
} from '@deja-qa/core'
import {
  countFlowAssertions,
  MaestroWalkRunner,
} from '@deja-qa/maestro'
import {
  runColdExploration,
  type ColdExploreReport,
  type ExploreConfiguration,
  type ExplorationMetrics,
  type VerificationDetails,
} from './index.ts'

export type OpenClawWalkExplorerOptions = {
  configuration: ExploreConfiguration
  buildFingerprint: string
  openClawExecutable: string
  maestroMcpProxy: string
  runner: MaestroWalkRunner
  store: WalkStore
}

function portable(root: string, path: string) {
  return relative(root, path).split('\\').join('/')
}

async function clearAndroidApp(configuration: ExploreConfiguration) {
  const sdk = process.env.ANDROID_HOME ?? process.env.ANDROID_SDK_ROOT
  const adb = sdk ? resolve(sdk, 'platform-tools', 'adb') : 'adb'
  const child = spawn(adb, ['-s', configuration.device, 'shell', 'pm', 'clear', configuration.appId], {
    cwd: configuration.workspaceRoot,
    stdio: ['ignore', 'pipe', 'pipe'],
    detached: process.platform !== 'win32',
    windowsHide: true,
  })
  child.stdout.resume()
  child.stderr.resume()
  let timedOut = false
  const timer = setTimeout(() => {
    timedOut = true
    if (child.pid) {
      try {
        process.kill(process.platform === 'win32' ? child.pid : -child.pid, 'SIGTERM')
      } catch {
        // The bounded child close event remains authoritative.
      }
    }
  }, 30_000)
  const exitCode = await new Promise<number | null>(resolveExit => {
    child.once('error', () => resolveExit(null))
    child.once('close', code => resolveExit(code))
  })
  clearTimeout(timer)
  if (timedOut || exitCode !== 0) {
    throw new Error(`adb pm clear failed for the configured fixture: exit ${String(exitCode)}${timedOut ? ' (timeout)' : ''}`)
  }
}

function mcpEnvironment(configuration: ExploreConfiguration, executable: string) {
  const env: Record<string, string> = {
    DEJA_MAESTRO: executable,
    DEJA_DEVICE: configuration.device,
    MAESTRO_CLI_NO_ANALYTICS: '1',
    MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED: 'true',
  }
  const prefixes: string[] = []
  const androidSdkRoot = process.env.ANDROID_HOME ?? process.env.ANDROID_SDK_ROOT
  if (androidSdkRoot) {
    env.ANDROID_HOME = androidSdkRoot
    env.ANDROID_SDK_ROOT = androidSdkRoot
    prefixes.push(resolve(androidSdkRoot, 'platform-tools'))
  }
  if (process.env.JAVA_HOME) {
    env.JAVA_HOME = process.env.JAVA_HOME
    prefixes.push(resolve(process.env.JAVA_HOME, 'bin'))
  }
  env.PATH = [...prefixes, process.env.PATH ?? ''].filter(Boolean).join(delimiter)
  return env
}

function verificationDetails(
  root: string,
  result: Awaited<ReturnType<MaestroWalkRunner['runFlow']>>,
  execution: Awaited<ReturnType<MaestroWalkRunner['adapt']>>,
): VerificationDetails {
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
    stdoutPath: result.artifacts ? portable(root, result.artifacts.stdoutPath) : null,
    stderrPath: result.artifacts ? portable(root, result.artifacts.stderrPath) : null,
    outputPath: execution.failure?.rawArtifactPath ??
      (result.artifacts ? portable(root, result.artifacts.outputPath) : null),
    failureClassification: execution.failure,
  }
}

function failureFromVerification(report: ColdExploreReport): WalkFailure | null {
  const failure = report.verification?.failureClassification
  if (!failure || typeof failure !== 'object') return null
  return failure as WalkFailure
}

export class OpenClawWalkExplorer implements WalkExplorer {
  private readonly options: OpenClawWalkExplorerOptions

  constructor(options: OpenClawWalkExplorerOptions) {
    this.options = options
  }

  async explore(request: ExploreRequest): Promise<ExplorationResult> {
    const { configuration, runner, store } = this.options
    const root = resolve(configuration.workspaceRoot)
    const proxy = resolve(root, this.options.maestroMcpProxy)
    if (proxy === root || !proxy.startsWith(`${root}${sep}`)) {
      throw new Error('Maestro MCP proxy must stay inside the configured workspace')
    }
    const attemptDirectory = resolve(
      root,
      `.deja/attempts/explore-${Date.now()}-${process.pid}`,
    )
    const report = await runColdExploration({
      goal: request.identity.goal,
      configuration,
      buildFingerprint: this.options.buildFingerprint,
      attemptDirectory,
      openClawExecutable: this.options.openClawExecutable,
      maestroMcpProxy: proxy,
      maestroMcpEnvironment: mcpEnvironment(configuration, runner.executable),
    }, {
      clearAppState: async () => clearAndroidApp(configuration),
      verifyCandidate: async (candidatePath, attempt) => {
        const raw = await runner.runFlow(
          candidatePath,
          resolve(attempt, 'verification'),
          configuration.verificationTimeoutMs,
        )
        return verificationDetails(root, raw, await runner.adapt(raw))
      },
      persistVerified: async (candidatePath, verification, metrics: ExplorationMetrics) => {
        if (!verification.passed || typeof verification.endedAt !== 'string') {
          throw new Error('Only an independently passing candidate can enter successful storage')
        }
        const persisted = await store.persistVerifiedCandidate({
          identity: request.identity,
          candidatePath,
          validation: { passed: true, at: verification.endedAt },
          origin: 'explore',
          exploration: metrics,
        })
        return {
          flowPath: persisted.flowPath,
          flowSha256: persisted.flowSha256,
          storedMetadataPath: persisted.storedMetadataPath,
          atomicWrite: persisted.atomicWrite,
        }
      },
    })

    const artifacts: RunArtifact[] = [
      { kind: 'attempt', path: report.attemptDirectory },
      { kind: 'output', path: report.activityPath },
      { kind: 'output', path: report.mcpActivityPath },
    ]
    if (report.candidatePath) artifacts.push({ kind: 'yaml', path: report.candidatePath })
    if (report.openClawResultPath) {
      artifacts.push({ kind: 'model-metrics', path: report.openClawResultPath })
    }
    if (report.flowPath) {
      artifacts.push({
        kind: 'yaml', path: report.flowPath,
        ...(report.flowSha256 ? { sha256: report.flowSha256 } : {}),
      })
    }
    const verificationOutput = report.verification?.outputPath
    if (typeof verificationOutput === 'string') {
      artifacts.push({ kind: 'output', path: verificationOutput })
    }
    return {
      passed: report.passed,
      durationMs: report.durationMs,
      explorationDurationMs: report.explorationDurationMs,
      verificationDurationMs: report.verificationDurationMs,
      mcpCalls: report.mcpCalls,
      modelCalls: report.modelCalls,
      modelCallBasis: report.modelCallBasis,
      model: report.model,
      provider: report.provider,
      assertions: report.flowPath ? await countFlowAssertions(resolve(root, report.flowPath)) : null,
      flowPath: report.flowPath,
      flowSha256: report.flowSha256,
      failure: failureFromVerification(report),
      artifacts,
      errors: report.errors,
    }
  }
}
