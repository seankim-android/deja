// typescript 7.0.2
// entry . (./src/index.ts)
// exports: FlowClosureError, InvalidSubflowMemoryError, InvalidWalkMemoryError, LOCAL_MEMORY_PATH, LOCAL_MEMORY_SCHEMA_VERSION, LocalVerifiedSubflowStore, LocalVerifiedSubflowStoreOptions, LocalWalkStore, LocalWalkStoreOptions, MAESTRO_FLOW_SCHEMA_VERSION, VERIFIED_SUBFLOW_INDEX_PATH, WalkMemory, checkStoredSubflows, collectFlowClosure, normalizeGoal, planStoredClosure, readStoredFlowFiles, snapshotFlowClosure
export declare function checkStoredSubflows(root: string, flowPath: string, subflowFiles: StoredFlowFile[] | undefined, legacyClosure?: boolean): Promise<StoredClosureCheck>;
export declare function collectFlowClosure(root: string, main: string): Promise<FlowClosure>;
export declare class FlowClosureError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export declare class InvalidSubflowMemoryError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export declare class InvalidWalkMemoryError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
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
    private readonly renameFile;
    constructor(options: LocalWalkStoreOptions);
    private loadMemory;
    private canonicalRoot;
    private existingFile;
    private storedFlowPath;
    private atomicWrite;
    private writeMemory;
    private persistVerified;
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
    renameFile?: (temporaryPath: string, destinationPath: string) => Promise<void>;
};
export declare const MAESTRO_FLOW_SCHEMA_VERSION = "maestro/v1";
export declare function normalizeGoal(goal: string): string;
export declare function planStoredClosure(closure: FlowClosure, goalSlug: string, platformSlug: string): StoredClosurePlan;
export declare function readStoredFlowFiles(value: unknown): StoredFlowFile[] | null;
export declare function snapshotFlowClosure(root: string, candidate: string, directory: string): Promise<CandidateClosureSnapshot>;
export declare const VERIFIED_SUBFLOW_INDEX_PATH = ".deja/segments/index.json";
export type WalkMemory = {
    schemaVersion: typeof LOCAL_MEMORY_SCHEMA_VERSION;
    walks: StoredWalk[];
};
// referenced
export type CandidateClosureSnapshot = {
    candidatePath: string;
    closureSha256: string;
};
// referenced
export type ClosureFile = {
    path: string;
    sha256: string;
    bytes: Buffer;
};
// referenced
export type FlowClosure = {
    main: string;
    files: ClosureFile[];
    sha256: string;
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
