import { Database } from 'bun:sqlite'
import { createHash, randomUUID } from 'node:crypto'
import { lstatSync, mkdirSync, realpathSync, statSync } from 'node:fs'
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
  type StoredFlowFile,
  type StoredWalk,
  type WalkIdentity,
  type WalkStore,
  type WalkValidation,
} from '@deja-qa/core'
import {
  checkStoredSubflows,
  collectFlowClosure,
  planStoredClosure,
  readStoredFlowFiles,
  snapshotFlowClosure,
  storeMetadataPath,
  WriterLock,
} from '@deja-qa/store-local'

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

export {
  InvalidStoreIdError,
  StoreLockContentionError,
  StoreLockUnavailableError,
  StoreLockedError,
  storeMetadataPath,
} from '@deja-qa/store-local'

export const SQLITE_MEMORY_SCHEMA_VERSION = 3
export const MAESTRO_FLOW_SCHEMA_VERSION = 'maestro/v1'
export const SQLITE_MEMORY_PATH = '.deja/memory/walks.sqlite'

export type SQLiteWalkStoreOptions = {
  workspaceRoot: string
  /**
   * Keep this store's history in its own folder, for example one per device. Without it the
   * store is the workspace's single store. One writer per store: a second one gets StoreLockedError.
   */
  storeId?: string
  /** Test-only deterministic filesystem failure injection; production uses fs.rename. */
  renameFile?: (temporaryPath: string, destinationPath: string) => Promise<void>
  /** Test-only transaction failure injection; production uses one IMMEDIATE SQLite transaction. */
  replaceMetadata?: (walks: StoredWalk[]) => void
}

export class InvalidWalkMemoryError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(`Invalid walk memory: ${message}`, options)
    this.name = 'InvalidWalkMemoryError'
  }
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

function normalizeIdentity(identity: WalkIdentity) {
  const goal = identity.goal.trim()
  const goalKey = normalizeGoalContract(goal).key
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

function flowSlug(goalKey: string) {
  const slug = goalKey.replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '')
  if (!slug) throw new TypeError('The normalized goal must contain a letter or number')
  return slug
}

function sameLogicalIdentity(left: StoredWalk | ReturnType<typeof normalizeIdentity>, right: WalkIdentity & { goalKey: string }) {
  return left.goalKey === right.goalKey && left.appId === right.appId &&
    left.platform === right.platform && left.flowSchemaVersion === right.flowSchemaVersion &&
    (left.adapter ?? null) === (right.adapter ?? null) &&
    (left.browser ?? null) === (right.browser ?? null)
}

type WalkRow = Record<string, unknown> & { id: number }
type Commit = (walks: StoredWalk[]) => void

export class SQLiteWalkStore implements WalkStore {
  readonly workspaceRoot: string
  readonly memoryPath: string
  /** The database path relative to the workspace. */
  readonly relativeMemoryPath: string
  private writerLock: WriterLock | null = null
  private readonly renameFile: (temporaryPath: string, destinationPath: string) => Promise<void>
  private readonly replaceMetadata: Commit
  private database: Database | null = null

  constructor(options: SQLiteWalkStoreOptions) {
    this.workspaceRoot = resolve(options.workspaceRoot)
    this.relativeMemoryPath = storeMetadataPath(SQLITE_MEMORY_PATH, options.storeId)
    this.memoryPath = resolve(this.workspaceRoot, this.relativeMemoryPath)
    this.renameFile = options.renameFile ?? rename
    this.replaceMetadata = options.replaceMetadata ?? (walks => this.commitMetadata(walks))
  }

  /** Close the database and release the writer lock. */
  close() {
    this.database?.close()
    this.database = null
    this.writerLock?.release()
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

  private openExisting(path: string) {
    if (this.database) return this.database
    try {
      this.database = new Database(path, { readonly: true, readwrite: false, create: false })
      this.database.exec('PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 1000;')
      return this.database
    } catch (error) {
      throw new InvalidWalkMemoryError('walks.sqlite cannot be read', { cause: error })
    }
  }

  private openWritable() {
    const metadata = canonicalMetadataTarget(this.workspaceRoot, this.memoryPath)
    if (this.database) {
      // A readonly handle opened by lookup cannot be upgraded. Reopen only the checked target.
      this.database.close()
      this.database = null
    }
    try {
      mkdirSync(dirname(metadata.path), { recursive: true })
      this.database = new Database(metadata.path, { create: true, readwrite: true })
      const prior = (this.database.query('PRAGMA user_version').get() as { user_version: number }).user_version
      this.database.exec(`
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 1000;
        PRAGMA synchronous = FULL;
        PRAGMA journal_mode = DELETE;
        CREATE TABLE IF NOT EXISTS walks (
          id INTEGER PRIMARY KEY,
          goal TEXT NOT NULL,
          goal_key TEXT NOT NULL,
          app_id TEXT NOT NULL,
          platform TEXT NOT NULL,
          flow_schema_version TEXT NOT NULL,
          build_fingerprint TEXT NOT NULL,
          adapter TEXT,
          browser TEXT,
          flow_path TEXT NOT NULL,
          flow_sha256 TEXT NOT NULL,
          last_passed INTEGER NOT NULL CHECK (last_passed IN (0, 1)),
          last_at TEXT NOT NULL,
          origin TEXT,
          exploration_json TEXT,
          has_files INTEGER NOT NULL DEFAULT 0,
          legacy_closure INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS validation_history (
          walk_id INTEGER NOT NULL REFERENCES walks(id) ON DELETE CASCADE,
          position INTEGER NOT NULL,
          passed INTEGER NOT NULL CHECK (passed IN (0, 1)),
          at TEXT NOT NULL,
          flow_sha256 TEXT NOT NULL,
          PRIMARY KEY (walk_id, position)
        );
        CREATE TABLE IF NOT EXISTS repair_history (
          walk_id INTEGER NOT NULL REFERENCES walks(id) ON DELETE CASCADE,
          position INTEGER NOT NULL,
          attempt_id TEXT NOT NULL,
          patch_path TEXT NOT NULL,
          previous_build_fingerprint TEXT NOT NULL,
          previous_flow_sha256 TEXT NOT NULL,
          PRIMARY KEY (walk_id, position)
        );
        CREATE TABLE IF NOT EXISTS walk_files (
          walk_id INTEGER NOT NULL REFERENCES walks(id) ON DELETE CASCADE,
          position INTEGER NOT NULL,
          path TEXT NOT NULL,
          sha256 TEXT NOT NULL,
          PRIMARY KEY (walk_id, position)
        );
      `)
      const columns = this.database.query('PRAGMA table_info(walks)').all() as { name: string }[]
      if (!columns.some(column => column.name === 'has_files')) {
        // A schema 2 database: its walks stay unchecked until the next write rewrites them as schema 3.
        this.database.exec('ALTER TABLE walks ADD COLUMN has_files INTEGER NOT NULL DEFAULT 0')
      }
      if (!columns.some(column => column.name === 'legacy_closure')) {
        this.database.exec('ALTER TABLE walks ADD COLUMN legacy_closure INTEGER NOT NULL DEFAULT 0')
      }
      // A new database is schema 3 at once. An older one is bumped in the transaction that rewrites it.
      if (prior === 0) this.database.exec(`PRAGMA user_version = ${SQLITE_MEMORY_SCHEMA_VERSION}`)
      return this.database
    } catch (error) {
      throw new InvalidWalkMemoryError('walks.sqlite cannot be initialized for a transaction', { cause: error })
    }
  }

  private loadMemory(): StoredWalk[] {
    const metadata = canonicalMetadataTarget(this.workspaceRoot, this.memoryPath)
    if (!metadata.exists) return []
    let database: Database
    try {
      database = this.openExisting(metadata.path)
      const schema = database.query('PRAGMA user_version').get() as { user_version: number }
      if (schema.user_version !== 2 && schema.user_version !== SQLITE_MEMORY_SCHEMA_VERSION) {
        throw new InvalidWalkMemoryError(`unsupported memory schemaVersion: ${schema.user_version}`)
      }
      const rows = database.query('SELECT * FROM walks ORDER BY id').all() as WalkRow[]
      return rows.map(row => this.readWalk(database, row, schema.user_version))
    } catch (error) {
      if (error instanceof InvalidWalkMemoryError) throw error
      throw new InvalidWalkMemoryError('walks.sqlite metadata is malformed or unreadable', { cause: error })
    }
  }

  private readWalk(database: Database, row: WalkRow, schemaVersion: number): StoredWalk {
    if (!nonEmptyString(row.goal) || row.goal_key !== normalizeGoalContract(row.goal).key ||
        !nonEmptyString(row.app_id) || !nonEmptyString(row.platform) ||
        !nonEmptyString(row.flow_schema_version) || !fingerprint(row.build_fingerprint) ||
        !nonEmptyString(row.flow_path) || !fingerprint(row.flow_sha256)) {
      throw new InvalidWalkMemoryError('walk entry has missing or malformed identity fields')
    }
    if (row.flow_schema_version !== MAESTRO_FLOW_SCHEMA_VERSION) {
      throw new InvalidWalkMemoryError(`unsupported flow schema in walk entry: ${String(row.flow_schema_version)}`)
    }
    if ((row.adapter !== null && !nonEmptyString(row.adapter)) ||
        (row.browser !== null && !nonEmptyString(row.browser)) ||
        (row.platform === 'web' && (!nonEmptyString(row.adapter) || !nonEmptyString(row.browser)))) {
      throw new InvalidWalkMemoryError('walk adapter/browser identity is missing or malformed')
    }
    if ((row.last_passed !== 0 && row.last_passed !== 1) || !timestamp(row.last_at)) {
      throw new InvalidWalkMemoryError('walk validation requires passed: boolean and an ISO timestamp')
    }
    const lastValidation = { passed: row.last_passed === 1, at: row.last_at }
    const historyRows = database.query(
      'SELECT passed, at, flow_sha256 FROM validation_history WHERE walk_id = ? ORDER BY position',
    ).all(row.id) as Record<string, unknown>[]
    if (historyRows.length === 0) {
      throw new InvalidWalkMemoryError('walk validation history must not be empty')
    }
    const validationHistory = historyRows.map(history => {
      if ((history.passed !== 0 && history.passed !== 1) || !timestamp(history.at) ||
          !fingerprint(history.flow_sha256)) {
        throw new InvalidWalkMemoryError('walk validation history entry is invalid')
      }
      return { passed: history.passed === 1, at: history.at, flowSha256: history.flow_sha256 }
    })
    const latest = validationHistory.at(-1)!
    if (latest.passed !== lastValidation.passed || latest.at !== lastValidation.at ||
        latest.flowSha256 !== row.flow_sha256) {
      throw new InvalidWalkMemoryError('lastValidation must equal the latest history entry for the stored flow')
    }
    if (row.origin !== null && row.origin !== 'remember' && row.origin !== 'explore' &&
        row.origin !== 'repair' && row.origin !== 'replayed-on-new-build') {
      throw new InvalidWalkMemoryError('walk origin must be remember, explore, repair, or replayed-on-new-build')
    }
    let exploration: ExplorationDetails | undefined
    if (row.exploration_json !== null) {
      if (!nonEmptyString(row.exploration_json)) {
        throw new InvalidWalkMemoryError('walk exploration metadata is invalid')
      }
      try {
        exploration = readExploration(JSON.parse(row.exploration_json))
      } catch (error) {
        if (error instanceof InvalidWalkMemoryError) throw error
        throw new InvalidWalkMemoryError('walk exploration metadata is malformed JSON', { cause: error })
      }
    }
    const repairRows = database.query(
      `SELECT attempt_id, patch_path, previous_build_fingerprint, previous_flow_sha256
       FROM repair_history WHERE walk_id = ? ORDER BY position`,
    ).all(row.id) as Record<string, unknown>[]
    const repairHistory = repairRows.length > 0 ? repairRows.map(repair => readRepairProvenance({
      attemptId: repair.attempt_id,
      patchPath: repair.patch_path,
      previousBuildFingerprint: repair.previous_build_fingerprint,
      previousFlowSha256: repair.previous_flow_sha256,
    })) : undefined
    // Schema 3 records the files a flow runs, even when there are none. A walk that lost the flag
    // would replay its subflows unchecked, so it is refused. Schema 2 stored the main flow only.
    let subflowFiles: StoredFlowFile[] | undefined
    if (schemaVersion >= 3) {
      if (row.has_files !== 1) {
        throw new InvalidWalkMemoryError('walk is missing subflowFiles, which memory schema 3 requires')
      }
      const read = readStoredFlowFiles(database.query(
        'SELECT path, sha256 FROM walk_files WHERE walk_id = ? ORDER BY position',
      ).all(row.id))
      if (read === null) throw new InvalidWalkMemoryError('walk subflowFiles is invalid')
      subflowFiles = read
    }
    if (row.legacy_closure !== undefined && row.legacy_closure !== 0 && row.legacy_closure !== 1) {
      throw new InvalidWalkMemoryError('walk legacyClosure is invalid')
    }
    const walk: StoredWalk = {
      goal: row.goal,
      goalKey: row.goal_key,
      appId: row.app_id,
      platform: row.platform,
      flowSchemaVersion: row.flow_schema_version,
      buildFingerprint: row.build_fingerprint,
      ...(row.adapter !== null ? { adapter: row.adapter as string } : {}),
      ...(row.browser !== null ? { browser: row.browser as string } : {}),
      flowPath: row.flow_path,
      flowSha256: row.flow_sha256,
      ...(subflowFiles ? { subflowFiles } : {}),
      ...(row.legacy_closure === 1 ? { legacyClosure: true as const } : {}),
      lastValidation,
      validationHistory,
      ...(row.origin !== null ? { origin: row.origin as StoredWalk['origin'] } : {}),
      ...(exploration ? { exploration } : {}),
      ...(repairHistory ? { repairHistory } : {}),
    }
    return walk
  }

  private validateUniqueIdentities(walks: StoredWalk[]) {
    const identities = new Set<string>()
    for (const walk of walks) {
      const identity = JSON.stringify([
        walk.goalKey, walk.appId, walk.platform, walk.flowSchemaVersion, walk.buildFingerprint,
        walk.adapter ?? null, walk.browser ?? null,
      ])
      if (identities.has(identity)) {
        throw new InvalidWalkMemoryError('duplicate exact walk identities are ambiguous')
      }
      identities.add(identity)
    }
  }

  private commitMetadata(walks: StoredWalk[]) {
    this.validateUniqueIdentities(walks)
    const database = this.openWritable()
    database.exec('BEGIN IMMEDIATE')
    try {
      database.query('DELETE FROM walks').run()
      for (const walk of walks) this.insertWalk(database, walk)
      database.exec(`PRAGMA user_version = ${SQLITE_MEMORY_SCHEMA_VERSION}`)
      database.exec('COMMIT')
    } catch (error) {
      try {
        database.exec('ROLLBACK')
      } catch {
        // Preserve the original transaction error; SQLite also rolls back on connection close.
      }
      throw new InvalidWalkMemoryError('SQLite metadata replacement failed; transaction rolled back', { cause: error })
    }
  }

  private insertWalk(database: Database, walk: StoredWalk) {
    const result = database.query(`
      INSERT INTO walks (
        goal, goal_key, app_id, platform, flow_schema_version, build_fingerprint,
        adapter, browser, flow_path, flow_sha256, last_passed, last_at, origin, exploration_json, has_files, legacy_closure
      ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?) RETURNING id
    `).get(
      walk.goal, walk.goalKey, walk.appId, walk.platform, walk.flowSchemaVersion,
      walk.buildFingerprint, walk.adapter ?? null, walk.browser ?? null, walk.flowPath,
      walk.flowSha256, walk.lastValidation.passed ? 1 : 0, walk.lastValidation.at,
      walk.origin ?? null, walk.exploration ? JSON.stringify(walk.exploration) : null,
      walk.legacyClosure || !walk.subflowFiles ? 1 : 0,
    ) as { id: number }
    for (const [position, validation] of walk.validationHistory.entries()) {
      database.query(
        'INSERT INTO validation_history (walk_id, position, passed, at, flow_sha256) VALUES (?, ?, ?, ?, ?)',
      ).run(result.id, position, validation.passed ? 1 : 0, validation.at, validation.flowSha256)
    }
    for (const [position, file] of (walk.subflowFiles ?? []).entries()) {
      database.query(
        'INSERT INTO walk_files (walk_id, position, path, sha256) VALUES (?, ?, ?, ?)',
      ).run(result.id, position, file.path, file.sha256)
    }
    for (const [position, repair] of (walk.repairHistory ?? []).entries()) {
      database.query(
        `INSERT INTO repair_history (
          walk_id, position, attempt_id, patch_path, previous_build_fingerprint, previous_flow_sha256
        ) VALUES (?, ?, ?, ?, ?, ?)`,
      ).run(
        result.id, position, repair.attemptId, repair.patchPath,
        repair.previousBuildFingerprint, repair.previousFlowSha256,
      )
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

  private async atomicWriteFlow(path: string, contents: Buffer) {
    const root = await this.canonicalRoot()
    await mkdir(dirname(path), { recursive: true })
    const canonicalParent = await realpath(dirname(path))
    if (!inside(root, canonicalParent)) {
      throw new InvalidWalkMemoryError(`atomic write destination escapes workspace: ${path}`)
    }
    const temporary = `${path}.${process.pid}-${randomUUID()}.tmp`
    await writeFile(temporary, contents)
    await this.renameFile(temporary, path)
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
    const root = await this.canonicalRoot()
    const candidate = await this.existingFile(root, options.candidatePath, true)
    if (!candidate) throw new Error(`Verified candidate flow does not exist: ${options.candidatePath}`)
    const memory = this.loadMemory()
    this.validateUniqueIdentities(memory)
    const previous = repair ? memory.find(item =>
      sameLogicalIdentity(item, identity) && item.buildFingerprint === repair.previous.buildFingerprint &&
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
      flowSlug(normalizeGoalContract(identity.platform).key),
    )
    const { flowPath, flowSha256 } = plan
    for (const write of plan.writes) await this.atomicWriteFlow(resolve(root, write.path), write.bytes)

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
    // Match LocalWalkStore: one successful replacement family per goal/app/platform/adapter/browser.
    const remaining = memory.filter(item => !(
      item.goalKey === identity.goalKey && item.appId === identity.appId &&
      item.platform === identity.platform &&
      (item.adapter ?? null) === (identity.adapter ?? null) &&
      (item.browser ?? null) === (identity.browser ?? null)
    ))
    this.replaceMetadata([...remaining, walk])
    return {
      walk,
      flowPath,
      flowSha256,
      storedMetadataPath: this.relativeMemoryPath,
      atomicWrite: true,
    }
  }

  async snapshotCandidate(candidatePath: string, directory: string): Promise<CandidateSnapshot> {
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
    const walks = this.loadMemory()
    this.validateUniqueIdentities(walks)
    return [...walks]
  }

  async lookup(identityInput: WalkIdentity): Promise<LookupHit | LookupMiss> {
    const identity = normalizeIdentity(identityInput)
    const memory = this.loadMemory()
    this.validateUniqueIdentities(memory)
    const sameGoal = memory.filter(item => item.goalKey === identity.goalKey)
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

  async lookupPreviousSuccessful(identityInput: WalkIdentity): Promise<LookupHit | LookupMiss> {
    const identity = normalizeIdentity(identityInput)
    const memory = this.loadMemory()
    this.validateUniqueIdentities(memory)
    const candidates = memory.filter(item =>
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
    const hit = await this.lookup(identity)
    if (hit.status !== 'hit') {
      throw new InvalidWalkMemoryError(`cannot validate a flow that is not a usable hit: ${hit.reason}`)
    }
    const memory = this.loadMemory()
    const index = memory.findIndex(item =>
      sameLogicalIdentity(item, identity) && item.buildFingerprint === identity.buildFingerprint &&
      item.flowSha256 === hit.walk.flowSha256,
    )
    if (index < 0) throw new InvalidWalkMemoryError('stored walk changed while validation was running')
    const previous = memory[index]
    const entry = { ...validation, flowSha256: previous.flowSha256 }
    const updated: StoredWalk = {
      ...previous,
      lastValidation: { passed: validation.passed, at: validation.at },
      validationHistory: [...previous.validationHistory, entry],
    }
    const walks = [...memory]
    walks[index] = updated
    this.replaceMetadata(walks)
    return updated
  }
}
