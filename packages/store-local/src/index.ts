import { createHash, randomUUID } from 'node:crypto'
import { lstatSync, realpathSync, statSync } from 'node:fs'
import { mkdir, readFile, realpath, rename, stat, writeFile } from 'node:fs/promises'
import { dirname, isAbsolute, relative, resolve, sep } from 'node:path'
import {
  ExpectedClosureMismatchError,
  ExpectedFlowChecksumMismatchError,
  normalizeGoal as normalizeGoalContract,
  type CandidateSnapshot,
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
} from '@deja-qa/core'
import {
  ConsumerFlows,
  readConsumerWalkFields,
} from './consumer-flow.ts'
import {
  checkStoredSubflows,
  collectFlowClosure,
  planStoredClosure,
  readStoredFlowFiles,
  snapshotFlowClosure,
} from './flow-closure.ts'
import { WriterLock, storeMetadataPath } from './writer-lock.ts'

export {
  type CandidateSnapshot,
  type ExplorationDetails,
  type LookupHit,
  type LookupMiss,
  type PersistedWalk,
  type PersistVerifiedOptions,
  type PersistVerifiedRepairOptions,
  type RepairProvenance,
  type StoredFlowFile,
  type StoredWalk,
  type WalkIdentity,
  type WalkValidation,
} from '@deja-qa/core'

export const LOCAL_MEMORY_SCHEMA_VERSION = 3
/** Written only when a walk replays the consumer's own file (manifest mode). */
export const LOCAL_MEMORY_CONSUMER_SCHEMA_VERSION = 4
export const MAESTRO_FLOW_SCHEMA_VERSION = 'maestro/v1'
export const LOCAL_MEMORY_PATH = '.deja/memory/walks.json'

export type WalkMemory = {
  schemaVersion: typeof LOCAL_MEMORY_SCHEMA_VERSION | typeof LOCAL_MEMORY_CONSUMER_SCHEMA_VERSION
  walks: StoredWalk[]
}

export type LocalWalkStoreOptions = {
  workspaceRoot: string
  /**
   * Keep this store's history in its own folder, for example one per device. Without it the
   * store is the workspace's single store. One writer per store: a second one gets StoreLockedError.
   */
  storeId?: string
  /**
   * Replay the consumer's own files through this committed manifest (path relative to the
   * workspace, for example `deja.walks.json`). The store then keeps history only: no copy of a flow
   * is made or read. Without it the store works as before, on its own copies under `.deja/flows`.
   */
  manifest?: {
    path: string
    /** Fixture values that must never be written into the manifest. */
    forbiddenValues?: string[]
  }
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

/** Resolve metadata without following an unchecked path outside the canonical workspace. */
function canonicalMetadataTarget(workspaceRoot: string, memoryPath: string) {
  try {
    const lexicalRoot = resolve(workspaceRoot)
    const root = realpathSync(lexicalRoot)
    if (!statSync(root).isDirectory()) {
      throw new InvalidWalkMemoryError('workspace root is not a directory')
    }
    const candidate = resolve(root, relative(lexicalRoot, memoryPath))
    let ancestor = candidate
    while (true) {
      try {
        lstatSync(ancestor)
        break
      } catch (error) {
        if (errorCode(error) === 'ENOTDIR') {
          throw new InvalidWalkMemoryError('metadata path traverses a non-directory', { cause: error })
        }
        if (errorCode(error) !== 'ENOENT') throw error
        const parent = dirname(ancestor)
        if (parent === ancestor || (parent !== root && !inside(root, parent))) {
          throw new InvalidWalkMemoryError('metadata path has no safe existing parent')
        }
        ancestor = parent
      }
    }
    const canonicalAncestor = realpathSync(ancestor)
    if (canonicalAncestor !== root && !inside(root, canonicalAncestor)) {
      throw new InvalidWalkMemoryError('metadata path escapes workspace')
    }
    const canonicalStat = statSync(canonicalAncestor)
    if (ancestor === candidate) {
      if (!canonicalStat.isFile()) {
        throw new InvalidWalkMemoryError('metadata target is not a regular file')
      }
      return { path: canonicalAncestor, exists: true }
    }
    if (!canonicalStat.isDirectory()) {
      throw new InvalidWalkMemoryError('metadata path traverses a non-directory')
    }
    const projected = resolve(canonicalAncestor, relative(ancestor, candidate))
    if (!inside(root, projected)) {
      throw new InvalidWalkMemoryError('metadata path escapes workspace')
    }
    return { path: projected, exists: false }
  } catch (error) {
    if (error instanceof InvalidWalkMemoryError) throw error
    throw new InvalidWalkMemoryError('metadata path cannot be resolved safely', { cause: error })
  }
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
      !value.patchPath.startsWith('.deja/patches/') || isAbsolute(value.patchPath) ||
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
  if (identity.platform === 'web' &&
      (!nonEmptyString(identity.adapter) || !nonEmptyString(identity.browser))) {
    throw new TypeError('Web cache identity requires explicit adapter and browser names')
  }
  if ((identity.adapter !== undefined && !nonEmptyString(identity.adapter)) ||
      (identity.browser !== undefined && !nonEmptyString(identity.browser))) {
    throw new TypeError('Adapter and browser identity must be non-empty when present')
  }
  return { ...identity, goal, goalKey }
}

function validateWalk(value: unknown, legacy = false, schemaVersion = LOCAL_MEMORY_SCHEMA_VERSION): StoredWalk {
  if (!object(value) || !nonEmptyString(value.goal) || value.goalKey !== normalizeGoal(value.goal) ||
      !nonEmptyString(value.appId) || !nonEmptyString(value.platform) ||
      !fingerprint(value.buildFingerprint) || !nonEmptyString(value.flowPath) ||
      !fingerprint(value.flowSha256)) {    throw new InvalidWalkMemoryError('walk entry has missing or malformed identity fields')
  }
  if (legacy && value.flowSchemaVersion !== undefined && value.flowSchemaVersion !== MAESTRO_FLOW_SCHEMA_VERSION) {
    throw new InvalidWalkMemoryError('legacy walk has an incompatible flow schema')
  }
  if ((value.adapter !== undefined && !nonEmptyString(value.adapter)) ||
      (value.browser !== undefined && !nonEmptyString(value.browser)) ||
      (value.platform === 'web' && (!nonEmptyString(value.adapter) || !nonEmptyString(value.browser)))) {
    throw new InvalidWalkMemoryError('walk adapter/browser identity is missing or malformed')
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
      value.origin !== 'repair' && value.origin !== 'replayed-on-new-build' && value.origin !== 'verify') {
    throw new InvalidWalkMemoryError('walk origin must be remember, explore, repair, replayed-on-new-build, or verify')
  }
  if (value.repairHistory !== undefined &&
      (!Array.isArray(value.repairHistory) || value.repairHistory.length === 0)) {
    throw new InvalidWalkMemoryError('walk repairHistory must be a non-empty array when present')
  }
  const repairHistory = value.repairHistory?.map(readRepairProvenance)
  if (value.closureSha256 !== undefined) {
    // A walk that replays the consumer's own file: history only, no copy and no file list.
    if (schemaVersion < 4 || value.subflowFiles !== undefined || value.legacyClosure !== undefined) {
      throw new InvalidWalkMemoryError('closureSha256 needs memory schema 4 and excludes subflowFiles')
    }
    const problem = readConsumerWalkFields({ flowPath: value.flowPath, closureSha256: value.closureSha256 })
    if (problem) throw new InvalidWalkMemoryError(`walk closure is invalid: ${problem}`)
  }
  // Schema 3 records the files a flow runs, even when there are none. A walk that lost the list
  // would replay its subflows unchecked, so it is refused. Earlier schemas stored the main flow only.
  else if ((schemaVersion >= 3) !== (value.subflowFiles !== undefined)) {
    throw new InvalidWalkMemoryError(schemaVersion >= 3
      ? 'walk is missing subflowFiles, which memory schema 3 requires'
      : 'subflowFiles needs memory schema 3')
  }
  const subflowFiles = value.subflowFiles === undefined ? undefined : readStoredFlowFiles(value.subflowFiles)
  if (subflowFiles === null) throw new InvalidWalkMemoryError('walk subflowFiles is invalid')
  if (value.legacyClosure !== undefined && (value.legacyClosure !== true || schemaVersion < 3)) {
    throw new InvalidWalkMemoryError('walk legacyClosure is invalid')
  }
  return {
    goal: value.goal,
    goalKey: value.goalKey,
    appId: value.appId,
    platform: value.platform,
    flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION,
    buildFingerprint: value.buildFingerprint,
    ...(value.adapter !== undefined ? { adapter: value.adapter as string } : {}),
    ...(value.browser !== undefined ? { browser: value.browser as string } : {}),
    flowPath: value.flowPath,
    flowSha256: value.flowSha256,
    ...(subflowFiles ? { subflowFiles } : {}),
    ...(value.legacyClosure ? { legacyClosure: true as const } : {}),
    ...(value.closureSha256 !== undefined ? { closureSha256: value.closureSha256 as string } : {}),
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
  if (parsed.schemaVersion !== 1 && parsed.schemaVersion !== 2 &&
      parsed.schemaVersion !== LOCAL_MEMORY_SCHEMA_VERSION &&
      parsed.schemaVersion !== LOCAL_MEMORY_CONSUMER_SCHEMA_VERSION) {
    throw new InvalidWalkMemoryError(`unsupported memory schemaVersion: ${String(parsed.schemaVersion)}`)
  }
  const legacy = parsed.schemaVersion === 1
  const walks = parsed.walks.map(walk => validateWalk(walk, legacy, parsed.schemaVersion as number))
  const identities = new Set<string>()
  for (const walk of walks) {
    const identity = JSON.stringify([
      walk.goalKey, walk.appId, walk.platform, walk.flowSchemaVersion, walk.buildFingerprint,
      walk.adapter ?? null, walk.browser ?? null,
    ])
    if (identities.has(identity)) throw new InvalidWalkMemoryError('duplicate exact walk identities are ambiguous')
    identities.add(identity)
  }
  return {
    schemaVersion: walks.some(walk => walk.closureSha256 !== undefined)
      ? LOCAL_MEMORY_CONSUMER_SCHEMA_VERSION : LOCAL_MEMORY_SCHEMA_VERSION,
    walks,
  }
}

function flowSlug(goalKey: string) {
  const slug = goalKey.replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '')
  if (!slug) throw new TypeError('The normalized goal must contain a letter or number')
  return slug
}

export class LocalWalkStore implements WalkStore {
  readonly workspaceRoot: string
  readonly memoryPath: string
  /** The metadata path relative to the workspace. */
  readonly relativeMemoryPath: string
  private readonly renameFile: (temporaryPath: string, destinationPath: string) => Promise<void>
  /** The manifest's workspace-relative path, when this store replays the consumer's own files. */
  readonly manifestPath: string | undefined
  private readonly consumer: ConsumerFlows | null
  private writerLock: WriterLock | null = null

  constructor(options: LocalWalkStoreOptions) {
    this.workspaceRoot = resolve(options.workspaceRoot)
    this.consumer = options.manifest
      ? new ConsumerFlows({
        workspaceRoot: this.workspaceRoot,
        manifestPath: options.manifest.path,
        ...(options.manifest.forbiddenValues ? { forbiddenValues: options.manifest.forbiddenValues } : {}),
      })
      : null
    this.manifestPath = this.consumer?.manifestPath
    this.relativeMemoryPath = storeMetadataPath(LOCAL_MEMORY_PATH, options.storeId)
    this.memoryPath = resolve(this.workspaceRoot, this.relativeMemoryPath)
    this.renameFile = options.renameFile ?? rename
  }

  /**
   * Take the single writer lock now (throws StoreLockedError). Writes take it on their own; a host
   * calls this first to be refused before a long run, not after it. Reads never take it.
   */
  acquireWriter() {
    if (!this.writerLock) {
      const metadata = canonicalMetadataTarget(this.workspaceRoot, this.memoryPath)
      this.writerLock = new WriterLock(`${metadata.path}.lock`)
    }
    this.writerLock.acquire()
  }

  /** Release the writer lock. The store can still be read, and takes the lock again on its next write. */
  close() {
    this.writerLock?.release()
  }

  private async loadMemory(): Promise<WalkMemory> {
    const metadata = canonicalMetadataTarget(this.workspaceRoot, this.memoryPath)
    if (!metadata.exists) return { schemaVersion: LOCAL_MEMORY_SCHEMA_VERSION, walks: [] }
    try {
      return parseMemory(await readFile(metadata.path, 'utf8'))
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
    const flowRoot = resolve(root, '.deja/flows')
    if (isAbsolute(walk.flowPath) || !inside(flowRoot, candidate)) {
      throw new InvalidWalkMemoryError(`stored flow must stay under .deja/flows: ${walk.flowPath}`)
    }
    const canonical = await this.existingFile(root, walk.flowPath)
    if (!canonical) return null
    const canonicalFlowRoot = await realpath(flowRoot)
    if (!inside(root, canonicalFlowRoot) || !inside(canonicalFlowRoot, canonical)) {
      throw new InvalidWalkMemoryError(`canonical stored flow escapes .deja/flows: ${walk.flowPath}`)
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
    // A unique name per call: stores in one process share a pid and may write the same
    // content-addressed flow at once. rename replaces the destination atomically, so a reader sees a
    // whole file, and two identical writes leave one identical file.
    const temporary = `${path}.${process.pid}-${randomUUID()}.tmp`
    await writeFile(temporary, contents)
    // If this rename is interrupted, only the *.tmp file remains. Lookup reads exact metadata and
    // content-addressed final flow names, never temporary files or unreferenced flows.
    await this.renameFile(temporary, path)
  }

  private async writeMemory(memory: WalkMemory) {
    this.acquireWriter() // Still the holder? A lost lock stops the write before it commits.
    const metadata = canonicalMetadataTarget(this.workspaceRoot, this.memoryPath)
    // A walk from before schema 3 has no stored subflows: mark it so lookup misses instead of guessing.
    const walks = memory.walks.map(walk => walk.subflowFiles || walk.closureSha256 !== undefined
      ? walk : { ...walk, subflowFiles: [], legacyClosure: true as const })
    const schemaVersion = walks.some(walk => walk.closureSha256 !== undefined)
      ? LOCAL_MEMORY_CONSUMER_SCHEMA_VERSION : LOCAL_MEMORY_SCHEMA_VERSION
    await this.atomicWrite(metadata.path, `${JSON.stringify({ schemaVersion, walks }, null, 2)}\n`)
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
    this.acquireWriter()
    if (this.consumer) return this.persistConsumer(this.consumer, identity, options, repair)
    const root = await this.canonicalRoot()
    const candidate = await this.existingFile(root, options.candidatePath, true)
    if (!candidate) throw new Error(`Verified candidate flow does not exist: ${options.candidatePath}`)
    // Validate all existing entries before creating a replacement. Corrupt memory is never silently
    // repaired by dropping entries or by declaring a newly verified candidate a successful recovery.
    const memory = await this.loadMemory()
    const previous = repair ? memory.walks.find(item =>
      item.goalKey === identity.goalKey && item.appId === identity.appId &&
      item.platform === identity.platform && item.flowSchemaVersion === identity.flowSchemaVersion &&
      (item.adapter ?? null) === (identity.adapter ?? null) &&
      (item.browser ?? null) === (identity.browser ?? null) &&
      item.buildFingerprint === repair.previous.buildFingerprint &&
      item.flowSha256 === repair.previous.flowSha256 && item.lastValidation.passed,
    ) : undefined
    if (repair && !previous) {
      throw new InvalidWalkMemoryError('the previous successful repair base changed before replacement')
    }
    if (repair) readRepairProvenance(repair.repair)
    const closure = await collectFlowClosure(root, candidate)
    const mainSha256 = closure.files.find(file => file.path === closure.main)!.sha256
    if (options.expectedFlowSha256 !== undefined && options.expectedFlowSha256 !== mainSha256) {
      throw new ExpectedFlowChecksumMismatchError(options.expectedFlowSha256, mainSha256)
    }
    if (options.expectedClosureSha256 !== undefined && options.expectedClosureSha256 !== closure.sha256) {
      throw new ExpectedClosureMismatchError(options.expectedClosureSha256, closure.sha256)
    }
    const plan = planStoredClosure(
      closure,
      flowSlug(identity.goalKey),
      flowSlug(normalizeGoal(identity.platform)),
    )
    const { flowPath, flowSha256 } = plan
    for (const write of plan.writes) await this.atomicWrite(resolve(root, write.path), write.bytes)

    const validation = { passed: true as const, at: options.validation.at }
    const historyEntry = { ...validation, flowSha256 }
    const exploration = previous?.exploration ?? options.exploration
    const walk: StoredWalk = {
      ...identity,
      flowPath,
      flowSha256,
      subflowFiles: plan.subflowFiles,
      lastValidation: validation,
      validationHistory: previous ? [...previous.validationHistory, historyEntry] : [historyEntry],
      origin: repair ? 'repair' : options.origin,
      ...(exploration ? { exploration } : {}),
      ...(repair && previous
        ? { repairHistory: [...(previous.repairHistory ?? []), repair.repair] } : {}),
    }
    const remaining = memory.walks.filter(item => !(
      item.goalKey === identity.goalKey && item.appId === identity.appId && item.platform === identity.platform &&
      (item.adapter ?? null) === (identity.adapter ?? null) &&
      (item.browser ?? null) === (identity.browser ?? null)
    ))
    await this.writeMemory({ schemaVersion: LOCAL_MEMORY_SCHEMA_VERSION, walks: [...remaining, walk] })
    return {
      walk,
      flowPath,
      flowSha256,
      storedMetadataPath: this.relativeMemoryPath,
      atomicWrite: true,
    }
  }

  /**
   * Manifest mode: keep history for the consumer's own file and, if its entry changed, write the
   * manifest. Memory is written first, so a failed manifest write leaves history that no entry matches.
   */
  private async persistConsumer(
    consumer: ConsumerFlows,
    identity: ReturnType<typeof normalizeIdentity>,
    options: PersistVerifiedOptions,
    repair: PersistVerifiedRepairOptions | undefined,
  ): Promise<PersistedWalk> {
    if (repair) {
      throw new Error('Repair is not supported with a manifest yet: it would edit a copy, not the consumer\'s file (DEJA-6)')
    }
    return consumer.withLock(async () => {
      // Validate all existing entries first: corrupt memory is never silently replaced.
      const memory = await this.loadMemory()
      const plan = await consumer.plan(memory.walks, identity, options)
      await this.writeMemory({
        schemaVersion: LOCAL_MEMORY_CONSUMER_SCHEMA_VERSION, walks: [...plan.remaining, plan.walk],
      })
      const changed = await plan.commit()
      return {
        walk: plan.walk,
        flowPath: plan.flowPath,
        flowSha256: plan.flowSha256,
        storedMetadataPath: this.relativeMemoryPath,
        atomicWrite: true,
        manifest: { path: consumer.manifestPath, changed },
      }
    })
  }

  async snapshotCandidate(candidatePath: string, directory: string): Promise<CandidateSnapshot> {
    // Manifest mode hashes the consumer's files and copies nothing: they run in place.
    if (this.consumer) return this.consumer.snapshot(candidatePath)
    const root = await this.canonicalRoot()
    const candidate = await this.existingFile(root, candidatePath, true)
    if (!candidate) throw new Error(`Verified candidate flow does not exist: ${candidatePath}`)
    return snapshotFlowClosure(root, candidate, directory)
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
    if (this.consumer) return this.consumer.lookup(memory.walks, identity)
    const sameGoal = memory.walks.filter(item => item.goalKey === identity.goalKey)
    if (sameGoal.length === 0) return { status: 'miss', reason: 'not-found' }
    const sameApp = sameGoal.filter(item => item.appId === identity.appId)
    if (sameApp.length === 0) return { status: 'miss', reason: 'wrong-app' }
    const samePlatform = sameApp.filter(item => item.platform === identity.platform)
    if (samePlatform.length === 0) return { status: 'miss', reason: 'wrong-platform' }
    const sameSchema = samePlatform.filter(item => item.flowSchemaVersion === identity.flowSchemaVersion)
    if (sameSchema.length === 0) return { status: 'miss', reason: 'wrong-flow-schema' }
    const sameAdapter = sameSchema.filter(item => (item.adapter ?? null) === (identity.adapter ?? null))
    if (sameAdapter.length === 0) return { status: 'miss', reason: 'wrong-adapter' }
    const sameBrowser = sameAdapter.filter(item => (item.browser ?? null) === (identity.browser ?? null))
    if (sameBrowser.length === 0) return { status: 'miss', reason: 'wrong-browser' }
    const walk = sameBrowser.find(item => item.buildFingerprint === identity.buildFingerprint)
    if (!walk) return { status: 'miss', reason: 'stale-build' }
    if (!walk.lastValidation.passed) return { status: 'miss', reason: 'failed-validation' }
    const flowPath = await this.storedFlowPath(walk)
    if (!flowPath) return { status: 'miss', reason: 'missing-flow' }
    if (sha256(await readFile(flowPath)) !== walk.flowSha256) {
      throw new InvalidWalkMemoryError(`stored flow checksum does not match ${walk.flowPath}`)
    }
    const subflows = await checkStoredSubflows(
      await this.canonicalRoot(), walk.flowPath, walk.subflowFiles, walk.legacyClosure === true,
    )
    if (subflows.status === 'legacy') return { status: 'miss', reason: 'legacy-closure-unknown' }
    if (subflows.status === 'missing') return { status: 'miss', reason: 'missing-flow' }
    if (subflows.status === 'invalid') throw new InvalidWalkMemoryError(subflows.message)
    return { status: 'hit', walk, flowPath }
  }

  /**
   * Select a previous successful flow only for an explicitly requested repair diagnosis. This is
   * never a warm-cache hit: the caller must execute and classify it before making any source change.
   */
  async lookupPreviousSuccessful(identityInput: WalkIdentity): Promise<LookupHit | LookupMiss> {
    const identity = normalizeIdentity(identityInput)
    const memory = await this.loadMemory()
    if (this.consumer) return this.consumer.lookup(memory.walks, identity, true)
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
    const subflows = await checkStoredSubflows(
      await this.canonicalRoot(), walk.flowPath, walk.subflowFiles, walk.legacyClosure === true,
    )
    if (subflows.status === 'legacy') return { status: 'miss', reason: 'legacy-closure-unknown' }
    if (subflows.status === 'missing') return { status: 'miss', reason: 'missing-flow' }
    if (subflows.status === 'invalid') throw new InvalidWalkMemoryError(subflows.message)
    return { status: 'hit', walk, flowPath }
  }

  async recordValidation(identityInput: WalkIdentity, validation: WalkValidation): Promise<StoredWalk> {
    if (!timestamp(validation.at)) throw new TypeError('Validation requires a valid timestamp')
    const identity = normalizeIdentity(identityInput)
    this.acquireWriter()
    // Manifest mode: a flow that changed since it was looked up must not be recorded as validated.
    if (this.consumer) await this.consumer.assertUnchanged((await this.loadMemory()).walks, identity)
    const hit = await this.lookup(identity)
    if (hit.status !== 'hit') {
      throw new InvalidWalkMemoryError(`cannot validate a flow that is not a usable hit: ${hit.reason}`)
    }
    const memory = await this.loadMemory()
    const index = memory.walks.findIndex(item =>
      item.goalKey === identity.goalKey && item.appId === identity.appId &&
      item.platform === identity.platform && item.flowSchemaVersion === identity.flowSchemaVersion &&
      (item.adapter ?? null) === (identity.adapter ?? null) &&
      (item.browser ?? null) === (identity.browser ?? null) &&
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

export {
  LocalVerifiedSubflowStore,
  VERIFIED_SUBFLOW_INDEX_PATH,
  InvalidSubflowMemoryError,
  type LocalVerifiedSubflowStoreOptions,
} from './verified-subflow-store.ts'
export {
  VERIFIED_SUBFLOW_SCHEMA_VERSION,
  validateSubflowContract,
  type CompositionModelMetrics,
  type CompositionStepMetrics,
  type PersistVerifiedSubflowOptions,
  type SubflowContract,
  type SubflowIdentity,
  type SubflowInput,
  type SubflowOutput,
  type VerifiedSubflow,
  type VerifiedSubflowLookup,
  type VerifiedSubflowStore,
} from '@deja-qa/core'

export { FlowClosureError } from './flow-closure.ts'
export {
  InvalidStoreIdError,
  StoreLockContentionError,
  StoreLockUnavailableError,
  StoreLockedError,
  WriterLock,
  storeMetadataPath,
  type LockHolder,
  type LockTestHooks,
} from './writer-lock.ts'
export {
  checkStoredSubflows,
  collectFlowClosure,
  snapshotFlowClosure,
  planStoredClosure,
  readStoredFlowFiles,
} from './flow-closure.ts'

export {
  ConsumerFlowError,
  ConsumerFlows,
  ManifestChangedError,
  ManifestLockedError,
  assertionsSha256,
  checkManifestPath,
  closureAssertions,
  readConsumerWalkFields,
  type ConsumerFlowsOptions,
  type ConsumerPlan,
} from './consumer-flow.ts'
export { MissingFlowFileError } from './flow-closure.ts'
