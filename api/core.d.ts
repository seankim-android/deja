// contracts.d.ts
export declare const WALK_RESULT_SCHEMA_VERSION = 1;
export type Goal = {
    text: string;
    key: string;
};
export declare function normalizeGoal(goal: string): Goal;
export type WalkIdentity = {
    goal: string;
    appId: string;
    platform: string;
    flowSchemaVersion: string;
    buildFingerprint: string;
    /** Optional for legacy cached walks; required by the Web adapter. Part of exact cache identity. */
    adapter?: string;
    browser?: string;
};
export type WalkValidation = {
    passed: boolean;
    at: string;
};
export type StoredValidation = WalkValidation & {
    flowSha256: string;
};
export type RepairProvenance = {
    attemptId: string;
    patchPath: string;
    previousBuildFingerprint: string;
    previousFlowSha256: string;
};
export type ExplorationDetails = {
    explorationDurationMs: number | null;
    verificationDurationMs: number | null;
    mcpCalls: number | null;
    modelCalls: number | null;
    modelCallBasis: string;
};
export type StoredWalk = WalkIdentity & {
    goalKey: string;
    flowPath: string;
    flowSha256: string;
    lastValidation: WalkValidation;
    validationHistory: StoredValidation[];
    origin?: 'remember' | 'explore' | 'repair';
    exploration?: ExplorationDetails;
    repairHistory?: RepairProvenance[];
};
export type LookupHit = {
    status: 'hit';
    walk: StoredWalk;
    flowPath: string;
};
export type CacheMissReason = 'not-found' | 'wrong-app' | 'wrong-platform' | 'wrong-flow-schema' | 'wrong-adapter' | 'wrong-browser' | 'stale-build' | 'missing-flow' | 'failed-validation';
export type LookupMiss = {
    status: 'miss';
    reason: CacheMissReason;
};
export type PersistVerifiedOptions = {
    identity: WalkIdentity;
    candidatePath: string;
    validation: WalkValidation;
    origin: 'remember' | 'explore';
    exploration?: ExplorationDetails;
};
export type PersistVerifiedRepairOptions = Omit<PersistVerifiedOptions, 'origin' | 'exploration'> & {
    previous: {
        buildFingerprint: string;
        flowSha256: string;
    };
    repair: RepairProvenance;
};
export type PersistedWalk = {
    walk: StoredWalk;
    flowPath: string;
    flowSha256: string;
    storedMetadataPath: string;
    atomicWrite: true;
};
export interface WalkStore {
    lookup(identity: WalkIdentity): Promise<LookupHit | LookupMiss>;
    lookupPreviousSuccessful(identity: WalkIdentity): Promise<LookupHit | LookupMiss>;
    persistVerifiedCandidate(options: PersistVerifiedOptions): Promise<PersistedWalk>;
    persistVerifiedRepair(options: PersistVerifiedRepairOptions): Promise<PersistedWalk>;
    recordValidation(identity: WalkIdentity, validation: WalkValidation): Promise<StoredWalk>;
    list(): Promise<StoredWalk[]>;
}
/** A bounded provider call for one goal. Implementations are not constructed on an exact hit. */
export interface GoalEmbeddingProvider {
    readonly id: string;
    embed(goal: string): Promise<readonly number[]>;
    /** Optional lexical-equivalence guard; the local provider uses it to reject hash collisions. */
    isCollisionSafeMatch?(requestedGoal: string, candidateGoal: string): boolean;
}
export type SemanticRetrievalOptions = {
    /** Lazy so even provider/client construction is absent from exact hits. Null means unavailable. */
    createProvider: () => GoalEmbeddingProvider | null;
    /** Cosine-similarity floor, inclusive. May raise, never lower, the conservative 0.90 default. */
    minimumSimilarity?: number;
    /** Required score distance from the runner-up. May raise, never lower, the 0.15 default. */
    minimumMargin?: number;
};
export type SemanticLookupStatus = 'hit' | 'miss' | 'unavailable' | 'disabled' | 'skipped-exact-hit' | 'not-applicable';
export type SemanticLookupReason = 'provider-unavailable' | 'adversarial-goal' | 'contradictory-goal' | 'no-eligible-candidates' | 'low-confidence' | 'ambiguous';
export type SemanticSelectedWalk = {
    goal: string;
    goalKey: string;
    flowPath: string;
    flowSha256: string;
};
/** Optional additive run-report data; schema 1 reports without it remain readable. */
export type SemanticLookupReport = {
    status: SemanticLookupStatus;
    reason?: SemanticLookupReason;
    provider: string | null;
    embeddingCalls: number;
    durationMs: number;
    candidates: number;
    eligibleCandidates: number;
    minimumSimilarity: number | null;
    minimumMargin: number | null;
    goalSimilarity: number | null;
    runnerUpSimilarity: number | null;
    selectedWalk: SemanticSelectedWalk | null;
};
export type FailureCategory = 'selector' | 'navigation' | 'product_assertion' | 'runner' | 'timeout' | 'unknown';
export type WalkFailure = {
    category: FailureCategory;
    advisory: true;
    repairEligible: boolean;
    failingStep: string | null;
    rawArtifactPath: string | null;
};
export type ArtifactKind = 'yaml' | 'output' | 'attempt' | 'repair-report' | 'diff' | 'memory' | 'model-metrics';
export type RunArtifact = {
    kind: ArtifactKind;
    path: string;
    sha256?: string;
};
export type SourceDiff = {
    path: string | null;
    affectedFiles: string[];
    contents: string;
    autoCommit: false;
};
export type RunnerRequest = {
    flowPath: string;
    artifactDirectory: string;
    timeoutMs?: number;
};
export type ExecutionResult = {
    passed: boolean;
    startedAt: string;
    endedAt: string;
    durationMs: number;
    exitCode: number | null;
    assertions: number | null;
    failure: WalkFailure | null;
    artifacts: RunArtifact[];
};
export interface WalkRunner {
    run(request: RunnerRequest): Promise<ExecutionResult>;
}
export type ExploreRequest = {
    identity: WalkIdentity;
};
export type ExplorationResult = {
    passed: boolean;
    durationMs: number;
    explorationDurationMs: number | null;
    verificationDurationMs: number | null;
    mcpCalls: number | null;
    modelCalls: number | null;
    modelCallBasis: string;
    model: string | null;
    provider: string | null;
    assertions: number | null;
    flowPath: string | null;
    flowSha256: string | null;
    failure: WalkFailure | null;
    artifacts: RunArtifact[];
    errors: string[];
};
export interface WalkExplorer {
    explore(request: ExploreRequest): Promise<ExplorationResult>;
}
export type RepairRequest = {
    identity: WalkIdentity;
};
export type RepairResult = {
    status: 'repaired' | 'rejected' | 'failed';
    passed: boolean;
    durationMs: number;
    modelCalls: 0;
    repairCount: 0 | 1;
    assertions: number | null;
    flowPath: string | null;
    flowSha256: string | null;
    failure: WalkFailure | null;
    artifacts: RunArtifact[];
    diffs: SourceDiff[];
    errors: string[];
    details?: Record<string, unknown>;
};
export interface SelectorRepairer {
    repair(request: RepairRequest): Promise<RepairResult>;
}
export type CacheStatus = 'hit' | 'miss' | 'not-checked';
export type RunOutcome = 'passed' | 'remembered' | 'cold-recorded' | 'repaired' | 'rejected' | 'exploration-rejected' | 'product-regression' | 'repair-rejected' | 'failed';
export type RunResult = {
    schemaVersion: typeof WALK_RESULT_SCHEMA_VERSION;
    command: 'explore' | 'run' | 'remember';
    identity: WalkIdentity;
    goalKey: string;
    cacheStatus: CacheStatus;
    cacheMissReason?: CacheMissReason;
    semantic?: SemanticLookupReport;
    outcome: RunOutcome;
    passed: boolean;
    startedAt: string;
    endedAt: string;
    durationMs: number;
    modelCalls: number | null;
    modelCallBasis: string;
    assertions: number | null;
    repairCount: 0 | 1;
    yamlPath: string | null;
    yamlSha256: string | null;
    failure: WalkFailure | null;
    artifacts: RunArtifact[];
    diffs: SourceDiff[];
    errors: string[];
    execution?: ExecutionResult;
    exploration?: ExplorationResult;
    repair?: RepairResult;
};
export type ReportReceipt = {
    reportPath: string;
    latestPath: string;
};
export interface RunReporter {
    report(result: RunResult): Promise<ReportReceipt>;
}

// create-deja.d.ts
import type { RunReporter, RunResult, SelectorRepairer, SemanticRetrievalOptions, WalkExplorer, WalkIdentity, WalkRunner, WalkStore } from './contracts.ts';
/** Bumped on a breaking change to a plugin contract. Plugins declare a peer range on core. */
export declare const CONTRACT_VERSION = 1;
/** The flow format the Maestro runner replays. Part of exact cache identity. */
export declare const DEFAULT_FLOW_SCHEMA_VERSION = "maestro/v1";
/**
 * What core needs from a platform adapter. Core imports no runner code, so this is structural:
 * the adapters in `@deja-qa/maestro` satisfy it as they are.
 */
export interface DejaAdapter {
    readonly id: string;
    readonly platform: string;
    buildFingerprint(workspaceRoot: string, buildPath: string): Promise<string>;
    cacheIdentity(input: {
        buildFingerprint: string;
    }): {
        buildFingerprint: string;
        adapter?: string;
        browser?: string;
    };
}
/** Passed to the lazy explorer and repairer factories. */
export type DejaFactoryContext = {
    identity: WalkIdentity;
};
export type CreateDejaOptions = {
    workspaceRoot: string;
    app: {
        appId: string;
        /** Build artifact, relative to `workspaceRoot`. The adapter hashes it into the identity. */
        buildPath: string;
    };
    adapter: DejaAdapter;
    store: WalkStore;
    runner: WalkRunner;
    /** Lazy: called only on a cold miss or an explicit `explore`. A cache hit never calls it. */
    explorer: (context: DejaFactoryContext) => WalkExplorer;
    /** Lazy: called only for an eligible stale selector. Omit to refuse repair. */
    repairer?: (context: DejaFactoryContext) => SelectorRepairer;
    /** Defaults to a reporter that does nothing. */
    reporter?: RunReporter;
    semantic?: SemanticRetrievalOptions;
    flowSchemaVersion?: string;
    now?: () => Date;
    artifactId?: (prefix: 'run' | 'remember') => string;
};
export type DejaClient = {
    /** The identity `run`, `explore` and `remember` use for this goal. */
    identity(goal: string): Promise<WalkIdentity>;
    run(goal: string): Promise<RunResult>;
    explore(goal: string): Promise<RunResult>;
    remember(goal: string, candidatePath: string): Promise<RunResult>;
};
export declare function createDeja(options: CreateDejaOptions): DejaClient;

// index.d.ts
export * from './contracts.ts';
export * from './semantic.ts';
export * from './route-graph.ts';
export * from './subflow.ts';
export { Deja, type DejaDependencies } from './orchestration.ts';
export { CONTRACT_VERSION, DEFAULT_FLOW_SCHEMA_VERSION, createDeja, type CreateDejaOptions, type DejaAdapter, type DejaClient, type DejaFactoryContext, } from './create-deja.ts';
export { UnsupportedPlatformExplorer } from './unsupported-platform-explorer.ts';

// orchestration.d.ts
import { type RunResult, type RunReporter, type SelectorRepairer, type SemanticRetrievalOptions, type WalkExplorer, type WalkIdentity, type WalkRunner, type WalkStore } from './contracts.ts';
export type DejaDependencies = {
    store: WalkStore;
    runner: WalkRunner;
    reporter: RunReporter;
    /** Lazy by contract: the factory must not be called until a cold miss or explicit explore. */
    createExplorer: () => WalkExplorer;
    /** Lazy by contract: only an eligible stale selector can construct a repairer. */
    createRepairer?: () => SelectorRepairer;
    /** Optional WALK-016 proof. Its provider factory is lazy and exact hits never invoke it. */
    semantic?: SemanticRetrievalOptions;
    now?: () => Date;
    artifactId?: (prefix: 'run' | 'remember') => string;
};
export declare class Deja {
    private readonly dependencies;
    constructor(dependencies: DejaDependencies);
    private now;
    private artifactId;
    private base;
    private finish;
    run(identity: WalkIdentity): Promise<RunResult>;
    explore(identity: WalkIdentity): Promise<RunResult>;
    remember(identity: WalkIdentity, candidatePath: string): Promise<RunResult>;
    private explorationDetails;
    private repairDetails;
}

// route-graph.d.ts
export declare const ROUTE_GRAPH_SCHEMA_VERSION = 1;
export declare const SCREEN_FINGERPRINT_VERSION = 1;
export declare const ROUTE_GRAPH_FALLBACK: 'existing-exact-replay-or-exploration';
export type ScreenObservation = {
    kind: 'android-resource-id';
    value: string;
};
export type ScreenNode = {
    nodeId: string;
    observations: ScreenObservation[];
    fingerprint: string;
    validatedAt: string;
};
export type RouteAction = {
    kind: 'tap';
    targetResourceId: string;
};
export type VerifiedTransition = {
    transitionId: string;
    fromNodeId: string;
    action: RouteAction;
    toNodeId: string;
    validatedAt: string;
};
export type FailedBranch = {
    branchId: string;
    fromNodeId: string;
    action: RouteAction;
    category: 'selector' | 'navigation' | 'product_assertion' | 'runner' | 'timeout' | 'unknown';
    failedAt: string;
};
export type EvidenceArtifact = {
    path: string;
    sha256: string;
};
export type RouteGraphProvenance = {
    kind: 'curated-android-evidence';
    ticket: string;
    runId: string;
    newDeviceRunRecorded: false;
    artifacts: EvidenceArtifact[];
} | {
    kind: 'deterministic-fixture';
    fixtureId: string;
    newDeviceRunRecorded: false;
};
export type RouteGraph = {
    schemaVersion: typeof ROUTE_GRAPH_SCHEMA_VERSION;
    appId: string;
    platform: 'android';
    buildFingerprint: string;
    screenFingerprintVersion: typeof SCREEN_FINGERPRINT_VERSION;
    validatedAt: string;
    replayValidatedAt: string | null;
    nodes: ScreenNode[];
    transitions: VerifiedTransition[];
    failedBranches: FailedBranch[];
    provenance: RouteGraphProvenance;
};
export type ScreenMatchOptions = {
    observations: ScreenObservation[];
    /** The logical screen expected at this point, when one is known. */
    expectedNodeId?: string;
    /** The last matched node; a subsequent match to it is a repeated observation. */
    previousNodeId?: string;
};
export type ScreenMatch = {
    status: 'matched' | 'repeated' | 'ambiguous' | 'changed' | 'unknown';
    fingerprint: string;
    candidateNodeIds: string[];
    /** Route-graph observations never authorize a walk-cache hit. */
    cacheHit: false;
    fallback: typeof ROUTE_GRAPH_FALLBACK | null;
};
export declare class InvalidRouteGraphError extends Error {
    constructor(message: string);
}
/** SHA-256 over a canonical, bounded set; observation input order never changes the fingerprint. */
export declare function fingerprintScreen(input: ScreenObservation[]): string;
export declare function makeScreenNode(nodeId: string, input: ScreenObservation[], validatedAt: string): ScreenNode;
/** Validate the complete closed schema, including recomputed fingerprints and node references. */
export declare function validateRouteGraph(value: unknown): RouteGraph;
/**
 * Diagnostic-only matching. Duplicate fingerprints are ambiguous; missing expected screens are
 * changed; unmatched screens without an expectation are unknown. None of these calls WalkStore.
 */
export declare function matchScreen(graphInput: RouteGraph, options: ScreenMatchOptions): ScreenMatch;

// semantic.d.ts
import { type LookupHit, type LookupMiss, type SemanticLookupReport, type SemanticRetrievalOptions, type WalkIdentity, type WalkStore } from './contracts.ts';
export declare const DEFAULT_MINIMUM_SIMILARITY = 0.9;
export declare const DEFAULT_MINIMUM_MARGIN = 0.15;
export type ExactFirstLookupResult = {
    status: 'hit';
    matchType: 'exact' | 'semantic';
    lookup: LookupHit;
    semantic: SemanticLookupReport;
} | {
    status: 'miss';
    miss: LookupMiss;
    semantic: SemanticLookupReport;
};
/** Narrow prompt-injection markers, not a claim of general adversarial-language detection. */
export declare function hasAdversarialGoalMarker(goal: string): boolean;
export declare function goalsAreContradictory(left: string, right: string): boolean;
export declare function cosineSimilarity(left: readonly number[], right: readonly number[]): number;
/**
 * Exact lookup always happens before the lazy semantic provider. Candidate goals are embedded only
 * after store.lookup has rechecked the full identity, latest pass, canonical path, and checksum.
 */
export declare class ExactFirstWalkRetriever {
    private readonly store;
    private readonly semantic?;
    private readonly now;
    constructor(options: {
        store: WalkStore;
        semantic?: SemanticRetrievalOptions;
        now?: () => number;
    });
    lookup(identity: WalkIdentity): Promise<ExactFirstLookupResult>;
}

// subflow.d.ts
import type { CacheMissReason, LookupMiss, WalkIdentity, WalkValidation } from './contracts.ts';
export declare const VERIFIED_SUBFLOW_SCHEMA_VERSION = 1;
/** The app/build half of exact walk identity. A segment has no goal and is never a semantic hit. */
export type SubflowIdentity = Omit<WalkIdentity, 'goal'>;
export type SubflowInput = {
    name: string;
    kind: 'text' | 'credential';
    required: boolean;
};
export type SubflowOutput = {
    name: string;
    kind: 'screen';
    resourceId: string;
};
export type SubflowPrecondition = {
    kind: 'clean-app-launch';
} | {
    kind: 'screen-visible';
    resourceId: string;
};
export type SubflowPostcondition = {
    kind: 'screen-visible';
    resourceId: string;
};
export type SubflowEvidenceArtifact = {
    path: string;
    sha256: string;
};
export type SubflowProvenance = {
    kind: 'curated-evidence';
    ticket: string;
    runId: string;
    newDeviceRunRecorded: false;
    artifacts: SubflowEvidenceArtifact[];
} | {
    kind: 'deterministic-fixture';
    fixtureId: string;
    newDeviceRunRecorded: false;
};
export type SubflowContract = {
    schemaVersion: typeof VERIFIED_SUBFLOW_SCHEMA_VERSION;
    segmentId: string;
    identity: SubflowIdentity;
    inputs: SubflowInput[];
    outputs: SubflowOutput[];
    preconditions: SubflowPrecondition[];
    postconditions: SubflowPostcondition[];
    stepCount: number;
    assertionCount: number;
    provenance: SubflowProvenance;
};
export type VerifiedSubflow = SubflowContract & {
    flowPath: string;
    flowSha256: string;
    lastValidation: WalkValidation & {
        passed: true;
    };
};
export type VerifiedSubflowHit = {
    status: 'hit';
    segment: VerifiedSubflow;
    flowPath: string;
};
export type VerifiedSubflowLookup = VerifiedSubflowHit | LookupMiss;
export type PersistVerifiedSubflowOptions = {
    contract: SubflowContract;
    candidatePath: string;
    validation: WalkValidation;
};
export interface VerifiedSubflowStore {
    lookup(segmentId: string, identity: SubflowIdentity): Promise<VerifiedSubflowLookup>;
    persistVerified(options: PersistVerifiedSubflowOptions): Promise<VerifiedSubflow>;
    list(): Promise<VerifiedSubflow[]>;
}
export type CompositionStepMetrics = {
    reusedSteps: number;
    newlyExploredSteps: number;
    totalSteps: number;
    reusedAssertions: number;
    newAssertions: number;
    totalAssertions: number;
};
export type CompositionModelMetrics = {
    composition: 0;
    newExploration: 0;
    segmentVerification: 0;
    fullJourneyVerification: 0;
    basis: string;
};
export declare class InvalidSubflowError extends Error {
    constructor(message: string);
}
/** Validate a closed, bounded segment contract; checksums and execution remain store/runner duties. */
export declare function validateSubflowContract(value: unknown): SubflowContract;
export declare function sameSubflowIdentity(left: SubflowIdentity, right: SubflowIdentity): boolean;
export declare function subflowMissReason(requested: SubflowIdentity, stored: readonly VerifiedSubflow[]): CacheMissReason;

// unsupported-platform-explorer.d.ts
import type { ExplorationResult, WalkExplorer } from './contracts.ts';
/** Reviewed-YAML-only platforms never instantiate the Android explorer or its credential chain. */
export declare class UnsupportedPlatformExplorer implements WalkExplorer {
    private readonly platform;
    constructor(platform: 'web' | 'ios');
    explore(): Promise<ExplorationResult>;
}
