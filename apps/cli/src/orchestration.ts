import { resolve } from 'node:path'
import {
  createDeja,
  UnsupportedPlatformExplorer,
  type EngineReport,
  type ExplorationResult,
  type RunReporter,
  type RunResult,
  type WalkExplorer,
} from '@deja-qa/core'
import { OpenClawWalkExplorer } from '@deja-qa/explorer-openclaw'
import { LocalWalkStore } from '@deja-qa/store-local'
import {
  MaestroWalkRunner,
  androidMaestroAdapter,
  iosMaestroAdapter,
  webMaestroAdapter,
} from '@deja-qa/maestro'
import { BoundedSelectorRepairer } from '@deja-qa/repair'
import {
  LOCAL_LEXICAL_EMBEDDING_PROVIDER,
  LocalLexicalEmbeddingProvider,
} from '@deja-qa/semantic-local'
import { buildFingerprint, CliConfigurationError, type ProductionConfiguration } from './config.ts'

/** Adds the engine to every report, so the JSON report, the text output and the summary agree. */
export class EngineRunReporter implements RunReporter {
  private readonly inner: RunReporter
  private readonly engine: EngineReport
  constructor(inner: RunReporter, engine: EngineReport) {
    this.inner = inner
    this.engine = engine
  }
  report(result: RunResult) {
    // The JUnit reporter adds its artifact to this same object, so do not copy it.
    result.engine = this.engine
    return this.inner.report(result)
  }
}

export type OrchestrationCapabilities = { repair: boolean; explore: boolean }

/** A miss stays a miss: no model is built and no exploration is attempted. */
export class ReplayOnlyExplorer implements WalkExplorer {
  async explore(): Promise<ExplorationResult> {
    return {
      passed: false,
      durationMs: 0,
      explorationDurationMs: 0,
      verificationDurationMs: null,
      mcpCalls: 0,
      modelCalls: 0,
      modelCallBasis: 'replay only: no explorer or model was invoked',
      model: null,
      provider: null,
      assertions: null,
      flowPath: null,
      flowSha256: null,
      failure: null,
      artifacts: [],
      errors: ['No saved walk matches this goal on this build. This tool replays saved walks only and never explores.'],
    }
  }
}

export function createStore(configuration: ProductionConfiguration, storeId: string | undefined) {
  return new LocalWalkStore({
    workspaceRoot: configuration.workspaceRoot,
    ...(storeId ? { storeId } : {}),
    ...(configuration.manifest ? {
      manifest: {
        path: configuration.manifest,
        ...(configuration.flowsDir ? { flowsDir: configuration.flowsDir } : {}),
        // A fixture login must never end up in a file that gets committed.
        forbiddenValues: configuration.explore.fixtureCredentials.map(credential => credential.value),
      },
    } : {}),
  })
}

export function createExecutionAdapter(configuration: ProductionConfiguration) {
  return configuration.explore.platform === 'web'
    ? webMaestroAdapter({
      adapterId: configuration.explore.adapter,
      headless: configuration.explore.headless,
      screenSize: configuration.explore.screenSize,
    })
    : configuration.explore.platform === 'ios'
      ? iosMaestroAdapter({
        device: configuration.explore.device,
        adapterId: configuration.explore.adapter,
      })
    : androidMaestroAdapter({ device: configuration.explore.device })
}

export function createOrchestration(
  configuration: ProductionConfiguration,
  reporter: RunReporter,
  junitPath: string | undefined,
  storeId: string | undefined,
  executable: string,
  crashReportDirectory: string | undefined,
  compiledBinary: boolean,
  /** `deja mcp` turns repair off and, for replay tools, exploration too. The CLI leaves both on. */
  capabilities: OrchestrationCapabilities = { repair: true, explore: true },
) {
  const store = createStore(configuration, storeId)
  const executionAdapter = createExecutionAdapter(configuration)
  const runner = new MaestroWalkRunner({
    workspaceRoot: configuration.workspaceRoot,
    executable,
    device: executionAdapter.device,
    adapter: executionAdapter,
    timeoutMs: configuration.maestro.timeoutMs,
    appId: configuration.explore.appId,
    ...(crashReportDirectory ? { crashReportDirectory } : {}),
    crashReportGraceMs: configuration.crashReportGraceMs,
    ...(junitPath ? { junitPath } : {}),
    ...(process.env.ANDROID_HOME || process.env.ANDROID_SDK_ROOT
      ? { androidSdkRoot: process.env.ANDROID_HOME ?? process.env.ANDROID_SDK_ROOT }
      : {}),
    ...(process.env.JAVA_HOME ? { javaHome: process.env.JAVA_HOME } : {}),
  })
  const orchestration = createDeja({
    workspaceRoot: configuration.workspaceRoot,
    app: { appId: configuration.explore.appId, buildPath: configuration.explore.buildPath },
    adapter: executionAdapter,
    store,
    ...(configuration.manifest ? { manifestPath: configuration.manifest } : {}),
    runner,
    reporter,
    ...(configuration.semantic ? {
      semantic: {
        createProvider: () => configuration.semantic!.provider === LOCAL_LEXICAL_EMBEDDING_PROVIDER
          ? new LocalLexicalEmbeddingProvider()
          : null,
        minimumSimilarity: configuration.semantic!.minimumSimilarity,
        minimumMargin: configuration.semantic!.minimumMargin,
      },
    } : {}),
    ...(configuration.staleBuild ? { staleBuild: configuration.staleBuild } : {}),
    explorer: ({ identity }) => {
      if (!capabilities.explore) return new ReplayOnlyExplorer()
      // OpenClaw starts its MCP proxy with process.execPath, which in a compiled binary is deja, not Bun.
      if (compiledBinary) {
        throw new CliConfigurationError('explore needs the Bun runtime; use the npm packages. The standalone binary replays stored walks only.')
      }
      if (configuration.explore.platform !== 'android') {
        return new UnsupportedPlatformExplorer(configuration.explore.platform)
      }
      return new OpenClawWalkExplorer({
        configuration: configuration.explore,
        buildFingerprint: identity.buildFingerprint,
        openClawExecutable: configuration.openClaw!.executable,
        maestroMcpProxy: configuration.openClaw!.mcpProxy,
        runner,
        store,
      })
    },
    ...(capabilities.repair && configuration.repair && configuration.explore.platform === 'android' && !configuration.manifest ? {
      repairer: () => new BoundedSelectorRepairer({
        workspaceRoot: configuration.workspaceRoot,
        policy: configuration.repair!.policy,
        timeoutMs: configuration.repair!.timeBudgetMs,
        apkPath: resolve(configuration.workspaceRoot, configuration.explore.buildPath),
        device: configuration.explore.device,
        store,
        memoryPath: store.relativeMemoryPath,
        runner,
        fingerprint: () => buildFingerprint(configuration, executionAdapter),
        ...(process.env.ANDROID_HOME || process.env.ANDROID_SDK_ROOT
          ? { androidSdkRoot: process.env.ANDROID_HOME ?? process.env.ANDROID_SDK_ROOT }
          : {}),
        ...(process.env.JAVA_HOME ? { javaHome: process.env.JAVA_HOME } : {}),
      }),
    } : {}),
  })
  return { store, orchestration, adapterId: executionAdapter.id }
}
