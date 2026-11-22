import {
  normalizeGoal,
  WALK_RESULT_SCHEMA_VERSION,
  type CacheStatus,
  type ExplorationResult,
  type RepairResult,
  type RunResult,
  type RunReporter,
  type SelectorRepairer,
  type WalkExplorer,
  type WalkIdentity,
  type WalkRunner,
  type WalkStore,
} from './contracts.ts'

type RunStart = Omit<RunResult,
  'endedAt' | 'durationMs' | 'outcome' | 'passed' | 'modelCalls' | 'modelCallBasis' |
  'assertions' | 'repairCount' | 'yamlPath' | 'yamlSha256' | 'failure' | 'artifacts' |
  'diffs' | 'errors'
>

type RunDetails = Pick<RunResult,
  'outcome' | 'passed' | 'modelCalls' | 'modelCallBasis' | 'assertions' | 'repairCount' |
  'yamlPath' | 'yamlSha256' | 'failure' | 'artifacts' | 'diffs' | 'errors'
> & Partial<RunResult>

export type WalkCacheDependencies = {
  store: WalkStore
  runner: WalkRunner
  reporter: RunReporter
  /** Lazy by contract: the factory must not be called until a cold miss or explicit explore. */
  createExplorer: () => WalkExplorer
  /** Lazy by contract: only an eligible stale selector can construct a repairer. */
  createRepairer?: () => SelectorRepairer
  now?: () => Date
  artifactId?: (prefix: 'run' | 'remember') => string
}

export class WalkCache {
  private readonly dependencies: WalkCacheDependencies

  constructor(dependencies: WalkCacheDependencies) {
    this.dependencies = dependencies
  }

  private now() {
    return (this.dependencies.now ?? (() => new Date()))()
  }

  private artifactId(prefix: 'run' | 'remember') {
    return this.dependencies.artifactId?.(prefix) ??
      `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`
  }

  private base(identity: WalkIdentity, command: RunResult['command'], cacheStatus: CacheStatus) {
    return {
      schemaVersion: WALK_RESULT_SCHEMA_VERSION,
      command,
      identity,
      goalKey: normalizeGoal(identity.goal).key,
      cacheStatus,
      startedAt: this.now().toISOString(),
    } as const
  }

  private async finish(
    start: RunStart,
    details: RunDetails,
  ) {
    const endedAt = this.now()
    const result: RunResult = {
      ...start,
      ...details,
      endedAt: endedAt.toISOString(),
      durationMs: Math.max(0, endedAt.getTime() - Date.parse(start.startedAt)),
    }
    await this.dependencies.reporter.report(result)
    return result
  }

  async run(identity: WalkIdentity): Promise<RunResult> {
    const start = this.base(identity, 'run', 'miss')
    // This lookup deliberately precedes both lazy factories. A warm hit cannot construct OpenClaw,
    // an MCP transport, or any model client, even if configuration for those plugins is present.
    const lookup = await this.dependencies.store.lookup(identity)

    if (lookup.status === 'miss') {
      const missStart = { ...start, cacheMissReason: lookup.reason }
      if (lookup.reason === 'stale-build') {
        if (!this.dependencies.createRepairer) {
          return this.finish(missStart, {
            outcome: 'repair-rejected', passed: false, modelCalls: 0,
            modelCallBasis: 'stale-build diagnosis never constructs a model client', assertions: null,
            repairCount: 0, yamlPath: null, yamlSha256: null, failure: null,
            artifacts: [], diffs: [], errors: ['A stale build requires a configured selector repairer; cold exploration was not substituted'],
          })
        }
        const repair = await this.dependencies.createRepairer().repair({ identity })
        return this.finish(missStart, this.repairDetails(repair))
      }
      const exploration = await this.dependencies.createExplorer().explore({ identity })
      return this.finish(missStart, this.explorationDetails(exploration))
    }

    const execution = await this.dependencies.runner.run({
      flowPath: lookup.flowPath,
      artifactDirectory: `.walkcache/reports/${this.artifactId('run')}`,
    })
    const hitStart = { ...start, cacheStatus: 'hit' as const, execution }
    if (execution.passed) {
      await this.dependencies.store.recordValidation(identity, {
        passed: true, at: execution.endedAt,
      })
      return this.finish(hitStart, {
        outcome: 'passed', passed: true, modelCalls: 0,
        modelCallBasis: 'exact local hit: explorer and model factories were not invoked',
        assertions: execution.assertions, repairCount: 0,
        yamlPath: lookup.walk.flowPath, yamlSha256: lookup.walk.flowSha256,
        failure: null, artifacts: execution.artifacts, diffs: [], errors: [],
      })
    }

    if (execution.failure?.category === 'selector' && execution.failure.repairEligible &&
        this.dependencies.createRepairer) {
      // Keep the previous successful validation intact until the bounded repair diagnoses the same
      // unchanged flow. The repair plugin, not core, owns eligibility and all source/write gates.
      const repair = await this.dependencies.createRepairer().repair({ identity })
      return this.finish(hitStart, {
        ...this.repairDetails(repair),
        yamlPath: repair.flowPath ?? lookup.walk.flowPath,
        yamlSha256: repair.flowSha256 ?? lookup.walk.flowSha256,
        assertions: repair.assertions ?? execution.assertions,
        execution,
      })
    }

    await this.dependencies.store.recordValidation(identity, {
      passed: false, at: execution.endedAt,
    })
    const productRegression = execution.failure?.category === 'product_assertion'
    return this.finish(hitStart, {
      outcome: productRegression ? 'product-regression' : 'failed', passed: false,
      modelCalls: 0,
      modelCallBasis: 'exact local hit: explorer and model factories were not invoked',
      assertions: execution.assertions, repairCount: 0,
      yamlPath: lookup.walk.flowPath, yamlSha256: lookup.walk.flowSha256,
      failure: execution.failure, artifacts: execution.artifacts, diffs: [],
      errors: productRegression
        ? ['Meaningful product assertion failed; repair was not attempted']
        : ['Replay failed with a non-repairable or unknown failure; repair was not attempted'],
    })
  }

  async explore(identity: WalkIdentity): Promise<RunResult> {
    const start = this.base(identity, 'explore', 'not-checked')
    const exploration = await this.dependencies.createExplorer().explore({ identity })
    return this.finish(start, this.explorationDetails(exploration))
  }

  async remember(identity: WalkIdentity, candidatePath: string): Promise<RunResult> {
    const start = this.base(identity, 'remember', 'not-checked')
    const execution = await this.dependencies.runner.run({
      flowPath: candidatePath,
      artifactDirectory: `.walkcache/attempts/${this.artifactId('remember')}`,
    })
    if (!execution.passed) {
      return this.finish(start, {
        outcome: 'rejected', passed: false, modelCalls: 0,
        modelCallBasis: 'remember performs independent verification only',
        assertions: execution.assertions, repairCount: 0,
        yamlPath: null, yamlSha256: null, failure: execution.failure,
        artifacts: execution.artifacts, diffs: [],
        errors: ['Candidate verification failed; successful memory was not replaced'], execution,
      })
    }
    const persisted = await this.dependencies.store.persistVerifiedCandidate({
      identity, candidatePath, validation: { passed: true, at: execution.endedAt }, origin: 'remember',
    })
    return this.finish(start, {
      outcome: 'remembered', passed: true, modelCalls: 0,
      modelCallBasis: 'remember performs independent verification only',
      assertions: execution.assertions, repairCount: 0,
      yamlPath: persisted.flowPath, yamlSha256: persisted.flowSha256,
      failure: null,
      artifacts: [
        ...execution.artifacts,
        { kind: 'yaml', path: persisted.flowPath, sha256: persisted.flowSha256 },
        { kind: 'memory', path: persisted.storedMetadataPath },
      ],
      diffs: [], errors: [], execution,
    })
  }

  private explorationDetails(exploration: ExplorationResult): RunDetails {
    return {
      outcome: exploration.passed ? 'cold-recorded' : 'exploration-rejected',
      passed: exploration.passed,
      modelCalls: exploration.modelCalls,
      modelCallBasis: exploration.modelCallBasis,
      assertions: exploration.assertions,
      repairCount: 0,
      yamlPath: exploration.flowPath,
      yamlSha256: exploration.flowSha256,
      failure: exploration.failure,
      artifacts: exploration.artifacts,
      diffs: [],
      errors: exploration.errors,
      exploration,
    }
  }

  private repairDetails(repair: RepairResult): RunDetails {
    return {
      outcome: repair.passed ? 'repaired' : repair.status === 'rejected' ? 'repair-rejected' : 'failed',
      passed: repair.passed,
      modelCalls: repair.modelCalls,
      modelCallBasis: 'bounded selector repair uses the verified deterministic policy only',
      assertions: repair.assertions,
      repairCount: repair.repairCount,
      yamlPath: repair.flowPath,
      yamlSha256: repair.flowSha256,
      failure: repair.failure,
      artifacts: repair.artifacts,
      diffs: repair.diffs,
      errors: repair.errors,
      repair,
    }
  }
}
