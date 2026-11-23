// typescript 7.0.2
// entry . (./src/index.ts)
// exports: ASSERTION_HASH_VERSION, ArtifactKind, AssertionHashChangedError, CONTRACT_VERSION, CacheMissReason, CacheStatus, CandidateSnapshot, CompositionModelMetrics, CompositionStepMetrics, CreateDejaOptions, DEFAULT_FLOW_SCHEMA_VERSION, DEFAULT_MINIMUM_MARGIN, DEFAULT_MINIMUM_SIMILARITY, Deja, DejaAdapter, DejaCallOptions, DejaClient, DejaClientRememberOptions, DejaClientRunOptions, DejaDependencies, DejaFactoryContext, DejaRememberOptions, DejaRunOptions, EvidenceArtifact, ExactFirstLookupResult, ExactFirstWalkRetriever, ExecutionResult, ExpectedClosureMismatchError, ExpectedFlowChecksumMismatchError, ExplorationDetails, ExplorationResult, ExploreRequest, FailedBranch, FailureCategory, FlowAssertion, FlowChangedError, FlowReference, FlowReferenceError, Goal, GoalEmbeddingProvider, InvalidRouteGraphError, InvalidSubflowError, InvalidWalkManifestError, LookupHit, LookupMiss, MANIFEST_HASH_VERSION, ManifestOwnerRules, ManifestProfile, ManifestWalk, NewBuildReplayReport, OwnerRuleError, PersistVerifiedOptions, PersistVerifiedRepairOptions, PersistVerifiedSubflowOptions, PersistedWalk, ROUTE_GRAPH_FALLBACK, ROUTE_GRAPH_SCHEMA_VERSION, RepairProvenance, RepairRequest, RepairResult, ReportReceipt, RouteAction, RouteGraph, RouteGraphProvenance, RunArtifact, RunOutcome, RunReporter, RunResult, RunnerRequest, SCREEN_FINGERPRINT_VERSION, ScreenMatch, ScreenMatchOptions, ScreenNode, ScreenObservation, SelectorRepairer, SemanticLookupReason, SemanticLookupReport, SemanticLookupStatus, SemanticRetrievalOptions, SemanticSelectedWalk, SourceDiff, StaleBuildPolicy, StoredFlowFile, StoredValidation, StoredWalk, SubflowContract, SubflowEvidenceArtifact, SubflowIdentity, SubflowInput, SubflowOutput, SubflowPostcondition, SubflowPrecondition, SubflowProvenance, UnsupportedPlatformExplorer, VERIFIED_SUBFLOW_SCHEMA_VERSION, VerifiedSubflow, VerifiedSubflowHit, VerifiedSubflowLookup, VerifiedSubflowStore, VerifiedTransition, WALK_MANIFEST_VERSION, WALK_RESULT_SCHEMA_VERSION, WalkExplorer, WalkFailure, WalkIdentity, WalkManifest, WalkRunner, WalkStore, WalkValidation, checkManifestFlowPath, cosineSimilarity, createDeja, emptyWalkManifest, findFlowAssertions, findFlowReferences, fingerprintScreen, goalsAreContradictory, hasAdversarialGoalMarker, makeScreenNode, manifestWalkKey, matchScreen, normalizeGoal, parseWalkManifest, sameSubflowIdentity, serializeWalkManifest, subflowMissReason, validateRouteGraph, validateSubflowContract
export type ArtifactKind = 'yaml' | 'output' | 'attempt' | 'repair-report' | 'diff' | 'memory' | 'model-metrics' | 'junit' | 'manifest';
export declare const ASSERTION_HASH_VERSION = 1;
export declare class AssertionHashChangedError extends Error {
    readonly manifestAssertionsSha256: string;
    readonly currentAssertionsSha256: string;
    constructor(manifestAssertionsSha256: string, currentAssertionsSha256: string);
}
export type CacheMissReason = 'not-found' | 'wrong-app' | 'wrong-platform' | 'wrong-flow-schema' | 'wrong-adapter' | 'wrong-browser' | 'stale-build' | 'missing-flow' | 'failed-validation' | 'legacy-closure-unknown' | 'flow-changed' | 'not-validated' | 'needs-migration';
export type CacheStatus = 'hit' | 'miss' | 'not-checked';
export type CandidateSnapshot = {
    candidatePath: string;
    closureSha256: string;
};
export declare function checkManifestFlowPath(value: unknown, where?: string): string;
export type CompositionModelMetrics = {
    composition: 0;
    newExploration: 0;
    segmentVerification: 0;
    fullJourneyVerification: 0;
    basis: string;
};
export type CompositionStepMetrics = {
    reusedSteps: number;
    newlyExploredSteps: number;
    totalSteps: number;
    reusedAssertions: number;
    newAssertions: number;
    totalAssertions: number;
};
export declare const CONTRACT_VERSION = 1;
export declare function cosineSimilarity(left: readonly number[], right: readonly number[]): number;
export declare function createDeja(options: CreateDejaOptions): DejaClient;
export type CreateDejaOptions = {
    workspaceRoot: string;
    app: {
        appId: string;
        buildPath: string;
    };
    adapter: DejaAdapter;
    store: WalkStore;
    manifestPath?: string;
    runner: WalkRunner;
    explorer: (context: DejaFactoryContext) => WalkExplorer;
    repairer?: (context: DejaFactoryContext) => SelectorRepairer;
    reporter?: RunReporter;
    semantic?: SemanticRetrievalOptions;
    staleBuild?: StaleBuildPolicy;
    flowSchemaVersion?: string;
    now?: () => Date;
    artifactId?: (prefix: 'run' | 'remember') => string;
};
export declare const DEFAULT_FLOW_SCHEMA_VERSION = "maestro/v1";
export declare const DEFAULT_MINIMUM_MARGIN = 0.15;
export declare const DEFAULT_MINIMUM_SIMILARITY = 0.9;
export declare class Deja {
    private readonly dependencies;
    constructor(dependencies: DejaDependencies);
    private get manifestMode();
    private now;
    private artifactId;
    private base;
    private finish;
    run(identity: WalkIdentity, options?: DejaRunOptions): Promise<RunResult>;
    private replayLastVerified;
    explore(identity: WalkIdentity): Promise<RunResult>;
    remember(identity: WalkIdentity, candidatePath: string, options?: DejaRememberOptions): Promise<RunResult>;
    private manifestExplorationRefused;
    private recordOrFlowChanged;
    private manifestMiss;
    private verifyFlow;
    private explorationDetails;
    private repairDetails;
}
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
export type DejaCallOptions = {
    signal?: AbortSignal;
    timeoutMs?: number;
};
export type DejaClient = {
    identity(goal: string): Promise<WalkIdentity>;
    run(goal: string, callOptions?: DejaClientRunOptions): Promise<RunResult>;
    explore(goal: string, callOptions?: DejaCallOptions): Promise<RunResult>;
    remember(goal: string, candidatePath: string, callOptions?: DejaClientRememberOptions): Promise<RunResult>;
};
export type DejaClientRememberOptions = DejaCallOptions & DejaRememberOptions;
export type DejaClientRunOptions = DejaCallOptions & DejaRunOptions;
export type DejaDependencies = {
    store: WalkStore;
    runner: WalkRunner;
    reporter: RunReporter;
    createExplorer: () => WalkExplorer;
    createRepairer?: () => SelectorRepairer;
    semantic?: SemanticRetrievalOptions;
    staleBuild?: StaleBuildPolicy;
    now?: () => Date;
    artifactId?: (prefix: 'run' | 'remember') => string;
};
export type DejaFactoryContext = {
    identity: WalkIdentity;
    signal?: AbortSignal;
    timeoutMs?: number;
};
export type DejaRememberOptions = {
    acceptAssertionChange?: boolean;
};
export type DejaRunOptions = {
    verify?: boolean;
};
export declare function emptyWalkManifest(): WalkManifest;
export type EvidenceArtifact = {
    path: string;
    sha256: string;
};
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
export declare class ExpectedClosureMismatchError extends Error {
    constructor(expected: string, actual: string);
}
export declare class ExpectedFlowChecksumMismatchError extends Error {
    constructor(expected: string, actual: string);
}
export type ExplorationDetails = {
    explorationDurationMs: number | null;
    verificationDurationMs: number | null;
    mcpCalls: number | null;
    modelCalls: number | null;
    modelCallBasis: string;
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
export type ExploreRequest = {
    identity: WalkIdentity;
};
export type FailedBranch = {
    branchId: string;
    fromNodeId: string;
    action: RouteAction;
    category: 'selector' | 'navigation' | 'product_assertion' | 'runner' | 'timeout' | 'unknown';
    failedAt: string;
};
export type FailureCategory = 'selector' | 'navigation' | 'product_assertion' | 'runner' | 'timeout' | 'unknown';
export declare function findFlowAssertions(contents: string): FlowAssertion[];
export declare function findFlowReferences(contents: string): FlowReference[];
export declare function fingerprintScreen(input: ScreenObservation[]): string;
export type FlowAssertion = {
    entry: string;
    command: string;
    id: string | null;
    always: boolean;
    call?: {
        kind: 'flow' | 'script';
        path: string;
    };
};
export declare class FlowChangedError extends Error {
    constructor(manifestClosureSha256: string, currentClosureSha256: string);
}
export type FlowReference = {
    kind: 'flow' | 'script';
    path: string;
};
export declare class FlowReferenceError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export type Goal = {
    text: string;
    key: string;
};
export interface GoalEmbeddingProvider {
    readonly id: string;
    embed(goal: string): Promise<readonly number[]>;
    isCollisionSafeMatch?(requestedGoal: string, candidateGoal: string): boolean;
}
export declare function goalsAreContradictory(left: string, right: string): boolean;
export declare function hasAdversarialGoalMarker(goal: string): boolean;
export declare class InvalidRouteGraphError extends Error {
    constructor(message: string);
}
export declare class InvalidSubflowError extends Error {
    constructor(message: string);
}
export declare class InvalidWalkManifestError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export type LookupHit = {
    status: 'hit';
    walk: StoredWalk;
    flowPath: string;
};
export type LookupMiss = {
    status: 'miss';
    reason: CacheMissReason;
    flowChange?: {
        manifestClosureSha256: string;
        currentClosureSha256: string;
    };
    flowPath?: string;
};
export declare function makeScreenNode(nodeId: string, input: ScreenObservation[], validatedAt: string): ScreenNode;
export declare const MANIFEST_HASH_VERSION = 1;
export type ManifestOwnerRules = {
    minAssertions?: number;
    requiredAssertionIds?: string[];
};
export type ManifestProfile = {
    name?: string;
    launchArguments?: Record<string, string | number | boolean>;
    clearState?: boolean;
};
export type ManifestWalk = {
    goal: string;
    appId: string;
    platform: string;
    adapter: string;
    browser?: string;
    flow: string;
    closureSha256: string;
    assertionsSha256: string;
    ownerRules?: ManifestOwnerRules;
    profile?: ManifestProfile;
};
export declare function manifestWalkKey(walk: Pick<ManifestWalk, 'goal' | 'appId' | 'platform' | 'adapter' | 'browser'>): string;
export declare function matchScreen(graphInput: RouteGraph, options: ScreenMatchOptions): ScreenMatch;
export type NewBuildReplayReport = {
    policy: StaleBuildPolicy;
    previousBuildFingerprint: string;
    previousFlowSha256: string;
    previousValidatedAt: string;
    recordedForNewBuild: boolean;
};
export declare function normalizeGoal(goal: string): Goal;
export declare class OwnerRuleError extends Error {
    constructor(message: string);
}
export declare function parseWalkManifest(contents: string): WalkManifest;
export type PersistedWalk = {
    walk: StoredWalk;
    flowPath: string;
    flowSha256: string;
    storedMetadataPath: string;
    atomicWrite: true;
    manifest?: {
        path: string;
        changed: boolean;
    };
};
export type PersistVerifiedOptions = {
    identity: WalkIdentity;
    candidatePath: string;
    validation: WalkValidation;
    origin: 'remember' | 'explore' | 'replayed-on-new-build' | 'verify';
    exploration?: ExplorationDetails;
    assertionChange?: 'refuse' | 'accept';
    expectedClosureSha256?: string;
    expectedFlowSha256?: string;
};
export type PersistVerifiedRepairOptions = Omit<PersistVerifiedOptions, 'origin' | 'exploration'> & {
    previous: {
        buildFingerprint: string;
        flowSha256: string;
    };
    repair: RepairProvenance;
};
export type PersistVerifiedSubflowOptions = {
    contract: SubflowContract;
    candidatePath: string;
    validation: WalkValidation;
};
export type RepairProvenance = {
    attemptId: string;
    patchPath: string;
    previousBuildFingerprint: string;
    previousFlowSha256: string;
};
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
export type ReportReceipt = {
    reportPath: string;
    latestPath: string;
};
export declare const ROUTE_GRAPH_FALLBACK: 'existing-exact-replay-or-exploration';
export declare const ROUTE_GRAPH_SCHEMA_VERSION = 1;
export type RouteAction = {
    kind: 'tap';
    targetResourceId: string;
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
export type RunArtifact = {
    kind: ArtifactKind;
    path: string;
    sha256?: string;
};
export type RunnerRequest = {
    flowPath: string;
    artifactDirectory: string;
    timeoutMs?: number;
};
export type RunOutcome = 'passed' | 'remembered' | 'cold-recorded' | 'repaired' | 'rejected' | 'exploration-rejected' | 'product-regression' | 'repair-rejected' | 'failed' | 'flow-changed' | 'not-validated' | 'needs-migration' | 'assertions-changed';
export interface RunReporter {
    report(result: RunResult): Promise<ReportReceipt>;
}
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
    replayedOnNewBuild?: NewBuildReplayReport;
};
export declare function sameSubflowIdentity(left: SubflowIdentity, right: SubflowIdentity): boolean;
export declare const SCREEN_FINGERPRINT_VERSION = 1;
export type ScreenMatch = {
    status: 'matched' | 'repeated' | 'ambiguous' | 'changed' | 'unknown';
    fingerprint: string;
    candidateNodeIds: string[];
    cacheHit: false;
    fallback: typeof ROUTE_GRAPH_FALLBACK | null;
};
export type ScreenMatchOptions = {
    observations: ScreenObservation[];
    expectedNodeId?: string;
    previousNodeId?: string;
};
export type ScreenNode = {
    nodeId: string;
    observations: ScreenObservation[];
    fingerprint: string;
    validatedAt: string;
};
export type ScreenObservation = {
    kind: 'android-resource-id';
    value: string;
};
export interface SelectorRepairer {
    repair(request: RepairRequest): Promise<RepairResult>;
}
export type SemanticLookupReason = 'provider-unavailable' | 'adversarial-goal' | 'contradictory-goal' | 'no-eligible-candidates' | 'low-confidence' | 'ambiguous';
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
export type SemanticLookupStatus = 'hit' | 'miss' | 'unavailable' | 'disabled' | 'skipped-exact-hit' | 'not-applicable';
export type SemanticRetrievalOptions = {
    createProvider: () => GoalEmbeddingProvider | null;
    minimumSimilarity?: number;
    minimumMargin?: number;
};
export type SemanticSelectedWalk = {
    goal: string;
    goalKey: string;
    flowPath: string;
    flowSha256: string;
};
export declare function serializeWalkManifest(manifest: WalkManifest): string;
export type SourceDiff = {
    path: string | null;
    affectedFiles: string[];
    contents: string;
    autoCommit: false;
};
export type StaleBuildPolicy = 'replay-last-verified';
export type StoredFlowFile = {
    path: string;
    sha256: string;
};
export type StoredValidation = WalkValidation & {
    flowSha256: string;
};
export type StoredWalk = WalkIdentity & {
    goalKey: string;
    flowPath: string;
    flowSha256: string;
    subflowFiles?: StoredFlowFile[];
    legacyClosure?: true;
    closureSha256?: string;
    lastValidation: WalkValidation;
    validationHistory: StoredValidation[];
    origin?: 'remember' | 'explore' | 'repair' | 'replayed-on-new-build' | 'verify';
    exploration?: ExplorationDetails;
    repairHistory?: RepairProvenance[];
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
export type SubflowEvidenceArtifact = {
    path: string;
    sha256: string;
};
export type SubflowIdentity = Omit<WalkIdentity, 'goal'>;
export type SubflowInput = {
    name: string;
    kind: 'text' | 'credential';
    required: boolean;
};
export declare function subflowMissReason(requested: SubflowIdentity, stored: readonly VerifiedSubflow[]): CacheMissReason;
export type SubflowOutput = {
    name: string;
    kind: 'screen';
    resourceId: string;
};
export type SubflowPostcondition = {
    kind: 'screen-visible';
    resourceId: string;
};
export type SubflowPrecondition = {
    kind: 'clean-app-launch';
} | {
    kind: 'screen-visible';
    resourceId: string;
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
export declare class UnsupportedPlatformExplorer implements WalkExplorer {
    private readonly platform;
    constructor(platform: 'web' | 'ios');
    explore(): Promise<ExplorationResult>;
}
export declare function validateRouteGraph(value: unknown): RouteGraph;
export declare function validateSubflowContract(value: unknown): SubflowContract;
export declare const VERIFIED_SUBFLOW_SCHEMA_VERSION = 1;
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
export interface VerifiedSubflowStore {
    lookup(segmentId: string, identity: SubflowIdentity): Promise<VerifiedSubflowLookup>;
    persistVerified(options: PersistVerifiedSubflowOptions): Promise<VerifiedSubflow>;
    list(): Promise<VerifiedSubflow[]>;
}
export type VerifiedTransition = {
    transitionId: string;
    fromNodeId: string;
    action: RouteAction;
    toNodeId: string;
    validatedAt: string;
};
export declare const WALK_MANIFEST_VERSION = 1;
export declare const WALK_RESULT_SCHEMA_VERSION = 1;
export interface WalkExplorer {
    explore(request: ExploreRequest): Promise<ExplorationResult>;
}
export type WalkFailure = {
    category: FailureCategory;
    advisory: true;
    repairEligible: boolean;
    failingStep: string | null;
    rawArtifactPath: string | null;
};
export type WalkIdentity = {
    goal: string;
    appId: string;
    platform: string;
    flowSchemaVersion: string;
    buildFingerprint: string;
    adapter?: string;
    browser?: string;
};
export type WalkManifest = {
    manifestVersion: typeof WALK_MANIFEST_VERSION;
    hashVersion: typeof MANIFEST_HASH_VERSION;
    walks: ManifestWalk[];
};
export interface WalkRunner {
    run(request: RunnerRequest): Promise<ExecutionResult>;
}
export interface WalkStore {
    readonly manifestPath?: string;
    lookup(identity: WalkIdentity): Promise<LookupHit | LookupMiss>;
    lookupPreviousSuccessful(identity: WalkIdentity): Promise<LookupHit | LookupMiss>;
    snapshotCandidate?(candidatePath: string, directory: string): Promise<CandidateSnapshot>;
    persistVerifiedCandidate(options: PersistVerifiedOptions): Promise<PersistedWalk>;
    persistVerifiedRepair(options: PersistVerifiedRepairOptions): Promise<PersistedWalk>;
    recordValidation(identity: WalkIdentity, validation: WalkValidation): Promise<StoredWalk>;
    list(): Promise<StoredWalk[]>;
}
export type WalkValidation = {
    passed: boolean;
    at: string;
};
