// typescript 7.0.2
// entry . (./src/index.ts)
// exports: ColdExploreHooks, ColdExploreOptions, ColdExploreReport, ExplorationMetrics, ExploreConfiguration, FixtureCredential, MaestroVerificationRunner, OpenClawEnvelope, OpenClawWalkExplorer, OpenClawWalkExplorerOptions, PersistenceDetails, VerificationDetails, explorerPrompt, extractCandidateYaml, loadExploreConfiguration, openClawConfig, runColdExploration, validateCandidate
export type ColdExploreHooks = {
    clearAppState: (phase: 'exploration' | 'verification') => Promise<void>;
    verifyCandidate: (candidatePath: string, attemptDirectory: string) => Promise<VerificationDetails>;
    persistVerified: (candidatePath: string, verification: VerificationDetails, metrics: ExplorationMetrics) => Promise<PersistenceDetails>;
};
export type ColdExploreOptions = {
    goal: string;
    configuration: ExploreConfiguration;
    buildFingerprint: string;
    attemptDirectory: string;
    openClawExecutable: string;
    maestroMcpProxy: string;
    maestroMcpEnvironment: Record<string, string>;
};
export type ColdExploreReport = {
    goalKey: string;
    goal: string;
    appId: string;
    platform: 'android';
    buildFingerprint: string;
    buildPath: string;
    workspaceRoot: string;
    cacheStatus: 'recorded' | 'rejected' | 'error';
    passed: boolean;
    durationMs: number;
    explorationDurationMs: number | null;
    verificationDurationMs: number | null;
    mcpCalls: number | null;
    mcpCallBasis: string;
    modelCalls: number | null;
    modelCallBasis: string;
    model: string;
    provider: string | null;
    explorerExitCode: number | null;
    attemptDirectory: string;
    candidatePath: string | null;
    activityPath: string;
    mcpActivityPath: string;
    openClawResultPath: string | null;
    flowPath: string | null;
    flowSha256: string | null;
    transport: 'maestro-mcp-stdio';
    accessPolicy: {
        allowedTools: string[];
        referenceFlow: 'denied';
        appSource: 'denied';
        candidateDestination: 'attempt-only';
    };
    cleanState: {
        exploration: boolean;
        independentVerification: boolean;
        candidateClearStateLaunch: boolean;
    };
    verification: VerificationDetails | null;
    persistence: PersistenceDetails | null;
    errors: string[];
};
export type ExplorationMetrics = {
    explorationDurationMs: number | null;
    verificationDurationMs: number | null;
    mcpCalls: number | null;
    modelCalls: number | null;
    modelCallBasis: string;
};
export type ExploreConfiguration = {
    workspaceRoot: string;
    appId: string;
    platform: 'android';
    buildPath: string;
    device: string;
    fixtureCredentials: FixtureCredential[];
    openClawModel: string;
    openClawBaseUrl: string;
    explorationTimeoutSeconds: number;
    verificationTimeoutMs: number;
};
export declare function explorerPrompt(goal: string, configuration: ExploreConfiguration): string;
export declare function extractCandidateYaml(finalText: string): string;
export type FixtureCredential = {
    label: string;
    value: string;
};
export declare function loadExploreConfiguration(configPath: string): ExploreConfiguration;
export type MaestroVerificationRunner = Pick<MaestroWalkRunner, 'executable' | 'runFlow' | 'adapt'>;
export declare function openClawConfig(options: ColdExploreOptions): {
    models: {
        providers: {
            [x: string]: {
                baseUrl: string;
                api: string;
                models: {
                    id: string;
                    name: string;
                    reasoning: boolean;
                    input: string[];
                    contextWindow: number;
                    maxTokens: number;
                    compat: {
                        supportsTools: boolean;
                    };
                }[];
            };
        };
    };
    agents: {
        defaults: {
            workspace: string;
            skills: never[];
            contextInjection: string;
        };
    };
    tools: {
        allow: string[];
        fs: {
            workspaceOnly: boolean;
        };
        codeMode: {
            enabled: boolean;
        };
    };
    mcp: {
        servers: {
            maestro: {
                command: string;
                args: string[];
                cwd: string;
                transport: string;
                connectionTimeoutMs: number;
                requestTimeoutMs: number;
                toolFilter: {
                    include: string[];
                };
                env: {
                    DEJA_MCP_LOG: string;
                };
            };
        };
    };
};
export type OpenClawEnvelope = {
    ok?: boolean;
    status?: string;
    final?: string;
    assistantTurns?: number;
    model?: string | null;
    provider?: string | null;
    sessionId?: string;
    error?: {
        message?: string;
        kind?: string;
    };
};
export declare class OpenClawWalkExplorer implements WalkExplorer {
    private readonly options;
    constructor(options: OpenClawWalkExplorerOptions);
    explore(request: ExploreRequest): Promise<ExplorationResult>;
}
export type OpenClawWalkExplorerOptions = {
    configuration: ExploreConfiguration;
    buildFingerprint: string;
    openClawExecutable: string;
    maestroMcpProxy: string;
    runner: MaestroVerificationRunner;
    store: WalkStore;
};
export type PersistenceDetails = {
    flowPath: string;
    flowSha256: string;
    [key: string]: unknown;
};
export declare function runColdExploration(options: ColdExploreOptions, hooks: ColdExploreHooks): Promise<ColdExploreReport>;
export declare function validateCandidate(candidate: string, appId: string): void;
export type VerificationDetails = {
    passed: boolean;
    [key: string]: unknown;
};
