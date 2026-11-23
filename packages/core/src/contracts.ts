export const WALK_RESULT_SCHEMA_VERSION = 1

export type Goal = {
  text: string
  key: string
}

export function normalizeGoal(goal: string): Goal {
  const text = goal.trim()
  const key = text.toLowerCase().replace(/\s+/g, ' ')
  if (!key) throw new TypeError('A non-empty goal is required')
  return { text, key }
}

export type WalkIdentity = {
  goal: string
  appId: string
  platform: string
  flowSchemaVersion: string
  buildFingerprint: string
  /** Optional for legacy cached walks; required by the Web adapter. Part of exact cache identity. */
  adapter?: string
  browser?: string
}

export type WalkValidation = {
  passed: boolean
  at: string
}

export type StoredValidation = WalkValidation & {
  flowSha256: string
}

export type RepairProvenance = {
  attemptId: string
  patchPath: string
  previousBuildFingerprint: string
  previousFlowSha256: string
}

export type ExplorationDetails = {
  explorationDurationMs: number | null
  verificationDurationMs: number | null
  mcpCalls: number | null
  modelCalls: number | null
  modelCallBasis: string
}

/** A file the stored flow runs (a subflow or script), kept under `.deja/flows` with its checksum. */
export type StoredFlowFile = {
  path: string
  sha256: string
}

export type StoredWalk = WalkIdentity & {
  goalKey: string
  flowPath: string
  flowSha256: string
  /**
   * Files the flow runs (empty when it runs none). Always present from memory schema 3; absent only
   * on walks read from schema 1 or 2, which stored the main flow alone.
   */
  subflowFiles?: StoredFlowFile[]
  /**
   * Set when a schema 1 or 2 walk was rewritten as schema 3. Its subflows were never stored, so a
   * flow that runs files cannot replay and lookup returns the `legacy-closure-unknown` miss.
   */
  legacyClosure?: true
  /**
   * Set when the walk replays the consumer's own files through a committed manifest (memory
   * schema 4). It is the sha256 of the main flow plus every file it runs, as the manifest records
   * it, and `flowPath` is then a workspace path of the consumer's file, never a copy.
   */
  closureSha256?: string
  lastValidation: WalkValidation
  validationHistory: StoredValidation[]
  /** `replayed-on-new-build` (WALK-021): an earlier build's verified YAML, replayed once on this build. */
  origin?: 'remember' | 'explore' | 'repair' | 'replayed-on-new-build' | 'verify'
  exploration?: ExplorationDetails
  repairHistory?: RepairProvenance[]
}

export type LookupHit = {
  status: 'hit'
  walk: StoredWalk
  flowPath: string
}

export type CacheMissReason = 'not-found' | 'wrong-app' | 'wrong-platform' |
  'wrong-flow-schema' | 'wrong-adapter' | 'wrong-browser' | 'stale-build' |
  'missing-flow' | 'failed-validation' | 'legacy-closure-unknown' |
  /** Manifest mode: the consumer's flow or a file it runs no longer matches the manifest's closure sha. */
  'flow-changed' |
  /** Manifest mode: the manifest lists the flow, but no build has validated it in this store. */
  'not-validated' |
  /** Manifest mode: the goal has only a memory schema 3 walk and no manifest entry. */
  'needs-migration'

export type LookupMiss = {
  status: 'miss'
  reason: CacheMissReason
  /** Set with `flow-changed`: the sha the manifest records and the sha of the files on disk now. */
  flowChange?: { manifestClosureSha256: string; currentClosureSha256: string }
  /**
   * Manifest mode, when the consumer's file exists (`flow-changed`, `not-validated`,
   * `failed-validation`, `stale-build`): its path, so a caller can re-run it as a check (`verify`).
   * It is never a cache hit.
   */
  flowPath?: string
}

export type PersistVerifiedOptions = {
  identity: WalkIdentity
  candidatePath: string
  validation: WalkValidation
  origin: 'remember' | 'explore' | 'replayed-on-new-build' | 'verify'
  exploration?: ExplorationDetails
  /**
   * Manifest mode only. What to do when the flow's assertion hash differs from the manifest's:
   * `refuse` (the default) throws `AssertionHashChangedError` before writing anything, `accept`
   * writes the new hash. Only a reviewed `remember` may pass `accept`; `verify` never does.
   */
  assertionChange?: 'refuse' | 'accept'
  /**
   * Checksum of the closure a store's `snapshotCandidate` returned. When set, persistence refuses
   * if the candidate and every file it runs no longer hash to it.
   */
  expectedClosureSha256?: string
  /**
   * When set, the store hashes the main flow it is about to write and refuses, before writing
   * anything, if it differs. Callers that replayed a candidate pass the sha256 of what actually ran.
   */
  expectedFlowSha256?: string
}

/** Thrown before any write when the candidate bytes differ from `expectedFlowSha256`. */
export class ExpectedFlowChecksumMismatchError extends Error {
  constructor(expected: string, actual: string) {
    super(`Verified candidate bytes (${actual}) differ from the expected checksum (${expected}); nothing was stored`)
    this.name = 'ExpectedFlowChecksumMismatchError'
  }
}

/** Thrown before any write when the flow or a file it runs differs from `expectedClosureSha256`. */
export class ExpectedClosureMismatchError extends Error {
  constructor(expected: string, actual: string) {
    super(`The candidate or a file it runs changed after it was verified (${actual}, expected ${expected}); nothing was stored`)
    this.name = 'ExpectedClosureMismatchError'
  }
}

/** Thrown in manifest mode when the flow's files no longer match the manifest's closure sha. */
export class FlowChangedError extends Error {
  constructor(manifestClosureSha256: string, currentClosureSha256: string) {
    super(`The flow or a file it runs changed (manifest ${manifestClosureSha256}, now ${currentClosureSha256}); re-verify it`)
    this.name = 'FlowChangedError'
  }
}

/** Thrown before any write when the flow's product assertions differ from the manifest's. */
export class AssertionHashChangedError extends Error {
  readonly manifestAssertionsSha256: string
  readonly currentAssertionsSha256: string

  constructor(manifestAssertionsSha256: string, currentAssertionsSha256: string) {
    super(`The flow's assertions changed (manifest ${manifestAssertionsSha256}, now ${currentAssertionsSha256}); only a reviewed remember can accept that`)
    this.manifestAssertionsSha256 = manifestAssertionsSha256
    this.currentAssertionsSha256 = currentAssertionsSha256
    this.name = 'AssertionHashChangedError'
  }
}

/** Thrown before any write when a manifest owner rule is not met (for example a required assertion id). */
export class OwnerRuleError extends Error {
  constructor(message: string) {
    super(`Owner rule not met: ${message}`)
    this.name = 'OwnerRuleError'
  }
}

export type PersistVerifiedRepairOptions = Omit<PersistVerifiedOptions, 'origin' | 'exploration'> & {
  previous: {
    buildFingerprint: string
    flowSha256: string
  }
  repair: RepairProvenance
}

export type PersistedWalk = {
  walk: StoredWalk
  flowPath: string
  flowSha256: string
  storedMetadataPath: string
  atomicWrite: true
  /**
   * Manifest mode: the manifest file Déjà wrote or checked, and the new flow files it wrote into the
   * consumer's tree (explore output). Déjà never commits any of them.
   */
  manifest?: { path: string; changed: boolean; filesWritten?: string[] }
}

export type CandidateSnapshot = {
  /** Workspace-relative path of the copied main flow. */
  candidatePath: string
  closureSha256: string
}

export type MigrateOptions = {
  /** Report what would be written, and write nothing. */
  dryRun?: boolean
  /** The adapter id to give a walk from before adapters were recorded. */
  adapter?: string
}

export type MigratedWalk = {
  goal: string
  platform: string
  /** The consumer file now named by the manifest, relative to the workspace root. */
  flow: string
  closureSha256: string
  assertionsSha256: string
  /** New files in the consumer's tree. */
  filesWritten: string[]
  /** Files that were already there with the same bytes. */
  filesUnchanged: string[]
  /** Validation entries kept from the old walk. */
  historyEntries: number
  /** Other old walks for this goal (older builds or flows) left only in the backup. */
  olderWalksDropped: number
}

export type SkippedWalk = {
  goal: string
  reason: 'not-passing' | 'subflows-not-stored' | 'copy-missing' | 'copy-changed' | 'adapter-unknown' |
    'would-overwrite' | 'manifest-differs' | 'closure-error' | 'holds-fixture-value' | 'manifest-invalid'
  detail: string
}

export type MigrationReport = {
  dryRun: boolean
  manifestPath: string
  manifestChanged: boolean
  migrated: MigratedWalk[]
  /** Left as memory schema 3 walks. They give `needs-migration` until fixed or re-remembered. */
  skipped: SkippedWalk[]
  /** The schema 3 store file, kept beside the new one. Null on a dry run or when nothing migrated. */
  backupPath: string | null
}

export interface WalkStore {
  /**
   * Set when the store replays the consumer's own files through a committed manifest. It is the
   * manifest's workspace-relative path. Stores without it replay their own copies.
   */
  readonly manifestPath?: string
  /**
   * Manifest mode: the folder (relative to the workspace root) where a verified explore result is
   * written as a new file of the consumer's. Without it, explore output has nowhere to go.
   */
  readonly manifestFlowsDir?: string
  lookup(identity: WalkIdentity): Promise<LookupHit | LookupMiss>
  lookupPreviousSuccessful(identity: WalkIdentity): Promise<LookupHit | LookupMiss>
  /**
   * Copy a candidate and every file it runs into `directory`, so the bytes that get verified are
   * the bytes that get stored. Returns the copy's main flow and its closure checksum, to pass back
   * as `expectedClosureSha256`. Stores without it are verified in place. In manifest mode nothing
   * is copied: it returns the consumer's own path and the closure checksum, the flow runs in place,
   * and the store re-hashes before it writes. An edit that is undone before the save is not seen.
   */
  snapshotCandidate?(candidatePath: string, directory: string): Promise<CandidateSnapshot>
  persistVerifiedCandidate(options: PersistVerifiedOptions): Promise<PersistedWalk>
  persistVerifiedRepair(options: PersistVerifiedRepairOptions): Promise<PersistedWalk>
  recordValidation(identity: WalkIdentity, validation: WalkValidation): Promise<StoredWalk>
  list(): Promise<StoredWalk[]>
  /**
   * Manifest mode: turn the store's schema 3 walks into the consumer's own files plus manifest
   * entries. Each copy under `.deja/flows` is written once into the flows folder; nothing is
   * overwritten and nothing is committed.
   */
  migrateToManifest?(options?: MigrateOptions): Promise<MigrationReport>
}

/** A bounded provider call for one goal. Implementations are not constructed on an exact hit. */
export interface GoalEmbeddingProvider {
  readonly id: string
  embed(goal: string): Promise<readonly number[]>
  /** Optional lexical-equivalence guard; the local provider uses it to reject hash collisions. */
  isCollisionSafeMatch?(requestedGoal: string, candidateGoal: string): boolean
}

export type SemanticRetrievalOptions = {
  /** Lazy so even provider/client construction is absent from exact hits. Null means unavailable. */
  createProvider: () => GoalEmbeddingProvider | null
  /** Cosine-similarity floor, inclusive. May raise, never lower, the conservative 0.90 default. */
  minimumSimilarity?: number
  /** Required score distance from the runner-up. May raise, never lower, the 0.15 default. */
  minimumMargin?: number
}

export type SemanticLookupStatus = 'hit' | 'miss' | 'unavailable' |
  'disabled' | 'skipped-exact-hit' | 'not-applicable'

export type SemanticLookupReason = 'provider-unavailable' | 'adversarial-goal' |
  'contradictory-goal' | 'no-eligible-candidates' | 'low-confidence' | 'ambiguous'

export type SemanticSelectedWalk = {
  goal: string
  goalKey: string
  flowPath: string
  flowSha256: string
}

/** Optional additive run-report data; schema 1 reports without it remain readable. */
export type SemanticLookupReport = {
  status: SemanticLookupStatus
  reason?: SemanticLookupReason
  provider: string | null
  embeddingCalls: number
  durationMs: number
  candidates: number
  eligibleCandidates: number
  minimumSimilarity: number | null
  minimumMargin: number | null
  goalSimilarity: number | null
  runnerUpSimilarity: number | null
  selectedWalk: SemanticSelectedWalk | null
}

export type FailureCategory = 'selector' | 'navigation' | 'product_assertion' |
  'runner' | 'timeout' | 'unknown'

export type WalkFailure = {
  category: FailureCategory
  advisory: true
  repairEligible: boolean
  failingStep: string | null
  rawArtifactPath: string | null
}

export type ArtifactKind = 'yaml' | 'output' | 'attempt' | 'repair-report' |
  'diff' | 'memory' | 'model-metrics' | 'junit' | 'manifest'

export type RunArtifact = {
  kind: ArtifactKind
  path: string
  sha256?: string
}

export type SourceDiff = {
  path: string | null
  affectedFiles: string[]
  contents: string
  autoCommit: false
}

export type RunnerRequest = {
  flowPath: string
  artifactDirectory: string
  timeoutMs?: number
  /** Flow variables passed to the runner (`maestro -e KEY=VALUE`). Result schema stays 1. */
  env?: Record<string, string>
}

export type ExecutionResult = {
  passed: boolean
  startedAt: string
  endedAt: string
  durationMs: number
  exitCode: number | null
  assertions: number | null
  failure: WalkFailure | null
  artifacts: RunArtifact[]
}

export interface WalkRunner {
  run(request: RunnerRequest): Promise<ExecutionResult>
}

export type ExploreRequest = {
  identity: WalkIdentity
}

export type ExplorationResult = {
  passed: boolean
  durationMs: number
  explorationDurationMs: number | null
  verificationDurationMs: number | null
  mcpCalls: number | null
  modelCalls: number | null
  modelCallBasis: string
  model: string | null
  provider: string | null
  assertions: number | null
  flowPath: string | null
  flowSha256: string | null
  failure: WalkFailure | null
  artifacts: RunArtifact[]
  errors: string[]
}

export interface WalkExplorer {
  explore(request: ExploreRequest): Promise<ExplorationResult>
}

export type RepairRequest = {
  identity: WalkIdentity
}

export type RepairResult = {
  status: 'repaired' | 'rejected' | 'failed'
  passed: boolean
  durationMs: number
  modelCalls: 0
  repairCount: 0 | 1
  assertions: number | null
  flowPath: string | null
  flowSha256: string | null
  failure: WalkFailure | null
  artifacts: RunArtifact[]
  diffs: SourceDiff[]
  errors: string[]
  details?: Record<string, unknown>
}

export interface SelectorRepairer {
  repair(request: RepairRequest): Promise<RepairResult>
}

export type CacheStatus = 'hit' | 'miss' | 'not-checked'
export type RunOutcome = 'passed' | 'remembered' | 'cold-recorded' | 'repaired' |
  'rejected' | 'exploration-rejected' | 'product-regression' | 'repair-rejected' | 'failed' |
  /** Manifest mode: the consumer's flow no longer matches the manifest; nothing was replayed. */
  'flow-changed' |
  /** Manifest mode: the manifest lists the flow but no build has validated it; run with verify. */
  'not-validated' |
  /** Manifest mode: the goal has only a schema 3 walk; run `deja migrate`. */
  'needs-migration' |
  /** Manifest mode: verify passed but the assertion hash differs; only a reviewed remember accepts it. */
  'assertions-changed'

/**
 * Opt-in WALK-021 policy for a stale-build miss with no repairer: replay the latest verified walk's
 * stored YAML against the new build. A pass stores that same verified YAML for the new build.
 */
export type StaleBuildPolicy = 'replay-last-verified'

/** Optional additive run-report data; schema 1 reports without it remain readable. */
export type NewBuildReplayReport = {
  policy: StaleBuildPolicy
  /** The build the replayed YAML was last verified on; never the build this run executed. */
  previousBuildFingerprint: string
  previousFlowSha256: string
  previousValidatedAt: string
  /** True only when the pass was stored as a verified walk for the new build fingerprint. */
  recordedForNewBuild: boolean
}

/** Optional additive run-report data: which Maestro executed the flows. Older reports omit it. */
export type EngineReport = {
  /** Strict x.y.z, or null when an override's version could not be read. */
  version: string | null
  /** "pinned" is Déjà's own managed install; "override" is DEJA_MAESTRO or maestro.executable. */
  source: 'pinned' | 'override'
  overrideVia?: 'config' | 'env'
  /** False when the version is outside the tested range or unknown (only with an explicit allow). */
  inRange: boolean
}

export type RunResult = {
  schemaVersion: typeof WALK_RESULT_SCHEMA_VERSION
  command: 'explore' | 'run' | 'remember'
  identity: WalkIdentity
  goalKey: string
  cacheStatus: CacheStatus
  cacheMissReason?: CacheMissReason
  semantic?: SemanticLookupReport
  outcome: RunOutcome
  passed: boolean
  startedAt: string
  endedAt: string
  durationMs: number
  modelCalls: number | null
  modelCallBasis: string
  assertions: number | null
  repairCount: 0 | 1
  yamlPath: string | null
  yamlSha256: string | null
  failure: WalkFailure | null
  artifacts: RunArtifact[]
  diffs: SourceDiff[]
  errors: string[]
  execution?: ExecutionResult
  exploration?: ExplorationResult
  repair?: RepairResult
  replayedOnNewBuild?: NewBuildReplayReport
  engine?: EngineReport
}

export type ReportReceipt = {
  reportPath: string
  latestPath: string
}

export interface RunReporter {
  report(result: RunResult): Promise<ReportReceipt>
}
