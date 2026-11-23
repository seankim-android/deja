import { describe, expect, test } from 'bun:test'
import {
  Deja,
  type ExecutionResult,
  type FailureCategory,
  type LookupHit,
  type LookupMiss,
  type PersistedWalk,
  type RunnerRequest,
  type RunReporter,
  type RunResult,
  type StoredWalk,
  type WalkIdentity,
  type WalkStore,
  type WalkValidation,
} from '../src/index.ts'

const identity: WalkIdentity = {
  goal: 'Checkout', appId: 'fixture.app', platform: 'ios', flowSchemaVersion: 'flow/v1',
  buildFingerprint: 'a'.repeat(64),
}
const newBuild: WalkIdentity = { ...identity, buildFingerprint: 'b'.repeat(64) }
const at = '2026-11-23T00:00:00.000Z'
const walk: StoredWalk = {
  ...identity, goalKey: 'checkout', flowPath: '.deja/flows/checkout/fixture.yaml',
  flowSha256: 'f'.repeat(64),
  lastValidation: { passed: true, at }, validationHistory: [{ passed: true, at, flowSha256: 'f'.repeat(64) }],
}
const hit: LookupHit = { status: 'hit', walk, flowPath: '/fixture/stored.yaml' }

class Store implements WalkStore {
  lookupResults: (LookupHit | LookupMiss)[] = [hit]
  lookups = 0
  validations: WalkValidation[] = []
  persists = 0
  async lookup() {
    const index = Math.min(this.lookups, this.lookupResults.length - 1)
    this.lookups += 1
    return this.lookupResults[index]
  }
  async lookupPreviousSuccessful(): Promise<LookupHit | LookupMiss> { return hit }
  async persistVerifiedCandidate(): Promise<PersistedWalk> {
    this.persists += 1
    return { walk, flowPath: walk.flowPath, flowSha256: walk.flowSha256, storedMetadataPath: 'm.json', atomicWrite: true }
  }
  async persistVerifiedRepair(): Promise<PersistedWalk> { throw new Error('unused') }
  async recordValidation(_identity: WalkIdentity, validation: WalkValidation) {
    this.validations.push(validation)
    return walk
  }
  async list() { return [walk] }
}

class Reporter implements RunReporter {
  results: RunResult[] = []
  async report(result: RunResult) {
    this.results.push(result)
    return { reportPath: 'r.json', latestPath: 'l.json' }
  }
}

function run(
  passed: boolean,
  category?: FailureCategory,
  step = `step for ${category}`,
  crash?: boolean,
): ExecutionResult {
  return {
    passed, startedAt: at, endedAt: at, durationMs: 10, exitCode: passed ? 0 : 1, assertions: 3,
    failure: passed || !category ? null : {
      category, advisory: true, repairEligible: category === 'selector' || category === 'navigation',
      failingStep: step, rawArtifactPath: `.deja/reports/${category}.log`,
      ...(crash ? { crash: { source: 'maestro-output' as const } } : {}),
    },
    artifacts: [{ kind: 'output', path: `.deja/reports/${passed ? 'ok' : category}.log` }],
  }
}

function setup(results: ExecutionResult[], lookup: (LookupHit | LookupMiss)[] = [hit]) {
  const store = new Store()
  store.lookupResults = lookup
  const reporter = new Reporter()
  const requests: RunnerRequest[] = []
  let explorers = 0
  let repairers = 0
  const deja = new Deja({
    store, reporter, artifactId: prefix => `${prefix}-test`,
    runner: {
      run: async request => {
        requests.push(request)
        return results[Math.min(requests.length - 1, results.length - 1)]
      },
    },
    createExplorer: () => { explorers += 1; throw new Error('no explorer') },
    createRepairer: () => { repairers += 1; throw new Error('no repairer') },
  })
  return { store, reporter, requests, deja, constructed: () => ({ explorers, repairers }) }
}

describe('opt-in rerun of a failed replay (DEJA-7)', () => {
  test('off by default: one replay, no attempts', async () => {
    const context = setup([run(false, 'unknown')])
    const result = await context.deja.run(identity)
    expect(context.requests).toHaveLength(1)
    expect(result.attempts).toBeUndefined()
    expect(result).toMatchObject({ outcome: 'failed', passed: false })
  })

  test('a passing replay is never rerun', async () => {
    const context = setup([run(true)])
    const result = await context.deja.run(identity, { rerunFailed: true })
    expect(context.requests).toHaveLength(1)
    expect(result).toMatchObject({ outcome: 'passed', passed: true })
    expect(result.attempts).toBeUndefined()
  })

  test('failed twice with the same label and step is a steady failure, reported as today', async () => {
    const context = setup([run(false, 'product_assertion'), run(false, 'product_assertion')])
    const result = await context.deja.run(identity, { rerunFailed: true })
    expect(result).toMatchObject({
      outcome: 'product-regression', passed: false, modelCalls: 0,
      failure: { category: 'product_assertion' },
    })
    expect(result.attempts?.map(attempt => [attempt.attempt, attempt.passed, attempt.failure?.category]))
      .toEqual([[1, false, 'product_assertion'], [2, false, 'product_assertion']])
    expect(result.artifacts.map(artifact => artifact.path))
      .toEqual(['.deja/reports/product_assertion.log', '.deja/reports/product_assertion.log'])
    // One failure is recorded, not two.
    expect(context.store.validations).toEqual([{ passed: false, at }])
  })

  test('failed, then passed: flaky, not a pass; the first failure is recorded and no pass is', async () => {
    const context = setup([run(false, 'selector'), run(true)])
    const result = await context.deja.run(identity, { rerunFailed: true })
    expect(result).toMatchObject({
      outcome: 'flaky', passed: false, modelCalls: 0, repairCount: 0,
      failure: { category: 'selector', failingStep: 'step for selector' },
    })
    expect(result.attempts?.map(attempt => attempt.passed)).toEqual([false, true])
    expect(result.errors.join(' ')).toContain('flaky, not a pass')
    expect(context.store.validations).toEqual([{ passed: false, at }])
    expect(context.store.persists).toBe(0)
    expect(context.constructed()).toEqual({ explorers: 0, repairers: 0 })
  })

  test('failed twice with different labels, or the same label at another step: flaky with both attempts', async () => {
    const labels = setup([run(false, 'selector'), run(false, 'product_assertion')])
    const first = await labels.deja.run(identity, { rerunFailed: true })
    expect(first).toMatchObject({ outcome: 'flaky', passed: false })
    expect(first.attempts?.map(attempt => attempt.failure?.category)).toEqual(['selector', 'product_assertion'])

    const steps = setup([run(false, 'selector', 'Tap on a'), run(false, 'selector', 'Tap on b')])
    const second = await steps.deja.run(identity, { rerunFailed: true })
    expect(second.outcome).toBe('flaky')
    expect(second.attempts?.map(attempt => attempt.failure?.failingStep)).toEqual(['Tap on a', 'Tap on b'])
  })

  test('a crash on the first attempt and a pass on the rerun is flaky with app_crash first', async () => {
    const context = setup([run(false, 'app_crash', 'Tap on a', true), run(true)])
    const result = await context.deja.run(identity, { rerunFailed: true })
    expect(result).toMatchObject({
      outcome: 'flaky', passed: false,
      failure: { category: 'app_crash', repairEligible: false, crash: { source: 'maestro-output' } },
    })
  })

  test('an app_crash never reaches the repairer, even when the same crash repeats', async () => {
    const context = setup([run(false, 'app_crash', 'Tap on a', true), run(false, 'app_crash', 'Tap on a', true)])
    const result = await context.deja.run(identity, { rerunFailed: true })
    expect(result).toMatchObject({ outcome: 'failed', passed: false, repairCount: 0 })
    expect(context.constructed().repairers).toBe(0)
  })

  test('the rerun replays the same file under the same timeout, in its own artifact folder', async () => {
    const context = setup([run(false, 'unknown'), run(true)])
    await context.deja.run(identity, { rerunFailed: true })
    expect(context.requests).toHaveLength(2)
    expect(context.requests[1].flowPath).toBe(context.requests[0].flowPath)
    expect(context.requests[1].timeoutMs).toBe(context.requests[0].timeoutMs)
    expect(context.requests[1].artifactDirectory).toBe(`${context.requests[0].artifactDirectory}/rerun`)
  })

  test('manifest mode: files that changed between the attempts give flow-changed and no rerun', async () => {
    const changed: LookupMiss = {
      status: 'miss', reason: 'flow-changed', flowPath: '/fixture/stored.yaml',
      flowChange: { manifestClosureSha256: 'a'.repeat(64), currentClosureSha256: 'b'.repeat(64) },
    }
    // The pre-check after the first attempt already sees the change.
    const before = setup([run(false, 'unknown'), run(true)], [hit, changed])
    const early = await before.deja.run(identity, { rerunFailed: true })
    expect(early).toMatchObject({ outcome: 'flow-changed', passed: false })
    expect(before.requests).toHaveLength(1)
    expect(before.store.validations).toEqual([])

    // A change during the rerun discards the rerun: neither flaky nor steady, nothing recorded.
    const during = setup([run(false, 'unknown'), run(true)], [hit, hit, changed])
    const late = await during.deja.run(identity, { rerunFailed: true })
    expect(late).toMatchObject({ outcome: 'flow-changed', passed: false })
    expect(during.requests).toHaveLength(2)
    expect(during.store.validations).toEqual([])
    expect(late.attempts).toBeUndefined()
  })

  test('stale-build replay: flaky when the rerun passes, nothing stored or recorded', async () => {
    const store = new Store()
    store.lookupResults = [{ status: 'miss', reason: 'stale-build' }]
    const reporter = new Reporter()
    const results = [run(false, 'unknown'), run(true)]
    let calls = 0
    const deja = new Deja({
      store, reporter, staleBuild: 'replay-last-verified', artifactId: p => `${p}-test`,
      runner: { run: async () => results[calls++] },
      createExplorer: () => { throw new Error('no explorer') },
    })
    const result = await deja.run(newBuild, { rerunFailed: true })
    expect(result).toMatchObject({
      outcome: 'flaky', passed: false, replayedOnNewBuild: { recordedForNewBuild: false },
    })
    expect(calls).toBe(2)
    expect(store.persists).toBe(0)
    expect(store.validations).toEqual([])
  })
})
