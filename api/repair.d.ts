// typescript 7.0.2
// entry . (./src/index.ts)
// exports: BoundedCheckoutRepairOptions, BoundedCheckoutRepairReport, BoundedCheckoutRepairer, BoundedSelectorRepairer, BoundedSelectorRepairerOptions, CHECKOUT_AFFECTED_FILES, CHECKOUT_COMPONENT_PATH, CHECKOUT_IDS_PATH, CheckoutRepairPlan, CheckoutRepairPolicy, PlannedRepairFile, RepairBuildInstallOptions, RepairBuildOptions, RepairBuildResult, RepairClock, RepairExecutionReport, createCheckoutRepairDiff, planCheckoutSelectorRepair, runCheckoutBuildAndInstall, runCheckoutGradleBuild
export declare class BoundedCheckoutRepairer {
    private readonly options;
    private consumed;
    constructor(options: BoundedCheckoutRepairOptions);
    repair(): Promise<BoundedCheckoutRepairReport>;
}
export type BoundedCheckoutRepairOptions = {
    workspaceRoot: string;
    goal: string;
    appId: string;
    platform: 'android';
    timeoutMs: number;
    clock?: RepairClock;
    policy: CheckoutRepairPolicy;
    attemptId?: string;
    store: WalkStore;
    memoryPath: string;
    fingerprint: () => Promise<string>;
    runFlow: (flowPath: string, artifactDirectory: string, timeoutMs: number) => Promise<MaestroRunResult>;
    build: (timeoutMs: number) => Promise<RepairBuildResult>;
    displayPatch?: (diff: string, affectedFiles: string[]) => void;
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
export declare class BoundedSelectorRepairer implements SelectorRepairer {
    private readonly options;
    constructor(options: BoundedSelectorRepairerOptions);
    repair(request: RepairRequest): Promise<RepairResult>;
}
export type BoundedSelectorRepairerOptions = {
    workspaceRoot: string;
    policy: CheckoutRepairPolicy;
    timeoutMs: number;
    apkPath: string;
    device: string;
    store: WalkStore;
    memoryPath: string;
    runner: MaestroWalkRunner;
    fingerprint: () => Promise<string>;
    androidSdkRoot?: string;
    javaHome?: string;
    displayPatch?: (diff: string, affectedFiles: string[]) => void;
};
export declare const CHECKOUT_AFFECTED_FILES: readonly ["app/src/main/java/dev/deja/checkout/MainActivity.java", "app/src/main/res/values/ids.xml"];
export declare const CHECKOUT_COMPONENT_PATH = "app/src/main/java/dev/deja/checkout/MainActivity.java";
export declare const CHECKOUT_IDS_PATH = "app/src/main/res/values/ids.xml";
export type CheckoutRepairPlan = {
    policy: CheckoutRepairPolicy;
    workspaceRoot: string;
    files: PlannedRepairFile[];
    diff: string;
};
export type CheckoutRepairPolicy = {
    workspaceRoot: string;
    sourceRoots: string[];
    permittedFileTypes: string[];
    affectedFiles: string[];
};
export declare function createCheckoutRepairDiff(files: Pick<PlannedRepairFile, 'path' | 'before' | 'after'>[]): string;
export declare function planCheckoutSelectorRepair(policy: CheckoutRepairPolicy): Promise<CheckoutRepairPlan>;
export type PlannedRepairFile = {
    path: string;
    absolutePath: string;
    before: string;
    after: string;
    beforeSha256: string;
    afterSha256: string;
};
export type RepairBuildInstallOptions = RepairBuildOptions & {
    device: string;
    apkPath: string;
    adbExecutable?: string;
};
export type RepairBuildOptions = {
    workspaceRoot: string;
    timeoutMs: number;
    javaHome?: string;
    androidSdkRoot?: string;
    gradleExecutable?: string;
};
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
export type RepairClock = {
    now: () => number;
    schedule: (run: () => void, delayMs: number) => () => void;
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
export declare function runCheckoutBuildAndInstall(options: RepairBuildInstallOptions): Promise<RepairBuildResult>;
export declare function runCheckoutGradleBuild(options: RepairBuildOptions): Promise<RepairBuildResult>;
