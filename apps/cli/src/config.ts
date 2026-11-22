import { existsSync } from 'node:fs'
import { readFile } from 'node:fs/promises'
import { dirname, resolve, sep } from 'node:path'
import { loadExploreConfiguration, type ExploreConfiguration } from '../../../packages/agent-openclaw/src/index.ts'
import {
  androidMaestroAdapter,
  type MaestroExecutionAdapter,
} from '../../../packages/maestro/src/index.ts'
import type { CheckoutRepairPolicy } from '../../../packages/repair/src/index.ts'

export class CliConfigurationError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'CliConfigurationError'
  }
}

export type WebExploreConfiguration = Omit<ExploreConfiguration,
  'platform' | 'openClawModel' | 'openClawBaseUrl'
> & {
  platform: 'web'
  adapter: string
  browser: 'chromium'
  headless: boolean
  screenSize: string
}

export type ProductionConfiguration = {
  configPath: string
  explore: ExploreConfiguration | WebExploreConfiguration
  workspaceRoot: string
  maestro: {
    executable: string
    timeoutMs: number
  }
  openClaw?: {
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

function loadWebExploreConfiguration(absolute: string, raw: Record<string, unknown>): WebExploreConfiguration {
  const workspaceRoot = resolve(dirname(absolute), String(raw.workspaceRoot ?? ''))
  const appId = typeof raw.appId === 'string' ? raw.appId.trim() : ''
  const buildPath = typeof raw.buildPath === 'string' ? raw.buildPath : ''
  if (!appId.startsWith('http://127.0.0.1:') && !appId.startsWith('http://localhost:')) {
    throw new CliConfigurationError('WALK-010 Web appId must be an explicit loopback fixture URL')
  }
  if (!raw.workspaceRoot || !buildPath || !inside(workspaceRoot, buildPath) ||
      !existsSync(workspaceRoot) || !existsSync(resolve(workspaceRoot, buildPath))) {
    throw new CliConfigurationError('Web config requires existing workspaceRoot and a buildPath inside it')
  }
  if (raw.device !== undefined && raw.device !== 'chromium') {
    throw new CliConfigurationError('WALK-010 supports only device: chromium')
  }
  if (raw.browser !== 'chromium') throw new CliConfigurationError('Web browser must be chromium')
  if (raw.headless !== undefined && typeof raw.headless !== 'boolean') {
    throw new CliConfigurationError('headless must be a boolean')
  }
  const adapter = text(raw.adapter, 'maestro-web/v1', 'adapter')
  const screenSize = text(raw.screenSize, '1280x800', 'screenSize')
  if (!/^[1-9][0-9]{2,4}x[1-9][0-9]{2,4}$/.test(screenSize)) {
    throw new CliConfigurationError('screenSize must use WIDTHxHEIGHT')
  }
  if (!Array.isArray(raw.fixtureCredentials)) {
    throw new CliConfigurationError('fixtureCredentials must be an explicit array')
  }
  const fixtureCredentials = raw.fixtureCredentials.map((entry, index) => {
    const credential = entry as Record<string, unknown>
    if (typeof credential.label !== 'string' || typeof credential.value !== 'string' ||
        !credential.label.trim() || credential.value.length > 4096) {
      throw new CliConfigurationError(`fixtureCredentials[${index}] requires a label and bounded fixture value`)
    }
    return { label: credential.label, value: credential.value }
  })
  return {
    workspaceRoot,
    appId,
    platform: 'web',
    buildPath,
    device: 'chromium',
    fixtureCredentials,
    explorationTimeoutSeconds: positiveInteger(raw.explorationTimeoutSeconds, 120, 'explorationTimeoutSeconds'),
    verificationTimeoutMs: positiveInteger(raw.verificationTimeoutMs, 120_000, 'verificationTimeoutMs'),
    adapter,
    browser: 'chromium',
    headless: raw.headless === undefined ? true : raw.headless === true,
    screenSize,
  }
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
  let explore: ExploreConfiguration | WebExploreConfiguration
  try {
    explore = raw.platform === 'web'
      ? loadWebExploreConfiguration(absolute, raw)
      : loadExploreConfiguration(absolute)
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
  let openClawConfiguration: ProductionConfiguration['openClaw']
  if (explore.platform === 'android') {
    const mcpProxy = text(
      openClaw.mcpProxy,
      'scripts/maestro-mcp-proxy.ts',
      'openClaw.mcpProxy',
    )
    if (!inside(root, mcpProxy)) throw new CliConfigurationError('openClaw.mcpProxy must be inside workspaceRoot')
    openClawConfiguration = {
      executable: text(
        openClaw.executable,
        process.env.WALKCACHE_OPENCLAW ?? 'openclaw',
        'openClaw.executable',
      ),
      mcpProxy,
    }
  }

  let repair: ProductionConfiguration['repair']
  if (explore.platform === 'web' && raw.repair !== undefined) {
    throw new CliConfigurationError('WALK-010 Web does not configure the Android-only repair plugin')
  }
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
    ...(openClawConfiguration ? { openClaw: openClawConfiguration } : {}),
    ...(repair ? { repair } : {}),
  }
}

export async function buildFingerprint(
  configuration: ProductionConfiguration,
  adapter?: MaestroExecutionAdapter,
) {
  const selected = adapter ?? androidMaestroAdapter({ device: configuration.explore.device })
  return selected.buildFingerprint(configuration.workspaceRoot, configuration.explore.buildPath)
}

export function defaultConfigPath(configDirectory = process.cwd()) {
  return resolve(configDirectory, 'walkcache.config.json')
}

export function configurationDirectory(configuration: ProductionConfiguration) {
  return dirname(configuration.configPath)
}
