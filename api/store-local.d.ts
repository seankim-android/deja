// typescript 7.0.2
// entry . (./src/index.ts)
// exports: ConsumerFlowError, ConsumerFlows, ConsumerFlowsOptions, ConsumerPlan, FlowClosureError, InvalidStoreIdError, InvalidSubflowMemoryError, InvalidWalkMemoryError, LOCAL_MEMORY_CONSUMER_SCHEMA_VERSION, LOCAL_MEMORY_PATH, LOCAL_MEMORY_SCHEMA_VERSION, LocalVerifiedSubflowStore, LocalVerifiedSubflowStoreOptions, LocalWalkStore, LocalWalkStoreOptions, LockHolder, LockTestHooks, MAESTRO_FLOW_SCHEMA_VERSION, ManifestChangedError, ManifestLockedError, MissingFlowFileError, StoreLockContentionError, StoreLockUnavailableError, StoreLockedError, VERIFIED_SUBFLOW_INDEX_PATH, WalkMemory, WriterLock, assertionsSha256, checkManifestPath, checkStoredSubflows, closureAssertions, collectFlowClosure, normalizeGoal, planStoredClosure, readConsumerWalkFields, readStoredFlowFiles, snapshotFlowClosure, storeMetadataPath
export declare function assertionsSha256(closure: FlowClosure): string;
export declare function checkManifestPath(path: string): string;
export declare function checkStoredSubflows(root: string, flowPath: string, subflowFiles: StoredFlowFile[] | undefined, legacyClosure?: boolean): Promise<StoredClosureCheck>;
export declare function closureAssertions(closure: FlowClosure): ClosureAssertion[];
export declare function collectFlowClosure(root: string, main: string): Promise<FlowClosure>;
export declare class ConsumerFlowError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export declare class ConsumerFlows {
    readonly manifestPath: string;
    private readonly workspaceRoot;
    private readonly forbiddenValues;
    constructor(options: ConsumerFlowsOptions);
    private canonicalRoot;
    private manifestFile;
    load(): Promise<WalkManifest>;
    private inspect;
    snapshot(candidatePath: string): Promise<CandidateSnapshot>;
    private consumerCandidate;
    lookup(walks: StoredWalk[], identity: NormalizedIdentity, previousSuccessful?: boolean): Promise<LookupHit | LookupMiss>;
    assertUnchanged(walks: StoredWalk[], identity: NormalizedIdentity): Promise<LookupHit | LookupMiss>;
    withLock<T>(work: () => Promise<T>): Promise<T>;
    plan(walks: StoredWalk[], identity: NormalizedIdentity, options: PersistVerifiedOptions): Promise<ConsumerPlan>;
    checkManifest(manifest: WalkManifest): string;
    writeManifest(manifest: WalkManifest): Promise<void>;
}
export type ConsumerFlowsOptions = {
    workspaceRoot: string;
    manifestPath: string;
    forbiddenValues?: string[];
};
export type ConsumerPlan = {
    walk: StoredWalk;
    remaining: StoredWalk[];
    flowPath: string;
    flowSha256: string;
    commit(): Promise<boolean>;
};
export declare class FlowClosureError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export declare class InvalidStoreIdError extends Error {
    constructor(id: string);
}
export declare class InvalidSubflowMemoryError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export declare class InvalidWalkMemoryError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export declare const LOCAL_MEMORY_CONSUMER_SCHEMA_VERSION = 4;
export declare const LOCAL_MEMORY_PATH = ".deja/memory/walks.json";
export declare const LOCAL_MEMORY_SCHEMA_VERSION = 3;
export declare class LocalVerifiedSubflowStore implements VerifiedSubflowStore {
    readonly workspaceRoot: string;
    readonly indexPath: string;
    private readonly renameFile;
    constructor(options: LocalVerifiedSubflowStoreOptions);
    private canonicalRoot;
    private existingFile;
    private loadIndex;
    private atomicWrite;
    private writeIndex;
    persistVerified(options: PersistVerifiedSubflowOptions): Promise<VerifiedSubflow>;
    private storedFlowPath;
    lookup(segmentId: string, identity: SubflowIdentity): Promise<VerifiedSubflowLookup>;
    list(): Promise<VerifiedSubflow[]>;
}
export type LocalVerifiedSubflowStoreOptions = {
    workspaceRoot: string;
    renameFile?: (temporaryPath: string, destinationPath: string) => Promise<void>;
};
export declare class LocalWalkStore implements WalkStore {
    readonly workspaceRoot: string;
    readonly memoryPath: string;
    readonly relativeMemoryPath: string;
    private readonly renameFile;
    readonly manifestPath: string | undefined;
    private readonly consumer;
    private writerLock;
    constructor(options: LocalWalkStoreOptions);
    acquireWriter(): void;
    close(): void;
    private loadMemory;
    private canonicalRoot;
    private existingFile;
    private storedFlowPath;
    private atomicWrite;
    private writeMemory;
    private persistVerified;
    private persistConsumer;
    snapshotCandidate(candidatePath: string, directory: string): Promise<CandidateSnapshot>;
    persistVerifiedCandidate(options: PersistVerifiedOptions): Promise<PersistedWalk>;
    persistVerifiedRepair(options: PersistVerifiedRepairOptions): Promise<PersistedWalk>;
    list(): Promise<StoredWalk[]>;
    lookup(identityInput: WalkIdentity): Promise<LookupHit | LookupMiss>;
    lookupPreviousSuccessful(identityInput: WalkIdentity): Promise<LookupHit | LookupMiss>;
    recordValidation(identityInput: WalkIdentity, validation: WalkValidation): Promise<StoredWalk>;
}
export type LocalWalkStoreOptions = {
    workspaceRoot: string;
    storeId?: string;
    manifest?: {
        path: string;
        forbiddenValues?: string[];
    };
    renameFile?: (temporaryPath: string, destinationPath: string) => Promise<void>;
};
export type LockHolder = {
    version: 1;
    pid: number;
    startedAt: string | null;
    host: string;
    machineId?: string | null;
    token: string;
    acquiredAt: string;
};
export type LockTestHooks = {
    beforeMoveAside?: (path: string) => void;
};
export declare const MAESTRO_FLOW_SCHEMA_VERSION = "maestro/v1";
export declare class ManifestChangedError extends Error {
    constructor();
}
export declare class ManifestLockedError extends Error {
    readonly lockPath: string;
    constructor(lockPath: string);
}
export declare class MissingFlowFileError extends FlowClosureError {
    constructor(path: string);
}
export declare function normalizeGoal(goal: string): string;
export declare function planStoredClosure(closure: FlowClosure, goalSlug: string, platformSlug: string): StoredClosurePlan;
export declare function readConsumerWalkFields(value: {
    flowPath: string;
    closureSha256: unknown;
}): string | null;
export declare function readStoredFlowFiles(value: unknown): StoredFlowFile[] | null;
export declare function snapshotFlowClosure(root: string, candidate: string, directory: string): Promise<CandidateClosureSnapshot>;
export declare class StoreLockContentionError extends StoreLockedError {
    readonly code = "STORE_LOCK_CONTENTION";
    constructor(lockPath: string, holder: LockHolder | null, reason: string);
}
export declare class StoreLockedError extends Error {
    readonly code: string;
    readonly lockPath: string;
    readonly holder: LockHolder | null;
    constructor(lockPath: string, holder: LockHolder | null, reason?: string);
}
export declare class StoreLockUnavailableError extends Error {
    readonly code = "STORE_LOCK_UNAVAILABLE";
    constructor(lockPath: string, cause: unknown);
}
export declare function storeMetadataPath(defaultPath: string, storeId: string | undefined): string;
export declare const VERIFIED_SUBFLOW_INDEX_PATH = ".deja/segments/index.json";
export type WalkMemory = {
    schemaVersion: typeof LOCAL_MEMORY_SCHEMA_VERSION | typeof LOCAL_MEMORY_CONSUMER_SCHEMA_VERSION;
    walks: StoredWalk[];
};
export declare class WriterLock {
    readonly path: string;
    private token;
    private readonly hooks;
    constructor(path: string, hooks?: LockTestHooks);
    get isHeld(): boolean;
    acquire(): void;
    private breakStale;
    release(): void;
    private installExitHook;
}
// referenced
export type CandidateClosureSnapshot = {
    candidatePath: string;
    closureSha256: string;
};
// referenced
export type ClosureAssertion = FlowAssertion & {
    reachedAlways: boolean;
};
// referenced
export type ClosureFile = {
    path: string;
    sha256: string;
    bytes: Buffer;
    kind: 'flow' | 'script';
};
// referenced
export type FlowClosure = {
    main: string;
    files: ClosureFile[];
    sha256: string;
};
// referenced
type NormalizedIdentity = WalkIdentity & {
    goalKey: string;
};
// referenced
export type StoredClosureCheck = {
    status: 'ok';
} | {
    status: 'legacy';
} | {
    status: 'missing';
    path: string;
} | {
    status: 'invalid';
    message: string;
};
// referenced
export type StoredClosurePlan = {
    flowPath: string;
    flowSha256: string;
    writes: {
        path: string;
        bytes: Buffer;
    }[];
    subflowFiles: StoredFlowFile[];
};
