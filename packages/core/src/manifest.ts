import { normalizeGoal } from './contracts.ts'

/** Version of the manifest file format. A reader refuses any other value. */
export const WALK_MANIFEST_VERSION = 1
/** Version of the closure and assertion hash forms the manifest's hashes use. */
export const MANIFEST_HASH_VERSION = 1

export type ManifestOwnerRules = {
  /** The flow must hold at least this many product assertions. */
  minAssertions?: number
  /** Each id must be asserted with `assertVisible` somewhere in the flow or its subflows. */
  requiredAssertionIds?: string[]
}

/** Launch settings the walk was made with. Names and plain values only, never secrets. */
export type ManifestProfile = {
  name?: string
  launchArguments?: Record<string, string | number | boolean>
  clearState?: boolean
}

export type ManifestWalk = {
  goal: string
  appId: string
  platform: string
  adapter: string
  /** Web only. */
  browser?: string
  /** Relative to the workspace root, with `/` separators, never under `.deja/`. */
  flow: string
  /** The main flow and every file it runs. */
  closureSha256: string
  /** The frozen product assertions. Only `remember` or an approved proposal changes it. */
  assertionsSha256: string
  ownerRules?: ManifestOwnerRules
  profile?: ManifestProfile
}

export type WalkManifest = {
  manifestVersion: typeof WALK_MANIFEST_VERSION
  hashVersion: typeof MANIFEST_HASH_VERSION
  walks: ManifestWalk[]
}

/** The manifest is missing a field, malformed, unsafe, or of a version this build does not know. */
export class InvalidWalkManifestError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(`Invalid walk manifest: ${message}`, options)
    this.name = 'InvalidWalkManifestError'
  }
}

export function emptyWalkManifest(): WalkManifest {
  return { manifestVersion: WALK_MANIFEST_VERSION, hashVersion: MANIFEST_HASH_VERSION, walks: [] }
}

const SHA = /^[a-f0-9]{64}$/
const HOME_PATH = /(^|[\s"'=:])(\/Users\/|\/home\/|\/var\/|\/private\/|~\/|[A-Za-z]:[\\/])/
const SECRET_NAME = /pass(word|wd)?|secret|token|api[-_]?key|credential|private[-_]?key/i

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function onlyKeys(value: Record<string, unknown>, allowed: string[], where: string) {
  const extra = Object.keys(value).filter(key => !allowed.includes(key))
  if (extra.length > 0) throw new InvalidWalkManifestError(`${where} has unknown field: ${extra[0]}`)
}

function text(value: unknown, where: string) {
  if (typeof value !== 'string' || value.trim().length === 0 || value.includes('\0')) {
    throw new InvalidWalkManifestError(`${where} must be a non-empty string`)
  }
  if (value.startsWith('/') || HOME_PATH.test(value)) {
    throw new InvalidWalkManifestError(`${where} looks like an absolute or home path`)
  }
  return value
}

/** A path inside the workspace, `/`-separated, outside `.deja/`. Throws on anything else. */
export function checkManifestFlowPath(value: unknown, where = 'flow'): string {
  if (typeof value !== 'string' || value.length === 0 || value.includes('\0') ||
      value.includes('\\') || value.startsWith('/') || /^[A-Za-z]:/.test(value)) {
    throw new InvalidWalkManifestError(`${where} must be a relative path with / separators`)
  }
  const segments = value.split('/')
  if (segments.some(segment => segment === '' || segment === '.' || segment === '..')) {
    throw new InvalidWalkManifestError(`${where} must not hold empty, . or .. segments: ${value}`)
  }
  if (segments[0] === '.deja') {
    throw new InvalidWalkManifestError(`${where} must be the consumer's own file, not under .deja/: ${value}`)
  }
  if (!/\.ya?ml$/i.test(value)) throw new InvalidWalkManifestError(`${where} must be a YAML file: ${value}`)
  return value
}

function readOwnerRules(value: unknown, where: string): ManifestOwnerRules {
  if (!object(value)) throw new InvalidWalkManifestError(`${where} must be an object`)
  onlyKeys(value, ['minAssertions', 'requiredAssertionIds'], where)
  const rules: ManifestOwnerRules = {}
  if (value.minAssertions !== undefined) {
    if (!Number.isInteger(value.minAssertions) || (value.minAssertions as number) < 0) {
      throw new InvalidWalkManifestError(`${where}.minAssertions must be a non-negative integer`)
    }
    rules.minAssertions = value.minAssertions as number
  }
  if (value.requiredAssertionIds !== undefined) {
    if (!Array.isArray(value.requiredAssertionIds)) {
      throw new InvalidWalkManifestError(`${where}.requiredAssertionIds must be an array`)
    }
    rules.requiredAssertionIds = value.requiredAssertionIds.map((id, index) =>
      text(id, `${where}.requiredAssertionIds[${index}]`))
  }
  return rules
}

function readProfile(value: unknown, where: string): ManifestProfile {
  if (!object(value)) throw new InvalidWalkManifestError(`${where} must be an object`)
  onlyKeys(value, ['name', 'launchArguments', 'clearState'], where)
  const profile: ManifestProfile = {}
  if (value.name !== undefined) profile.name = text(value.name, `${where}.name`)
  if (value.clearState !== undefined) {
    if (typeof value.clearState !== 'boolean') {
      throw new InvalidWalkManifestError(`${where}.clearState must be a boolean`)
    }
    profile.clearState = value.clearState
  }
  if (value.launchArguments !== undefined) {
    if (!object(value.launchArguments)) {
      throw new InvalidWalkManifestError(`${where}.launchArguments must be an object`)
    }
    const launchArguments: Record<string, string | number | boolean> = {}
    for (const [key, argument] of Object.entries(value.launchArguments)) {
      if (SECRET_NAME.test(key)) {
        throw new InvalidWalkManifestError(`${where}.launchArguments.${key} looks like a secret`)
      }
      if (typeof argument === 'string') launchArguments[key] = text(argument, `${where}.launchArguments.${key}`)
      else if (typeof argument === 'boolean' || (typeof argument === 'number' && Number.isFinite(argument))) {
        launchArguments[key] = argument
      } else throw new InvalidWalkManifestError(`${where}.launchArguments.${key} must be a string, number or boolean`)
    }
    profile.launchArguments = launchArguments
  }
  return profile
}

function readWalk(value: unknown, index: number): ManifestWalk {
  const where = `walks[${index}]`
  if (!object(value)) throw new InvalidWalkManifestError(`${where} must be an object`)
  onlyKeys(value, [
    'goal', 'appId', 'platform', 'adapter', 'browser', 'flow', 'closureSha256', 'assertionsSha256',
    'ownerRules', 'profile',
  ], where)
  const goal = text(value.goal, `${where}.goal`)
  try {
    normalizeGoal(goal)
  } catch {
    throw new InvalidWalkManifestError(`${where}.goal must not be empty`)
  }
  const platform = text(value.platform, `${where}.platform`)
  if (platform !== 'ios' && platform !== 'android' && platform !== 'web') {
    throw new InvalidWalkManifestError(`${where}.platform must be ios, android or web`)
  }
  if (!SHA.test(String(value.closureSha256)) || !SHA.test(String(value.assertionsSha256))) {
    throw new InvalidWalkManifestError(`${where} needs lowercase hex SHA-256 closureSha256 and assertionsSha256`)
  }
  if (platform === 'web' ? value.browser === undefined : value.browser !== undefined) {
    throw new InvalidWalkManifestError(`${where}.browser is required for web and not allowed otherwise`)
  }
  return {
    goal,
    appId: text(value.appId, `${where}.appId`),
    platform,
    adapter: text(value.adapter, `${where}.adapter`),
    ...(value.browser !== undefined ? { browser: text(value.browser, `${where}.browser`) } : {}),
    flow: checkManifestFlowPath(value.flow, `${where}.flow`),
    closureSha256: value.closureSha256 as string,
    assertionsSha256: value.assertionsSha256 as string,
    ...(value.ownerRules !== undefined ? { ownerRules: readOwnerRules(value.ownerRules, `${where}.ownerRules`) } : {}),
    ...(value.profile !== undefined ? { profile: readProfile(value.profile, `${where}.profile`) } : {}),
  }
}

/** The identity of one entry: a goal is one walk per app, platform, adapter and browser. */
export function manifestWalkKey(walk: Pick<ManifestWalk, 'goal' | 'appId' | 'platform' | 'adapter' | 'browser'>) {
  return JSON.stringify([
    normalizeGoal(walk.goal).key, walk.appId, walk.platform, walk.adapter, walk.browser ?? null,
  ])
}

/** Read a manifest. Fails closed on an unknown version, an unknown field, an unsafe value or a duplicate. */
export function parseWalkManifest(contents: string): WalkManifest {
  let parsed: unknown
  try {
    parsed = JSON.parse(contents)
  } catch (error) {
    throw new InvalidWalkManifestError('not valid JSON', { cause: error })
  }
  if (!object(parsed)) throw new InvalidWalkManifestError('expected an object')
  if (parsed.manifestVersion !== WALK_MANIFEST_VERSION) {
    throw new InvalidWalkManifestError(`unsupported manifestVersion: ${String(parsed.manifestVersion)}`)
  }
  if (parsed.hashVersion !== MANIFEST_HASH_VERSION) {
    throw new InvalidWalkManifestError(`unsupported hashVersion: ${String(parsed.hashVersion)}`)
  }
  onlyKeys(parsed, ['manifestVersion', 'hashVersion', 'walks'], 'manifest')
  if (!Array.isArray(parsed.walks)) throw new InvalidWalkManifestError('walks must be an array')
  const walks = parsed.walks.map(readWalk)
  const seen = new Set<string>()
  for (const walk of walks) {
    const key = manifestWalkKey(walk)
    if (seen.has(key)) throw new InvalidWalkManifestError(`duplicate entry for goal "${walk.goal}"`)
    seen.add(key)
  }
  return { manifestVersion: WALK_MANIFEST_VERSION, hashVersion: MANIFEST_HASH_VERSION, walks }
}

function ordered(walk: ManifestWalk): ManifestWalk {
  return {
    goal: walk.goal,
    appId: walk.appId,
    platform: walk.platform,
    adapter: walk.adapter,
    ...(walk.browser !== undefined ? { browser: walk.browser } : {}),
    flow: walk.flow,
    closureSha256: walk.closureSha256,
    assertionsSha256: walk.assertionsSha256,
    ...(walk.ownerRules ? {
      ownerRules: {
        ...(walk.ownerRules.minAssertions !== undefined ? { minAssertions: walk.ownerRules.minAssertions } : {}),
        ...(walk.ownerRules.requiredAssertionIds ? { requiredAssertionIds: walk.ownerRules.requiredAssertionIds } : {}),
      },
    } : {}),
    ...(walk.profile ? {
      profile: {
        ...(walk.profile.name !== undefined ? { name: walk.profile.name } : {}),
        ...(walk.profile.launchArguments ? {
          launchArguments: Object.fromEntries(
            Object.entries(walk.profile.launchArguments).sort(([left], [right]) => (left < right ? -1 : 1)),
          ),
        } : {}),
        ...(walk.profile.clearState !== undefined ? { clearState: walk.profile.clearState } : {}),
      },
    } : {}),
  }
}

/** The bytes Déjà writes: fixed field order, sorted entries, no timestamps, one trailing newline. */
export function serializeWalkManifest(manifest: WalkManifest): string {
  const walks = manifest.walks.map(ordered).sort((left, right) => {
    const a = manifestWalkKey(left)
    const b = manifestWalkKey(right)
    return a < b ? -1 : a > b ? 1 : 0
  })
  return `${JSON.stringify({
    manifestVersion: WALK_MANIFEST_VERSION, hashVersion: MANIFEST_HASH_VERSION, walks,
  }, null, 2)}\n`
}
