import { createHash } from 'node:crypto'
import { mkdir, readFile, realpath, rename, stat, writeFile } from 'node:fs/promises'
import { dirname, isAbsolute, resolve, sep } from 'node:path'
import {
  normalizeGoal as normalizeGoalContract,
  type ExplorationDetails,
  type LookupHit,
  type LookupMiss,
  type PersistedWalk,
  type PersistVerifiedOptions,
  type PersistVerifiedRepairOptions,
  type RepairProvenance,
  type StoredWalk,
  type WalkIdentity,
  type WalkStore,
  type WalkValidation,
} from '../../core/src/index.ts'

export {
  type ExplorationDetails,
  type LookupHit,
  type LookupMiss,
  type PersistedWalk,
  type PersistVerifiedOptions,
  type PersistVerifiedRepairOptions,
  type RepairProvenance,
  type StoredWalk,
  type WalkIdentity,
  type WalkValidation,
} from '../../core/src/index.ts'

export const LOCAL_MEMORY_SCHEMA_VERSION = 2
export const MAESTRO_FLOW_SCHEMA_VERSION = 'maestro/v1'
export const LOCAL_MEMORY_PATH = '.walkcache/memory/walks.json'

export type WalkMemory = {
  schemaVersion: typeof LOCAL_MEMORY_SCHEMA_VERSION
  walks: StoredWalk[]
}

export type LocalWalkStoreOptions = {
  workspaceRoot: string
  /** Injectable only to model filesystem failures deterministically; production uses fs.rename. */
  renameFile?: (temporaryPath: string, destinationPath: string) => Promise<void>
}

export class InvalidWalkMemoryError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(`Invalid walk memory: ${message}`, options)
    this.name = 'InvalidWalkMemoryError'
  }
}

export function normalizeGoal(goal: string) {
  return normalizeGoalContract(goal).key
}

function errorCode(error: unknown) {
  return (error as NodeJS.ErrnoException).code
}

function inside(root: string, candidate: string) {
  return candidate !== root && candidate.startsWith(`${root}${sep}`)
}

function sha256(contents: Buffer) {
  return createHash('sha256').update(contents).digest('hex')
}

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function nonEmptyString(value: unknown): value is string {
  return typeof value === 'string' && value.trim().length > 0
}

function fingerprint(value: unknown): value is string {
  return typeof value === 'string' && /^[a-f0-9]{64}$/.test(value)
}

function timestamp(value: unknown): value is string {
  return typeof value === 'string' && Number.isFinite(Date.parse(value))
}

function readValidation(value: unknown): WalkValidation {
  if (!object(value) || typeof value.passed !== 'boolean' || !timestamp(value.at)) {
    throw new InvalidWalkMemoryError('walk validation requires passed: boolean and an ISO timestamp')
  }
  return { passed: value.passed, at: value.at }
}

function readExploration(value: unknown): ExplorationDetails {
  if (!object(value) || !nonEmptyString(value.modelCallBasis)) {
    throw new InvalidWalkMemoryError('walk exploration metadata is invalid')
  }
  const result = {} as ExplorationDetails
  for (const key of [
    'explorationDurationMs', 'verificationDurationMs', 'mcpCalls', 'modelCalls',
  ] as const) {
    const metric = value[key]
    if (metric !== null && (typeof metric !== 'number' || !Number.isFinite(metric) || metric < 0)) {
      throw new InvalidWalkMemoryError(`walk exploration ${key} must be a non-negative number or null`)
    }
    result[key] = metric as number | null
  }
  result.modelCallBasis = value.modelCallBasis
  return result
}

function readRepairProvenance(value: unknown): RepairProvenance {
  if (!object(value) || !nonEmptyString(value.attemptId) || !nonEmptyString(value.patchPath) ||
      !value.patchPath.startsWith('.walkcache/patches/') || isAbsolute(value.patchPath) ||
      value.patchPath.split('/').includes('..') || !fingerprint(value.previousBuildFingerprint) ||
      !fingerprint(value.previousFlowSha256)) {
    throw new InvalidWalkMemoryError('walk repair provenance is invalid')
  }
  return {
    attemptId: value.attemptId,
    patchPath: value.patchPath,
    previousBuildFingerprint: value.previousBuildFingerprint,
    previousFlowSha256: value.previousFlowSha256,
  }
}

function normalizeIdentity(identity: WalkIdentity) {
  const goal = identity.goal.trim()
  const goalKey = normalizeGoal(goal)
  if (!goalKey || !nonEmptyString(identity.appId) || !nonEmptyString(identity.platform) ||
      !nonEmptyString(identity.flowSchemaVersion) || !fingerprint(identity.buildFingerprint)) {
    throw new TypeError('A goal, app, platform, flow schema, and SHA-256 build fingerprint are required')
  }
  return { ...identity, goal, goalKey }
}

function validateWalk(value: unknown, legacy = false): StoredWalk {
  if (!object(value) || !nonEmptyString(value.goal) || value.goalKey !== normalizeGoal(value.goal) ||
      !nonEmptyString(value.appId) || !nonEmptyString(value.platform) ||
      !fingerprint(value.buildFingerprint) || !nonEmptyString(value.flowPath) ||
      !fingerprint(value.flowSha256)) {
    throw new InvalidWalkMemoryError('walk entry has missing or malformed identity fields')
  }
  if (legacy && value.flowSchemaVersion !== undefined && value.flowSchemaVersion !== MAESTRO_FLOW_SCHEMA_VERSION) {
    throw new InvalidWalkMemoryError('legacy walk has an incompatible flow schema')
  }
  if (!legacy && value.flowSchemaVersion !== MAESTRO_FLOW_SCHEMA_VERSION) {
    throw new InvalidWalkMemoryError(`unsupported flow schema in walk entry: ${String(value.flowSchemaVersion)}`)
  }
  const lastValidation = readValidation(value.lastValidation)
  const validationHistory = legacy
    ? [{ ...lastValidation, flowSha256: value.flowSha256 }]
    : value.validationHistory
  if (!Array.isArray(validationHistory) || validationHistory.length === 0) {
    throw new InvalidWalkMemoryError('walk validation history must not be empty')
  }
  const history = validationHistory.map(entry => {
    if (!object(entry) || !fingerprint(entry.flowSha256)) {
      throw new InvalidWalkMemoryError('walk validation history entry has no flow checksum')
    }
    return { ...readValidation(entry), flowSha256: entry.flowSha256 }
  })
  const latest = history.at(-1)!
  if (latest.passed !== lastValidation.passed || latest.at !== lastValidation.at ||
      latest.flowSha256 !== value.flowSha256) {
    throw new InvalidWalkMemoryError('lastValidation must equal the latest history entry for the stored flow')
  }
  if (value.origin !== undefined && value.origin !== 'remember' && value.origin !== 'explore' &&
      value.origin !== 'repair') {
    throw new InvalidWalkMemoryError('walk origin must be remember, explore, or repair')
  }
  if (value.repairHistory !== undefined &&
      (!Array.isArray(value.repairHistory) || value.repairHistory.length === 0)) {
    throw new InvalidWalkMemoryError('walk repairHistory must be a non-empty array when present')
  }
  const repairHistory = value.repairHistory?.map(readRepairProvenance)
  return {
    goal: value.goal,
    goalKey: value.goalKey,
    appId: value.appId,
    platform: value.platform,
    flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION,
    buildFingerprint: value.buildFingerprint,
    flowPath: value.flowPath,
    flowSha256: value.flowSha256,
    lastValidation,
    validationHistory: history,
    ...(value.origin ? { origin: value.origin } : {}),
    ...(value.exploration !== undefined ? { exploration: readExploration(value.exploration) } : {}),
    ...(repairHistory ? { repairHistory } : {}),
  }
}

function parseMemory(contents: string): WalkMemory {
  let parsed: unknown
  try {
    parsed = JSON.parse(contents)
  } catch (error) {
    throw new InvalidWalkMemoryError('walks.json is not valid JSON', { cause: error })
  }
  if (!object(parsed) || !Array.isArray(parsed.walks)) {
    throw new InvalidWalkMemoryError('walks.json requires a schemaVersion and walks array')
  }
  if (parsed.schemaVersion !== 1 && parsed.schemaVersion !== LOCAL_MEMORY_SCHEMA_VERSION) {
    throw new InvalidWalkMemoryError(`unsupported memory schemaVersion: ${String(parsed.schemaVersion)}`)
  }
  const legacy = parsed.schemaVersion === 1
  const walks = parsed.walks.map(walk => validateWalk(walk, legacy))
  const identities = new Set<string>()
  for (const walk of walks) {
    const identity = JSON.stringify([
      walk.goalKey, walk.appId, walk.platform, walk.flowSchemaVersion, walk.buildFingerprint,
    ])
    if (identities.has(identity)) throw new InvalidWalkMemoryError('duplicate exact walk identities are ambiguous')
    identities.add(identity)
  }
  return { schemaVersion: LOCAL_MEMORY_SCHEMA_VERSION, walks }
}

function flowSlug(goalKey: string) {
  const slug = goalKey.replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '')
  if (!slug) throw new TypeError('The normalized goal must contain a letter or number')
  return slug
}

export class LocalWalkStore implements WalkStore {
  readonly workspaceRoot: string
  readonly memoryPath: string
  private readonly renameFile: (temporaryPath: string, destinationPath: string) => Promise<void>

  constructor(options: LocalWalkStoreOptions) {
    this.workspaceRoot = resolve(options.workspaceRoot)
    this.memoryPath = resolve(this.workspaceRoot, LOCAL_MEMORY_PATH)
    this.renameFile = options.renameFile ?? rename
  }

  private async loadMemory(): Promise<WalkMemory> {
    try {
      return parseMemory(await readFile(this.memoryPath, 'utf8'))
    } catch (error) {
      if (errorCode(error) === 'ENOENT') {
        return { schemaVersion: LOCAL_MEMORY_SCHEMA_VERSION, walks: [] }
      }
      if (error instanceof InvalidWalkMemoryError) throw error
      throw new InvalidWalkMemoryError('walks.json cannot be read', { cause: error })
    }
  }

  private async canonicalRoot() {
    return realpath(this.workspaceRoot)
  }

  private async existingFile(root: string, path: string, allowAbsolute = false) {
    if (!allowAbsolute && isAbsolute(path)) {
      throw new InvalidWalkMemoryError(`flow path must be relative: ${path}`)
    }
    const candidate = allowAbsolute && isAbsolute(path) ? resolve(path) : resolve(root, path)
    if (!inside(root, candidate)) throw new InvalidWalkMemoryError(`flow path escapes workspace: ${path}`)
    try {
      const canonical = await realpath(candidate)
      if (!inside(root, canonical) || !(await stat(canonical)).isFile()) {
        throw new InvalidWalkMemoryError(`flow path does not resolve to a workspace file: ${path}`)
      }
      return canonical
    } catch (error) {
      if (error instanceof InvalidWalkMemoryError) throw error
      if (errorCode(error) === 'ENOENT') return null
      throw new InvalidWalkMemoryError(`flow path cannot be resolved: ${path}`, { cause: error })
    }
  }

  private async storedFlowPath(walk: StoredWalk): Promise<string | null> {
    const root = await this.canonicalRoot()
    const candidate = resolve(root, walk.flowPath)
    const flowRoot = resolve(root, '.walkcache/flows')
    if (isAbsolute(walk.flowPath) || !inside(flowRoot, candidate)) {
      throw new InvalidWalkMemoryError(`stored flow must stay under .walkcache/flows: ${walk.flowPath}`)
    }
    const canonical = await this.existingFile(root, walk.flowPath)
    if (!canonical) return null
    const canonicalFlowRoot = await realpath(flowRoot)
    if (!inside(root, canonicalFlowRoot) || !inside(canonicalFlowRoot, canonical)) {
      throw new InvalidWalkMemoryError(`canonical stored flow escapes .walkcache/flows: ${walk.flowPath}`)
    }
    return canonical
  }

  private async atomicWrite(path: string, contents: string | Buffer) {
    const root = await this.canonicalRoot()
    await mkdir(dirname(path), { recursive: true })
    const canonicalParent = await realpath(dirname(path))
    if (!inside(root, canonicalParent)) {
      throw new InvalidWalkMemoryError(`atomic write destination escapes workspace: ${path}`)
    }
    const temporary = `${path}.${process.pid}.tmp`
    await writeFile(temporary, contents)
    // If this rename is interrupted, only the *.tmp file remains. Lookup reads exact metadata and
    // content-addressed final flow names, never temporary files or unreferenced flows.
    await this.renameFile(temporary, path)
  }

  private async writeMemory(memory: WalkMemory) {
    await this.atomicWrite(this.memoryPath, `${JSON.stringify(memory, null, 2)}\n`)
  }

  private async persistVerified(
    options: PersistVerifiedOptions,
    repair?: PersistVerifiedRepairOptions,
  ): Promise<PersistedWalk> {
    if (!options.validation.passed) {
      throw new Error('Only an independently passing candidate can enter successful storage')
    }
    if (!timestamp(options.validation.at)) {
      throw new TypeError('Verified persistence requires a valid validation timestamp')
    }
    const identity = normalizeIdentity(options.identity)
    const root = await this.canonicalRoot()
    const candidate = await this.existingFile(root, options.candidatePath, true)
    if (!candidate) throw new Error(`Verified candidate flow does not exist: ${options.candidatePath}`)
    // Validate all existing entries before creating a replacement. Corrupt memory is never silently
    // repaired by dropping entries or by declaring a newly verified candidate a successful recovery.
    const memory = await this.loadMemory()
    const previous = repair ? memory.walks.find(item =>
      item.goalKey === identity.goalKey && item.appId === identity.appId &&
      item.platform === identity.platform && item.flowSchemaVersion === identity.flowSchemaVersion &&
      item.buildFingerprint === repair.previous.buildFingerprint &&
      item.flowSha256 === repair.previous.flowSha256 && item.lastValidation.passed,
    ) : undefined
    if (repair && !previous) {
      throw new InvalidWalkMemoryError('the previous successful repair base changed before replacement')
    }
    if (repair) readRepairProvenance(repair.repair)
    const candidateBytes = await readFile(candidate)
    const flowSha256 = sha256(candidateBytes)
    const platformSlug = flowSlug(normalizeGoal(identity.platform))
    const flowPath = `.walkcache/flows/${flowSlug(identity.goalKey)}/${platformSlug}-${flowSha256}.yaml`
    await this.atomicWrite(resolve(root, flowPath), candidateBytes)

    const validation = { passed: true as const, at: options.validation.at }
    const historyEntry = { ...validation, flowSha256 }
    const exploration = previous?.exploration ?? options.exploration
    const walk: StoredWalk = {
      ...identity,
      flowPath,
      flowSha256,
      lastValidation: validation,
      validationHistory: previous ? [...previous.validationHistory, historyEntry] : [historyEntry],
      origin: repair ? 'repair' : options.origin,
      ...(exploration ? { exploration } : {}),
      ...(repair && previous
        ? { repairHistory: [...(previous.repairHistory ?? []), repair.repair] } : {}),
    }
    const remaining = memory.walks.filter(item => !(
      item.goalKey === identity.goalKey && item.appId === identity.appId && item.platform === identity.platform
    ))
    await this.writeMemory({ schemaVersion: LOCAL_MEMORY_SCHEMA_VERSION, walks: [...remaining, walk] })
    return {
      walk,
      flowPath,
      flowSha256,
      storedMetadataPath: LOCAL_MEMORY_PATH,
      atomicWrite: true,
    }
  }

  async persistVerifiedCandidate(options: PersistVerifiedOptions): Promise<PersistedWalk> {
    return this.persistVerified(options)
  }

  async persistVerifiedRepair(options: PersistVerifiedRepairOptions): Promise<PersistedWalk> {
    return this.persistVerified({ ...options, origin: 'remember' }, options)
  }

  async list(): Promise<StoredWalk[]> {
    return [...(await this.loadMemory()).walks]
  }

  async lookup(identityInput: WalkIdentity): Promise<LookupHit | LookupMiss> {
    const identity = normalizeIdentity(identityInput)
    const memory = await this.loadMemory()
    const sameGoal = memory.walks.filter(item => item.goalKey === identity.goalKey)
    if (sameGoal.length === 0) return { status: 'miss', reason: 'not-found' }
    const sameApp = sameGoal.filter(item => item.appId === identity.appId)
    if (sameApp.length === 0) return { status: 'miss', reason: 'wrong-app' }
    const samePlatform = sameApp.filter(item => item.platform === identity.platform)
    if (samePlatform.length === 0) return { status: 'miss', reason: 'wrong-platform' }
    const sameSchema = samePlatform.filter(item => item.flowSchemaVersion === identity.flowSchemaVersion)
    if (sameSchema.length === 0) return { status: 'miss', reason: 'wrong-flow-schema' }
    const walk = sameSchema.find(item => item.buildFingerprint === identity.buildFingerprint)
    if (!walk) return { status: 'miss', reason: 'stale-build' }
    if (!walk.lastValidation.passed) return { status: 'miss', reason: 'failed-validation' }
    const flowPath = await this.storedFlowPath(walk)
    if (!flowPath) return { status: 'miss', reason: 'missing-flow' }
    if (sha256(await readFile(flowPath)) !== walk.flowSha256) {
      throw new InvalidWalkMemoryError(`stored flow checksum does not match ${walk.flowPath}`)
    }
    return { status: 'hit', walk, flowPath }
  }

  /**
   * Select a previous successful flow only for an explicitly requested repair diagnosis. This is
   * never a warm-cache hit: the caller must execute and classify it before making any source change.
   */
  async lookupPreviousSuccessful(identityInput: WalkIdentity): Promise<LookupHit | LookupMiss> {
    const identity = normalizeIdentity(identityInput)
    const memory = await this.loadMemory()
    const candidates = memory.walks.filter(item =>
      item.goalKey === identity.goalKey && item.appId === identity.appId &&
      item.platform === identity.platform && item.flowSchemaVersion === identity.flowSchemaVersion &&
      item.lastValidation.passed,
    )
    if (candidates.length === 0) return { status: 'miss', reason: 'not-found' }
    candidates.sort((left, right) => right.lastValidation.at.localeCompare(left.lastValidation.at))
    const walk = candidates[0]
    const flowPath = await this.storedFlowPath(walk)
    if (!flowPath) return { status: 'miss', reason: 'missing-flow' }
    if (sha256(await readFile(flowPath)) !== walk.flowSha256) {
      throw new InvalidWalkMemoryError(`stored flow checksum does not match ${walk.flowPath}`)
    }
    return { status: 'hit', walk, flowPath }
  }

  async recordValidation(identityInput: WalkIdentity, validation: WalkValidation): Promise<StoredWalk> {
    if (!timestamp(validation.at)) throw new TypeError('Validation requires a valid timestamp')
    const identity = normalizeIdentity(identityInput)
    const hit = await this.lookup(identity)
    if (hit.status !== 'hit') {
      throw new InvalidWalkMemoryError(`cannot validate a flow that is not a usable hit: ${hit.reason}`)
    }
    const memory = await this.loadMemory()
    const index = memory.walks.findIndex(item =>
      item.goalKey === identity.goalKey && item.appId === identity.appId &&
      item.platform === identity.platform && item.flowSchemaVersion === identity.flowSchemaVersion &&
      item.buildFingerprint === identity.buildFingerprint && item.flowSha256 === hit.walk.flowSha256,
    )
    if (index < 0) throw new InvalidWalkMemoryError('stored walk changed while validation was running')
    const previous = memory.walks[index]
    const entry = { ...validation, flowSha256: previous.flowSha256 }
    const updated: StoredWalk = {
      ...previous,
      lastValidation: { passed: validation.passed, at: validation.at },
      validationHistory: [...previous.validationHistory, entry],
    }
    const walks = [...memory.walks]
    walks[index] = updated
    await this.writeMemory({ schemaVersion: LOCAL_MEMORY_SCHEMA_VERSION, walks })
    return updated
  }
}
