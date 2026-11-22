import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import {
  WalkCache,
  type ExecutionResult,
  type ExplorationResult,
  type FailureCategory,
  type LookupHit,
  type LookupMiss,
  type PersistedWalk,
  type PersistVerifiedOptions,
  type PersistVerifiedRepairOptions,
  type RepairResult,
  type RunReporter,
  type RunResult,
  type StoredWalk,
  type WalkIdentity,
  type WalkStore,
  type WalkValidation,
} from '../src/index.ts'

const identity: WalkIdentity = {
  goal: 'Checkout',
  appId: 'fixture.app',
  platform: 'fixture-platform',
  flowSchemaVersion: 'flow/v1',
  buildFingerprint: 'a'.repeat(64),
}
const timestamp = '2026-11-22T00:00:00.000Z'
const storedWalk: StoredWalk = {
  ...identity,
  goalKey: 'checkout',
  flowPath: '.walkcache/flows/checkout/fixture.yaml',
  flowSha256: 'f'.repeat(64),
  lastValidation: { passed: true, at: timestamp },
  validationHistory: [{ passed: true, at: timestamp, flowSha256: 'f'.repeat(64) }],
}

class FakeStore implements WalkStore {
  lookupResult: LookupHit | LookupMiss = { status: 'miss', reason: 'not-found' }
  validations: WalkValidation[] = []
  persistCalls = 0

  async lookup() {
    return this.lookupResult
  }

  async lookupPreviousSuccessful(): Promise<LookupMiss> {
    return { status: 'miss', reason: 'not-found' }
  }

  async persistVerifiedCandidate(_options: PersistVerifiedOptions): Promise<PersistedWalk> {
    this.persistCalls += 1
    return {
      walk: storedWalk,
      flowPath: storedWalk.flowPath,
      flowSha256: storedWalk.flowSha256,
      storedMetadataPath: '.walkcache/memory/walks.json',
      atomicWrite: true,
    }
  }

  async persistVerifiedRepair(_options: PersistVerifiedRepairOptions): Promise<PersistedWalk> {
    throw new Error('Core delegates repair persistence to the repair plugin')
  }

  async recordValidation(_identity: WalkIdentity, validation: WalkValidation) {
    this.validations.push(validation)
    return { ...storedWalk, lastValidation: validation }
  }

  async list() {
    return [storedWalk]
  }
}

class FakeReporter implements RunReporter {
  results: RunResult[] = []
  async report(result: RunResult) {
    this.results.push(result)
    return { reportPath: '.walkcache/reports/test.json', latestPath: '.walkcache/reports/latest.json' }
  }
}

function failure(category: FailureCategory) {
  return {
    category,
    advisory: true as const,
    repairEligible: category === 'selector' || category === 'navigation',
    failingStep: `fixture ${category} step`,
    rawArtifactPath: '.walkcache/reports/output.log',
  }
}

function execution(passed: boolean, category?: FailureCategory): ExecutionResult {
  return {
    passed,
    startedAt: timestamp,
    endedAt: timestamp,
    durationMs: 25,
    exitCode: passed ? 0 : 1,
    assertions: 9,
    failure: passed || !category ? null : failure(category),
    artifacts: [{ kind: 'output', path: '.walkcache/reports/output.log' }],
  }
}

function exploration(passed = true): ExplorationResult {
  return {
    passed,
    durationMs: 100,
    explorationDurationMs: 60,
    verificationDurationMs: 25,
    mcpCalls: 4,
    modelCalls: 3,
    modelCallBasis: 'fixture explorer turns',
    model: 'fixture-model',
    provider: 'fixture-provider',
    assertions: 9,
    flowPath: passed ? storedWalk.flowPath : null,
    flowSha256: passed ? storedWalk.flowSha256 : null,
    failure: passed ? null : failure('product_assertion'),
    artifacts: [{ kind: 'attempt', path: '.walkcache/attempts/explore-test' }],
    errors: passed ? [] : ['verification rejected'],
  }
}

function repair(status: RepairResult['status'] = 'repaired'): RepairResult {
  const passed = status === 'repaired'
  return {
    status,
    passed,
    durationMs: 50,
    modelCalls: 0,
    repairCount: passed ? 1 : 0,
    assertions: 9,
    flowPath: storedWalk.flowPath,
    flowSha256: storedWalk.flowSha256,
    failure: passed ? null : failure('selector'),
    artifacts: [{ kind: 'repair-report', path: '.walkcache/patches/repair-test/repair-report.json' }],
    diffs: passed ? [{
      path: '.walkcache/patches/repair-test/proposed.patch',
      affectedFiles: ['fixture/component.ext'],
      contents: '- old selector\n+ new selector\n',
      autoCommit: false,
    }] : [],
    errors: passed ? [] : ['not eligible'],
  }
}

function fixture(
  lookup: LookupHit | LookupMiss,
  runnerResults = [execution(true)],
  now?: () => Date,
) {
  const store = new FakeStore()
  store.lookupResult = lookup
  const reporter = new FakeReporter()
  let runnerCalls = 0
  let explorerConstructions = 0
  let explorerCalls = 0
  let modelConstructions = 0
  let repairConstructions = 0
  let repairCalls = 0
  let explorationResult = exploration()
  let repairResult = repair()
  const cache = new WalkCache({
    store,
    reporter,
    ...(now ? { now } : {}),
    artifactId: prefix => `${prefix}-test`,
    runner: {
      run: async () => {
        runnerCalls += 1
        return runnerResults[Math.min(runnerCalls - 1, runnerResults.length - 1)]
      },
    },
    createExplorer: () => {
      explorerConstructions += 1
      // The production explorer owns its vendor model client; reaching this line is the only
      // place where such a client may be constructed.
      modelConstructions += 1
      return {
        explore: async () => {
          explorerCalls += 1
          return explorationResult
        },
      }
    },
    createRepairer: () => {
      repairConstructions += 1
      return {
        repair: async () => {
          repairCalls += 1
          return repairResult
        },
      }
    },
  })
  return {
    store, reporter, cache,
    counts: () => ({ runnerCalls, explorerConstructions, explorerCalls, repairConstructions, repairCalls }),
    modelConstructions: () => modelConstructions,
    setExploration: (result: ExplorationResult) => { explorationResult = result },
    setRepair: (result: RepairResult) => { repairResult = result },
  }
}

const hit: LookupHit = { status: 'hit', walk: storedWalk, flowPath: '/fixture/stored.yaml' }

describe('vendor-independent orchestration', () => {
  test('core source has no platform, demo, UI, filesystem, or vendor imports', async () => {
    for (const filename of ['contracts.ts', 'orchestration.ts', 'index.ts']) {
      const source = await readFile(resolve(import.meta.dir, `../src/${filename}`), 'utf8')
      for (const forbidden of ['node:', '../../maestro', '../../agent-openclaw', 'react', 'checkout', 'android']) {
        expect(source.toLowerCase()).not.toContain(forbidden)
      }
    }
  })

  test('a warm constructor/spy proves no explorer or repairer is constructed or invoked', async () => {
    const context = fixture(hit)
    const result = await context.cache.run(identity)
    expect(result).toMatchObject({
      cacheStatus: 'hit', outcome: 'passed', passed: true, modelCalls: 0,
      assertions: 9, repairCount: 0, yamlPath: storedWalk.flowPath,
    })
    expect(context.counts()).toEqual({
      runnerCalls: 1, explorerConstructions: 0, explorerCalls: 0,
      repairConstructions: 0, repairCalls: 0,
    })
    expect(context.modelConstructions()).toBe(0)
    expect(context.store.validations).toEqual([{ passed: true, at: timestamp }])
    expect(context.reporter.results).toHaveLength(1)
  })

  test('a normal miss invokes the cold explorer once and returns its verified metrics', async () => {
    const context = fixture({ status: 'miss', reason: 'not-found' })
    const result = await context.cache.run(identity)
    expect(result).toMatchObject({
      cacheStatus: 'miss', cacheMissReason: 'not-found', outcome: 'cold-recorded',
      modelCalls: 3, assertions: 9, repairCount: 0,
    })
    expect(context.counts()).toEqual({
      runnerCalls: 0, explorerConstructions: 1, explorerCalls: 1,
      repairConstructions: 0, repairCalls: 0,
    })
    expect(result.exploration?.verificationDurationMs).toBe(25)
    expect(context.modelConstructions()).toBe(1)
  })

  test('explicit explore also uses the injected cold lifecycle and times the whole command', async () => {
    let clock = Date.parse(timestamp)
    const context = fixture(hit, undefined, () => new Date(clock += 1000))
    const result = await context.cache.explore(identity)
    expect(result).toMatchObject({ command: 'explore', cacheStatus: 'not-checked', outcome: 'cold-recorded' })
    expect(result.durationMs).toBe(1000)
    expect(context.counts().explorerConstructions).toBe(1)
    expect(context.counts().runnerCalls).toBe(0)
  })

  test('a stale-build miss uses exactly one bounded repair and no explorer', async () => {
    const context = fixture({ status: 'miss', reason: 'stale-build' })
    const result = await context.cache.run(identity)
    expect(result).toMatchObject({
      cacheStatus: 'miss', cacheMissReason: 'stale-build', outcome: 'repaired',
      modelCalls: 0, repairCount: 1,
    })
    expect(result.diffs[0].autoCommit).toBe(false)
    expect(context.counts()).toEqual({
      runnerCalls: 0, explorerConstructions: 0, explorerCalls: 0,
      repairConstructions: 1, repairCalls: 1,
    })
  })

  test('a stale selector on an exact hit enters only the bounded repair plugin', async () => {
    const context = fixture(hit, [execution(false, 'selector')])
    const result = await context.cache.run(identity)
    expect(result.outcome).toBe('repaired')
    expect(result.repairCount).toBe(1)
    expect(context.counts()).toEqual({
      runnerCalls: 1, explorerConstructions: 0, explorerCalls: 0,
      repairConstructions: 1, repairCalls: 1,
    })
    expect(context.store.validations).toHaveLength(0)
  })

  for (const category of ['product_assertion', 'unknown'] as const) {
    test(`${category} reports and stops without constructing a repairer or explorer`, async () => {
      const context = fixture(hit, [execution(false, category)])
      const result = await context.cache.run(identity)
      expect(result.passed).toBe(false)
      expect(result.outcome).toBe(category === 'product_assertion' ? 'product-regression' : 'failed')
      expect(result.repairCount).toBe(0)
      expect(result.diffs).toHaveLength(0)
      expect(context.counts()).toEqual({
        runnerCalls: 1, explorerConstructions: 0, explorerCalls: 0,
        repairConstructions: 0, repairCalls: 0,
      })
      expect(context.store.validations).toEqual([{ passed: false, at: timestamp }])
    })
  }

  test('remember independently verifies before persistence and rejects failures without exploration', async () => {
    const passing = fixture(hit)
    const remembered = await passing.cache.remember(identity, 'candidate.yaml')
    expect(remembered).toMatchObject({ outcome: 'remembered', passed: true, modelCalls: 0, assertions: 9 })
    expect(passing.store.persistCalls).toBe(1)
    expect(passing.counts().explorerConstructions).toBe(0)

    const failing = fixture(hit, [execution(false, 'product_assertion')])
    const rejected = await failing.cache.remember(identity, 'candidate.yaml')
    expect(rejected).toMatchObject({ outcome: 'rejected', passed: false, repairCount: 0 })
    expect(failing.store.persistCalls).toBe(0)
    expect(failing.counts().repairConstructions).toBe(0)
  })

  test('rejected exploration and repair remain failures rather than falling through to a pass', async () => {
    const cold = fixture({ status: 'miss', reason: 'not-found' })
    cold.setExploration(exploration(false))
    const rejectedCold = await cold.cache.run(identity)
    expect(rejectedCold).toMatchObject({ outcome: 'exploration-rejected', passed: false, yamlPath: null })

    const stale = fixture({ status: 'miss', reason: 'stale-build' })
    stale.setRepair(repair('rejected'))
    const rejectedRepair = await stale.cache.run(identity)
    expect(rejectedRepair).toMatchObject({ outcome: 'repair-rejected', passed: false, repairCount: 0 })
    expect(stale.counts().explorerConstructions).toBe(0)
  })
})
