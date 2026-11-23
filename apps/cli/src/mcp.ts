import { realpath, stat } from 'node:fs/promises'
import { extname, isAbsolute, relative, resolve, sep } from 'node:path'
import { Server } from '@modelcontextprotocol/sdk/server/index.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import type { Transport } from '@modelcontextprotocol/sdk/shared/transport.js'
import {
  CallToolRequestSchema,
  ErrorCode,
  ListToolsRequestSchema,
  McpError,
  type CallToolResult,
  type Tool,
} from '@modelcontextprotocol/sdk/types.js'
import { InvalidWalkManifestError, type RunResult } from '@deja-qa/core'
import {
  ConsumerFlowError,
  InvalidWalkMemoryError,
  ManifestLockedError,
  StoreLockedError,
  StoreLockUnavailableError,
} from '@deja-qa/store-local'
import { CliConfigurationError, defaultConfigPath, loadProductionConfiguration, type ProductionConfiguration } from './config.ts'
import { selectEngine } from './engine.ts'
import { createOrchestration, EngineRunReporter, type OrchestrationCapabilities } from './orchestration.ts'
import { createRedactor, redactJson, type Redactor } from './redaction.ts'
import { JsonRunReporter, readRunReport } from './reporter.ts'
import { isCompiledBinary, versionNumber } from './version.ts'

/*
 * `deja mcp` (DEJA-9): a stdio MCP server so a coding agent can replay saved walks. The device, app,
 * workspace and store come from the host's config and flags; no tool can name another one. Tools are
 * replay-only: no repair, no verify override, no assertion override. `deja_explore` is registered
 * only when the config sets "mcp": { "explore": true }.
 */

type FieldSpec = { description: string; maxLength: number; required: boolean }
type ToolSpec = {
  tool: Tool
  fields: Record<string, FieldSpec>
}

const GOAL: FieldSpec = {
  description: 'The goal of a saved walk, for example "checkout". Matched exactly (and semantically only if the host enabled it).',
  maxLength: 500,
  required: true,
}

function spec(
  name: string,
  description: string,
  fields: Record<string, FieldSpec>,
  annotations: Tool['annotations'],
): ToolSpec {
  return {
    fields,
    tool: {
      name,
      description,
      inputSchema: {
        type: 'object',
        properties: Object.fromEntries(Object.entries(fields).map(([key, field]) => [key, {
          type: 'string', description: field.description, minLength: 1, maxLength: field.maxLength,
        }])),
        required: Object.entries(fields).filter(([, field]) => field.required).map(([key]) => key),
        additionalProperties: false,
      },
      annotations,
    },
  }
}

const TOOLS: Record<string, ToolSpec> = {
  deja_list: spec('deja_list',
    'List the saved walks for this workspace: goal, platform, last validation and flow path. Reads only.',
    {}, { title: 'List saved walks', readOnlyHint: true, destructiveHint: false, idempotentHint: true }),
  deja_run: spec('deja_run',
    'Replay one saved walk on the device the host configured and return the structured result: outcome, ' +
    'cache status, failure label, failing step and artifact paths. Replay only: a goal with no saved walk ' +
    'returns a not-found miss and never starts an exploration. A stale selector is reported, not repaired.',
    { goal: GOAL }, { title: 'Replay a saved walk', readOnlyHint: false, destructiveHint: false, idempotentHint: false }),
  deja_inspect: spec('deja_inspect',
    'Read one saved, redacted run report from .deja/reports. Defaults to the latest report.',
    {
      report: {
        description: 'Report path relative to the workspace, under .deja/reports/. Default: .deja/reports/latest.json.',
        maxLength: 1024, required: false,
      },
    }, { title: 'Inspect a saved report', readOnlyHint: true, destructiveHint: false, idempotentHint: true }),
  deja_remember: spec('deja_remember',
    'Verify a Maestro YAML flow you wrote, and save it as the walk for a goal only if it fully passes. ' +
    'An unverified or failing flow is never saved. The path is relative to the workspace and must stay inside it.',
    {
      goal: GOAL,
      flow: {
        description: 'Workspace-relative path of the .yaml or .yml flow, outside .deja/.',
        maxLength: 1024, required: true,
      },
    }, { title: 'Verify and save a flow', readOnlyHint: false, destructiveHint: false, idempotentHint: false }),
  deja_explore: spec('deja_explore',
    'Explore a goal cold with the configured model and store the result only if it verifies. Spends model calls.',
    { goal: GOAL }, { title: 'Explore a goal (spends model calls)', readOnlyHint: false, destructiveHint: false, idempotentHint: false }),
}

class InputError extends Error {}

function parseInput(toolSpec: ToolSpec, raw: unknown): Record<string, string> {
  const value = raw === undefined ? {} : raw
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new InputError('Arguments must be an object')
  const input = value as Record<string, unknown>
  const unknown = Object.keys(input).filter(key => !(key in toolSpec.fields))
  if (unknown.length > 0) throw new InputError(`Unknown argument: ${unknown.join(', ')}`)
  const parsed: Record<string, string> = {}
  for (const [key, field] of Object.entries(toolSpec.fields)) {
    const entry = input[key]
    if (entry === undefined) {
      if (field.required) throw new InputError(`${key} is required`)
      continue
    }
    if (typeof entry !== 'string') throw new InputError(`${key} must be a string`)
    const text = entry.trim()
    if (text.length === 0) throw new InputError(`${key} must not be empty`)
    if (text.length > field.maxLength) throw new InputError(`${key} is longer than ${field.maxLength} characters`)
    // eslint-disable-next-line no-control-regex
    if (/[\u0000-\u001f\u007f]/.test(text)) throw new InputError(`${key} must not contain control characters`)
    parsed[key] = text
  }
  return parsed
}

export class FlowPathError extends Error {
  readonly code = 'INVALID_PATH'
}

function inside(root: string, candidate: string) {
  return candidate !== root && candidate.startsWith(`${root}${sep}`)
}

function underDeja(root: string, candidate: string) {
  const first = relative(root, candidate).split(sep)[0]
  return first === '.deja'
}

/**
 * The flow an agent names, confined to the workspace: relative, a .yaml/.yml file, never under
 * `.deja/`, and (after resolving every symlink) still a file inside the real workspace. Returns the
 * workspace-relative path to hand to `remember`.
 */
export async function confineFlow(workspaceRoot: string, flow: string): Promise<string> {
  if (isAbsolute(flow)) throw new FlowPathError('flow must be a path relative to the workspace')
  if (!['.yaml', '.yml'].includes(extname(flow).toLowerCase())) throw new FlowPathError('flow must be a .yaml or .yml file')
  const root = await realpath(resolve(workspaceRoot))
  const lexical = resolve(root, flow)
  if (!inside(root, lexical)) throw new FlowPathError('flow must stay inside the workspace')
  if (underDeja(root, lexical)) throw new FlowPathError('flow must not be under .deja/')
  let canonical: string
  try {
    canonical = await realpath(lexical)
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') throw new FlowPathError('flow does not exist')
    throw error
  }
  if (!inside(root, canonical)) throw new FlowPathError('flow must stay inside the workspace, symlinks included')
  if (underDeja(root, canonical)) throw new FlowPathError('flow must not resolve into .deja/')
  if (!(await stat(canonical)).isFile()) throw new FlowPathError('flow must be a file')
  return relative(root, lexical)
}

async function confineReport(workspaceRoot: string, report: string): Promise<string> {
  if (isAbsolute(report)) throw new FlowPathError('report must be a path relative to the workspace')
  const root = await realpath(resolve(workspaceRoot))
  const reports = resolve(root, '.deja/reports')
  const lexical = resolve(root, report)
  if (!inside(reports, lexical)) throw new FlowPathError('report must be under .deja/reports/')
  let canonical: string
  try {
    canonical = await realpath(lexical)
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') throw new FlowPathError('report does not exist')
    throw error
  }
  if (!inside(reports, canonical)) throw new FlowPathError('report must stay under .deja/reports/, symlinks included')
  return relative(root, canonical)
}

function errorCode(error: unknown): string {
  if (error instanceof StoreLockedError) return error.code
  if (error instanceof StoreLockUnavailableError) return error.code
  if (error instanceof ManifestLockedError) return 'MANIFEST_LOCKED'
  if (error instanceof InvalidWalkManifestError || error instanceof ConsumerFlowError) return 'MANIFEST_INVALID'
  if (error instanceof InvalidWalkMemoryError) return 'MEMORY_INVALID'
  if (error instanceof FlowPathError) return error.code
  if (error instanceof CliConfigurationError) return 'CONFIGURATION'
  if (error instanceof Error && error.name === 'AbortError') return 'ABORTED'
  return 'INTERNAL'
}

export type McpServerOptions = {
  configuration: ProductionConfiguration
  storeId?: string
  env?: NodeJS.ProcessEnv
  compiledBinary?: boolean
  /** Where engine warnings go. Never stdout: that carries the protocol. */
  stderr?: (text: string) => void
}

export function createMcpServer(options: McpServerOptions): Server {
  const { configuration } = options
  const root = configuration.workspaceRoot
  const compiled = options.compiledBinary ?? isCompiledBinary()
  const exploreEnabled = configuration.mcp?.explore === true
  const stderr = options.stderr ?? (text => process.stderr.write(text))
  const baseRedact = createRedactor(
    root, configuration.explore.fixtureCredentials.map(credential => credential.value),
  )
  // The workspace may be reached through a symlink; redact its real path too.
  let redact: Redactor = baseRedact
  const realRoot = realpath(root).then(real => {
    if (real !== root) {
      const realRedact = createRedactor(real, configuration.explore.fixtureCredentials.map(credential => credential.value))
      redact = text => realRedact(baseRedact(text))
    }
    return real
  }, () => root)

  const tools = Object.values(TOOLS)
    .filter(entry => entry.tool.name !== 'deja_explore' || exploreEnabled)
  const server = new Server(
    { name: 'deja', version: versionNumber() },
    {
      capabilities: { tools: {} },
      instructions: 'Déjà replays saved Maestro walks for this workspace. Call deja_list, then deja_run with a goal. ' +
        'A goal with no saved walk returns a not-found miss; write a flow and use deja_remember to save it.',
    },
  )

  const asResult = (value: unknown, isError = false): CallToolResult => {
    const safe = redactJson(value, text => redact(text))
    return {
      content: [{ type: 'text', text: JSON.stringify(safe, null, 2) }],
      structuredContent: safe as Record<string, unknown>,
      ...(isError ? { isError: true } : {}),
    }
  }

  const relativeArtifacts = (result: RunResult, real: string): RunResult => {
    const toRelative = (path: string) => {
      for (const base of [root, real]) {
        if (isAbsolute(path) && (path === base || inside(base, path))) return relative(base, path) || '.'
      }
      return path
    }
    const artifacts = (list: RunResult['artifacts']) => list.map(artifact => ({ ...artifact, path: toRelative(artifact.path) }))
    const failure = <T extends RunResult['failure']>(value: T): T =>
      value && value.rawArtifactPath ? { ...value, rawArtifactPath: toRelative(value.rawArtifactPath) } : value
    return {
      ...result,
      yamlPath: result.yamlPath === null ? null : toRelative(result.yamlPath),
      failure: failure(result.failure),
      artifacts: artifacts(result.artifacts),
      ...(result.execution
        ? { execution: { ...result.execution, failure: failure(result.execution.failure), artifacts: artifacts(result.execution.artifacts) } }
        : {}),
    }
  }

  const prepare = async (capabilities: OrchestrationCapabilities) => {
    const selected = await selectEngine(configuration, { env: options.env ?? process.env })
    if (selected.warning) stderr(`${redact(selected.warning)}\n`)
    const jsonReporter = new JsonRunReporter(root, text => redact(text))
    const reporter = new EngineRunReporter(jsonReporter, selected.report)
    const { store, orchestration } = createOrchestration(
      configuration, reporter, undefined, options.storeId, selected.resolved.executable, compiled, capabilities,
    )
    return { store, orchestration, jsonReporter, engine: selected.report }
  }

  /** Writers take the store's one lock before any work, so a second concurrent call is refused at once. */
  const write = async (
    capabilities: OrchestrationCapabilities,
    work: (context: Awaited<ReturnType<typeof prepare>>) => Promise<RunResult>,
  ) => {
    const context = await prepare(capabilities)
    try {
      context.store.acquireWriter()
      const result = await work(context)
      const real = await realRoot
      return asResult({ ...relativeArtifacts({ ...result, engine: context.engine }, real), report: context.jsonReporter.receipt })
    } finally {
      context.store.close()
    }
  }

  server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: tools.map(entry => entry.tool) }))

  server.setRequestHandler(CallToolRequestSchema, async (request, extra): Promise<CallToolResult> => {
    const entry = tools.find(candidate => candidate.tool.name === request.params.name)
    if (!entry) throw new McpError(ErrorCode.InvalidParams, `Unknown tool: ${request.params.name}`)
    let input: Record<string, string>
    try {
      input = parseInput(entry, request.params.arguments)
    } catch (error) {
      if (error instanceof InputError) throw new McpError(ErrorCode.InvalidParams, error.message)
      throw error
    }
    const callOptions = { signal: extra.signal }
    try {
      switch (entry.tool.name) {
        case 'deja_list': {
          const { store } = createOrchestration(
            configuration, new JsonRunReporter(root, redact), undefined, options.storeId, '', compiled,
            { repair: false, explore: false },
          )
          try {
            const walks = await store.list()
            return asResult({
              ...(store.manifestPath ? { manifest: store.manifestPath } : {}),
              walks: walks.map(walk => ({
                goal: walk.goal,
                goalKey: walk.goalKey,
                appId: walk.appId,
                platform: walk.platform,
                ...(walk.adapter ? { adapter: walk.adapter } : {}),
                buildFingerprint: walk.buildFingerprint,
                flowPath: walk.flowPath,
                lastValidation: walk.lastValidation,
                origin: walk.origin ?? null,
              })),
            })
          } finally {
            store.close()
          }
        }
        case 'deja_inspect': {
          const path = await confineReport(root, input.report ?? '.deja/reports/latest.json')
          return asResult(await readRunReport(root, path, text => redact(text)))
        }
        case 'deja_run':
          return await write({ repair: false, explore: false },
            ({ orchestration }) => orchestration.run(input.goal!, callOptions))
        case 'deja_remember': {
          const flow = await confineFlow(root, input.flow!)
          return await write({ repair: false, explore: false },
            ({ orchestration }) => orchestration.remember(input.goal!, flow, callOptions))
        }
        default:
          return await write({ repair: false, explore: true },
            ({ orchestration }) => orchestration.explore(input.goal!, callOptions))
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error)
      return asResult({ error: { code: errorCode(error), message } }, true)
    }
  })
  return server
}

type McpArguments = { configPath: string; storeId?: string }

export function parseMcpArguments(argv: string[]): McpArguments {
  let configPath: string | undefined
  let storeId: string | undefined
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]!
    if (argument !== '--config' && argument !== '--store') {
      throw new CliConfigurationError(`mcp accepts only --config and --store, not ${argument}`)
    }
    const value = argv[index + 1]
    if (!value || value.startsWith('--')) {
      throw new CliConfigurationError(`${argument} requires ${argument === '--store' ? 'a store id' : 'a file path'}`)
    }
    index += 1
    if (argument === '--config') configPath = value
    else storeId = value
  }
  return { configPath: configPath ?? defaultConfigPath(), ...(storeId ? { storeId } : {}) }
}

/** Serve on stdio until the host closes it. Stdout is the protocol; everything else goes to stderr. */
export async function runMcpServer(
  argv: string[],
  dependencies: { env?: NodeJS.ProcessEnv; compiledBinary?: boolean; transport?: Transport } = {},
): Promise<void> {
  const parsed = parseMcpArguments(argv)
  const configuration = await loadProductionConfiguration(parsed.configPath)
  const server = createMcpServer({
    configuration,
    ...(parsed.storeId ? { storeId: parsed.storeId } : {}),
    ...(dependencies.env ? { env: dependencies.env } : {}),
    ...(dependencies.compiledBinary !== undefined ? { compiledBinary: dependencies.compiledBinary } : {}),
  })
  const closed = new Promise<void>(done => { server.onclose = done })
  await server.connect(dependencies.transport ?? new StdioServerTransport())
  if (!dependencies.transport) process.stdin.once('end', () => { void server.close() })
  await closed
}
