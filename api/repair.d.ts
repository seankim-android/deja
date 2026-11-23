// adapter.d.ts
import type { RepairRequest, RepairResult, SelectorRepairer, WalkStore } from '@deja-qa/core';
import { type CheckoutRepairPolicy } from './index.ts';
import { MaestroWalkRunner } from '@deja-qa/maestro';
export type BoundedSelectorRepairerOptions = {
    workspaceRoot: string;
    policy: CheckoutRepairPolicy;
    timeoutMs: number;
    apkPath: string;
    device: string;
    store: WalkStore;
    /** Workspace-relative path of the store's durable memory file; see BoundedCheckoutRepairOptions. */
    memoryPath: string;
    runner: MaestroWalkRunner;
    fingerprint: () => Promise<string>;
    androidSdkRoot?: string;
    javaHome?: string;
    displayPatch?: (diff: string, affectedFiles: string[]) => void;
};
export declare class BoundedSelectorRepairer implements SelectorRepairer {
    private readonly options;
    constructor(options: BoundedSelectorRepairerOptions);
    repair(request: RepairRequest): Promise<RepairResult>;
}

// build.d.ts
export type RepairBuildResult = {
    command: string[];
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
    errors: string[];
    installCommand?: string[];
    installExitCode?: number | null;
};
export type RepairBuildOptions = {
    workspaceRoot: string;
    timeoutMs: number;
    javaHome?: string;
    androidSdkRoot?: string;
    gradleExecutable?: string;
};
export type RepairBuildInstallOptions = RepairBuildOptions & {
    device: string;
    apkPath: string;
    adbExecutable?: string;
};
export declare function runCheckoutGradleBuild(options: RepairBuildOptions): Promise<RepairBuildResult>;
export declare function runCheckoutBuildAndInstall(options: RepairBuildInstallOptions): Promise<RepairBuildResult>;
export declare function writeRepairProcessArtifacts(directory: string, result: Pick<RepairBuildResult, 'stdout' | 'stderr' | 'output'>): Promise<void>;

// index.d.ts
export { BoundedSelectorRepairer, type BoundedSelectorRepairerOptions, } from './adapter.ts';
import type { WalkStore } from '@deja-qa/core';
import { type MaestroFailureClassification, type MaestroRunResult } from '@deja-qa/maestro';
import { type RepairBuildResult } from './build.ts';
import { type CheckoutRepairPolicy } from './policy.ts';
export { runCheckoutBuildAndInstall, runCheckoutGradleBuild, type RepairBuildInstallOptions, type RepairBuildOptions, type RepairBuildResult, } from './build.ts';
export { CHECKOUT_AFFECTED_FILES, CHECKOUT_COMPONENT_PATH, CHECKOUT_IDS_PATH, createCheckoutRepairDiff, planCheckoutSelectorRepair, type CheckoutRepairPlan, type CheckoutRepairPolicy, type PlannedRepairFile, } from './policy.ts';
export type BoundedCheckoutRepairOptions = {
    workspaceRoot: string;
    goal: string;
    appId: string;
    platform: 'android';
    timeoutMs: number;
    policy: CheckoutRepairPolicy;
    attemptId?: string;
    /** The walk store the repaired flow is persisted through. */
    store: WalkStore;
    /**
     * Workspace-relative path of the store's durable memory file. Repair proves this file is
     * byte-for-byte unchanged unless a verified repair is persisted.
     */
    memoryPath: string;
    fingerprint: () => Promise<string>;
    runFlow: (flowPath: string, artifactDirectory: string, timeoutMs: number) => Promise<MaestroRunResult>;
    build: (timeoutMs: number) => Promise<RepairBuildResult>;
    displayPatch?: (diff: string, affectedFiles: string[]) => void;
};
export type RepairExecutionReport = {
    startedAt: string;
    endedAt: string;
    durationMs: number;
    monotonicDurationMs: number;
    exitCode: number | null;
    timedOut: boolean;
    passed: boolean;
    errors: unknown[];
    stdoutPath: string;
    stderrPath: string;
    outputPath: string;
    failureClassification: MaestroFailureClassification | null;
};
export type BoundedCheckoutRepairReport = {
    schemaVersion: 1;
    command: 'repair';
    attemptId: string;
    attemptDirectory: string;
    goalKey: string;
    appId: string;
    platform: 'android';
    startedAt: string;
    endedAt: string;
    durationMs: number;
    monotonicDurationMs: number;
    timeBudgetMs: number;
    status: 'repaired' | 'rejected' | 'failed';
    passed: boolean;
    modelCalls: 0;
    repairAttempts: 0 | 1;
    oneAttemptOnly: true;
    autoCommit: false;
    pullRequest: false;
    affectedFiles: string[];
    proposedPatchPath: string | null;
    previous: {
        buildFingerprint: string;
        flowPath: string;
        flowSha256: string;
        memoryPath: string;
        memorySha256: string;
    };
    diagnosisBuildFingerprint: string;
    buildFingerprintAfter: string | null;
    buildIdentityRecomputed: boolean;
    diagnosis: RepairExecutionReport | null;
    build: Record<string, unknown> | null;
    verification: RepairExecutionReport | null;
    persistence: Record<string, unknown> | null;
    preservation: Record<string, unknown>;
    rejectionReason: string | null;
    error: string | null;
};
export declare class BoundedCheckoutRepairer {
    private readonly options;
    private consumed;
    constructor(options: BoundedCheckoutRepairOptions);
    repair(): Promise<BoundedCheckoutRepairReport>;
}

// policy.d.ts
export declare const CHECKOUT_COMPONENT_PATH = "app/src/main/java/dev/deja/checkout/MainActivity.java";
export declare const CHECKOUT_IDS_PATH = "app/src/main/res/values/ids.xml";
export declare const CHECKOUT_AFFECTED_FILES: readonly ["app/src/main/java/dev/deja/checkout/MainActivity.java", "app/src/main/res/values/ids.xml"];
export type CheckoutRepairPolicy = {
    workspaceRoot: string;
    sourceRoots: string[];
    permittedFileTypes: string[];
    affectedFiles: string[];
};
export type PlannedRepairFile = {
    path: string;
    absolutePath: string;
    before: string;
    after: string;
    beforeSha256: string;
    afterSha256: string;
};
export type CheckoutRepairPlan = {
    policy: CheckoutRepairPolicy;
    workspaceRoot: string;
    files: PlannedRepairFile[];
    diff: string;
};
export declare function createCheckoutRepairDiff(files: Pick<PlannedRepairFile, 'path' | 'before' | 'after'>[]): string;
export declare function planCheckoutSelectorRepair(policy: CheckoutRepairPolicy): Promise<CheckoutRepairPlan>;
export declare function applyCheckoutSelectorRepair(plan: CheckoutRepairPlan): Promise<void>;
export declare function rollbackCheckoutSelectorRepair(plan: CheckoutRepairPlan): Promise<void>;
export declare function checkoutSourceDigests(plan: CheckoutRepairPlan): Promise<any>;
export declare function sourceTreeDigest(workspaceRoot: string, paths: string[]): Promise<{
    [k: string]: string;
}>;
