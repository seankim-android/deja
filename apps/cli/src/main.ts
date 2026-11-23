import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import {
  createDeja,
  InvalidWalkManifestError,
  UnsupportedPlatformExplorer,
  type EngineReport,
  type RunReporter,
  type RunResult,
} from '@deja-qa/core'
import { OpenClawWalkExplorer } from '@deja-qa/explorer-openclaw'
import {
  ConsumerFlowError,
  InvalidWalkMemoryError,
  LocalWalkStore,
  ManifestLockedError,
  StoreLockedError,
  StoreLockUnavailableError,
} from '@deja-qa/store-local'
import {
  dejaHome,
  MaestroWalkRunner,
  type InstallEngineOptions,
  androidMaestroAdapter,
  iosMaestroAdapter,
  webMaestroAdapter,
} from '@deja-qa/maestro'
import { BoundedSelectorRepairer } from '@deja-qa/repair'
import {
  LOCAL_LEXICAL_EMBEDDING_PROVIDER,
  LocalLexicalEmbeddingProvider,
} from '@deja-qa/semantic-local'
import {
  buildFingerprint,
  CliConfigurationError,
  defaultConfigPath,
  loadProductionConfiguration,
  type ProductionConfiguration,
} from './config.ts'
import { prepareJunit, JunitReporter, writeErroredJunit } from './junit.ts'
import { HELP, renderMigration, renderResult, renderSummary, renderWalks } from './output.ts'
import { createRedactor, redactJson } from './redaction.ts'
import { JsonRunReporter, readRunReport } from './reporter.ts'
import { renderDoctor, runDoctor, type DoctorProbe } from './doctor.ts'
import { installEngineCommand, selectEngine } from './engine.ts'

export type CliIo = {
  stdout: (text: string) => void
  stderr: (text: string) => void
}

/** Test seams: a fake download for `engine install`, a fake probe for `doctor`, and the environment. */
export type CliDependencies = {
  engineInstall?: InstallEngineOptions
  doctorProbe?: DoctorProbe
  env?: NodeJS.ProcessEnv
}

/** Adds the engine to every report, so the JSON report, the text output and the summary agree. */
class EngineRunReporter implements RunReporter {
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

async function configuredOverride(configPath: string) {
  try {
    const raw = JSON.parse(await readFile(configPath, 'utf8')) as { maestro?: { executable?: unknown; allowUntested?: unknown } }
    return {
      ...(typeof raw.maestro?.executable === 'string' && raw.maestro.executable.trim()
        ? { configExecutable: raw.maestro.executable.trim() }
        : {}),
      allowUntested: raw.maestro?.allowUntested === true,
    }
  } catch {
    return { allowUntested: false }
  }
}

type ParsedArguments = {
  command: string
  positionals: string[]
  configPath: string
  flow?: string
  junit?: string
  store?: string
  json: boolean
  summary: boolean
  help: boolean
  verify: boolean
  acceptAssertionChange: boolean
  dryRun: boolean
}

function parseArguments(argv: string[]): ParsedArguments {
  const positionals: string[] = []
  let configPath: string | undefined
  let flow: string | undefined
  let junit: string | undefined
  let store: string | undefined
  let json = false
  let summary = false
  let help = false
  let verify = false
  let acceptAssertionChange = false
  let dryRun = false
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--json') json = true
    else if (argument === '--summary') summary = true
    else if (argument === '--verify') verify = true
    else if (argument === '--accept-assertion-change') acceptAssertionChange = true
    else if (argument === '--dry-run') dryRun = true
    else if (argument === '-h' || argument === '--help' || argument === 'help') help = true
    else if (argument === '--config' || argument === '--flow' || argument === '--junit' || argument === '--store') {
      const value = argv[index + 1]
      if (!value || value.startsWith('--')) {
        throw new CliConfigurationError(`${argument} requires ${argument === '--store' ? 'a store id' : 'a file path'}`)
      }
      index += 1
      if (argument === '--config') configPath = value
      else if (argument === '--junit') junit = value
      else if (argument === '--store') store = value
      else flow = value
    } else if (argument.startsWith('-')) {
      throw new CliConfigurationError(`Unknown option: ${argument}`)
    } else positionals.push(argument)
  }
  const command = positionals.shift() ?? ''
  return {
    command,
    positionals,
    configPath: configPath ?? defaultConfigPath(),
    ...(flow ? { flow } : {}),
    ...(junit ? { junit } : {}),
    ...(store ? { store } : {}),
    json,
    summary,
    help,
    verify,
    acceptAssertionChange,
    dryRun,
  }
}

function goal(positionals: string[]) {
  const value = positionals.join(' ').trim()
  if (!value) throw new CliConfigurationError('A non-empty goal is required')
  return value
}

function resultExitCode(result: RunResult) {
  if (result.passed) return 0
  if (result.outcome === 'exploration-rejected') return 3
  if (['flow-changed', 'not-validated', 'needs-migration', 'assertions-changed'].includes(result.outcome)) return 6
  return 1
}

function createOrchestration(
  configuration: ProductionConfiguration,
  reporter: RunReporter,
  junitPath: string | undefined,
  storeId: string | undefined,
  executable: string,
) {
  const store = new LocalWalkStore({
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
  const executionAdapter = configuration.explore.platform === 'web'
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
  const runner = new MaestroWalkRunner({
    workspaceRoot: configuration.workspaceRoot,
    executable,
    device: executionAdapter.device,
    adapter: executionAdapter,
    timeoutMs: configuration.maestro.timeoutMs,
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
    ...(configuration.repair && configuration.explore.platform === 'android' && !configuration.manifest ? {
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

export async function runCli(argv: string[], io: CliIo = {
  stdout: text => process.stdout.write(text),
  stderr: text => process.stderr.write(text),
}, dependencies: CliDependencies = {}): Promise<number> {
  const env = dependencies.env ?? process.env
  let redact = createRedactor(process.cwd(), [])
  let parsed: ParsedArguments | undefined
  let junitPath: string | undefined
  let openedStore: LocalWalkStore | undefined
  // One summary line per run. It is the last stdout line, or stderr with --json so stdout stays JSON.
  const summarize = (result: RunResult | null, goalText: string) => {
    if (!parsed?.summary) return
    const line = `${renderSummary(result, goalText, redact)}\n`
    if (parsed.json) io.stderr(line)
    else io.stdout(line)
  }
  try {
    parsed = parseArguments(argv)
    if (parsed.help || parsed.command === '') {
      io.stdout(HELP)
      return parsed.command === '' && !parsed.help ? 2 : 0
    }
    if (!['explore', 'run', 'remember', 'list', 'inspect', 'migrate', 'engine', 'doctor'].includes(parsed.command)) {
      throw new CliConfigurationError(`Unknown command: ${parsed.command}`)
    }
    if (parsed.command === 'engine') {
      if (parsed.positionals.length !== 1 || parsed.positionals[0] !== 'install') {
        throw new CliConfigurationError('engine requires the subcommand: install')
      }
      if (parsed.junit || parsed.flow || parsed.summary) {
        throw new CliConfigurationError('engine install accepts only --json')
      }
      const result = await installEngineCommand({
        home: dejaHome(env),
        ...dependencies.engineInstall,
      })
      io.stdout(parsed.json
        ? `${JSON.stringify({ engine: result }, null, 2)}\n`
        : `Maestro ${result.version} ${result.status === 'skipped' ? 'is already installed and its checksum is valid' : 'installed'}: ${result.executable}\n`)
      return 0
    }
    if (parsed.command === 'doctor') {
      if (parsed.junit || parsed.flow || parsed.summary || parsed.positionals.length > 0) {
        throw new CliConfigurationError('doctor accepts only --config and --json')
      }
      const override = await configuredOverride(parsed.configPath)
      const report = await runDoctor({
        env,
        ...override,
        ...(dependencies.doctorProbe ? { probe: dependencies.doctorProbe } : {}),
      })
      io.stdout(parsed.json ? `${JSON.stringify(report, null, 2)}\n` : renderDoctor(report))
      return report.ok ? 0 : 1
    }
    if (parsed.command !== 'remember' && parsed.flow) {
      throw new CliConfigurationError('--flow is valid only with remember')
    }
    if (parsed.verify && parsed.command !== 'run') {
      throw new CliConfigurationError('--verify is valid only with run')
    }
    if (parsed.dryRun && parsed.command !== 'migrate') {
      throw new CliConfigurationError('--dry-run is valid only with migrate')
    }
    if (parsed.acceptAssertionChange && parsed.command !== 'remember') {
      throw new CliConfigurationError('--accept-assertion-change is valid only with remember')
    }
    const runsMaestro = ['explore', 'run', 'remember'].includes(parsed.command)
    if (parsed.junit && !runsMaestro) {
      throw new CliConfigurationError('--junit is valid only with explore, run, or remember')
    }
    if (parsed.summary && (parsed.command === 'list' || parsed.command === 'migrate')) {
      throw new CliConfigurationError('--summary is valid only with explore, run, remember, or inspect')
    }
    if (parsed.junit) junitPath = resolve(process.cwd(), parsed.junit)
    const configuration = await loadProductionConfiguration(parsed.configPath)
    if ((parsed.verify || parsed.acceptAssertionChange || parsed.command === 'migrate') && !configuration.manifest) {
      throw new CliConfigurationError(`${parsed.command === 'migrate' ? 'migrate' : parsed.verify ? '--verify' : '--accept-assertion-change'} needs a "manifest" in the config`)
    }
    redact = createRedactor(
      configuration.workspaceRoot,
      configuration.explore.fixtureCredentials.map(credential => credential.value),
    )

    if (parsed.command === 'inspect') {
      if (parsed.positionals.length !== 1) {
        throw new CliConfigurationError('inspect requires exactly one report path')
      }
      const report = await readRunReport(
        configuration.workspaceRoot,
        parsed.positionals[0],
        redact,
      )
      io.stdout(parsed.json
        ? `${JSON.stringify(redactJson(report, redact), null, 2)}\n`
        : renderResult(report, null, redact))
      summarize(report, report.goalKey)
      return resultExitCode(report)
    }

    const jsonReporter = new JsonRunReporter(configuration.workspaceRoot, redact)
    if (runsMaestro) junitPath ??= configuration.junitPath
    // Pick the engine before anything runs: a missing or untested Maestro is a configuration error.
    const selected = runsMaestro ? await selectEngine(configuration, { env }) : undefined
    if (selected?.warning) io.stderr(`${redact(selected.warning)}\n`)
    const engine = selected?.report
    const baseReporter = junitPath
      ? new JunitReporter(jsonReporter, junitPath, configuration.workspaceRoot, redact)
      : jsonReporter
    const reporter = engine ? new EngineRunReporter(baseReporter, engine) : baseReporter
    const { store, orchestration, adapterId } = createOrchestration(
      configuration, reporter, runsMaestro ? junitPath : undefined, parsed.store,
      selected?.resolved.executable ?? '',
    )
    openedStore = store
    if (parsed.command === 'list') {
      if (parsed.positionals.length > 0) throw new CliConfigurationError('list accepts no positional arguments')
      const walks = await store.list()
      io.stdout(parsed.json
        ? `${JSON.stringify(redactJson({ walks }, redact), null, 2)}\n`
        : renderWalks(walks, redact))
      return 0
    }

    if (parsed.command === 'migrate') {
      if (parsed.positionals.length > 0) throw new CliConfigurationError('migrate accepts no positional arguments')
      // A real migration takes the writer lock itself; a dry run only reads.
      const report = await store.migrateToManifest({ dryRun: parsed.dryRun, adapter: adapterId })
      io.stdout(parsed.json
        ? `${JSON.stringify(redactJson({ migration: report }, redact), null, 2)}\n`
        : renderMigration(report, redact))
      return 0
    }

    // Refuse a second writer now, not after a long run.
    store.acquireWriter()
    if (parsed.command === 'remember' && !parsed.flow) {
      throw new CliConfigurationError('remember requires --flow <candidate.yaml>')
    }
    const goalText = goal(parsed.positionals)
    // A file left by an earlier run must never be read as this run's result.
    if (junitPath) await prepareJunit(junitPath)
    const ran = parsed.command === 'explore'
      ? await orchestration.explore(goalText)
      : parsed.command === 'remember'
        ? await orchestration.remember(goalText, parsed.flow!, {
          ...(parsed.acceptAssertionChange ? { acceptAssertionChange: true } : {}),
        })
        : await orchestration.run(goalText, { ...(parsed.verify ? { verify: true } : {}) })
    const result: RunResult = engine ? { ...ran, engine } : ran
    io.stdout(parsed.json
      ? `${JSON.stringify(redactJson({ ...result, report: jsonReporter.receipt }, redact), null, 2)}\n`
      : renderResult(result, jsonReporter.receipt, redact))
    summarize(result, result.goalKey)
    return resultExitCode(result)
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    io.stderr(`${redact(message)}\nRun "deja --help" for usage and exit codes.\n`)
    const goalText = parsed?.positionals.join(' ').trim().toLowerCase().replace(/\s+/g, ' ') ?? ''
    if (parsed && ['explore', 'run', 'remember'].includes(parsed.command)) {
      // A host that reads the JUnit file must see an error, never a missing file or an old report.
      if (junitPath) {
        await writeErroredJunit(junitPath, goalText || parsed.command, redact(message)).catch(() => {})
      }
      summarize(null, goalText)
    }
    if (error instanceof StoreLockUnavailableError) {
      io.stderr('Store lock unavailable: this filesystem cannot hold the writer lock. Use a local disk for the workspace.\n')
      return 5
    }
    if (error instanceof ManifestLockedError) {
      io.stderr('Manifest locked: another run is writing deja.walks.json. All stores share it, so a different --store does not help. Run again in a moment.\n')
      return 5
    }
    if (error instanceof StoreLockedError) {
      io.stderr(error.code === 'STORE_LOCK_CONTENTION'
        ? 'Store lock contention: another run was taking over a stale lock. Run again.\n'
        : 'Store locked: another run is writing this store. Pass a different --store <id> for each device.\n')
      return 5
    }
    if (error instanceof InvalidWalkManifestError || error instanceof ConsumerFlowError) {
      io.stderr('Manifest: fix the manifest or the flow path named above and run again. Déjà never edits it silently.\n')
      return 2
    }
    if (error instanceof InvalidWalkMemoryError) {
      io.stderr('Memory recovery: preserve/quarantine the ignored .deja directory, then explore cold; never treat corrupt memory as a pass.\n')
      return 4
    }
    return 2
  } finally {
    openedStore?.close()
  }
}
