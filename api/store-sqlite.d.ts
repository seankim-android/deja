// index.d.ts
import { type LookupHit, type LookupMiss, type PersistedWalk, type PersistVerifiedOptions, type PersistVerifiedRepairOptions, type StoredWalk, type WalkIdentity, type WalkStore, type WalkValidation } from '@deja-qa/core';
export { type ExplorationDetails, type LookupHit, type LookupMiss, type PersistedWalk, type PersistVerifiedOptions, type PersistVerifiedRepairOptions, type RepairProvenance, type StoredWalk, type WalkIdentity, type WalkValidation, } from '@deja-qa/core';
export declare const SQLITE_MEMORY_SCHEMA_VERSION = 2;
export declare const MAESTRO_FLOW_SCHEMA_VERSION = "maestro/v1";
export declare const SQLITE_MEMORY_PATH = ".deja/memory/walks.sqlite";
export type SQLiteWalkStoreOptions = {
    workspaceRoot: string;
    /** Test-only deterministic filesystem failure injection; production uses fs.rename. */
    renameFile?: (temporaryPath: string, destinationPath: string) => Promise<void>;
    /** Test-only transaction failure injection; production uses one IMMEDIATE SQLite transaction. */
    replaceMetadata?: (walks: StoredWalk[]) => void;
};
export declare class InvalidWalkMemoryError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
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
