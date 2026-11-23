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

export type StoredWalk = WalkIdentity & {
  goalKey: string
  flowPath: string
  flowSha256: string
  lastValidation: WalkValidation
  validationHistory: StoredValidation[]
  origin?: 'remember' | 'explore' | 'repair'
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
  'missing-flow' | 'failed-validation'

export type LookupMiss = {
  status: 'miss'
  reason: CacheMissReason
}

export type PersistVerifiedOptions = {
  identity: WalkIdentity
  candidatePath: string
  validation: WalkValidation
  origin: 'remember' | 'explore'
  exploration?: ExplorationDetails
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
}

export interface WalkStore {
  lookup(identity: WalkIdentity): Promise<LookupHit | LookupMiss>
  lookupPreviousSuccessful(identity: WalkIdentity): Promise<LookupHit | LookupMiss>
  persistVerifiedCandidate(options: PersistVerifiedOptions): Promise<PersistedWalk>
  persistVerifiedRepair(options: PersistVerifiedRepairOptions): Promise<PersistedWalk>
  recordValidation(identity: WalkIdentity, validation: WalkValidation): Promise<StoredWalk>
  list(): Promise<StoredWalk[]>
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
  'diff' | 'memory' | 'model-metrics'

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
  'rejected' | 'exploration-rejected' | 'product-regression' | 'repair-rejected' | 'failed'

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
}

export type ReportReceipt = {
  reportPath: string
  latestPath: string
}

export interface RunReporter {
  report(result: RunResult): Promise<ReportReceipt>
}
