// typescript 7.0.2
// entry . (./src/index.ts)
// exports: InvalidSubflowMemoryError, InvalidWalkMemoryError, LOCAL_MEMORY_PATH, LOCAL_MEMORY_SCHEMA_VERSION, LocalVerifiedSubflowStore, LocalVerifiedSubflowStoreOptions, LocalWalkStore, LocalWalkStoreOptions, MAESTRO_FLOW_SCHEMA_VERSION, VERIFIED_SUBFLOW_INDEX_PATH, WalkMemory, normalizeGoal
export declare class InvalidSubflowMemoryError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export declare class InvalidWalkMemoryError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export declare const LOCAL_MEMORY_PATH = ".deja/memory/walks.json";
export declare const LOCAL_MEMORY_SCHEMA_VERSION = 2;
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
export declare const VERIFIED_SUBFLOW_INDEX_PATH = ".deja/segments/index.json";
export type WalkMemory = {
    schemaVersion: typeof LOCAL_MEMORY_SCHEMA_VERSION;
    walks: StoredWalk[];
};
