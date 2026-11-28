import { mkdir, mkdtemp, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { evaluateGate, gatePassed, type GateGoalResult, type GateIdentity, type WalkStore } from '@deja-run/core'
import { resolveHarness, type ContractHarness } from './harness.ts'
import type { PortableStoreImplementation } from './portable-contract.ts'
import {
  contractIdentity, initialValidation, laterValidation, type WalkStoreContractImplementation,
} from './walk-store-contract.ts'

const FLOW = 'appId: dev.deja.checkout\n---\n- assertVisible: Order confirmed\n'
const OTHER_BUILD = 'b'.repeat(64)

function gateIdentity(overrides: Partial<GateIdentity> = {}): GateIdentity {
  const { goal: _goal, ...rest } = contractIdentity()
  return { ...rest, ...overrides }
}

/** The one outcome row a store must give for a goal, whatever it keeps its history in. */
function row(
  goal: string,
  outcome: GateGoalResult['outcome'],
  extra: Partial<GateGoalResult> = {},
): GateGoalResult {
  return {
    goal, goalKey: goal.toLowerCase(), outcome, validatedAt: null, flowSha256: null, closureSha256: null,
    ...extra,
  }
}

/**
 * The release gate (DEJA-8) must give the same answer for the same history from every store. These
 * cases use only `WalkStore.list` and `WalkStore.lookup`, so any store can run them, including a
 * third-party one. The gate only reads: each case checks the history is unchanged afterwards.
 */
export function runGateContractTests(implementation: PortableStoreImplementation, harness?: ContractHarness) {
  const { describe, test, expect } = resolveHarness(harness)

  async function context() {
    const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-gate-contract-')))
    const workspaceRoot = join(temporary, 'workspace')
    await mkdir(join(workspaceRoot, '.deja/attempts'), { recursive: true })
    const candidatePath = '.deja/attempts/candidate.yaml'
    await writeFile(join(workspaceRoot, candidatePath), FLOW)
    const stores: WalkStore[] = []
    const store = implementation.createStore(workspaceRoot)
    stores.push(store)
    const persist = (goal: string, at: string, buildFingerprint = contractIdentity().buildFingerprint) =>
      store.persistVerifiedCandidate({
        identity: contractIdentity({ goal, buildFingerprint }), candidatePath,
        validation: { passed: true, at }, origin: 'explore',
      })
    const fail = (goal: string, at: string) =>
      store.recordValidation(contractIdentity({ goal }), { passed: false, at })
    return {
      store, persist, fail,
      cleanup: async () => {
        for (const item of stores) await implementation.closeStore?.(item)
        await rm(temporary, { recursive: true, force: true })
      },
    }
  }

  describe(`${implementation.name}: release gate`, () => {
    test('reports passed, failed, flaky and not-run, and opens only when every goal passed', async () => {
      const ctx = await context()
      try {
        const sha = (await ctx.persist('Pass', initialValidation)).flowSha256
        await ctx.persist('Fail', initialValidation)
        await ctx.fail('Fail', laterValidation)
        await ctx.persist('Flake', initialValidation)
        await ctx.fail('Flake', laterValidation)
        const before = await ctx.store.list()

        const results = await evaluateGate(
          ctx.store, ['Pass', 'Fail', 'Flake', 'Never run'], gateIdentity(),
          { flakyFailures: [{ goalKey: 'flake', buildFingerprint: contractIdentity().buildFingerprint, at: laterValidation }] },
        )
        expect(results.map(result => [result.goal, result.outcome])).toEqual([
          ['Pass', 'passed'], ['Fail', 'failed'], ['Flake', 'flaky'], ['Never run', 'not-run'],
        ])
        expect(results[0]).toEqual(row('Pass', 'passed', { validatedAt: initialValidation, flowSha256: sha }))
        expect(results[1]).toMatchObject({ validatedAt: laterValidation, flowSha256: sha })
        expect(results[2]).toMatchObject({ validatedAt: laterValidation, flowSha256: sha })
        expect(results[3]).toMatchObject({ validatedAt: null, flowSha256: null })
        expect(gatePassed(results)).toBe(false)
        expect(gatePassed(results.slice(0, 1))).toBe(true)
        // The gate wrote nothing.
        expect(await ctx.store.list()).toEqual(before)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a flaky report counts only for the failed validation it names, never for a pass', async () => {
      const ctx = await context()
      try {
        await ctx.persist('Flake', initialValidation)
        await ctx.fail('Flake', laterValidation)
        await ctx.persist('Pass', initialValidation)
        const flakyFailures = [
          { goalKey: 'flake', buildFingerprint: contractIdentity().buildFingerprint, at: initialValidation },
          { goalKey: 'pass', buildFingerprint: contractIdentity().buildFingerprint, at: initialValidation },
          { goalKey: 'flake', buildFingerprint: OTHER_BUILD, at: laterValidation },
        ]
        const results = await evaluateGate(ctx.store, ['Flake', 'Pass'], gateIdentity(), { flakyFailures })
        expect(results.map(result => result.outcome)).toEqual(['failed', 'passed'])
      } finally {
        await ctx.cleanup()
      }
    })

    test('a newer failure after an older pass on the same build closes the gate, and a newer pass reopens it', async () => {
      const ctx = await context()
      try {
        await ctx.persist('Checkout', initialValidation)
        await ctx.fail('Checkout', laterValidation)
        expect((await evaluateGate(ctx.store, ['Checkout'], gateIdentity()))[0].outcome).toBe('failed')
        await ctx.persist('Checkout', '2026-11-22T00:02:00.000Z')
        expect(await evaluateGate(ctx.store, ['Checkout'], gateIdentity())).toMatchObject([
          { outcome: 'passed', validatedAt: '2026-11-22T00:02:00.000Z' },
        ])
      } finally {
        await ctx.cleanup()
      }
    })

    test('a pass on another build never counts, and each build is judged on its own history', async () => {
      const ctx = await context()
      try {
        await ctx.persist('Checkout', initialValidation, OTHER_BUILD)
        const here = await evaluateGate(ctx.store, ['Checkout'], gateIdentity())
        expect(here).toMatchObject([{ outcome: 'not-run', validatedAt: null }])
        const there = await evaluateGate(ctx.store, ['Checkout'], gateIdentity({ buildFingerprint: OTHER_BUILD }))
        expect(there).toMatchObject([{ outcome: 'passed' }])
        // Another app, platform or adapter is not this build either.
        expect((await evaluateGate(ctx.store, ['Checkout'], gateIdentity({ appId: 'dev.deja.other' })))[0].outcome).toBe('not-run')
        expect((await evaluateGate(ctx.store, ['Checkout'], gateIdentity({ platform: 'ios' })))[0].outcome).toBe('not-run')
      } finally {
        await ctx.cleanup()
      }
    })

    test('latest is write order, never a timestamp compare: equal timestamps and clock skew cannot reorder', async () => {
      const ctx = await context()
      try {
        // A failure recorded later but stamped earlier than the pass is still the latest.
        await ctx.persist('Skewed', laterValidation)
        await ctx.fail('Skewed', initialValidation)
        // A failure stamped the same instant as the pass, recorded after it.
        await ctx.persist('Equal', initialValidation)
        await ctx.fail('Equal', initialValidation)
        // A pass recorded after a failure, stamped earlier than it.
        await ctx.persist('Recovered', initialValidation)
        await ctx.fail('Recovered', laterValidation)
        await ctx.persist('Recovered', initialValidation)
        const results = await evaluateGate(ctx.store, ['Skewed', 'Equal', 'Recovered'], gateIdentity())
        expect(results.map(result => [result.goal, result.outcome])).toEqual([
          ['Skewed', 'failed'], ['Equal', 'failed'], ['Recovered', 'passed'],
        ])
      } finally {
        await ctx.cleanup()
      }
    })

    test('goals are matched by normalized text and listed once', async () => {
      const ctx = await context()
      try {
        await ctx.persist('Check   Out', initialValidation)
        const results = await evaluateGate(ctx.store, ['check out', '  CHECK OUT ', 'Check   Out'], gateIdentity())
        expect(results).toHaveLength(1)
        expect(results[0]).toMatchObject({ goalKey: 'check out', outcome: 'passed' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('an empty goal list never passes', () => {
      expect(gatePassed([])).toBe(false)
    })
  })
}

/**
 * The cases that need a store's own files: a stored flow that was changed or removed, metadata that
 * is corrupt, a manifest flow that changed, and a writer that holds the lock while the gate reads.
 */
export function runGateStoreContractTests(implementation: WalkStoreContractImplementation, harness?: ContractHarness) {
  const { describe, test, expect } = resolveHarness(harness)

  async function context() {
    const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-gate-store-')))
    const workspaceRoot = join(temporary, 'workspace')
    await mkdir(join(workspaceRoot, '.deja/attempts'), { recursive: true })
    await mkdir(join(workspaceRoot, '.maestro/flows'), { recursive: true })
    await writeFile(join(workspaceRoot, '.deja/attempts/candidate.yaml'), FLOW)
    const stores: WalkStore[] = []
    const open = (manifest = false) => {
      const store = implementation.createStore(workspaceRoot, undefined, undefined, manifest ? { path: 'deja.walks.json' } : undefined)
      stores.push(store)
      return store
    }
    return {
      workspaceRoot, open,
      cleanup: async () => {
        for (const store of stores.reverse()) await implementation.closeStore(store)
        await rm(temporary, { recursive: true, force: true })
      },
    }
  }

  const seed = (store: WalkStore, goal = 'Checkout') => store.persistVerifiedCandidate({
    identity: contractIdentity({ goal }), candidatePath: '.deja/attempts/candidate.yaml',
    validation: { passed: true, at: initialValidation }, origin: 'explore',
  })

  describe(`${implementation.name}: release gate (store files)`, () => {
    test('metadata that cannot be read or trusted is an error, never a pass', async () => {
      for (const kind of ['malformed', 'unsupported-schema', 'invalid-entry', 'duplicate-identity'] as const) {
        const ctx = await context()
        try {
          await seed(ctx.open())
          await implementation.tamperMetadata(ctx.workspaceRoot, kind)
          await expect(evaluateGate(ctx.open(), ['Checkout'], gateIdentity())).rejects.toThrow()
        } finally {
          await ctx.cleanup()
        }
      }
    })

    test('a stored flow that was changed or removed is invalid, never a pass', async () => {
      const ctx = await context()
      try {
        const persisted = await seed(ctx.open())
        await writeFile(join(ctx.workspaceRoot, persisted.flowPath), `${FLOW}- assertVisible: Anything\n`)
        expect(await evaluateGate(ctx.open(), ['Checkout'], gateIdentity())).toMatchObject([{ outcome: 'invalid' }])
        await rm(join(ctx.workspaceRoot, persisted.flowPath))
        expect(await evaluateGate(ctx.open(), ['Checkout'], gateIdentity())).toMatchObject([{ outcome: 'invalid' }])
      } finally {
        await ctx.cleanup()
      }
    })

    test('it reads while another run holds the writer lock, and takes no lock or file of its own', async () => {
      const ctx = await context()
      try {
        const writer = ctx.open()
        await seed(writer)
        const locking = writer as WalkStore & { acquireWriter(): void }
        locking.acquireWriter()
        const before = await implementation.snapshotMetadata(ctx.workspaceRoot)
        const reader = ctx.open()
        expect(await evaluateGate(reader, ['Checkout'], gateIdentity())).toMatchObject([{ outcome: 'passed' }])
        expect((await implementation.snapshotMetadata(ctx.workspaceRoot)).equals(before)).toBe(true)
        // The writer still holds its lock: the gate neither took nor released it.
        expect(() => locking.acquireWriter()).not.toThrow()
      } finally {
        await ctx.cleanup()
      }
    })

    test('manifest mode: a goal with only an old-store walk needs migration and closes the gate', async () => {
      const ctx = await context()
      try {
        await seed(ctx.open(), 'Today')
        const results = await evaluateGate(ctx.open(true), ['Today'], gateIdentity())
        expect(results).toMatchObject([{ outcome: 'not-run' }])
        expect(results[0].detail).toMatch(/needs-migration/)
        expect(gatePassed(results)).toBe(false)
      } finally {
        await ctx.cleanup()
      }
    })

    test('manifest mode: a failure is reported for the manifest\'s current closure only', async () => {
      const ctx = await context()
      try {
        const identity = { adapter: 'maestro-ios/v1', platform: 'ios' }
        await writeFile(join(ctx.workspaceRoot, '.maestro/flows/today.yaml'), FLOW)
        const store = ctx.open(true)
        const persisted = await store.persistVerifiedCandidate({
          identity: contractIdentity({ goal: 'Today', ...identity }), candidatePath: '.maestro/flows/today.yaml',
          validation: { passed: true, at: initialValidation }, origin: 'remember',
        })
        await store.recordValidation(contractIdentity({ goal: 'Today', ...identity }), { passed: false, at: laterValidation })
        const results = await evaluateGate(ctx.open(true), ['Today'], gateIdentity(identity))
        expect(results).toMatchObject([{ outcome: 'failed', validatedAt: laterValidation, flowSha256: persisted.flowSha256 }])
        expect(results[0].closureSha256).toMatch(/^[0-9a-f]{64}$/)
      } finally {
        await ctx.cleanup()
      }
    })

    test('manifest mode: a flow that changed since it was validated is flow-changed, even with a recorded pass', async () => {
      const ctx = await context()
      try {
        const adapter = 'maestro-ios/v1'
        const identity = { adapter, platform: 'ios' }
        await mkdir(join(ctx.workspaceRoot, '.maestro/flows'), { recursive: true })
        await writeFile(join(ctx.workspaceRoot, '.maestro/flows/today.yaml'), FLOW)
        const store = ctx.open(true)
        const persisted = await store.persistVerifiedCandidate({
          identity: contractIdentity({ goal: 'Today', ...identity }),
          candidatePath: '.maestro/flows/today.yaml',
          validation: { passed: true, at: initialValidation }, origin: 'remember',
        })
        const gate = () => evaluateGate(ctx.open(true), ['Today'], gateIdentity(identity))
        expect(await gate()).toMatchObject([{ outcome: 'passed', validatedAt: initialValidation }])
        expect((await gate())[0].closureSha256).toMatch(/^[0-9a-f]{64}$/)
        // Another build has no validation, in manifest mode too.
        expect((await evaluateGate(ctx.open(true), ['Today'], gateIdentity({ ...identity, buildFingerprint: OTHER_BUILD })))[0].outcome)
          .toBe('not-run')

        await writeFile(join(ctx.workspaceRoot, persisted.flowPath), `${FLOW}- assertVisible: Changed\n`)
        const changed = await gate()
        expect(changed).toMatchObject([{ outcome: 'flow-changed' }])
        expect(changed[0].detail).toMatch(/manifest [0-9a-f]{64}, now [0-9a-f]{64}/)
        expect(gatePassed(changed)).toBe(false)
      } finally {
        await ctx.cleanup()
      }
    })
  })
}
