import { resolve } from 'node:path'
import { createDeja, UnsupportedPlatformExplorer, type RunResult } from '@deja-qa/core'
import { OpenClawWalkExplorer } from '@deja-qa/explorer-openclaw'
import {
  InvalidWalkMemoryError,
  LOCAL_MEMORY_PATH,
  LocalWalkStore,
} from '@deja-qa/store-local'
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
import {
  buildFingerprint,
  CliConfigurationError,
  defaultConfigPath,
  loadProductionConfiguration,
  type ProductionConfiguration,
} from './config.ts'
import { HELP, renderResult, renderWalks } from './output.ts'
import { createRedactor, redactJson } from './redaction.ts'
import { JsonRunReporter, readRunReport } from './reporter.ts'

export type CliIo = {
  stdout: (text: string) => void
  stderr: (text: string) => void
}

type ParsedArguments = {
  command: string
  positionals: string[]
  configPath: string
  flow?: string
  json: boolean
  help: boolean
}

function parseArguments(argv: string[]): ParsedArguments {
  const positionals: string[] = []
  let configPath: string | undefined
  let flow: string | undefined
  let json = false
  let help = false
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--json') json = true
    else if (argument === '-h' || argument === '--help' || argument === 'help') help = true
    else if (argument === '--config' || argument === '--flow') {
      const value = argv[index + 1]
      if (!value || value.startsWith('--')) {
        throw new CliConfigurationError(`${argument} requires a file path`)
      }
      index += 1
      if (argument === '--config') configPath = value
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
    json,
    help,
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
  return 1
}

function createOrchestration(
  configuration: ProductionConfiguration,
  reporter: JsonRunReporter,
) {
  const store = new LocalWalkStore({ workspaceRoot: configuration.workspaceRoot })
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
    executable: configuration.maestro.executable,
    device: executionAdapter.device,
    adapter: executionAdapter,
    timeoutMs: configuration.maestro.timeoutMs,
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
    ...(configuration.repair && configuration.explore.platform === 'android' ? {
      repairer: () => new BoundedSelectorRepairer({
        workspaceRoot: configuration.workspaceRoot,
        policy: configuration.repair!.policy,
        timeoutMs: configuration.repair!.timeBudgetMs,
        apkPath: resolve(configuration.workspaceRoot, configuration.explore.buildPath),
        device: configuration.explore.device,
        store,
        memoryPath: LOCAL_MEMORY_PATH,
        runner,
        fingerprint: () => buildFingerprint(configuration, executionAdapter),
        ...(process.env.ANDROID_HOME || process.env.ANDROID_SDK_ROOT
          ? { androidSdkRoot: process.env.ANDROID_HOME ?? process.env.ANDROID_SDK_ROOT }
          : {}),
        ...(process.env.JAVA_HOME ? { javaHome: process.env.JAVA_HOME } : {}),
      }),
    } : {}),
  })
  return { store, orchestration }
}

export async function runCli(argv: string[], io: CliIo = {
  stdout: text => process.stdout.write(text),
  stderr: text => process.stderr.write(text),
}): Promise<number> {
  let redact = createRedactor(process.cwd(), [])
  try {
    const parsed = parseArguments(argv)
    if (parsed.help || parsed.command === '') {
      io.stdout(HELP)
      return parsed.command === '' && !parsed.help ? 2 : 0
    }
    if (!['explore', 'run', 'remember', 'list', 'inspect'].includes(parsed.command)) {
      throw new CliConfigurationError(`Unknown command: ${parsed.command}`)
    }
    if (parsed.command !== 'remember' && parsed.flow) {
      throw new CliConfigurationError('--flow is valid only with remember')
    }
    const configuration = await loadProductionConfiguration(parsed.configPath)
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
      return resultExitCode(report)
    }

    const reporter = new JsonRunReporter(configuration.workspaceRoot, redact)
    const { store, orchestration } = createOrchestration(configuration, reporter)
    if (parsed.command === 'list') {
      if (parsed.positionals.length > 0) throw new CliConfigurationError('list accepts no positional arguments')
      const walks = await store.list()
      io.stdout(parsed.json
        ? `${JSON.stringify(redactJson({ walks }, redact), null, 2)}\n`
        : renderWalks(walks, redact))
      return 0
    }

    if (parsed.command === 'remember' && !parsed.flow) {
      throw new CliConfigurationError('remember requires --flow <candidate.yaml>')
    }
    const goalText = goal(parsed.positionals)
    const result = parsed.command === 'explore'
      ? await orchestration.explore(goalText)
      : parsed.command === 'remember'
        ? await orchestration.remember(goalText, parsed.flow!)
        : await orchestration.run(goalText)
    io.stdout(parsed.json
      ? `${JSON.stringify(redactJson({ ...result, report: reporter.receipt }, redact), null, 2)}\n`
      : renderResult(result, reporter.receipt, redact))
    return resultExitCode(result)
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    io.stderr(`${redact(message)}\nRun "deja --help" for usage and exit codes.\n`)
    if (error instanceof InvalidWalkMemoryError) {
      io.stderr('Memory recovery: preserve/quarantine the ignored .deja directory, then explore cold; never treat corrupt memory as a pass.\n')
      return 4
    }
    return 2
  }
}
