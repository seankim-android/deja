import {
  AssertionHashChangedError,
  ExpectedClosureMismatchError,
  ExpectedFlowChecksumMismatchError,
  FlowChangedError,
  normalizeGoal,
  OwnerRuleError,
  WALK_RESULT_SCHEMA_VERSION,
  type CacheStatus,
  type ExecutionResult,
  type ExplorationResult,
  type LookupHit,
  type LookupMiss,
  type PersistedWalk,
  type RepairResult,
  type RunAttempt,
  type RunnerRequest,
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

/** Options for one `run`. */
export type DejaRunOptions = {
  /**
   * Manifest mode: when the consumer's flow changed, or no build has validated it, re-run its
   * current file in place as a check. A pass with an unchanged assertion hash updates the
   * manifest's sha. A changed assertion hash fails and writes nothing.
   */
  verify?: boolean
  /**
   * Opt-in (DEJA-7): when a replay fails, replay the same bytes once more under the same timeout and
   * report both attempts. A rerun never turns a failure into a pass, never explores or repairs, and
   * makes no model call. A failure that changes label or step, or passes, is reported as `flaky`.
   */
  rerunFailed?: boolean
}

/** Options for one `remember`. */
export type DejaRememberOptions = {
  /**
   * Manifest mode: let this reviewed `remember` change the manifest's assertion hash. Without it, a
   * flow whose assertions differ from the manifest's is refused.
   */
  acceptAssertionChange?: boolean
}

export class Deja {
  private readonly dependencies: DejaDependencies

  constructor(dependencies: DejaDependencies) {
    if (dependencies.store.manifestPath !== undefined && dependencies.createRepairer) {
      // Repair works on a stored copy; with a manifest the flow is the consumer's own file (DEJA-6).
      throw new TypeError('Repair is not supported with a manifest yet; omit the repairer')
    }
    this.dependencies = dependencies
  }

  private get manifestMode() {
    return this.dependencies.store.manifestPath !== undefined
  }

  /** Manifest mode without a flows folder: a verified explore result would have nowhere to go. */
  private get exploreOutputMissing() {
    return this.manifestMode && this.dependencies.store.manifestFlowsDir === undefined
  }

  private now() {
    return (this.dependencies.now ?? (() => new Date()))()
  }

  private artifactId(prefix: 'run' | 'remember') {
    return this.dependencies.artifactId?.(prefix) ??
      `${prefix}-${Date.now()}-${crypto.randomUUID().slice(0, 8)}`
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

  async run(identity: WalkIdentity, options: DejaRunOptions = {}): Promise<RunResult> {
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
      // With a manifest the goal belongs to the consumer's files. No miss may reach the explorer, and
      // only a stale build may reach the replay policy; the rest is a precise refusal or a verify.
      if (this.manifestMode && (retrieval.miss.reason !== 'stale-build' || options.verify)) {
        return this.manifestMiss(identity, missStart, retrieval.miss, options)
      }
      if (retrieval.miss.reason === 'stale-build') {
        if (!this.dependencies.createRepairer &&
            this.dependencies.staleBuild === 'replay-last-verified') {
          const replayed = await this.replayLastVerified(identity, missStart, options.rerunFailed === true)
          if (replayed) return replayed
        }
        if (!this.dependencies.createRepairer) {
          return this.finish(missStart, {
            outcome: 'repair-rejected', passed: false, modelCalls: 0,
            modelCallBasis: 'stale-build diagnosis never constructs a model client', assertions: null,
            repairCount: 0, yamlPath: null, yamlSha256: null, failure: null,
            artifacts: [], diffs: [], errors: [this.manifestMode
              ? 'This build has not validated the flow. Run with --verify, or set staleBuild to replay-last-verified; cold exploration was not substituted'
              : 'A stale build requires a configured selector repairer; cold exploration was not substituted'],
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
    const hitRequest = {
      flowPath: lookup.flowPath,
      artifactDirectory: `.deja/reports/${this.artifactId('run')}`,
    }
    const execution = await this.dependencies.runner.run(hitRequest)
    const hitStart = {
      ...start, cacheStatus: 'hit' as const, execution, semantic: semanticReport,
    }
    const modelCallBasis = retrieval.matchType === 'exact'
      ? 'exact local hit: explorer and model factories were not invoked'
      : 'semantic hit: embedding calls reported separately; stored YAML replayed and explorer not invoked'
    if (execution.passed) {
      const changed = await this.recordOrFlowChanged(selectedIdentity, {
        passed: true, at: execution.endedAt,
      }, hitStart, execution)
      if (changed) return changed
      return this.finish(hitStart, {
        outcome: 'passed', passed: true, modelCalls: 0,
        modelCallBasis,
        assertions: execution.assertions, repairCount: 0,
        yamlPath: lookup.walk.flowPath, yamlSha256: lookup.walk.flowSha256,
        failure: null, artifacts: execution.artifacts, diffs: [], errors: [],
      })
    }

    // Opt-in rerun: the same stored bytes once more. It never repairs, explores or counts as a pass.
    let rerun: Rerun | null = null
    if (options.rerunFailed) {
      const again = await this.rerunOnce(hitRequest, () => this.dependencies.store.lookup(selectedIdentity))
      if (again === 'changed') return this.finish(hitStart, this.changedBetweenAttempts(execution))
      rerun = rerunVerdict(execution, again)
      if (rerun.verdict === 'flaky') {
        const changed = await this.recordOrFlowChanged(selectedIdentity, {
          passed: false, at: execution.endedAt,
        }, hitStart, execution)
        if (changed) return changed
        return this.finish(hitStart, this.flakyDetails(execution, again, rerun, {
          modelCallBasis, yamlPath: lookup.walk.flowPath, yamlSha256: lookup.walk.flowSha256,
        }))
      }
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
        ...(rerun ? { attempts: rerun.attempts } : {}),
      })
    }

    const changedAfterFailure = await this.recordOrFlowChanged(selectedIdentity, {
      passed: false, at: execution.endedAt,
    }, hitStart, execution)
    if (changedAfterFailure) return changedAfterFailure
    const productRegression = execution.failure?.category === 'product_assertion'
    return this.finish(hitStart, {
      outcome: productRegression ? 'product-regression' : 'failed', passed: false,
      modelCalls: 0,
      modelCallBasis,
      assertions: execution.assertions, repairCount: 0,
      yamlPath: lookup.walk.flowPath, yamlSha256: lookup.walk.flowSha256,
      failure: execution.failure,
      artifacts: rerun ? [...execution.artifacts, ...rerun.second.artifacts] : execution.artifacts,
      diffs: [],
      errors: [
        productRegression
          ? 'Meaningful product assertion failed; repair was not attempted'
          : 'Replay failed with a non-repairable or unknown failure; repair was not attempted',
        ...(rerun ? [STEADY_MESSAGE] : []),
      ],
      ...(rerun ? { attempts: rerun.attempts } : {}),
    })
  }

  /**
   * Replay the latest verified walk of another build against this build. Returns null when no
   * eligible previous walk exists, so the caller keeps its ordinary stale-build rejection.
   */
  private async replayLastVerified(
    identity: WalkIdentity,
    missStart: RunStart,
    rerunFailed = false,
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
    // Replay a private copy of the stored flow and everything it runs, so the files that pass are the
    // files that get stored for this build.
    const reportDirectory = `.deja/reports/${this.artifactId('run')}`
    const snapshot = await store.snapshotCandidate?.(previous.flowPath, `${reportDirectory}/replay`)
    const execution = await this.dependencies.runner.run({
      flowPath: snapshot?.candidatePath ?? previous.flowPath,
      artifactDirectory: reportDirectory,
    })
    const replayStart = { ...missStart, execution }
    const modelCallBasis = 'stale build replayed the last verified YAML; explorer, repairer, and model factories were not invoked'

    if (!execution.passed) {
      // The new build has no walk to mark failed, and the previous build's walk stays verified.
      // Nothing is persisted, repaired, or explored.
      let rerun: Rerun | null = null
      if (rerunFailed) {
        const again = await this.rerunOnce({
          flowPath: snapshot?.candidatePath ?? previous.flowPath,
          artifactDirectory: reportDirectory,
        }, () => store.lookupPreviousSuccessful(identity))
        if (again === 'changed') return this.finish(replayStart, this.changedBetweenAttempts(execution))
        rerun = rerunVerdict(execution, again)
        if (rerun.verdict === 'flaky') {
          return this.finish(replayStart, {
            ...this.flakyDetails(execution, again, rerun, {
              modelCallBasis, yamlPath: walk.flowPath, yamlSha256: walk.flowSha256,
            }),
            replayedOnNewBuild: { ...replay, recordedForNewBuild: false },
          })
        }
      }
      const productRegression = execution.failure?.category === 'product_assertion'
      return this.finish(replayStart, {
        outcome: productRegression ? 'product-regression' : 'failed', passed: false,
        modelCalls: 0, modelCallBasis,
        assertions: execution.assertions, repairCount: 0,
        yamlPath: walk.flowPath, yamlSha256: walk.flowSha256,
        failure: execution.failure,
        artifacts: rerun ? [...execution.artifacts, ...rerun.second.artifacts] : execution.artifacts,
        diffs: [],
        errors: [
          `The last verified walk (build ${walk.buildFingerprint}) failed on this unverified build; it was not stored, repaired, or explored`,
          ...(rerun ? [STEADY_MESSAGE] : []),
        ],
        replayedOnNewBuild: { ...replay, recordedForNewBuild: false },
        ...(rerun ? { attempts: rerun.attempts } : {}),
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
        candidatePath: snapshot?.candidatePath ?? current.flowPath,
        validation: { passed: true, at: execution.endedAt },
        origin: 'replayed-on-new-build',
        // The store refuses before writing anything if the bytes are no longer the replayed ones.
        expectedFlowSha256: walk.flowSha256,
        ...(snapshot ? { expectedClosureSha256: snapshot.closureSha256 } : {}),
        ...(walk.exploration ? { exploration: walk.exploration } : {}),
      })
    } catch (error) {
      if (!(error instanceof ExpectedFlowChecksumMismatchError) &&
          !(error instanceof ExpectedClosureMismatchError)) throw error
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
    if (this.exploreOutputMissing) return this.finish(start, this.manifestExplorationRefused())
    if (this.manifestMode) {
      // A forced exploration would spend a model run on a goal whose flow is already the consumer's.
      const current = await this.dependencies.store.lookup(identity)
      if (current.status === 'hit' || current.reason !== 'not-found') {
        return this.finish(start, {
          ...this.manifestExplorationRefused(),
          errors: ['The manifest already lists this goal, so it was not explored. Edit your flow and run remember, or run it.'],
        })
      }
    }
    const exploration = await this.dependencies.createExplorer().explore({ identity })
    return this.finish(start, this.explorationDetails(exploration))
  }

  async remember(
    identity: WalkIdentity,
    candidatePath: string,
    options: DejaRememberOptions = {},
  ): Promise<RunResult> {
    const start = this.base(identity, 'remember', 'not-checked')
    const attempt = `.deja/attempts/${this.artifactId('remember')}`
    // Verify a private copy of the flow and everything it runs, then store exactly those bytes.
    const snapshot = await this.dependencies.store.snapshotCandidate?.(candidatePath, `${attempt}/candidate`)
    const execution = await this.dependencies.runner.run({
      flowPath: snapshot?.candidatePath ?? candidatePath,
      artifactDirectory: attempt,
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
    let persisted: PersistedWalk
    try {
      persisted = await this.dependencies.store.persistVerifiedCandidate({
        identity, candidatePath: snapshot?.candidatePath ?? candidatePath,
        validation: { passed: true, at: execution.endedAt }, origin: 'remember',
        ...(this.manifestMode
          ? { assertionChange: options.acceptAssertionChange ? 'accept' as const : 'refuse' as const }
          : {}),
        ...(snapshot ? { expectedClosureSha256: snapshot.closureSha256 } : {}),
      })
    } catch (error) {
      if (error instanceof AssertionHashChangedError) {
        return this.finish(start, {
          outcome: 'assertions-changed', passed: false, modelCalls: 0,
          modelCallBasis: 'remember performs independent verification only',
          assertions: execution.assertions, repairCount: 0, yamlPath: null, yamlSha256: null,
          failure: null, artifacts: execution.artifacts, diffs: [],
          errors: [`${error.message}. Review the flow's assertions, then pass acceptAssertionChange (--accept-assertion-change).`],
          execution,
        })
      }
      if (error instanceof OwnerRuleError) {
        return this.finish(start, {
          outcome: 'rejected', passed: false, modelCalls: 0,
          modelCallBasis: 'remember performs independent verification only',
          assertions: execution.assertions, repairCount: 0, yamlPath: null, yamlSha256: null,
          failure: null, artifacts: execution.artifacts, diffs: [], errors: [error.message], execution,
        })
      }
      throw error
    }
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
        ...(persisted.manifest ? [{ kind: 'manifest' as const, path: persisted.manifest.path }] : []),
      ],
      diffs: [], errors: [], execution,
    })
  }

  private manifestExplorationRefused(): RunDetails {
    return {
      outcome: 'exploration-rejected', passed: false, modelCalls: 0,
      modelCallBasis: 'manifest mode: cold exploration was not attempted, so no model client was built',
      assertions: null, repairCount: 0, yamlPath: null, yamlSha256: null,
      failure: null, artifacts: [], diffs: [],
      errors: ['Cold exploration needs a flows folder with a manifest, so its result can be written into your tree. Set one, or write the flow in your own files and run remember.'],
    }
  }

  /** Record a replay result; if the consumer's files moved during the run, report that instead. */
  private async recordOrFlowChanged(
    identity: WalkIdentity,
    validation: { passed: boolean; at: string },
    start: RunStart,
    execution: RunResult['execution'] & object,
  ): Promise<RunResult | null> {
    try {
      await this.dependencies.store.recordValidation(identity, validation)
      return null
    } catch (error) {
      if (!(error instanceof FlowChangedError)) throw error
      return this.finish(start, this.flowChangedDetails(
        execution, `${error.message}. The run is not recorded as a pass or a failure.`,
      ))
    }
  }

  private flowChangedDetails(execution: ExecutionResult, message: string): RunDetails {
    return {
      outcome: 'flow-changed', passed: false, modelCalls: 0,
      modelCallBasis: 'the flow changed during the run; nothing was recorded',
      assertions: execution.assertions, repairCount: 0, yamlPath: null, yamlSha256: null,
      failure: null, artifacts: execution.artifacts, diffs: [], errors: [message],
    }
  }

  private changedBetweenAttempts(execution: ExecutionResult): RunDetails {
    return this.flowChangedDetails(execution,
      'The flow or a file it runs changed between the two attempts, so it was not rerun and nothing was recorded.')
  }

  /**
   * Replay the same bytes once more, under the same timeout. The files are re-checked before and
   * after: if they changed (manifest mode: the closure no longer matches), nothing is rerun or
   * reported as flaky. No explorer, repairer or model client is touched.
   */
  private async rerunOnce(
    request: RunnerRequest,
    recheck: () => Promise<LookupHit | LookupMiss>,
  ): Promise<ExecutionResult | 'changed'> {
    const changed = async () => {
      const current = await recheck()
      return current.status === 'miss' &&
        (current.reason === 'flow-changed' || current.reason === 'missing-flow')
    }
    if (await changed()) return 'changed'
    const second = await this.dependencies.runner.run({
      ...request, artifactDirectory: `${request.artifactDirectory}/rerun`,
    })
    return await changed() ? 'changed' : second
  }

  /** The second attempt passed, or the two failures differ: not a pass, not a steady failure. */
  private flakyDetails(
    first: ExecutionResult,
    second: ExecutionResult,
    rerun: Rerun,
    base: { modelCallBasis: string; yamlPath: string | null; yamlSha256: string | null },
  ): RunDetails {
    return {
      outcome: 'flaky', passed: false, modelCalls: 0, modelCallBasis: base.modelCallBasis,
      assertions: first.assertions, repairCount: 0,
      yamlPath: base.yamlPath, yamlSha256: base.yamlSha256,
      // The first failure is the recorded one; the second attempt is in `attempts`.
      failure: first.failure, artifacts: [...first.artifacts, ...second.artifacts], diffs: [],
      errors: [second.passed
        ? 'The replay failed, then passed on a rerun of the same YAML. This is flaky, not a pass; the first failure is recorded.'
        : 'The replay failed twice with a different label or step on the same YAML. This is flaky, not a pass; the first failure is recorded.'],
      attempts: rerun.attempts,
    }
  }

  /** Manifest mode: every miss except a stale build ends here, before any explorer or repairer. */
  private async manifestMiss(
    identity: WalkIdentity,
    missStart: RunStart,
    miss: LookupMiss,
    options: DejaRunOptions,
  ): Promise<RunResult> {
    const refuse = (outcome: RunResult['outcome'], message: string) => this.finish(missStart, {
      outcome, passed: false, modelCalls: 0,
      modelCallBasis: 'manifest mode: this miss never constructs an explorer, repairer or model client',
      assertions: null, repairCount: 0, yamlPath: null, yamlSha256: null,
      failure: null, artifacts: [], diffs: [], errors: [message],
    })
    const verifiable = miss.flowPath !== undefined &&
      ['flow-changed', 'not-validated', 'failed-validation', 'stale-build'].includes(miss.reason)
    if (options.verify && verifiable) {
      return this.verifyFlow(identity, missStart, miss.flowPath!, options.rerunFailed === true)
    }
    switch (miss.reason) {
      case 'flow-changed':
        return refuse('flow-changed',
          'The flow or a file it runs no longer matches the manifest. Nothing was replayed. Run with --verify to re-check it.')
      case 'missing-flow':
        return refuse('flow-changed', 'A file the manifest names is missing. Nothing was replayed.')
      case 'not-validated':
        return refuse('not-validated',
          'The manifest lists this flow, but no build has validated it in this store. Run with --verify.')
      case 'needs-migration':
        return refuse('needs-migration',
          'This goal has only a walk from the old store. Run "deja migrate", or run remember on your own flow file.')
      case 'failed-validation':
        return refuse('failed', 'The last run of this flow on this build failed. Fix the app or the flow, or run with --verify.')
      case 'not-found': {
        // A new goal: explore it cold, when verified output can be written into the consumer's tree.
        if (this.exploreOutputMissing) return this.finish(missStart, this.manifestExplorationRefused())
        const exploration = await this.dependencies.createExplorer().explore({ identity })
        return this.finish(missStart, this.explorationDetails(exploration))
      }
      default:
        return refuse('failed', `The manifest does not match this run (${miss.reason}).`)
    }
  }

  /**
   * Re-run the consumer's current file in place. Only a pass whose assertions are unchanged can
   * update the manifest; the store checks that, and re-hashes the files so a change during the run
   * is refused.
   */
  private async verifyFlow(
    identity: WalkIdentity,
    missStart: RunStart,
    flowPath: string,
    rerunFailed = false,
  ): Promise<RunResult> {
    const { store, runner } = this.dependencies
    const attempt = `.deja/attempts/${this.artifactId('run')}`
    const snapshot = await store.snapshotCandidate?.(flowPath, `${attempt}/candidate`)
    const execution = await runner.run({
      flowPath: snapshot?.candidatePath ?? flowPath,
      artifactDirectory: attempt,
    })
    const verifyStart = { ...missStart, execution }
    const modelCallBasis = 'verify replays the consumer\'s current file in place; explorer, repairer and model factories were not invoked'
    if (!execution.passed) {
      let rerun: Rerun | null = null
      if (rerunFailed) {
        const again = await this.rerunOnce({
          flowPath: snapshot?.candidatePath ?? flowPath, artifactDirectory: attempt,
        }, () => store.lookup(identity))
        if (again === 'changed') return this.finish(verifyStart, this.changedBetweenAttempts(execution))
        rerun = rerunVerdict(execution, again)
        if (rerun.verdict === 'flaky') {
          return this.finish(verifyStart, this.flakyDetails(execution, again, rerun, {
            modelCallBasis, yamlPath: null, yamlSha256: null,
          }))
        }
      }
      const productRegression = execution.failure?.category === 'product_assertion'
      return this.finish(verifyStart, {
        outcome: productRegression ? 'product-regression' : 'failed', passed: false,
        modelCalls: 0, modelCallBasis, assertions: execution.assertions, repairCount: 0,
        yamlPath: null, yamlSha256: null, failure: execution.failure,
        artifacts: rerun ? [...execution.artifacts, ...rerun.second.artifacts] : execution.artifacts,
        diffs: [],
        errors: [
          'The flow did not pass on this build; the manifest and history are unchanged',
          ...(rerun ? [STEADY_MESSAGE] : []),
        ],
        ...(rerun ? { attempts: rerun.attempts } : {}),
      })
    }
    let persisted: PersistedWalk
    try {
      persisted = await store.persistVerifiedCandidate({
        identity, candidatePath: snapshot?.candidatePath ?? flowPath,
        validation: { passed: true, at: execution.endedAt }, origin: 'verify',
        assertionChange: 'refuse',
        ...(snapshot ? { expectedClosureSha256: snapshot.closureSha256 } : {}),
      })
    } catch (error) {
      const outcome = error instanceof AssertionHashChangedError ? 'assertions-changed'
        : error instanceof ExpectedClosureMismatchError || error instanceof FlowChangedError ? 'flow-changed'
          : error instanceof OwnerRuleError ? 'failed' : null
      if (!outcome) throw error
      return this.finish(verifyStart, {
        outcome, passed: false, modelCalls: 0, modelCallBasis,
        assertions: execution.assertions, repairCount: 0, yamlPath: null, yamlSha256: null,
        failure: null, artifacts: execution.artifacts, diffs: [],
        errors: [error instanceof AssertionHashChangedError
          ? `${error.message}. The manifest and history are unchanged. Review the change, then run remember with --accept-assertion-change.`
          : `${(error as Error).message}. The manifest and history are unchanged.`],
      })
    }
    return this.finish(verifyStart, {
      outcome: 'passed', passed: true, modelCalls: 0, modelCallBasis,
      assertions: execution.assertions, repairCount: 0,
      yamlPath: persisted.flowPath, yamlSha256: persisted.flowSha256,
      failure: null,
      artifacts: [
        ...execution.artifacts,
        { kind: 'yaml', path: persisted.flowPath, sha256: persisted.flowSha256 },
        { kind: 'memory', path: persisted.storedMetadataPath },
        ...(persisted.manifest ? [{ kind: 'manifest' as const, path: persisted.manifest.path }] : []),
      ],
      diffs: [], errors: [],
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

const STEADY_MESSAGE =
  'The rerun failed with the same label and step, so this is a steady failure, not a flaky one.'

type Rerun = { verdict: 'steady' | 'flaky'; attempts: RunAttempt[]; second: ExecutionResult }

function attemptOf(attempt: 1 | 2, execution: ExecutionResult): RunAttempt {
  return {
    attempt, passed: execution.passed, startedAt: execution.startedAt, endedAt: execution.endedAt,
    durationMs: execution.durationMs, failure: execution.failure, artifacts: execution.artifacts,
  }
}

/** Both attempts failed with the same label and step: steady. A pass, or any difference: flaky. */
function rerunVerdict(first: ExecutionResult, second: ExecutionResult): Rerun {
  const steady = !second.passed &&
    (first.failure?.category ?? null) === (second.failure?.category ?? null) &&
    (first.failure?.failingStep ?? null) === (second.failure?.failingStep ?? null)
  return {
    verdict: steady ? 'steady' : 'flaky', second,
    attempts: [attemptOf(1, first), attemptOf(2, second)],
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
