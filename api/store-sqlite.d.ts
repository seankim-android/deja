// typescript 7.0.2
// entry . (./src/index.ts)
// exports: InvalidWalkMemoryError, MAESTRO_FLOW_SCHEMA_VERSION, SQLITE_MEMORY_PATH, SQLITE_MEMORY_SCHEMA_VERSION, SQLiteWalkStore, SQLiteWalkStoreOptions
export declare class InvalidWalkMemoryError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export declare const MAESTRO_FLOW_SCHEMA_VERSION = "maestro/v1";
export declare const SQLITE_MEMORY_PATH = ".deja/memory/walks.sqlite";
export declare const SQLITE_MEMORY_SCHEMA_VERSION = 2;
export declare class SQLiteWalkStore implements WalkStore {
    readonly workspaceRoot: string;
    readonly memoryPath: string;
    private readonly renameFile;
    private readonly replaceMetadata;
    private database;
    constructor(options: SQLiteWalkStoreOptions);
    close(): void;
    private openExisting;
    private openWritable;
    private loadMemory;
    private readWalk;
    private validateUniqueIdentities;
    private commitMetadata;
    private insertWalk;
    private canonicalRoot;
    private existingFile;
    private storedFlowPath;
    private atomicWriteFlow;
    private persistVerified;
    persistVerifiedCandidate(options: PersistVerifiedOptions): Promise<PersistedWalk>;
    persistVerifiedRepair(options: PersistVerifiedRepairOptions): Promise<PersistedWalk>;
    list(): Promise<StoredWalk[]>;
    lookup(identityInput: WalkIdentity): Promise<LookupHit | LookupMiss>;
    lookupPreviousSuccessful(identityInput: WalkIdentity): Promise<LookupHit | LookupMiss>;
    recordValidation(identityInput: WalkIdentity, validation: WalkValidation): Promise<StoredWalk>;
}
export type SQLiteWalkStoreOptions = {
    workspaceRoot: string;
    renameFile?: (temporaryPath: string, destinationPath: string) => Promise<void>;
    replaceMetadata?: (walks: StoredWalk[]) => void;
};
