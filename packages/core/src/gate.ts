import { normalizeGoal, type LookupMiss, type StoredWalk, type WalkIdentity, type WalkStore } from './contracts.ts'

/**
 * What the release gate says about one goal on one build fingerprint (DEJA-8).
 *
 * - `passed`: the latest validation on this fingerprint is a pass.
 * - `failed`: the latest validation on this fingerprint is a failure.
 * - `flaky`: that failure came from a run that passed on a rerun, or failed differently (DEJA-7).
 * - `not-run`: no validation on this fingerprint, even if older builds passed.
 * - `flow-changed`: manifest mode, the flow or a file it runs no longer matches the manifest.
 * - `invalid`: the stored walk or the flow file cannot be trusted (corrupt, checksum mismatch, missing).
 *
 * Only `passed` opens the gate.
 */
export type GateOutcome = 'passed' | 'failed' | 'flaky' | 'not-run' | 'flow-changed' | 'invalid'

export type GateGoalResult = {
  goal: string
  goalKey: string
  outcome: GateOutcome
  /** The time of the validation the outcome comes from. Null when there is none on this fingerprint. */
  validatedAt: string | null
  /** The checksum of the main flow that validation ran. Null when there is none. */
  flowSha256: string | null
  /** Manifest mode: the closure checksum (the main flow plus every file it runs). */
  closureSha256: string | null
  /** A short reason, for any outcome except a plain `passed`. */
  detail?: string
}

/** The gate's identity filter: the build fingerprint and the exact identity fields a replay keys on. */
export type GateIdentity = Omit<WalkIdentity, 'goal'>

/** A failed validation that a run report names as flaky. The store alone cannot tell (see `GateOptions`). */
export type GateFlakyFailure = {
  goalKey: string
  buildFingerprint: string
  /** The `at` of the failed validation the flaky run recorded. */
  at: string
}

export type GateOptions = {
  /**
   * DEJA-7 records a flaky run's first failure as an ordinary failed validation and never records
   * the rerun pass, so history alone reads `failed`. The host lists the failures its run reports
   * name as flaky; a matching failed validation then reads `flaky`. Both are closed outcomes.
   */
  flakyFailures?: readonly GateFlakyFailure[]
}

export function gatePassed(results: readonly GateGoalResult[]) {
  return results.length > 0 && results.every(result => result.outcome === 'passed')
}

function sameLogicalIdentity(walk: StoredWalk, identity: GateIdentity) {
  return walk.appId === identity.appId && walk.platform === identity.platform &&
    walk.flowSchemaVersion === identity.flowSchemaVersion &&
    (walk.adapter ?? null) === (identity.adapter ?? null) &&
    (walk.browser ?? null) === (identity.browser ?? null) &&
    walk.buildFingerprint === identity.buildFingerprint
}

function missDetail(miss: LookupMiss) {
  return `no passing validation on this build (${miss.reason})`
}

/**
 * Read recorded validations and say, per goal, whether this exact build passed. Read-only: it calls
 * `store.list()` and `store.lookup()` and nothing else, never takes the writer lock, runs no flow,
 * and makes no model call. A pass on any other fingerprint never counts. A store that cannot be
 * read throws; the caller must treat that as a closed gate.
 */
export async function evaluateGate(
  store: Pick<WalkStore, 'list' | 'lookup'>,
  goals: readonly string[],
  identity: GateIdentity,
  options: GateOptions = {},
): Promise<GateGoalResult[]> {
  const walks = await store.list()
  const results: GateGoalResult[] = []
  const seen = new Set<string>()
  for (const raw of goals) {
    const { text, key } = normalizeGoal(raw)
    if (seen.has(key)) continue
    seen.add(key)
    const base = { goal: text, goalKey: key }
    const none = { validatedAt: null, flowSha256: null, closureSha256: null }
    let lookup
    try {
      lookup = await store.lookup({ ...identity, goal: text })
    } catch (error) {
      results.push({
        ...base, outcome: 'invalid', ...none,
        detail: error instanceof Error ? error.message : String(error),
      })
      continue
    }
    if (lookup.status === 'hit') {
      results.push({
        ...base, outcome: 'passed', validatedAt: lookup.walk.lastValidation.at,
        flowSha256: lookup.walk.flowSha256, closureSha256: lookup.walk.closureSha256 ?? null,
      })
      continue
    }
    if (lookup.reason === 'flow-changed') {
      const change = lookup.flowChange
      results.push({
        ...base, outcome: 'flow-changed', ...none,
        closureSha256: change?.currentClosureSha256 ?? null,
        detail: change
          ? `the flow or a file it runs changed (manifest ${change.manifestClosureSha256}, now ${change.currentClosureSha256})`
          : 'the flow or a file it runs changed',
      })
      continue
    }
    if (lookup.reason === 'missing-flow') {
      results.push({ ...base, outcome: 'invalid', ...none, detail: 'the flow file is missing' })
      continue
    }
    if (lookup.reason === 'failed-validation') {
      const failed = walks
        .filter(walk => walk.goalKey === key && sameLogicalIdentity(walk, identity) && !walk.lastValidation.passed)
        .sort((left, right) => right.lastValidation.at.localeCompare(left.lastValidation.at))[0]
      if (failed) {
        const flaky = options.flakyFailures?.some(item =>
          item.goalKey === key && item.buildFingerprint === identity.buildFingerprint &&
          item.at === failed.lastValidation.at) === true
        results.push({
          ...base, outcome: flaky ? 'flaky' : 'failed', validatedAt: failed.lastValidation.at,
          flowSha256: failed.flowSha256, closureSha256: failed.closureSha256 ?? null,
          detail: flaky
            ? 'the latest run on this build failed, then passed on a rerun or failed differently'
            : 'the latest validation on this build is a failure',
        })
        continue
      }
    }
    results.push({ ...base, outcome: 'not-run', ...none, detail: missDetail(lookup) })
  }
  return results
}
