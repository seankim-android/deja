import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import {
  CONTRACT_VERSION,
  createDeja,
  UnsupportedPlatformExplorer,
  type CreateDejaOptions,
  type DejaAdapter,
  type ExecutionResult,
  type ExplorationResult,
  type GoalEmbeddingProvider,
  type LookupHit,
  type LookupMiss,
  type PersistedWalk,
  type RunnerRequest,
  type RunResult,
  type StoredWalk,
  type WalkIdentity,
  type WalkRunner,
  type WalkStore,
} from '../src/index.ts'

const timestamp = '2026-11-22T00:00:00.000Z'
const fingerprint = 'b'.repeat(64)

const adapter: DejaAdapter = {
  id: 'fixture-adapter',
  platform: 'fixture-platform',
  buildFingerprint: async () => fingerprint,
  cacheIdentity: ({ buildFingerprint }) => ({ buildFingerprint, adapter: 'fixture-adapter' }),
}

function storedFor(identity: WalkIdentity): StoredWalk {
  return {
    ...identity,
    goalKey: 'checkout',
    flowPath: '.deja/flows/checkout/fixture.yaml',
    flowSha256: 'f'.repeat(64),
    lastValidation: { passed: true, at: timestamp },
    validationHistory: [{ passed: true, at: timestamp, flowSha256: 'f'.repeat(64) }],
  }
}

const previousFingerprint = 'c'.repeat(64)

class FakeStore implements WalkStore {
  hit = false
  /** A walk verified on another build exists, so an exact lookup says `stale-build`. */
  staleWithPrevious = false
  persisted: WalkIdentity[] = []
  async lookup(identity: WalkIdentity): Promise<LookupHit | LookupMiss> {
    if (this.staleWithPrevious) return { status: 'miss', reason: 'stale-build' }
    if (!this.hit) return { status: 'miss', reason: 'not-found' }
    const walk = storedFor(identity)
    return { status: 'hit', walk, flowPath: walk.flowPath, flowSha256: walk.flowSha256 } as LookupHit
  }
  async lookupPreviousSuccessful(identity: WalkIdentity): Promise<LookupHit | LookupMiss> {
    if (!this.staleWithPrevious) return { status: 'miss', reason: 'not-found' }
    const walk = storedFor({ ...identity, buildFingerprint: previousFingerprint })
    return { status: 'hit', walk, flowPath: walk.flowPath, flowSha256: walk.flowSha256 } as LookupHit
  }
  async persistVerifiedCandidate(options: { identity: WalkIdentity }): Promise<PersistedWalk> {
    this.persisted.push(options.identity)
    const walk = storedFor(options.identity)
    return {
      walk, flowPath: walk.flowPath, flowSha256: walk.flowSha256,
      storedMetadataPath: '.deja/memory/walks.json', atomicWrite: true,
    } as PersistedWalk
  }
  async persistVerifiedRepair(): Promise<PersistedWalk> {
    throw new Error('not used')
  }
  async recordValidation(identity: WalkIdentity, validation: { passed: boolean; at: string }) {
    return { ...storedFor(identity), lastValidation: validation }
  }
  async list() {
    return []
  }
}

const passingExecution: ExecutionResult = {
  passed: true, startedAt: timestamp, endedAt: timestamp, durationMs: 5, exitCode: 0,
  assertions: 3, failure: null, artifacts: [],
}

const fakeRunner: WalkRunner = { run: async () => passingExecution }

function exploration(flowPath: string): ExplorationResult {
  return {
    passed: true, durationMs: 10, explorationDurationMs: 5, verificationDurationMs: 5,
    mcpCalls: 1, modelCalls: 1, modelCallBasis: 'fixture', model: 'fixture-model',
    provider: 'fixture-provider', assertions: 3, flowPath, flowSha256: 'f'.repeat(64),
    failure: null, artifacts: [], errors: [],
  }
}

function options(store: FakeStore, extra: Partial<CreateDejaOptions> = {}): CreateDejaOptions {
  return {
    workspaceRoot: '.',
    app: { appId: 'fixture.app', buildPath: 'build/app.bin' },
    adapter,
    store,
    runner: fakeRunner,
    explorer: ({ identity }) => ({ explore: async () => exploration(storedFor(identity).flowPath) }),
    ...extra,
  }
}

describe('createDeja', () => {
  test('exports a numeric contract version', () => {
    expect(CONTRACT_VERSION).toBe(1)
  })

  test('the SDK plan usage example runs against fakes and builds the identity', async () => {
    const store = new FakeStore()
    store.hit = true
    const deja = createDeja(options(store))
    const result: RunResult = await deja.run('checkout')
    expect(result.schemaVersion).toBe(1)
    expect(result.passed).toBe(true)
    expect(result.outcome).toBe('passed')
    expect(result.cacheStatus).toBe('hit')
    expect(result.modelCalls).toBe(0)
    expect(result.identity).toEqual({
      goal: 'checkout',
      appId: 'fixture.app',
      platform: 'fixture-platform',
      flowSchemaVersion: 'maestro/v1',
      buildFingerprint: fingerprint,
      adapter: 'fixture-adapter',
    })
    expect(await deja.identity('checkout')).toEqual(result.identity)
  })

  test('a cache hit constructs no explorer, repairer or embedding client', async () => {
    const store = new FakeStore()
    store.hit = true
    const constructed: string[] = []
    const deja = createDeja(options(store, {
      explorer: () => {
        constructed.push('explorer')
        throw new Error('explorer must not be built')
      },
      repairer: () => {
        constructed.push('repairer')
        throw new Error('repairer must not be built')
      },
      semantic: {
        createProvider: (): GoalEmbeddingProvider => {
          constructed.push('embedding')
          throw new Error('embedding client must not be built')
        },
      },
    }))
    const result = await deja.run('checkout')
    expect(result.outcome).toBe('passed')
    expect(constructed).toEqual([])
  })

  test('a miss builds the explorer once, with the call identity', async () => {
    const store = new FakeStore()
    const seen: WalkIdentity[] = []
    const deja = createDeja(options(store, {
      explorer: ({ identity }) => {
        seen.push(identity)
        return { explore: async () => exploration('.deja/flows/checkout/fixture.yaml') }
      },
    }))
    const result = await deja.run('checkout')
    expect(result.outcome).toBe('cold-recorded')
    expect(seen).toHaveLength(1)
    expect(seen[0]?.buildFingerprint).toBe(fingerprint)
  })

  test('the default reporter does nothing and a custom reporter receives results', async () => {
    const store = new FakeStore()
    store.hit = true
    await createDeja(options(store)).run('checkout')
    const reported: RunResult[] = []
    const deja = createDeja(options(store, {
      reporter: {
        report: async result => {
          reported.push(result)
          return { reportPath: 'a', latestPath: 'b' }
        },
      },
    }))
    await deja.run('checkout')
    expect(reported).toHaveLength(1)
  })

  test('web and iOS get the same refusal from core, with zero model calls', async () => {
    for (const platform of ['web', 'ios'] as const) {
      const deja = createDeja(options(new FakeStore(), {
        explorer: () => new UnsupportedPlatformExplorer(platform),
      }))
      const result = await deja.explore('checkout')
      expect(result.outcome).toBe('exploration-rejected')
      expect(result.modelCalls).toBe(0)
    }
  })

  test('no provider key appears in options, reports or errors', async () => {
    const secret = 'sk-test-secret-value-1234567890'
    // A key option does not type-check.
    const withKey: CreateDejaOptions = {
      ...options(new FakeStore()),
      // @ts-expect-error createDeja has no provider key option
      apiKey: secret,
    }
    expect(withKey).toBeDefined()

    // The declared option names never mention a credential.
    const source = readFileSync(new URL('../src/create-deja.ts', import.meta.url), 'utf8')
    const declared = source.slice(
      source.indexOf('export type CreateDejaOptions'), source.indexOf('export type DejaClient'),
    )
    expect(declared).not.toMatch(/api[-_]?key|secret|token|password|credential/i)

    // Results from a miss, a hit and a rejected exploration carry nothing the host did not report.
    process.env.FIXTURE_PROVIDER_KEY = secret
    try {
      const results: RunResult[] = []
      for (const hit of [false, true]) {
        const store = new FakeStore()
        store.hit = hit
        results.push(await createDeja(options(store)).run('checkout'))
      }
      const rejecting = createDeja(options(new FakeStore(), {
        explorer: () => ({
          explore: async () => ({ ...exploration('x'), passed: false, errors: ['verification rejected'] }),
        }),
      }))
      results.push(await rejecting.run('checkout'))
      expect(JSON.stringify(results)).not.toContain(secret)
    } finally {
      delete process.env.FIXTURE_PROVIDER_KEY
    }
  })

  describe('staleBuild and per-call options', () => {
    const iosAdapter: DejaAdapter = { ...adapter, platform: 'ios', cacheIdentity: ({ buildFingerprint }) => ({ buildFingerprint }) }

    test('staleBuild replays the last verified flow on a new build, with no explorer', async () => {
      const store = new FakeStore()
      store.staleWithPrevious = true
      const constructed: string[] = []
      const explorer = () => {
        constructed.push('explorer')
        throw new Error('explorer must not be built')
      }
      const without = await createDeja(options(store, { adapter: iosAdapter, explorer })).run('checkout')
      expect(without.outcome).toBe('repair-rejected')
      const result = await createDeja(options(store, {
        adapter: iosAdapter, explorer, staleBuild: 'replay-last-verified',
      })).run('checkout')
      expect(result.outcome).toBe('passed')
      expect(result.modelCalls).toBe(0)
      expect(result.replayedOnNewBuild?.previousBuildFingerprint).toBe(previousFingerprint)
      expect(store.persisted).toHaveLength(1)
      expect(store.persisted[0]?.buildFingerprint).toBe(fingerprint)
      expect(constructed).toEqual([])
    })

    test('timeoutMs reaches the runner and an aborted signal stops the run', async () => {
      const store = new FakeStore()
      store.hit = true
      const requests: RunnerRequest[] = []
      const runner: WalkRunner = {
        run: async request => {
          requests.push(request)
          return passingExecution
        },
      }
      const deja = createDeja(options(store, { runner }))
      await deja.run('checkout', { timeoutMs: 1234 })
      expect(requests[0]?.timeoutMs).toBe(1234)
      await deja.run('checkout')
      expect(requests[1]?.timeoutMs).toBeUndefined()
      const controller = new AbortController()
      controller.abort(new Error('stop'))
      await expect(deja.run('checkout', { signal: controller.signal })).rejects.toThrow('stop')
      expect(requests).toHaveLength(2)
    })
  })
})
