// typescript 7.0.2
// entry . (./src/index.ts)
// exports: ContractScene, ExplorationDriverContractImplementation, MetadataTamper, StoreFailureInjection, WalkStoreContractImplementation, contractIdentity, initialValidation, laterValidation, localStoreContract, runExplorationDriverContractTests, runWalkStoreContractTests, sqliteStoreContract
export declare function contractIdentity(overrides?: Partial<WalkIdentity>): WalkIdentity;
export type ContractScene = {
    ids: string[];
    labels?: string[];
    keyboard?: boolean;
    extra?: number;
};
export type ExplorationDriverContractImplementation = {
    name: string;
    appId: string;
    fingerprint: FingerprintConfig;
    create(scene: ContractScene): {
        driver: ExplorationDriver;
        relaunches(): number;
        failNextAct(): void;
    };
};
export declare const initialValidation = "2026-11-22T00:00:00.000Z";
export declare const laterValidation = "2026-11-22T00:01:00.000Z";
export declare const localStoreContract: WalkStoreContractImplementation;
export type MetadataTamper = 'malformed' | 'unsupported-schema' | 'invalid-entry' | 'duplicate-identity';
export declare function runExplorationDriverContractTests(implementation: ExplorationDriverContractImplementation): void;
export declare function runWalkStoreContractTests(implementation: WalkStoreContractImplementation): void;
export declare const sqliteStoreContract: WalkStoreContractImplementation;
export type StoreFailureInjection = {
    failFlowRename?: boolean;
    failMetadataReplacement?: boolean;
};
export type WalkStoreContractImplementation = {
    name: string;
    metadataPath(workspaceRoot: string): string;
    createStore(workspaceRoot: string, failures?: StoreFailureInjection): WalkStore;
    closeStore(store: WalkStore): void | Promise<void>;
    snapshotMetadata(workspaceRoot: string): Promise<Buffer>;
    tamperMetadata(workspaceRoot: string, kind: MetadataTamper): Promise<void>;
    setFirstFlowPath(workspaceRoot: string, flowPath: string): Promise<void>;
    editSubflowMetadata(workspaceRoot: string, kind: 'strip' | 'empty'): Promise<void>;
    downgradeToSchema2(workspaceRoot: string): Promise<void>;
};
