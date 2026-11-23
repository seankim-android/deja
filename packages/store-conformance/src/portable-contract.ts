import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { ExpectedFlowChecksumMismatchError, type WalkStore } from '@deja-qa/core'
import { resolveHarness, type ContractHarness } from './harness.ts'
import { contractIdentity, initialValidation, laterValidation } from './walk-store-contract.ts'

/**
 * What a store needs to run the portable suite. Unlike `WalkStoreContractImplementation`, nothing
 * here assumes where the store keeps its metadata: a store that holds it in memory or in a remote
 * service can pass. The store still writes the verified flow as a file under the workspace, because
 * the runner executes a file.
 */
export type PortableStoreImplementation = {
  name: string
  /** A store for `workspaceRoot`. Two stores for the same root must see the same walks. */
  createStore(workspaceRoot: string): WalkStore
  closeStore?(store: WalkStore): void | Promise<void>
}

/** The behavior every `WalkStore` shares, whatever it uses to remember walks. */
export function runPortableWalkStoreContractTests(implementation: PortableStoreImplementation, harness?: ContractHarness) {
  const { describe, test, expect } = resolveHarness(harness)

  async function context() {
    const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-portable-contract-')))
    const workspaceRoot = join(temporary, 'workspace')
    await mkdir(join(workspaceRoot, '.deja/attempts'), { recursive: true })
    const candidatePath = '.deja/attempts/candidate.yaml'
    const yaml = 'appId: dev.deja.checkout\n---\n- assertVisible: Order confirmed\n'
    await writeFile(join(workspaceRoot, candidatePath), yaml)
    const stores: WalkStore[] = []
    const makeStore = () => {
      const store = implementation.createStore(workspaceRoot)
      stores.push(store)
      return store
    }
    const seed = async (store: WalkStore, overrides: Parameters<typeof contractIdentity>[0] = {}) => store.persistVerifiedCandidate({
      identity: contractIdentity(overrides), candidatePath,
      validation: { passed: true, at: initialValidation }, origin: 'explore',
    })
    const cleanup = async () => {
      for (const store of stores) await implementation.closeStore?.(store)
      await rm(temporary, { recursive: true, force: true })
    }
    return { workspaceRoot, candidatePath, yaml, makeStore, seed, cleanup }
  }

  describe(`${implementation.name}: portable WalkStore contract`, () => {
    test('a first run is a miss with nothing listed', async () => {
      const ctx = await context()
      try {
        const store = ctx.makeStore()
        expect(await store.lookup(contractIdentity())).toEqual({ status: 'miss', reason: 'not-found' })
        expect(await store.list()).toEqual([])
      } finally {
        await ctx.cleanup()
      }
    })

    test('a verified walk persists across instances, normalizes the goal and replays the stored bytes', async () => {
      const ctx = await context()
      try {
        const persisted = await ctx.seed(ctx.makeStore(), { goal: '  Check   Out  ' })
        const result = await ctx.makeStore().lookup(contractIdentity({ goal: '\tcheck  out\n' }))
        expect(result.status).toBe('hit')
        if (result.status !== 'hit') throw new Error('expected hit')
        expect(result.walk.goal).toBe('Check   Out')
        expect(result.walk.goalKey).toBe('check out')
        expect(result.walk.flowSha256).toBe(persisted.flowSha256)
        expect(result.walk.lastValidation.passed).toBe(true)
        expect(result.walk.validationHistory).toHaveLength(1)
        expect(result.flowPath).toBe(resolve(ctx.workspaceRoot, persisted.flowPath))
        expect(await readFile(result.flowPath, 'utf8')).toBe(ctx.yaml)
      } finally {
        await ctx.cleanup()
      }
    })

    test('another app, platform or build is a typed miss, never a hit', async () => {
      const ctx = await context()
      try {
        const store = ctx.makeStore()
        await ctx.seed(store)
        expect(await store.lookup(contractIdentity({ appId: 'dev.deja.other' }))).toEqual({ status: 'miss', reason: 'wrong-app' })
        expect(await store.lookup(contractIdentity({ platform: 'ios' }))).toEqual({ status: 'miss', reason: 'wrong-platform' })
        expect(await store.lookup(contractIdentity({ buildFingerprint: 'b'.repeat(64) }))).toEqual({ status: 'miss', reason: 'stale-build' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('a candidate whose bytes differ from expectedFlowSha256 is refused before anything is stored', async () => {
      const ctx = await context()
      try {
        const store = ctx.makeStore()
        await expect(store.persistVerifiedCandidate({
          identity: contractIdentity(), candidatePath: ctx.candidatePath,
          validation: { passed: true, at: initialValidation }, origin: 'remember',
          expectedFlowSha256: 'c'.repeat(64),
        })).rejects.toBeInstanceOf(ExpectedFlowChecksumMismatchError)
        expect(await store.list()).toEqual([])
        expect(await store.lookup(contractIdentity())).toEqual({ status: 'miss', reason: 'not-found' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('a failed validation is appended to history and blocks reuse', async () => {
      const ctx = await context()
      try {
        const store = ctx.makeStore()
        const persisted = await ctx.seed(store)
        const updated = await store.recordValidation(contractIdentity(), { passed: false, at: laterValidation })
        expect(updated.lastValidation).toEqual({ passed: false, at: laterValidation })
        expect(updated.validationHistory.map(entry => entry.passed)).toEqual([true, false])
        expect(updated.validationHistory[1].flowSha256).toBe(persisted.flowSha256)
        expect(await store.lookup(contractIdentity())).toEqual({ status: 'miss', reason: 'failed-validation' })
        const stored = await ctx.makeStore().list()
        expect(stored).toHaveLength(1)
        expect(stored[0].validationHistory.map(entry => entry.passed)).toEqual([true, false])
      } finally {
        await ctx.cleanup()
      }
    })

    test('persisting the same identity again replaces the walk instead of adding a second one', async () => {
      const ctx = await context()
      try {
        const store = ctx.makeStore()
        await ctx.seed(store)
        await store.persistVerifiedCandidate({
          identity: contractIdentity(), candidatePath: ctx.candidatePath,
          validation: { passed: true, at: laterValidation }, origin: 'remember',
        })
        const stored = await store.list()
        expect(stored).toHaveLength(1)
        expect(stored[0].lastValidation.at).toBe(laterValidation)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a stored flow that is gone is a missing-flow miss', async () => {
      const ctx = await context()
      try {
        const store = ctx.makeStore()
        const persisted = await ctx.seed(store)
        await rm(join(ctx.workspaceRoot, persisted.flowPath))
        expect(await store.lookup(contractIdentity())).toEqual({ status: 'miss', reason: 'missing-flow' })
      } finally {
        await ctx.cleanup()
      }
    })
  })
}
