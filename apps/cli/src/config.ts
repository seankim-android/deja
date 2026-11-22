import { createHash } from 'node:crypto'
import { readFile } from 'node:fs/promises'
import { dirname, resolve, sep } from 'node:path'
import { loadExploreConfiguration, type ExploreConfiguration } from '../../../packages/agent-openclaw/src/index.ts'
import type { CheckoutRepairPolicy } from '../../../packages/repair/src/index.ts'

export class CliConfigurationError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'CliConfigurationError'
  }
}

export type ProductionConfiguration = {
  configPath: string
  explore: ExploreConfiguration
  workspaceRoot: string
  maestro: {
    executable: string
    timeoutMs: number
  }
  openClaw: {
    executable: string
    mcpProxy: string
  }
  repair?: {
    policy: CheckoutRepairPolicy
    timeBudgetMs: number
  }
}

function text(value: unknown, fallback: string, label: string) {
  if (value === undefined) return fallback
  if (typeof value !== 'string' || !value.trim()) {
    throw new CliConfigurationError(`${label} must be a non-empty string`)
  }
  return value
}

function positiveInteger(value: unknown, fallback: number, label: string) {
  const result = value === undefined ? fallback : Number(value)
  if (!Number.isInteger(result) || result <= 0 || result > 2_147_483_647) {
    throw new CliConfigurationError(`${label} must be a positive integer`)
  }
  return result
}

function inside(root: string, candidate: string) {
  const absolute = resolve(root, candidate)
  return absolute !== root && absolute.startsWith(`${root}${sep}`)
}

export async function loadProductionConfiguration(configPath: string): Promise<ProductionConfiguration> {
  const absolute = resolve(configPath)
  let raw: Record<string, unknown>
  try {
    const parsed = JSON.parse(await readFile(absolute, 'utf8')) as unknown
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) throw new Error('expected an object')
    raw = parsed as Record<string, unknown>
  } catch (error) {
    throw new CliConfigurationError(`Cannot read WalkCache config ${configPath}`, { cause: error })
  }
  let explore: ExploreConfiguration
  try {
    explore = loadExploreConfiguration(absolute)
  } catch (error) {
    throw new CliConfigurationError('Invalid WalkCache fixture configuration', { cause: error })
  }
  const root = explore.workspaceRoot
  const maestro = typeof raw.maestro === 'object' && raw.maestro !== null
    ? raw.maestro as Record<string, unknown>
    : {}
  const openClaw = typeof raw.openClaw === 'object' && raw.openClaw !== null
    ? raw.openClaw as Record<string, unknown>
    : {}
  const mcpProxy = text(
    openClaw.mcpProxy,
    'scripts/maestro-mcp-proxy.ts',
    'openClaw.mcpProxy',
  )
  if (!inside(root, mcpProxy)) throw new CliConfigurationError('openClaw.mcpProxy must be inside workspaceRoot')

  let repair: ProductionConfiguration['repair']
  if (raw.repair !== undefined) {
    if (!raw.repair || typeof raw.repair !== 'object' || Array.isArray(raw.repair)) {
      throw new CliConfigurationError('repair must be an object')
    }
    const configured = raw.repair as Record<string, unknown>
    const policy: CheckoutRepairPolicy = {
      workspaceRoot: root,
      sourceRoots: [],
      permittedFileTypes: [],
      affectedFiles: [],
    }
    for (const key of ['sourceRoots', 'permittedFileTypes', 'affectedFiles'] as const) {
      const values = configured[key]
      if (!Array.isArray(values) || values.some(value => typeof value !== 'string')) {
        throw new CliConfigurationError(`repair.${key} must be an array of strings`)
      }
      policy[key] = values
    }
    repair = {
      policy,
      timeBudgetMs: positiveInteger(configured.timeBudgetMs, 240_000, 'repair.timeBudgetMs'),
    }
  }

  return {
    configPath: absolute,
    explore,
    workspaceRoot: root,
    maestro: {
      executable: text(
        maestro.executable,
        process.env.WALKCACHE_MAESTRO ?? 'maestro',
        'maestro.executable',
      ),
      timeoutMs: positiveInteger(
        maestro.timeoutMs,
        Number(process.env.WALKCACHE_TIMEOUT_MS ?? 120_000),
        'maestro.timeoutMs',
      ),
    },
    openClaw: {
      executable: text(
        openClaw.executable,
        process.env.WALKCACHE_OPENCLAW ?? 'openclaw',
        'openClaw.executable',
      ),
      mcpProxy,
    },
    ...(repair ? { repair } : {}),
  }
}

export async function buildFingerprint(configuration: ProductionConfiguration) {
  const contents = await readFile(resolve(configuration.workspaceRoot, configuration.explore.buildPath))
  return createHash('sha256').update(contents).digest('hex')
}

export function defaultConfigPath(configDirectory = process.cwd()) {
  return resolve(configDirectory, 'walkcache.config.json')
}

export function configurationDirectory(configuration: ProductionConfiguration) {
  return dirname(configuration.configPath)
}
