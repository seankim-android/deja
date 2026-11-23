import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import {
  InvalidWalkManifestError,
  type RunResult,
} from '@deja-qa/core'
import {
  ConsumerFlowError,
  InvalidWalkMemoryError,
  LocalWalkStore,
  ManifestLockedError,
  StoreLockedError,
  StoreLockUnavailableError,
} from '@deja-qa/store-local'
import { dejaHome, type InstallEngineOptions } from '@deja-qa/maestro'
import {
  CliConfigurationError,
  defaultConfigPath,
  loadProductionConfiguration,
} from './config.ts'
import { prepareJunit, JunitReporter, writeErroredJunit } from './junit.ts'
import { HELP, renderMigration, renderResult, renderSummary, renderWalks } from './output.ts'
import { createRedactor, redactJson } from './redaction.ts'
import { JsonRunReporter, readRunReport } from './reporter.ts'
import { renderDoctor, runDoctor, type DoctorProbe } from './doctor.ts'
import { installEngineCommand, selectEngine } from './engine.ts'
import { runGate } from './gate.ts'
import { runManifestCheck } from './manifest-check.ts'
import { createExecutionAdapter, createOrchestration, createStore, EngineRunReporter } from './orchestration.ts'
import { isCompiledBinary, versionLine } from './version.ts'

export type CliIo = {
  stdout: (text: string) => void
  stderr: (text: string) => void
}

/** Test seams: a fake download for `engine install`, a fake probe for `doctor`, and the environment. */
export type CliDependencies = {
  engineInstall?: InstallEngineOptions
  doctorProbe?: DoctorProbe
  env?: NodeJS.ProcessEnv
  /** Where iOS crash reports are written; defaults to ~/Library/Logs/DiagnosticReports. */
  crashReportDirectory?: string
  /** Overrides compiled-binary detection, so a test can exercise the explore refusal. */
  compiledBinary?: boolean
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
  goals?: string
  build?: string
  label?: string
  evidence?: string
  json: boolean
  summary: boolean
  help: boolean
  version: boolean
  verify: boolean
  rerunFailed: boolean
  acceptAssertionChange: boolean
  dryRun: boolean
  untracked: boolean
  /** The `--config` value as typed, when it was given. */
  configGiven?: string
}

function parseArguments(argv: string[]): ParsedArguments {
  const positionals: string[] = []
  let configPath: string | undefined
  let flow: string | undefined
  let junit: string | undefined
  let store: string | undefined
  let goals: string | undefined
  let build: string | undefined
  let label: string | undefined
  let evidence: string | undefined
  let json = false
  let summary = false
  let help = false
  let version = false
  let verify = false
  let rerunFailed = false
  let acceptAssertionChange = false
  let dryRun = false
  let untracked = false
  let afterDashes = false
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    // After `--` every word is a positional, so a goal that starts with `-` can be given.
    if (afterDashes) positionals.push(argument)
    else if (argument === '--') afterDashes = true
    else if (argument === '--json') json = true
    else if (argument === '--summary') summary = true
    else if (argument === '--version' || argument === '-v') version = true
    else if (argument === '--verify') verify = true
    else if (argument === '--rerun-failed') rerunFailed = true
    else if (argument === '--accept-assertion-change') acceptAssertionChange = true
    else if (argument === '--dry-run') dryRun = true
    else if (argument === '--untracked') untracked = true
    else if (argument === '-h' || argument === '--help' || argument === 'help') help = true
    else if (['--config', '--flow', '--junit', '--store', '--goals', '--build', '--label', '--evidence'].includes(argument)) {
      const value = argv[index + 1]
      if (!value || value.startsWith('--')) {
        throw new CliConfigurationError(`${argument} requires ${argument === '--store' ? 'a store id' : argument === '--label' ? 'a label' : 'a file path'}`)
      }
      index += 1
      if (argument === '--config') configPath = value
      else if (argument === '--goals') goals = value
      else if (argument === '--build') build = value
      else if (argument === '--label') label = value
      else if (argument === '--evidence') evidence = value
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
    ...(configPath ? { configGiven: configPath } : {}),
    ...(flow ? { flow } : {}),
    ...(junit ? { junit } : {}),
    ...(store ? { store } : {}),
    ...(goals ? { goals } : {}),
    ...(build ? { build } : {}),
    ...(label ? { label } : {}),
    ...(evidence ? { evidence } : {}),
    json,
    summary,
    help,
    version,
    verify,
    rerunFailed,
    acceptAssertionChange,
    dryRun,
    untracked,
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
  if (result.outcome === 'closure-error') return 2
  if (['flow-changed', 'not-validated', 'needs-migration', 'assertions-changed'].includes(result.outcome)) return 6
  if (result.outcome === 'flaky') return 7
  return 1
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
    if (parsed.version) {
      io.stdout(versionLine())
      return 0
    }
    if (parsed.help || parsed.command === '') {
      io.stdout(HELP)
      return parsed.command === '' && !parsed.help ? 2 : 0
    }
    if (!['explore', 'run', 'remember', 'list', 'inspect', 'migrate', 'gate', 'engine', 'doctor', 'mcp', 'manifest'].includes(parsed.command)) {
      throw new CliConfigurationError(`Unknown command: ${parsed.command}`)
    }
    if (parsed.command === 'mcp') {
      if (parsed.positionals.length > 0 || parsed.junit || parsed.flow || parsed.summary || parsed.json ||
          parsed.verify || parsed.rerunFailed || parsed.acceptAssertionChange || parsed.dryRun ||
          parsed.goals || parsed.build || parsed.label || parsed.evidence) {
        throw new CliConfigurationError('mcp accepts only --config and --store')
      }
      const { runMcpServer } = await import('./mcp.ts')
      await runMcpServer(
        ['--config', parsed.configPath, ...(parsed.store ? ['--store', parsed.store] : [])],
        { env, ...(dependencies.crashReportDirectory ? { crashReportDirectory: dependencies.crashReportDirectory } : {}), ...(dependencies.compiledBinary !== undefined ? { compiledBinary: dependencies.compiledBinary } : {}) },
      )
      return 0
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
    if (parsed.untracked && parsed.command !== 'manifest') {
      throw new CliConfigurationError('--untracked is valid only with manifest check')
    }
    if (parsed.command === 'manifest') {
      if (parsed.positionals.length !== 1 || parsed.positionals[0] !== 'check') {
        throw new CliConfigurationError('manifest requires the subcommand: check')
      }
      if (parsed.junit || parsed.flow || parsed.summary || parsed.verify || parsed.rerunFailed ||
          parsed.acceptAssertionChange || parsed.dryRun || parsed.store ||
          parsed.goals || parsed.build || parsed.label || parsed.evidence) {
        throw new CliConfigurationError('manifest check accepts only --config, --json and --untracked')
      }
    }
    if (parsed.command !== 'remember' && parsed.flow) {
      throw new CliConfigurationError('--flow is valid only with remember')
    }
    if (parsed.verify && parsed.command !== 'run') {
      throw new CliConfigurationError('--verify is valid only with run')
    }
    if (parsed.command !== 'gate' && (parsed.goals || parsed.build || parsed.label || parsed.evidence)) {
      throw new CliConfigurationError('--goals, --build, --label and --evidence are valid only with gate')
    }
    if (parsed.command === 'gate') {
      if (!parsed.goals) throw new CliConfigurationError('gate requires --goals <file>')
      if (parsed.positionals.length > 0) throw new CliConfigurationError('gate accepts no positional arguments')
      if (parsed.verify || parsed.dryRun || parsed.acceptAssertionChange || parsed.summary) {
        throw new CliConfigurationError('gate accepts only --goals, --build, --label, --evidence, --store, --config and --json')
      }
    }
    if (parsed.rerunFailed && parsed.command !== 'run') {
      throw new CliConfigurationError('--rerun-failed is valid only with run')
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

    if (parsed.command === 'manifest') {
      // Read-only and static: no store, no lock, no engine, no device, no manifest write.
      return await runManifestCheck({
        configuration, redact, json: parsed.json, untracked: parsed.untracked, io,
        ...(parsed.configGiven ? { configGiven: parsed.configGiven } : {}),
      })
    }

    if (parsed.command === 'gate') {
      // Read-only: no engine, no runner, no writer lock. The store is only listed and looked up.
      return await runGate({
        configuration,
        store: createStore(configuration, parsed.store),
        adapter: createExecutionAdapter(configuration),
        options: {
          goalsPath: parsed.goals!,
          ...(parsed.build ? { buildPath: parsed.build } : {}),
          ...(parsed.label ? { label: parsed.label } : {}),
          ...(parsed.evidence ? { evidencePath: parsed.evidence } : {}),
        },
        redact,
        json: parsed.json,
        io,
      })
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
      selected?.resolved.executable ?? '', dependencies.crashReportDirectory,
      dependencies.compiledBinary ?? isCompiledBinary(),
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
        : await orchestration.run(goalText, {
          ...(parsed.verify ? { verify: true } : {}),
          ...(parsed.rerunFailed ? { rerunFailed: true } : {}),
        })
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
