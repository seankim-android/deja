// typescript 7.0.2
// entry . (./src/index.ts)
// exports: AdapterCacheIdentity, AdapterIdentityInput, AndroidAdapterOptions, COMPOSITION_MODEL_METRICS, CompositionRunnerObservation, CompositionRunnerTrace, IosAdapterOptions, IosExplorationDriverOptions, LoginCompositionResult, MAESTRO_COMPOSITE_FLOW_SCHEMA, MAESTRO_FLOW_SCHEMA_VERSION, MaestroArtifactPaths, MaestroCommandOptions, MaestroExecutionAdapter, MaestroFailureCategory, MaestroFailureClassification, MaestroFailureInput, MaestroFlowContainment, MaestroFlowResult, MaestroHierarchyResult, MaestroLoginComposer, MaestroLoginComposerOptions, MaestroPlatform, MaestroRunResult, MaestroRunnerError, MaestroRunnerErrorKind, MaestroTransport, MaestroWalkRunner, MaestroWalkRunnerOptions, McpMaestroTransport, McpMaestroTransportOptions, ProcessMaestroTransport, ProcessMaestroTransportOptions, RunMaestroOptions, SegmentVerificationResult, WebAdapterOptions, androidMaestroAdapter, classifyMaestroFailure, composeMaestroLoginFlow, countFlowAssertions, countMaestroAssertions, countMaestroSteps, escapeRegex, idSelector, iosExplorationDriver, iosFingerprintRules, iosMaestroAdapter, parseMaestroHierarchy, readCompositionRunnerTrace, readMaestroContinuation, resolveFlowPath, runMaestro, tapSegment, validateMaestroFlowReference, waitVisibleSegment, webMaestroAdapter
export type AdapterCacheIdentity = AdapterIdentityInput & {
    adapter?: string;
    browser?: string;
};
export type AdapterIdentityInput = {
    buildFingerprint: string;
};
export type AndroidAdapterOptions = {
    device: string;
};
export declare function androidMaestroAdapter(options: AndroidAdapterOptions): MaestroExecutionAdapter;
export declare function classifyMaestroFailure(input: MaestroFailureInput): MaestroFailureClassification | null;
export declare function composeMaestroLoginFlow(appId: string, segmentReference: string, continuationYaml: string, containment?: MaestroFlowContainment): string;
export declare const COMPOSITION_MODEL_METRICS: CompositionModelMetrics;
export type CompositionRunnerObservation = {
    kind: 'launch';
    clearState: boolean;
} | {
    kind: 'assertVisible';
    resourceId: string | null;
    text: string | null;
} | {
    kind: 'action';
    command: 'tapOn' | 'inputText';
};
export type CompositionRunnerTrace = {
    observations: CompositionRunnerObservation[];
    finalVisibleResourceIds: string[];
    finalScreen: string;
};
export declare function countFlowAssertions(flowPath: string): Promise<number | null>;
export declare function countMaestroAssertions(contents: string): number;
export declare function countMaestroSteps(contents: string): number;
export declare function escapeRegex(text: string): string;
export declare function idSelector(id: string): string;
export type IosAdapterOptions = {
    device: string;
    adapterId?: string;
};
export declare function iosExplorationDriver(options: IosExplorationDriverOptions): ExplorationDriver & {
    readonly appId: string | null;
};
export type IosExplorationDriverOptions = {
    transport: MaestroTransport;
    sheetPattern?: RegExp;
    now?: () => Date;
};
export declare const iosFingerprintRules: FingerprintRules;
export declare function iosMaestroAdapter(options: IosAdapterOptions): MaestroExecutionAdapter;
export type LoginCompositionResult = {
    status: 'composed' | 'rejected' | 'persist-failed';
    passed: boolean;
    verificationPassed: boolean;
    identity: WalkIdentity;
    candidatePath: string | null;
    candidateSha256: string | null;
    segment: VerifiedSubflow | null;
    persisted: PersistedWalk | null;
    execution: ExecutionResult | null;
    steps: CompositionStepMetrics | null;
    modelCalls: CompositionModelMetrics;
    errors: string[];
};
export declare const MAESTRO_COMPOSITE_FLOW_SCHEMA = "maestro/v1";
export declare const MAESTRO_FLOW_SCHEMA_VERSION = "maestro/v1";
export type MaestroArtifactPaths = {
    directory: string;
    stdoutPath: string;
    stderrPath: string;
    outputPath: string;
};
export type MaestroCommandOptions = {
    junitPath?: string;
};
export interface MaestroExecutionAdapter {
    readonly id: string;
    readonly platform: MaestroPlatform;
    readonly browser: string | null;
    readonly device: string;
    cacheIdentity(input: AdapterIdentityInput): AdapterCacheIdentity;
    buildFingerprint(workspaceRoot: string, buildPath: string): Promise<string>;
    commandArguments(flowPath: string, options?: MaestroCommandOptions): string[];
}
export type MaestroFailureCategory = 'selector' | 'navigation' | 'product_assertion' | 'runner' | 'timeout' | 'unknown';
export type MaestroFailureClassification = {
    category: MaestroFailureCategory;
    failingStep: string | null;
    rawArtifactPath: string | null;
    advisory: true;
    repairEligible: boolean;
};
export type MaestroFailureInput = {
    passed: boolean;
    timedOut: boolean;
    output: string;
    errors: Pick<MaestroRunnerError, 'kind'>[];
    rawArtifactPath?: string | null;
};
export type MaestroFlowContainment = {
    workspaceRoot: string;
    flowDirectory: string;
};
export type MaestroFlowResult = {
    passed: boolean;
    exitCode: number | null;
    output: string;
    durationMs: number;
    timedOut: boolean;
};
export type MaestroHierarchyResult = {
    raw: unknown;
    durationMs: number;
};
export declare class MaestroLoginComposer {
    private readonly options;
    constructor(options: MaestroLoginComposerOptions);
    private artifactId;
    verifyAndSaveSegment(contractInput: SubflowContract, candidatePath: string): Promise<SegmentVerificationResult>;
    compose(identity: WalkIdentity, segmentId: string, continuationYaml: string): Promise<LoginCompositionResult>;
}
export type MaestroLoginComposerOptions = {
    workspaceRoot: string;
    segments: VerifiedSubflowStore;
    walks: WalkStore;
    runner: WalkRunner;
    artifactId?: (prefix: 'login-verification' | 'composition-verification') => string;
};
export type MaestroPlatform = 'android' | 'web' | 'ios';
export type MaestroRunnerError = {
    kind: MaestroRunnerErrorKind;
    message: string;
    code?: string;
    path?: string;
};
export type MaestroRunnerErrorKind = 'configuration' | 'flow-path' | 'launch' | 'timeout' | 'execution' | 'output-artifact';
export type MaestroRunResult = {
    platform: MaestroPlatform;
    device: string;
    flowPath: string;
    startedAt: string;
    endedAt: string;
    durationMs: number;
    monotonicDurationMs: number;
    exitCode: number | null;
    signal: NodeJS.Signals | null;
    timedOut: boolean;
    passed: boolean;
    stdout: string;
    stderr: string;
    output: string;
    errors: MaestroRunnerError[];
    artifacts?: MaestroArtifactPaths;
};
export interface MaestroTransport {
    readonly kind: 'process' | 'mcp';
    hierarchy(): Promise<MaestroHierarchyResult>;
    runFlow(yaml: string, env?: Record<string, string>): Promise<MaestroFlowResult>;
    close(): Promise<void>;
}
export declare class MaestroWalkRunner implements WalkRunner {
    readonly executable: string;
    private readonly options;
    constructor(options: MaestroWalkRunnerOptions);
    runFlow(flowPath: string, artifactDirectory: string, timeoutMs?: number, env?: Record<string, string>): Promise<MaestroRunResult>;
    adapt(result: MaestroRunResult): Promise<ExecutionResult>;
    run(request: RunnerRequest): Promise<ExecutionResult>;
}
export type MaestroWalkRunnerOptions = {
    workspaceRoot: string;
    executable: string;
    device: string;
    timeoutMs: number;
    adapter?: MaestroExecutionAdapter;
    androidSdkRoot?: string;
    javaHome?: string;
    junitPath?: string;
};
export declare class McpMaestroTransport implements MaestroTransport {
    readonly kind: 'mcp';
    private child;
    private buffer;
    private nextId;
    private readonly pending;
    private ready;
    startupMs: number;
    private readonly options;
    constructor(options: McpMaestroTransportOptions);
    private fail;
    private send;
    private request;
    private start;
    private call;
    hierarchy(): Promise<MaestroHierarchyResult>;
    runFlow(yaml: string, env?: Record<string, string>): Promise<MaestroFlowResult>;
    close(): Promise<void>;
}
export type McpMaestroTransportOptions = {
    executable: string;
    device: string;
    timeoutMs?: number;
};
export declare function parseMaestroHierarchy(raw: unknown): RawElement[];
export declare class ProcessMaestroTransport implements MaestroTransport {
    readonly kind: 'process';
    private readonly options;
    constructor(options: ProcessMaestroTransportOptions);
    hierarchy(): Promise<MaestroHierarchyResult>;
    runFlow(yaml: string, env?: Record<string, string>): Promise<MaestroFlowResult>;
    close(): Promise<void>;
}
export type ProcessMaestroTransportOptions = {
    executable: string;
    device: string;
    timeoutMs?: number;
};
export declare function readCompositionRunnerTrace(workspaceRoot: string, execution: ExecutionResult): Promise<CompositionRunnerTrace | null>;
export declare function readMaestroContinuation(path: string): Promise<string>;
export declare function resolveFlowPath(workspaceRoot: string, flowPath: string): string;
export declare function runMaestro(options: RunMaestroOptions): Promise<MaestroRunResult>;
export type RunMaestroOptions = {
    executable: string;
    device: string;
    platform: MaestroPlatform;
    adapter?: MaestroExecutionAdapter;
    workspaceRoot: string;
    flowPath: string;
    timeoutMs: number;
    env?: Record<string, string>;
    androidSdkRoot?: string;
    javaHome?: string;
    junitPath?: string;
    artifacts?: {
        directory: string;
    };
};
export type SegmentVerificationResult = {
    status: 'verified' | 'rejected';
    passed: boolean;
    segment: VerifiedSubflow | null;
    execution: ExecutionResult;
    errors: string[];
};
export declare function tapSegment(id: string): Segment;
export declare function validateMaestroFlowReference(reference: string, containment?: MaestroFlowContainment): string;
export declare function waitVisibleSegment(id: string, timeoutMs?: number): Segment;
export type WebAdapterOptions = {
    device?: string;
    headless?: boolean;
    screenSize?: string;
    adapterId?: string;
};
export declare function webMaestroAdapter(options?: WebAdapterOptions): MaestroExecutionAdapter;
