import {
  ExpectedFlowChecksumMismatchError,
  normalizeGoal,
  WALK_RESULT_SCHEMA_VERSION,
  type CacheStatus,
  type ExplorationResult,
  type PersistedWalk,
  type RepairResult,
  type RunResult,
  type RunReporter,
  type SelectorRepairer,
  type SemanticRetrievalOptions,
  type StaleBuildPolicy,
  type StoredWalk,
  type WalkExplorer,
  type WalkIdentity,
  type WalkRunner,
  type WalkStore,
} from './contracts.ts'
import { ExactFirstWalkRetriever } from './semantic.ts'

type RunStart = Omit<RunResult,
  'endedAt' | 'durationMs' | 'outcome' | 'passed' | 'modelCalls' | 'modelCallBasis' |
  'assertions' | 'repairCount' | 'yamlPath' | 'yamlSha256' | 'failure' | 'artifacts' |
  'diffs' | 'errors'
>

type RunDetails = Pick<RunResult,
  'outcome' | 'passed' | 'modelCalls' | 'modelCallBasis' | 'assertions' | 'repairCount' |
  'yamlPath' | 'yamlSha256' | 'failure' | 'artifacts' | 'diffs' | 'errors'
> & Partial<RunResult>

export type DejaDependencies = {
  store: WalkStore
  runner: WalkRunner
  reporter: RunReporter
  /** Lazy by contract: the factory must not be called until a cold miss or explicit explore. */
  createExplorer: () => WalkExplorer
  /** Lazy by contract: only an eligible stale selector can construct a repairer. */
  createRepairer?: () => SelectorRepairer
  /** Optional WALK-016 proof. Its provider factory is lazy and exact hits never invoke it. */
  semantic?: SemanticRetrievalOptions
  /**
   * Opt-in WALK-021 policy, used only on a stale-build miss with no repairer. Absent means the
   * stale build is rejected as before. It never explores, repairs, or constructs a lazy factory.
   */
  staleBuild?: StaleBuildPolicy
  now?: () => Date
  artifactId?: (prefix: 'run' | 'remember') => string
}

export class Deja {
  private readonly dependencies: DejaDependencies

  constructor(dependencies: DejaDependencies) {
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
    // Exact lookup deliberately precedes every lazy factory, including semantic embeddings. A warm
    // hit cannot construct OpenClaw, an MCP transport, an embedding client, or a repairer.
    const retrieval = await new ExactFirstWalkRetriever({
      store: this.dependencies.store,
      ...(this.dependencies.semantic ? { semantic: this.dependencies.semantic } : {}),
    }).lookup(identity)

    if (retrieval.status === 'miss') {
      const missStart = {
        ...start, cacheMissReason: retrieval.miss.reason, semantic: retrieval.semantic,
      }
      if (retrieval.semantic.reason === 'adversarial-goal') {
        return this.finish(missStart, {
          outcome: 'exploration-rejected', passed: false, modelCalls: 0,
          modelCallBasis: 'adversarial goal marker rejected before explorer construction',
          assertions: null, repairCount: 0, yamlPath: null, yamlSha256: null,
          failure: null, artifacts: [], diffs: [],
          errors: ['Semantic goal safety check rejected the request; cold exploration was not attempted'],
        })
      }
      if (retrieval.miss.reason === 'stale-build') {
        if (!this.dependencies.createRepairer &&
            this.dependencies.staleBuild === 'replay-last-verified') {
          const replayed = await this.replayLastVerified(identity, missStart)
          if (replayed) return replayed
        }
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

    let lookup = retrieval.lookup
    const selectedIdentity = retrieval.matchType === 'semantic'
      ? walkIdentity(lookup.walk)
      : identity
    let semanticReport = retrieval.semantic
    if (retrieval.matchType === 'semantic') {
      // Provider calls above may await arbitrary work. Revalidate the winner after all those calls
      // and immediately before replay so a stale checksum/latest pass cannot execute changed bytes.
      const revalidated = await this.dependencies.store.lookup(selectedIdentity)
      if (revalidated.status === 'miss') {
        semanticReport = {
          ...semanticReport, status: 'miss', reason: 'no-eligible-candidates',
          eligibleCandidates: 0, selectedWalk: null,
        }
        return this.finish({
          ...start, cacheMissReason: revalidated.reason, semantic: semanticReport,
        }, {
          outcome: 'failed', passed: false, modelCalls: 0,
          modelCallBasis: 'semantic candidate changed before replay; explorer and runner were not invoked',
          assertions: null, repairCount: 0, yamlPath: null, yamlSha256: null,
          failure: null, artifacts: [], diffs: [],
          errors: ['Semantic candidate changed before replay; no flow was executed or explored'],
        })
      }
      lookup = revalidated
      semanticReport = {
        ...semanticReport,
        selectedWalk: {
          goal: lookup.walk.goal,
          goalKey: lookup.walk.goalKey,
          flowPath: lookup.walk.flowPath,
          flowSha256: lookup.walk.flowSha256,
        },
      }
    }
    const execution = await this.dependencies.runner.run({
      flowPath: lookup.flowPath,
      artifactDirectory: `.deja/reports/${this.artifactId('run')}`,
    })
    const hitStart = {
      ...start, cacheStatus: 'hit' as const, execution, semantic: semanticReport,
    }
    const modelCallBasis = retrieval.matchType === 'exact'
      ? 'exact local hit: explorer and model factories were not invoked'
      : 'semantic hit: embedding calls reported separately; stored YAML replayed and explorer not invoked'
    if (execution.passed) {
      await this.dependencies.store.recordValidation(selectedIdentity, {
        passed: true, at: execution.endedAt,
      })
      return this.finish(hitStart, {
        outcome: 'passed', passed: true, modelCalls: 0,
        modelCallBasis,
        assertions: execution.assertions, repairCount: 0,
        yamlPath: lookup.walk.flowPath, yamlSha256: lookup.walk.flowSha256,
        failure: null, artifacts: execution.artifacts, diffs: [], errors: [],
      })
    }

    if (retrieval.matchType === 'exact' &&
        execution.failure?.category === 'selector' && execution.failure.repairEligible &&
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

    await this.dependencies.store.recordValidation(selectedIdentity, {
      passed: false, at: execution.endedAt,
    })
    const productRegression = execution.failure?.category === 'product_assertion'
    return this.finish(hitStart, {
      outcome: productRegression ? 'product-regression' : 'failed', passed: false,
      modelCalls: 0,
      modelCallBasis,
      assertions: execution.assertions, repairCount: 0,
      yamlPath: lookup.walk.flowPath, yamlSha256: lookup.walk.flowSha256,
      failure: execution.failure, artifacts: execution.artifacts, diffs: [],
      errors: productRegression
        ? ['Meaningful product assertion failed; repair was not attempted']
        : ['Replay failed with a non-repairable or unknown failure; repair was not attempted'],
    })
  }

  /**
   * Replay the latest verified walk of another build against this build. Returns null when no
   * eligible previous walk exists, so the caller keeps its ordinary stale-build rejection.
   */
  private async replayLastVerified(
    identity: WalkIdentity,
    missStart: RunStart,
  ): Promise<RunResult | null> {
    if (identity.platform !== 'ios') return null
    const { store } = this.dependencies
    // The store re-hashes the stored YAML against its recorded checksum here and throws on any
    // mismatch, exactly as an exact hit does. No other await sits between this and the replay.
    const previous = await store.lookupPreviousSuccessful(identity)
    if (previous.status !== 'hit' || !sameReplayFamily(previous.walk, identity)) return null
    const walk = previous.walk
    const replay = {
      policy: 'replay-last-verified' as const,
      previousBuildFingerprint: walk.buildFingerprint,
      previousFlowSha256: walk.flowSha256,
      previousValidatedAt: walk.lastValidation.at,
    }
    const execution = await this.dependencies.runner.run({
      flowPath: previous.flowPath,
      artifactDirectory: `.deja/reports/${this.artifactId('run')}`,
    })
    const replayStart = { ...missStart, execution }
    const modelCallBasis = 'stale build replayed the last verified YAML; explorer, repairer, and model factories were not invoked'

    if (!execution.passed) {
      // The new build has no walk to mark failed, and the previous build's walk stays verified.
      // Nothing is persisted, repaired, or explored.
      const productRegression = execution.failure?.category === 'product_assertion'
      return this.finish(replayStart, {
        outcome: productRegression ? 'product-regression' : 'failed', passed: false,
        modelCalls: 0, modelCallBasis,
        assertions: execution.assertions, repairCount: 0,
        yamlPath: walk.flowPath, yamlSha256: walk.flowSha256,
        failure: execution.failure, artifacts: execution.artifacts, diffs: [],
        errors: [
          `The last verified walk (build ${walk.buildFingerprint}) failed on this unverified build; it was not stored, repaired, or explored`,
        ],
        replayedOnNewBuild: { ...replay, recordedForNewBuild: false },
      })
    }

    // Re-check the replayed bytes are still the verified ones before they become this build's walk.
    const current = await store.lookupPreviousSuccessful(identity)
    if (current.status !== 'hit' || current.walk.flowSha256 !== walk.flowSha256 ||
        current.walk.buildFingerprint !== walk.buildFingerprint) {
      return this.finish(replayStart, {
        outcome: 'failed', passed: false, modelCalls: 0, modelCallBasis,
        assertions: execution.assertions, repairCount: 0,
        yamlPath: walk.flowPath, yamlSha256: walk.flowSha256,
        failure: null, artifacts: execution.artifacts, diffs: [],
        errors: ['The last verified walk changed during replay; nothing was stored for this build'],
        replayedOnNewBuild: { ...replay, recordedForNewBuild: false },
      })
    }
    let persisted: PersistedWalk
    try {
      persisted = await store.persistVerifiedCandidate({
        identity,
        candidatePath: current.flowPath,
        validation: { passed: true, at: execution.endedAt },
        origin: 'replayed-on-new-build',
        // The store refuses before writing anything if the bytes are no longer the replayed ones.
        expectedFlowSha256: walk.flowSha256,
        ...(walk.exploration ? { exploration: walk.exploration } : {}),
      })
    } catch (error) {
      if (!(error instanceof ExpectedFlowChecksumMismatchError)) throw error
      return this.finish(replayStart, {
        outcome: 'failed', passed: false, modelCalls: 0, modelCallBasis,
        assertions: execution.assertions, repairCount: 0,
        yamlPath: walk.flowPath, yamlSha256: walk.flowSha256,
        failure: null, artifacts: execution.artifacts, diffs: [],
        errors: ['The stored YAML changed after the replay; nothing was stored for this build and the previous build\'s walk is unchanged'],
        replayedOnNewBuild: { ...replay, recordedForNewBuild: false },
      })
    }
    return this.finish(replayStart, {
      outcome: 'passed', passed: true, modelCalls: 0, modelCallBasis,
      assertions: execution.assertions, repairCount: 0,
      yamlPath: persisted.flowPath, yamlSha256: persisted.flowSha256,
      failure: null,
      artifacts: [
        ...execution.artifacts,
        { kind: 'yaml', path: persisted.flowPath, sha256: persisted.flowSha256 },
        { kind: 'memory', path: persisted.storedMetadataPath },
      ],
      diffs: [], errors: [],
      replayedOnNewBuild: { ...replay, recordedForNewBuild: true },
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
      artifactDirectory: `.deja/attempts/${this.artifactId('remember')}`,
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

/** lookupPreviousSuccessful ignores adapter/browser; a replay must stay inside the same family. */
function sameReplayFamily(walk: StoredWalk, identity: WalkIdentity) {
  return (walk.adapter ?? null) === (identity.adapter ?? null) &&
    (walk.browser ?? null) === (identity.browser ?? null) &&
    walk.buildFingerprint !== identity.buildFingerprint
}

function walkIdentity(walk: StoredWalk): WalkIdentity {
  return {
    goal: walk.goal,
    appId: walk.appId,
    platform: walk.platform,
    flowSchemaVersion: walk.flowSchemaVersion,
    buildFingerprint: walk.buildFingerprint,
    ...(walk.adapter !== undefined ? { adapter: walk.adapter } : {}),
    ...(walk.browser !== undefined ? { browser: walk.browser } : {}),
  }
}
