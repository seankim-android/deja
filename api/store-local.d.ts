// index.d.ts
import { type LookupHit, type LookupMiss, type PersistedWalk, type PersistVerifiedOptions, type PersistVerifiedRepairOptions, type StoredWalk, type WalkIdentity, type WalkStore, type WalkValidation } from '@deja-qa/core';
export { type ExplorationDetails, type LookupHit, type LookupMiss, type PersistedWalk, type PersistVerifiedOptions, type PersistVerifiedRepairOptions, type RepairProvenance, type StoredWalk, type WalkIdentity, type WalkValidation, } from '@deja-qa/core';
export declare const LOCAL_MEMORY_SCHEMA_VERSION = 2;
export declare const MAESTRO_FLOW_SCHEMA_VERSION = "maestro/v1";
export declare const LOCAL_MEMORY_PATH = ".deja/memory/walks.json";
export type WalkMemory = {
    schemaVersion: typeof LOCAL_MEMORY_SCHEMA_VERSION;
    walks: StoredWalk[];
};
export type LocalWalkStoreOptions = {
    workspaceRoot: string;
    /** Injectable only to model filesystem failures deterministically; production uses fs.rename. */
    renameFile?: (temporaryPath: string, destinationPath: string) => Promise<void>;
};
export declare class InvalidWalkMemoryError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export declare function normalizeGoal(goal: string): string;
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
    /**
     * Select a previous successful flow only for an explicitly requested repair diagnosis. This is
     * never a warm-cache hit: the caller must execute and classify it before making any source change.
     */
    lookupPreviousSuccessful(identityInput: WalkIdentity): Promise<LookupHit | LookupMiss>;
    recordValidation(identityInput: WalkIdentity, validation: WalkValidation): Promise<StoredWalk>;
}
export { LocalVerifiedSubflowStore, VERIFIED_SUBFLOW_INDEX_PATH, InvalidSubflowMemoryError, type LocalVerifiedSubflowStoreOptions, } from './verified-subflow-store.ts';
export { VERIFIED_SUBFLOW_SCHEMA_VERSION, validateSubflowContract, type CompositionModelMetrics, type CompositionStepMetrics, type PersistVerifiedSubflowOptions, type SubflowContract, type SubflowIdentity, type SubflowInput, type SubflowOutput, type VerifiedSubflow, type VerifiedSubflowLookup, type VerifiedSubflowStore, } from '@deja-qa/core';

// verified-subflow-store.d.ts
import { type PersistVerifiedSubflowOptions, type SubflowIdentity, type VerifiedSubflow, type VerifiedSubflowLookup, type VerifiedSubflowStore } from '@deja-qa/core';
export declare const VERIFIED_SUBFLOW_INDEX_PATH = ".deja/segments/index.json";
export declare class InvalidSubflowMemoryError extends Error {
    constructor(message: string, options?: ErrorOptions);
}
export type LocalVerifiedSubflowStoreOptions = {
    workspaceRoot: string;
    /** Test-only injection; production uses fs.rename. */
    renameFile?: (temporaryPath: string, destinationPath: string) => Promise<void>;
};
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
