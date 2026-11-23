// typescript 7.0.2
// entry . (./src/index.ts)
// exports: ContractExpect, ContractHarness, ContractScene, ExpectResult, ExplorationDriverContractImplementation, Matchers, MetadataTamper, PortableStoreImplementation, StoreFailureInjection, WalkStoreContractImplementation, contractIdentity, initialValidation, laterValidation, localStoreContract, resolveHarness, runConsumerFlowContractTests, runExplorationDriverContractTests, runGateContractTests, runGateStoreContractTests, runPortableWalkStoreContractTests, runSqliteDriverContractTests, runWalkStoreContractTests, runWriterLockContractTests, sqliteStoreContract
export type ContractExpect = {
    (actual: unknown, message?: string): ExpectResult;
    stringMatching(pattern: RegExp): unknown;
};
export type ContractHarness = {
    describe(name: string, body: () => void): void;
    test(name: string, body: () => void | Promise<void>): void;
    expect: ContractExpect;
};
export declare function contractIdentity(overrides?: Partial<WalkIdentity>): WalkIdentity;
export type ContractScene = {
    ids: string[];
    labels?: string[];
    ghost?: string[];
    keyboard?: boolean;
    extra?: number;
};
export type ExpectResult = Matchers & {
    not: Matchers;
    rejects: Matchers;
    resolves: Matchers;
};
export type ExplorationDriverContractImplementation = {
    name: string;
    appId: string;
    backIds: string[];
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
export type Matchers = {
    toBe(expected: unknown): void;
    toEqual(expected: unknown): void;
    toMatchObject(expected: unknown): void;
    toHaveLength(length: number): void;
    toContain(item: unknown): void;
    toMatch(pattern: RegExp): void;
    toThrow(expected?: string | RegExp | (new (...args: never[]) => unknown)): void;
    toBeInstanceOf(constructor: abstract new (...args: never[]) => unknown): void;
    toBeGreaterThan(value: number): void;
    toBeLessThan(value: number): void;
    toBeUndefined(): void;
    toBeDefined(): void;
    toBeNull(): void;
};
export type MetadataTamper = 'malformed' | 'unsupported-schema' | 'invalid-entry' | 'duplicate-identity';
export type PortableStoreImplementation = {
    name: string;
    createStore(workspaceRoot: string): WalkStore;
    closeStore?(store: WalkStore): void | Promise<void>;
};
export declare function resolveHarness(harness?: ContractHarness): ContractHarness;
export declare function runConsumerFlowContractTests(implementation: WalkStoreContractImplementation, harness?: ContractHarness): void;
export declare function runExplorationDriverContractTests(implementation: ExplorationDriverContractImplementation, harness?: ContractHarness): void;
export declare function runGateContractTests(implementation: PortableStoreImplementation, harness?: ContractHarness): void;
export declare function runGateStoreContractTests(implementation: WalkStoreContractImplementation, harness?: ContractHarness): void;
export declare function runPortableWalkStoreContractTests(implementation: PortableStoreImplementation, harness?: ContractHarness): void;
export declare function runSqliteDriverContractTests(harness?: ContractHarness): void;
export declare function runWalkStoreContractTests(implementation: WalkStoreContractImplementation, harness?: ContractHarness): void;
export declare function runWriterLockContractTests(implementation: WalkStoreContractImplementation, harness?: ContractHarness): void;
export declare const sqliteStoreContract: WalkStoreContractImplementation;
export type StoreFailureInjection = {
    failFlowRename?: boolean;
    failMetadataReplacement?: boolean;
};
export type WalkStoreContractImplementation = {
    name: string;
    kind: 'local' | 'sqlite';
    metadataPath(workspaceRoot: string, storeId?: string): string;
    createStore(workspaceRoot: string, failures?: StoreFailureInjection, storeId?: string, manifest?: {
        path: string;
        forbiddenValues?: string[];
        flowsDir?: string;
    }): WalkStore;
    closeStore(store: WalkStore): void | Promise<void>;
    snapshotMetadata(workspaceRoot: string): Promise<Buffer>;
    tamperMetadata(workspaceRoot: string, kind: MetadataTamper): Promise<void>;
    setFirstFlowPath(workspaceRoot: string, flowPath: string): Promise<void>;
    editSubflowMetadata(workspaceRoot: string, kind: 'strip' | 'empty'): Promise<void>;
    downgradeToSchema2(workspaceRoot: string): Promise<void>;
};

// entry ./node (./src/node-binding.ts)
// exports: nodeHarness
export declare const nodeHarness: ContractHarness;
// referenced
export type ContractExpect = {
    (actual: unknown, message?: string): ExpectResult;
    stringMatching(pattern: RegExp): unknown;
};
// referenced
export type ContractHarness = {
    describe(name: string, body: () => void): void;
    test(name: string, body: () => void | Promise<void>): void;
    expect: ContractExpect;
};
// referenced
export type ExpectResult = Matchers & {
    not: Matchers;
    rejects: Matchers;
    resolves: Matchers;
};
// referenced
export type Matchers = {
    toBe(expected: unknown): void;
    toEqual(expected: unknown): void;
    toMatchObject(expected: unknown): void;
    toHaveLength(length: number): void;
    toContain(item: unknown): void;
    toMatch(pattern: RegExp): void;
    toThrow(expected?: string | RegExp | (new (...args: never[]) => unknown)): void;
    toBeInstanceOf(constructor: abstract new (...args: never[]) => unknown): void;
    toBeGreaterThan(value: number): void;
    toBeLessThan(value: number): void;
    toBeUndefined(): void;
    toBeDefined(): void;
    toBeNull(): void;
};
