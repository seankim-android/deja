import { existsSync } from 'node:fs'
import { readFile } from 'node:fs/promises'
import { dirname, resolve, sep } from 'node:path'
import { loadExploreConfiguration, type ExploreConfiguration } from '@deja-qa/explorer-openclaw'
import {
  androidMaestroAdapter,
  type MaestroExecutionAdapter,
} from '@deja-qa/maestro'
import type { CheckoutRepairPolicy } from '@deja-qa/repair'
import { checkManifestPath } from '@deja-qa/store-local'

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

export type IosExploreConfiguration = Omit<ExploreConfiguration,
  'platform' | 'openClawModel' | 'openClawBaseUrl'
> & {
  platform: 'ios'
  adapter: string
}

export type ProductionConfiguration = {
  configPath: string
  /** Absolute path for Maestro's JUnit report (config `junit`, relative to the config file). */
  junitPath?: string
  explore: ExploreConfiguration | WebExploreConfiguration | IosExploreConfiguration
  workspaceRoot: string
  maestro: {
    executable: string
    timeoutMs: number
  }
  openClaw?: {
    executable: string
    mcpProxy: string
  }
  semantic?: {
    provider: string | null
    minimumSimilarity: number
    minimumMargin: number
  }
  repair?: {
    policy: CheckoutRepairPolicy
    timeBudgetMs: number
  }
  /**
   * The committed walk manifest, relative to `workspaceRoot` (config `manifest`). Set, the CLI runs
   * the consumer's own flow files and the manifest names them; unset, it uses the stores' own copies.
   */
  manifest?: string
  /** WALK-021, iOS only: on a stale build, replay the last verified walk instead of rejecting. */
  staleBuild?: 'replay-last-verified'
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

function unitInterval(value: unknown, fallback: number, label: string) {
  const result = value === undefined ? fallback : value
  if (typeof result !== 'number' || !Number.isFinite(result) || result < fallback || result > 1) {
    throw new CliConfigurationError(`${label} must be a finite number between ${fallback} and 1`)
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

function loadIosExploreConfiguration(absolute: string, raw: Record<string, unknown>): IosExploreConfiguration {
  const workspaceRoot = resolve(dirname(absolute), String(raw.workspaceRoot ?? ''))
  const appId = typeof raw.appId === 'string' ? raw.appId.trim() : ''
  const buildPath = typeof raw.buildPath === 'string' ? raw.buildPath : ''
  if (!/^[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+$/.test(appId)) {
    throw new CliConfigurationError('iOS appId must be an explicit fixture bundle identifier')
  }
  if (!raw.workspaceRoot || !buildPath.endsWith('.app') || !inside(workspaceRoot, buildPath) ||
      !existsSync(workspaceRoot) || !existsSync(resolve(workspaceRoot, buildPath))) {
    throw new CliConfigurationError('iOS config requires existing workspaceRoot and an .app buildPath inside it')
  }
  if (typeof raw.device !== 'string' || !raw.device.trim()) {
    throw new CliConfigurationError('iOS config requires an explicit simulator UDID')
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
    platform: 'ios',
    buildPath,
    device: raw.device.trim(),
    fixtureCredentials,
    explorationTimeoutSeconds: positiveInteger(raw.explorationTimeoutSeconds, 120, 'explorationTimeoutSeconds'),
    verificationTimeoutMs: positiveInteger(raw.verificationTimeoutMs, 120_000, 'verificationTimeoutMs'),
    adapter: text(raw.adapter, 'maestro-ios/v1', 'adapter'),
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
    throw new CliConfigurationError(`Cannot read Déjà config ${configPath}`, { cause: error })
  }
  let explore: ExploreConfiguration | WebExploreConfiguration | IosExploreConfiguration
  try {
    explore = raw.platform === 'web'
      ? loadWebExploreConfiguration(absolute, raw)
      : raw.platform === 'ios'
        ? loadIosExploreConfiguration(absolute, raw)
        : loadExploreConfiguration(absolute)
  } catch (error) {
    throw new CliConfigurationError('Invalid Déjà fixture configuration', { cause: error })
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
        process.env.DEJA_OPENCLAW ?? 'openclaw',
        'openClaw.executable',
      ),
      mcpProxy,
    }
  }

  let repair: ProductionConfiguration['repair']
  if (explore.platform === 'web' && raw.repair !== undefined) {
    throw new CliConfigurationError('WALK-010 Web does not configure the Android-only repair plugin')
  }
  if (explore.platform === 'ios' && raw.repair !== undefined) {
    throw new CliConfigurationError('WALK-011 iOS does not configure the Android-only repair plugin')
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

  let manifest: string | undefined
  if (raw.manifest !== undefined) {
    try {
      manifest = checkManifestPath(text(raw.manifest, '', 'manifest'))
    } catch (error) {
      throw new CliConfigurationError(`manifest must be a relative .json path inside workspaceRoot: ${(error as Error).message}`, { cause: error })
    }
    if (raw.repair !== undefined) {
      throw new CliConfigurationError('repair is not supported with a manifest yet (DEJA-6)')
    }
  }

  let staleBuild: ProductionConfiguration['staleBuild']
  if (raw.staleBuild !== undefined) {
    if (explore.platform !== 'ios') {
      throw new CliConfigurationError('WALK-021 staleBuild is supported only for platform: ios')
    }
    if (raw.staleBuild !== 'replay-last-verified') {
      throw new CliConfigurationError('staleBuild must be "replay-last-verified" when present')
    }
    staleBuild = raw.staleBuild
  }

  let semantic: ProductionConfiguration['semantic']
  if (raw.semantic !== undefined) {
    if (!raw.semantic || typeof raw.semantic !== 'object' || Array.isArray(raw.semantic)) {
      throw new CliConfigurationError('semantic must be an object')
    }
    const configured = raw.semantic as Record<string, unknown>
    if (configured.provider !== undefined &&
        (typeof configured.provider !== 'string' || !configured.provider.trim())) {
      throw new CliConfigurationError('semantic.provider must be a non-empty string when present')
    }
    semantic = {
      provider: typeof configured.provider === 'string' ? configured.provider.trim() : null,
      minimumSimilarity: unitInterval(
        configured.minimumSimilarity, 0.90, 'semantic.minimumSimilarity',
      ),
      minimumMargin: unitInterval(configured.minimumMargin, 0.15, 'semantic.minimumMargin'),
    }
  }

  return {
    configPath: absolute,
    ...(raw.junit !== undefined
      ? { junitPath: resolve(dirname(absolute), text(raw.junit, '', 'junit')) }
      : {}),
    explore,
    workspaceRoot: root,
    maestro: {
      executable: text(
        maestro.executable,
        process.env.DEJA_MAESTRO ?? 'maestro',
        'maestro.executable',
      ),
      timeoutMs: positiveInteger(
        maestro.timeoutMs,
        Number(process.env.DEJA_TIMEOUT_MS ?? 120_000),
        'maestro.timeoutMs',
      ),
    },
    ...(openClawConfiguration ? { openClaw: openClawConfiguration } : {}),
    ...(semantic ? { semantic } : {}),
    ...(repair ? { repair } : {}),
    ...(staleBuild ? { staleBuild } : {}),
    ...(manifest ? { manifest } : {}),
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
  return resolve(configDirectory, 'deja.config.json')
}

export function configurationDirectory(configuration: ProductionConfiguration) {
  return dirname(configuration.configPath)
}
