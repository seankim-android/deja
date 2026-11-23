// typescript 7.0.2
// entry ./reporter (./src/reporter.ts)
// exports: JsonRunReporter, readRunReport
export declare class JsonRunReporter implements RunReporter {
    private readonly workspaceRoot;
    private readonly redact;
    private lastReceipt;
    constructor(workspaceRoot: string, redact: Redactor);
    get receipt(): ReportReceipt | null;
    report(result: RunResult): Promise<ReportReceipt>;
}
export declare function readRunReport(workspaceRoot: string, reportPath: string, redact: Redactor): Promise<RunResult>;
// referenced
export type Redactor = (text: string) => string;

// entry ./redaction (./src/redaction.ts)
// exports: Redactor, createRedactor, redactJson
export declare function createRedactor(workspaceRoot: string, fixtureValues: string[]): Redactor;
export declare function redactJson<T>(value: T, redact: Redactor): T;
export type Redactor = (text: string) => string;
