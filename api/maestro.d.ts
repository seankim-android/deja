// execution-observations.d.ts
import type { ExecutionResult } from '@deja-qa/core';
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
/**
 * Read the bounded observation protocol emitted by the executable composition fixture within its
 * actual process output. Ordinary Maestro output without observations cannot authorize a segment.
 */
export declare function readCompositionRunnerTrace(workspaceRoot: string, execution: ExecutionResult): Promise<CompositionRunnerTrace | null>;

// failure-classification.d.ts
import type { MaestroRunnerError } from './index.ts';
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
/**
 * Classify only explicit runner signals and narrow Maestro output shapes. These are advisory
 * heuristics, not a root-cause diagnosis. Conflicting or unrecognized output stays unknown.
 */
export declare function classifyMaestroFailure(input: MaestroFailureInput): MaestroFailureClassification | null;

// index.d.ts
import type { MaestroExecutionAdapter, MaestroPlatform } from './platform-adapter.ts';
/** The Maestro flow schema that walk identities and stored flows are keyed on. */
export declare const MAESTRO_FLOW_SCHEMA_VERSION = "maestro/v1";
export { MaestroWalkRunner, countFlowAssertions, type MaestroWalkRunnerOptions, } from './walk-runner.ts';
export { COMPOSITION_MODEL_METRICS, MAESTRO_COMPOSITE_FLOW_SCHEMA, MaestroLoginComposer, composeMaestroLoginFlow, countMaestroAssertions, countMaestroSteps, readMaestroContinuation, validateMaestroFlowReference, type LoginCompositionResult, type MaestroFlowContainment, type MaestroLoginComposerOptions, type SegmentVerificationResult, } from './subflow-composition.ts';
export { readCompositionRunnerTrace, type CompositionRunnerObservation, type CompositionRunnerTrace, } from './execution-observations.ts';
export { androidMaestroAdapter, iosMaestroAdapter, webMaestroAdapter, type AdapterCacheIdentity, type AdapterIdentityInput, type AndroidAdapterOptions, type IosAdapterOptions, type MaestroExecutionAdapter, type MaestroPlatform, type WebAdapterOptions, } from './platform-adapter.ts';
export { classifyMaestroFailure, type MaestroFailureCategory, type MaestroFailureClassification, type MaestroFailureInput, } from './failure-classification.ts';
export type MaestroRunnerErrorKind = 'configuration' | 'flow-path' | 'launch' | 'timeout' | 'execution' | 'output-artifact';
export type MaestroRunnerError = {
    kind: MaestroRunnerErrorKind;
    message: string;
    code?: string;
    path?: string;
};
export type MaestroArtifactPaths = {
    directory: string;
    stdoutPath: string;
    stderrPath: string;
    outputPath: string;
};
export type RunMaestroOptions = {
    executable: string;
    device: string;
    platform: MaestroPlatform;
    adapter?: MaestroExecutionAdapter;
    workspaceRoot: string;
    flowPath: string;
    timeoutMs: number;
    androidSdkRoot?: string;
    javaHome?: string;
    artifacts?: {
        directory: string;
    };
};
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
export declare function resolveFlowPath(workspaceRoot: string, flowPath: string): string;
export declare function runMaestro(options: RunMaestroOptions): Promise<MaestroRunResult>;

// platform-adapter.d.ts
export type MaestroPlatform = 'android' | 'web' | 'ios';
export type AdapterIdentityInput = {
    buildFingerprint: string;
};
export type AdapterCacheIdentity = AdapterIdentityInput & {
    adapter?: string;
    browser?: string;
};
/** The composition-root adapter owns runner selection and its share of exact cache identity. */
export interface MaestroExecutionAdapter {
    readonly id: string;
    readonly platform: MaestroPlatform;
    readonly browser: string | null;
    readonly device: string;
    cacheIdentity(input: AdapterIdentityInput): AdapterCacheIdentity;
    buildFingerprint(workspaceRoot: string, buildPath: string): Promise<string>;
    commandArguments(flowPath: string): string[];
}
export type AndroidAdapterOptions = {
    device: string;
};
export type WebAdapterOptions = {
    device?: string;
    headless?: boolean;
    screenSize?: string;
    adapterId?: string;
};
export type IosAdapterOptions = {
    device: string;
    adapterId?: string;
};
export declare function androidMaestroAdapter(options: AndroidAdapterOptions): MaestroExecutionAdapter;
export declare function webMaestroAdapter(options?: WebAdapterOptions): MaestroExecutionAdapter;
export declare function iosMaestroAdapter(options: IosAdapterOptions): MaestroExecutionAdapter;

// subflow-composition.d.ts
import { type CompositionModelMetrics, type CompositionStepMetrics, type ExecutionResult, type PersistedWalk, type SubflowContract, type VerifiedSubflow, type VerifiedSubflowStore, type WalkIdentity, type WalkRunner, type WalkStore } from '@deja-qa/core';
export declare const MAESTRO_COMPOSITE_FLOW_SCHEMA = "maestro/v1";
export declare const COMPOSITION_MODEL_METRICS: CompositionModelMetrics;
export type SegmentVerificationResult = {
    status: 'verified' | 'rejected';
    passed: boolean;
    segment: VerifiedSubflow | null;
    execution: ExecutionResult;
    errors: string[];
};
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
export type MaestroLoginComposerOptions = {
    workspaceRoot: string;
    segments: VerifiedSubflowStore;
    walks: WalkStore;
    runner: WalkRunner;
    artifactId?: (prefix: 'login-verification' | 'composition-verification') => string;
};
export type MaestroFlowContainment = {
    workspaceRoot: string;
    flowDirectory: string;
};
export declare function validateMaestroFlowReference(reference: string, containment?: MaestroFlowContainment): string;
/** Compose by reference: no login command or fixture value is copied into the composed YAML. */
export declare function composeMaestroLoginFlow(appId: string, segmentReference: string, continuationYaml: string, containment?: MaestroFlowContainment): string;
export declare function countMaestroSteps(contents: string): number;
export declare function countMaestroAssertions(contents: string): number;
/** A one-level, clean-launch login segment plus explicitly supplied new checkout steps. */
export declare class MaestroLoginComposer {
    private readonly options;
    constructor(options: MaestroLoginComposerOptions);
    private artifactId;
    verifyAndSaveSegment(contractInput: SubflowContract, candidatePath: string): Promise<SegmentVerificationResult>;
    compose(identity: WalkIdentity, segmentId: string, continuationYaml: string): Promise<LoginCompositionResult>;
}
/** Read a continuation without treating it as a new runner or route-planning interface. */
export declare function readMaestroContinuation(path: string): Promise<string>;

// walk-runner.d.ts
import type { ExecutionResult, RunnerRequest, WalkRunner } from '@deja-qa/core';
import { type MaestroExecutionAdapter, type MaestroRunResult } from './index.ts';
export type MaestroWalkRunnerOptions = {
    workspaceRoot: string;
    executable: string;
    device: string;
    timeoutMs: number;
    adapter?: MaestroExecutionAdapter;
    androidSdkRoot?: string;
    javaHome?: string;
};
export declare function countFlowAssertions(flowPath: string): Promise<number | null>;
export declare class MaestroWalkRunner implements WalkRunner {
    readonly executable: string;
    private readonly options;
    constructor(options: MaestroWalkRunnerOptions);
    runFlow(flowPath: string, artifactDirectory: string, timeoutMs?: number): Promise<MaestroRunResult>;
    adapt(result: MaestroRunResult): Promise<ExecutionResult>;
    run(request: RunnerRequest): Promise<ExecutionResult>;
}
