// index.d.ts
export { runWalkStoreContractTests, contractIdentity, initialValidation, laterValidation, type MetadataTamper, type StoreFailureInjection, type WalkStoreContractImplementation, } from './walk-store-contract.ts';
export { localStoreContract } from './local-adapter.ts';
export { sqliteStoreContract } from './sqlite-adapter.ts';

// local-adapter.d.ts
import type { WalkStoreContractImplementation } from './walk-store-contract.ts';
export declare const localStoreContract: WalkStoreContractImplementation;

// sqlite-adapter.d.ts
import type { WalkStoreContractImplementation } from './walk-store-contract.ts';
export declare const sqliteStoreContract: WalkStoreContractImplementation;

// walk-store-contract.d.ts
import type { WalkIdentity, WalkStore } from '@deja-qa/core';
export declare const initialValidation = "2026-11-22T00:00:00.000Z";
export declare const laterValidation = "2026-11-22T00:01:00.000Z";
export declare function contractIdentity(overrides?: Partial<WalkIdentity>): WalkIdentity;
export type StoreFailureInjection = {
    failFlowRename?: boolean;
    failMetadataReplacement?: boolean;
};
export type MetadataTamper = 'malformed' | 'unsupported-schema' | 'invalid-entry' | 'duplicate-identity';
/** Backend-specific setup and tamper hooks; every behavioral assertion below is shared verbatim. */
export type WalkStoreContractImplementation = {
    name: string;
    metadataPath(workspaceRoot: string): string;
    createStore(workspaceRoot: string, failures?: StoreFailureInjection): WalkStore;
    closeStore(store: WalkStore): void | Promise<void>;
    snapshotMetadata(workspaceRoot: string): Promise<Buffer>;
    tamperMetadata(workspaceRoot: string, kind: MetadataTamper): Promise<void>;
    setFirstFlowPath(workspaceRoot: string, flowPath: string): Promise<void>;
};
export declare function runWalkStoreContractTests(implementation: WalkStoreContractImplementation): void;
