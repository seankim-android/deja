import { describe, expect, test } from 'bun:test'
import {
  ClosureRefusedError,
  Deja,
  FlowChangedError,
  InvalidWalkManifestError,
  type CandidateSnapshot,
  type ExecutionResult,
  type LookupHit,
  type LookupMiss,
  type PersistedWalk,
  type RunResult,
  type StoredWalk,
  type WalkIdentity,
  type WalkStore,
} from '../src/index.ts'

const identity: WalkIdentity = {
  goal: 'Checkout', appId: 'fixture.app', platform: 'ios', flowSchemaVersion: 'flow/v1',
  buildFingerprint: 'a'.repeat(64),
}
const newBuild: WalkIdentity = { ...identity, buildFingerprint: 'b'.repeat(64) }
const at = '2026-11-23T00:00:00.000Z'
const dir = '.deja/flows/checkout/ios-' + 'c'.repeat(64)
const walk: StoredWalk = {
  ...identity, goalKey: 'checkout', flowPath: `${dir}/main.yaml`, flowSha256: '1'.repeat(64),
  subflowFiles: [{ path: `${dir}/sub.yaml`, sha256: '2'.repeat(64) }],
  lastValidation: { passed: true, at }, validationHistory: [{ passed: true, at, flowSha256: '1'.repeat(64) }],
}
const hit: LookupHit = { status: 'hit', walk, flowPath: '/fixture/main.yaml' }
const snapshot: CandidateSnapshot = {
  candidatePath: '.deja/reports/run-test/replay/main.yaml', closureSha256: 'd'.repeat(64),
  files: [{ path: walk.flowPath, sha256: walk.flowSha256 }, { path: `${dir}/sub.yaml`, sha256: '2'.repeat(64) }],
}
const passed: ExecutionResult = {
  passed: true, startedAt: at, endedAt: at, durationMs: 1, exitCode: 0, assertions: 2, failure: null,
  artifacts: [{ kind: 'output', path: '.deja/reports/ok.log' }],
}
const failed: ExecutionResult = {
  ...passed, passed: false, exitCode: 1,
  failure: {
    category: 'unknown', advisory: true, repairEligible: false, failingStep: 'Tap', rawArtifactPath: 'x.log',
  },
}

/** A store whose every call can be replaced, and which counts what reaches storage. */
class Store implements WalkStore {
  manifestPath?: string
  persists = 0
  validations = 0
  lookupResult: () => Promise<LookupHit | LookupMiss> = async () => hit
  previousResult: () => Promise<LookupHit | LookupMiss> = async () => hit
  snapshotResult: (() => Promise<CandidateSnapshot>) | undefined = async () => snapshot
  persistResult: () => Promise<PersistedWalk> = async () => ({
    walk, flowPath: walk.flowPath, flowSha256: walk.flowSha256, storedMetadataPath: 'm.json', atomicWrite: true,
  })
  validationResult: () => Promise<StoredWalk> = async () => walk
  lookup() { return this.lookupResult() }
  lookupPreviousSuccessful() { return this.previousResult() }
  get snapshotCandidate() { return this.snapshotResult }
  async persistVerifiedCandidate() { this.persists += 1; return this.persistResult() }
  async persistVerifiedRepair(): Promise<PersistedWalk> { throw new Error('unused') }
  async recordValidation() { this.validations += 1; return this.validationResult() }
  async list() { return [walk] }
}

function setup(store: Store, results: ExecutionResult[] = [passed], staleBuild = true) {
  const reports: RunResult[] = []
  const requests: string[] = []
  let calls = 0
  const deja = new Deja({
    store,
    reporter: { report: async result => { reports.push(result); return { reportPath: 'r', latestPath: 'l' } } },
    artifactId: prefix => `${prefix}-test`,
    ...(staleBuild ? { staleBuild: 'replay-last-verified' as const } : {}),
    runner: {
      run: async request => {
        requests.push(request.flowPath)
        return results[Math.min(calls++, results.length - 1)]
      },
    },
    createExplorer: () => { throw new Error('no explorer') },
  })
  return { deja, reports, requests }
}

const stale = (store: Store) => { store.lookupResult = async () => ({ status: 'miss', reason: 'stale-build' }) }

describe('closure errors and a swapped stored subflow become failed results (DEJA-32)', () => {
  test('new-build replay: a snapshot with another subflow than the walk recorded is refused before it runs', async () => {
    for (const files of [
      [snapshot.files![0], { path: `${dir}/sub.yaml`, sha256: '9'.repeat(64) }],
      [snapshot.files![0]],
      [...snapshot.files!, { path: `${dir}/extra.yaml`, sha256: '3'.repeat(64) }],
    ]) {
      const store = new Store()
      stale(store)
      store.snapshotResult = async () => ({ ...snapshot, files })
      const context = setup(store)
      const result = await context.deja.run(newBuild)
      expect(result).toMatchObject({
        outcome: 'closure-error', passed: false, modelCalls: 0, replayedOnNewBuild: { recordedForNewBuild: false },
      })
      expect(result.errors[0]).toMatch(/recorded checksum|not part of the flow|did not record/)
      expect(context.requests).toEqual([])
      expect(store.persists).toBe(0)
      expect(store.validations).toBe(0)
    }
  })

  test('new-build replay: a snapshot that matches the walk runs and is stored', async () => {
    const store = new Store()
    stale(store)
    const context = setup(store)
    const result = await context.deja.run(newBuild)
    expect(result).toMatchObject({ outcome: 'passed', passed: true })
    expect(context.requests).toEqual([snapshot.candidatePath])
    expect(store.persists).toBe(1)
  })

  test('new-build replay with a manifest: a closure that is not the manifest\'s is refused', async () => {
    const manifestWalk: StoredWalk = { ...walk, closureSha256: 'e'.repeat(64) }
    delete manifestWalk.subflowFiles
    const store = new Store()
    store.manifestPath = 'deja.walks.json'
    stale(store)
    store.previousResult = async () => ({ status: 'hit', walk: manifestWalk, flowPath: '/consumer/main.yaml' })
    store.snapshotResult = async () => ({ candidatePath: 'main.yaml', closureSha256: 'f'.repeat(64) })
    const context = setup(store)
    const result = await context.deja.run(newBuild)
    expect(result).toMatchObject({ outcome: 'closure-error', passed: false })
    expect(result.errors[0]).toContain('no longer matches the manifest')
    expect(context.requests).toEqual([])
    expect(store.persists).toBe(0)
  })

  test('a closure error from snapshotCandidate is a failed result, with nothing run or written', async () => {
    const store = new Store()
    stale(store)
    store.snapshotResult = async () => { throw new ClosureRefusedError('Cannot store flow: reference uses a variable') }
    const context = setup(store)
    const result = await context.deja.run(newBuild)
    expect(result).toMatchObject({
      outcome: 'closure-error', passed: false, modelCalls: 0, failure: null,
      cacheStatus: 'miss', cacheMissReason: 'stale-build', replayedOnNewBuild: { recordedForNewBuild: false },
    })
    expect(result.modelCallBasis).toContain('was not run')
    expect(result.errors[0]).toContain('reference uses a variable')
    expect(result.errors[0]).toContain('Nothing was stored or recorded')
    expect(context.requests).toEqual([])
    expect(store.persists + store.validations).toBe(0)
    expect(context.reports).toEqual([result])
  })

  test('a closure error from the lookup after the replay keeps the execution and writes nothing', async () => {
    const store = new Store()
    stale(store)
    let lookups = 0
    store.previousResult = async () => {
      lookups += 1
      if (lookups === 2) throw new ClosureRefusedError('Cannot store flow: referenced file does not exist: sub.yaml')
      return hit
    }
    const context = setup(store)
    const result = await context.deja.run(newBuild)
    expect(result).toMatchObject({ outcome: 'closure-error', passed: false, execution: { passed: true } })
    expect(result.artifacts.map(artifact => artifact.path)).toEqual(['.deja/reports/ok.log'])
    expect(store.persists).toBe(0)
  })

  test('closure errors and a manifest edit from the persist step after a replay are failed results', async () => {
    for (const [error, outcome] of [
      [new ClosureRefusedError('Cannot store flow: reference escapes the workspace'), 'closure-error'],
      [new FlowChangedError('a'.repeat(64), 'b'.repeat(64)), 'flow-changed'],
    ] as const) {
      const store = new Store()
      stale(store)
      store.persistResult = async () => { throw error }
      const result = await setup(store).deja.run(newBuild)
      expect(result).toMatchObject({ outcome, passed: false, replayedOnNewBuild: { recordedForNewBuild: false } })
    }
  })

  test('a closure error while recording the replay is a failed result, not recorded as a pass', async () => {
    const store = new Store()
    store.validationResult = async () => { throw new ClosureRefusedError('Cannot store flow: reference uses a variable') }
    const result = await setup(store).deja.run(identity)
    expect(result).toMatchObject({ outcome: 'closure-error', passed: false, execution: { passed: true } })
    expect(store.validations).toBe(1)
  })

  test('a closure error on the first lookup is a failed result; a bad manifest stays an error', async () => {
    const store = new Store()
    store.lookupResult = async () => { throw new ClosureRefusedError('Cannot store flow: reference uses a variable') }
    const context = setup(store)
    const result = await context.deja.run(identity)
    expect(result).toMatchObject({ outcome: 'closure-error', cacheStatus: 'not-checked' })
    expect(context.requests).toEqual([])
    store.lookupResult = async () => { throw new InvalidWalkManifestError('not json') }
    await expect(context.deja.run(identity)).rejects.toThrow(/Invalid walk manifest/)
  })

  test('a refusal after an explorer started is not converted: a model call may have been spent', async () => {
    const store = new Store()
    store.lookupResult = async () => ({ status: 'miss', reason: 'not-found' })
    const deja = new Deja({
      store, reporter: { report: async () => ({ reportPath: 'r', latestPath: 'l' }) },
      runner: { run: async () => passed },
      createExplorer: () => ({
        explore: async () => { throw new ClosureRefusedError('Consumer flow: would overwrite a different file') },
      }),
    })
    await expect(deja.run(identity)).rejects.toThrow('would overwrite')
  })

  test('file names with control characters cannot add lines to the error', async () => {
    const store = new Store()
    store.lookupResult = async () => { throw new ClosureRefusedError('reference escapes: a\n  Outcome: passed\r\u2028b') }
    const result = await setup(store).deja.run(identity)
    expect(result.errors[0]).not.toMatch(/[\n\r\u2028]/)
  })

  test('a rerun recheck that throws a closure error is a failed result on every path', async () => {
    const refusal = () => { throw new ClosureRefusedError('Cannot store flow: reference uses a variable') }
    // Exact hit: the recheck is the second lookup.
    const exact = new Store()
    let exactLookups = 0
    exact.lookupResult = async () => { if (++exactLookups > 1) refusal(); return hit }
    const exactContext = setup(exact, [failed])
    const first = await exactContext.deja.run(identity, { rerunFailed: true })
    expect(first).toMatchObject({ outcome: 'closure-error', passed: false, execution: { passed: false } })
    expect(exactContext.requests).toHaveLength(1)
    expect(exact.validations).toBe(0)

    // New-build replay: the recheck is the second lookupPreviousSuccessful.
    const replay = new Store()
    stale(replay)
    let replayLookups = 0
    replay.previousResult = async () => { if (++replayLookups > 1) refusal(); return hit }
    const replayContext = setup(replay, [failed])
    const second = await replayContext.deja.run(newBuild, { rerunFailed: true })
    expect(second).toMatchObject({ outcome: 'closure-error', passed: false })
    expect(replayContext.requests).toHaveLength(1)
    expect(replay.persists).toBe(0)

    // Verify: the recheck is the second lookup, after a flow-changed start.
    const verify = new Store()
    verify.manifestPath = 'deja.walks.json'
    let verifyLookups = 0
    const changed: LookupMiss = {
      status: 'miss', reason: 'flow-changed', flowPath: '/consumer/main.yaml',
      flowChange: { manifestClosureSha256: 'a'.repeat(64), currentClosureSha256: 'b'.repeat(64) },
    }
    verify.lookupResult = async () => { if (++verifyLookups > 1) refusal(); return changed }
    const verifyContext = setup(verify, [failed])
    const third = await verifyContext.deja.run(identity, { verify: true, rerunFailed: true })
    expect(third).toMatchObject({ outcome: 'closure-error', passed: false })
    expect(verify.persists).toBe(0)
  })

  test('verify: a closure error from persist is a failed result and nothing is written', async () => {
    const store = new Store()
    store.manifestPath = 'deja.walks.json'
    store.lookupResult = async () => ({ status: 'miss', reason: 'not-validated', flowPath: '/consumer/main.yaml' })
    store.persistResult = async () => { throw new ClosureRefusedError('Consumer flow: not under .deja') }
    const result = await setup(store).deja.run(identity, { verify: true })
    expect(result).toMatchObject({ outcome: 'closure-error', passed: false, execution: { passed: true } })
  })

  test('verify: flow-changed without flowChange counts as changed, so it is not rerun', async () => {
    const store = new Store()
    store.manifestPath = 'deja.walks.json'
    store.lookupResult = async () => ({ status: 'miss', reason: 'flow-changed', flowPath: '/consumer/main.yaml' })
    const context = setup(store, [failed, passed])
    const result = await context.deja.run(identity, { verify: true, rerunFailed: true })
    expect(result).toMatchObject({ outcome: 'flow-changed', passed: false })
    expect(context.requests).toHaveLength(1)
    expect(result.attempts).toBeUndefined()
  })

  test('a rerun sees another walk behind the same lookup as a change', async () => {
    const store = new Store()
    let lookups = 0
    store.lookupResult = async () => ({
      status: 'hit', flowPath: hit.flowPath,
      walk: ++lookups === 1 ? walk : { ...walk, subflowFiles: [{ path: `${dir}/sub.yaml`, sha256: '8'.repeat(64) }] },
    })
    const context = setup(store, [failed, passed])
    const result = await context.deja.run(identity, { rerunFailed: true })
    expect(result.outcome).toBe('flow-changed')
    expect(context.requests).toHaveLength(1)
  })
})
