import { describe, expect, test } from 'bun:test'
import { existsSync } from 'node:fs'
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  AssertionHashChangedError,
  ExpectedClosureMismatchError,
  FlowChangedError,
  InvalidWalkManifestError,
  OwnerRuleError,
  parseWalkManifest,
  type PersistVerifiedOptions,
  type WalkIdentity,
  type WalkStore,
} from '@deja-qa/core'
import { ConsumerFlowError } from '@deja-qa/store-local'
import { initialValidation, laterValidation, type WalkStoreContractImplementation } from './walk-store-contract.ts'

const MANIFEST = 'deja.walks.json'
const MAIN = '.maestro/flows/today.yaml'
const SUB = '.maestro/subflows/type.yaml'
const MAIN_YAML = [
  'appId: dev.deja.fixture',
  '---',
  '- launchApp:',
  '    clearState: true',
  '- assertVisible:',
  '    id: "screen.today"',
  '- tapOn:',
  '    id: "today.answer"',
  '- runFlow:',
  '    file: ../subflows/type.yaml',
  '    env:',
  '      TEXT: "Popcorn"',
  '- assertVisible:',
  '    id: "today.saved"',
  '',
].join('\n')
const SUB_YAML = 'appId: dev.deja.fixture\n---\n- inputText: ${TEXT}\n- assertVisible:\n    id: "space"\n'

function identity(overrides: Partial<WalkIdentity> = {}): WalkIdentity {
  return {
    goal: 'Today',
    appId: 'dev.deja.fixture',
    platform: 'ios',
    flowSchemaVersion: 'maestro/v1',
    buildFingerprint: 'a'.repeat(64),
    adapter: 'maestro-ios/v1',
    ...overrides,
  }
}

/**
 * The manifest-mode contract: the consumer's own files run in place, the manifest names them, and a
 * store keeps history only. Every case runs on a temporary workspace built here at run time.
 */
export function runConsumerFlowContractTests(implementation: WalkStoreContractImplementation) {
  async function context() {
    const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-consumer-contract-')))
    const workspaceRoot = join(temporary, 'repo')
    await mkdir(join(workspaceRoot, '.maestro/flows'), { recursive: true })
    await mkdir(join(workspaceRoot, '.maestro/subflows'), { recursive: true })
    await writeFile(join(workspaceRoot, MAIN), MAIN_YAML)
    await writeFile(join(workspaceRoot, SUB), SUB_YAML)
    const stores: WalkStore[] = []
    const open = (storeId?: string, manifest = true) => {
      const store = implementation.createStore(
        workspaceRoot, undefined, storeId, manifest ? { path: MANIFEST } : undefined,
      )
      stores.push(store)
      return store
    }
    const persist = (store: WalkStore, overrides: Partial<PersistVerifiedOptions> = {}) =>
      store.persistVerifiedCandidate({
        identity: identity(),
        candidatePath: MAIN,
        validation: { passed: true, at: initialValidation },
        origin: 'remember',
        ...overrides,
      })
    const manifestText = () => readFile(join(workspaceRoot, MANIFEST), 'utf8')
    return {
      workspaceRoot, open, persist, manifestText,
      write: (path: string, contents: string) => writeFile(join(workspaceRoot, path), contents),
      cleanup: async () => {
        for (const store of stores.reverse()) await implementation.closeStore(store)
        await rm(temporary, { recursive: true, force: true })
      },
    }
  }

  describe(`${implementation.name}: manifest mode (consumer-owned flows)`, () => {
    test('remember records the consumer file in the manifest and replays it in place, with no copy', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        expect(store.manifestPath).toBe(MANIFEST)
        const snapshot = await store.snapshotCandidate!(MAIN, '.deja/attempts/x/candidate')
        // Nothing is copied: the snapshot is the consumer's own path, plus the sha of its closure.
        expect(snapshot.candidatePath).toBe(MAIN)
        expect(existsSync(join(ctx.workspaceRoot, '.deja/attempts'))).toBe(false)
        const persisted = await ctx.persist(store, { expectedClosureSha256: snapshot.closureSha256 })
        expect(persisted.flowPath).toBe(MAIN)
        expect(persisted.manifest).toEqual({ path: MANIFEST, changed: true })
        expect(existsSync(join(ctx.workspaceRoot, '.deja/flows'))).toBe(false)

        const manifest = parseWalkManifest(await ctx.manifestText())
        expect(manifest.walks).toHaveLength(1)
        expect(manifest.walks[0]).toMatchObject({
          goal: 'Today', appId: 'dev.deja.fixture', platform: 'ios', adapter: 'maestro-ios/v1',
          flow: MAIN, closureSha256: snapshot.closureSha256,
        })
        // Machine data stays out of the committed file.
        expect(await ctx.manifestText()).not.toContain(ctx.workspaceRoot)
        expect(await ctx.manifestText()).not.toContain('2026-')

        const hit = await store.lookup(identity())
        if (hit.status !== 'hit') throw new Error(`expected hit, got ${hit.reason}`)
        expect(hit.flowPath).toBe(join(ctx.workspaceRoot, MAIN))
        expect(hit.walk).toMatchObject({ flowPath: MAIN, closureSha256: snapshot.closureSha256 })
        expect(hit.walk.subflowFiles).toBeUndefined()
        expect(await store.lookup(identity({ goal: ' TODAY ' }))).toMatchObject({ status: 'hit' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('a changed main flow or subflow is flow-changed, and restoring it is a hit again', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        await ctx.persist(store)
        expect((await store.lookup(identity())).status).toBe('hit')

        await ctx.write(MAIN, `${MAIN_YAML}# edited\n`)
        const mainChanged = await store.lookup(identity())
        expect(mainChanged).toMatchObject({ status: 'miss', reason: 'flow-changed' })
        if (mainChanged.status !== 'miss') throw new Error('expected miss')
        expect(mainChanged.flowChange?.manifestClosureSha256).not.toBe(mainChanged.flowChange?.currentClosureSha256)
        expect(mainChanged.flowPath).toBe(join(ctx.workspaceRoot, MAIN))
        await ctx.write(MAIN, MAIN_YAML)
        expect((await store.lookup(identity())).status).toBe('hit')

        await ctx.write(SUB, `${SUB_YAML}# edited\n`)
        expect(await store.lookup(identity())).toMatchObject({ status: 'miss', reason: 'flow-changed' })
        expect(await store.lookupPreviousSuccessful(identity())).toMatchObject({ status: 'miss', reason: 'flow-changed' })
        await ctx.write(SUB, SUB_YAML)
        expect((await store.lookup(identity())).status).toBe('hit')
      } finally {
        await ctx.cleanup()
      }
    })

    test('a missing main flow or subflow is missing-flow, never a hit', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        await ctx.persist(store)
        await rm(join(ctx.workspaceRoot, SUB))
        expect(await store.lookup(identity())).toEqual({ status: 'miss', reason: 'missing-flow' })
        await ctx.write(SUB, SUB_YAML)
        await rm(join(ctx.workspaceRoot, MAIN))
        expect(await store.lookup(identity())).toEqual({ status: 'miss', reason: 'missing-flow' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('a fresh clone (manifest entry, empty .deja) is not-validated, never a hit or a not-found', async () => {
      const ctx = await context()
      try {
        await ctx.persist(ctx.open())
        // A second checkout has the committed manifest and files but no machine data.
        await rm(join(ctx.workspaceRoot, '.deja'), { recursive: true, force: true })
        const fresh = ctx.open('fresh')
        expect(await fresh.lookup(identity())).toMatchObject({
          status: 'miss', reason: 'not-validated', flowPath: join(ctx.workspaceRoot, MAIN),
        })
        expect((await fresh.lookupPreviousSuccessful(identity())).status).toBe('miss')
        // verify accepts it: same closure, same assertions, so the manifest bytes stay put.
        const before = await ctx.manifestText()
        const verified = await ctx.persist(fresh, { origin: 'verify', validation: { passed: true, at: laterValidation } })
        expect(verified.manifest?.changed).toBe(false)
        expect(await ctx.manifestText()).toBe(before)
        expect((await fresh.lookup(identity())).status).toBe('hit')
      } finally {
        await ctx.cleanup()
      }
    })

    test('verify after a non-assertion edit updates the manifest sha and keeps the assertion hash', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        await ctx.persist(store)
        const before = parseWalkManifest(await ctx.manifestText()).walks[0]
        await ctx.write(MAIN, MAIN_YAML.replace('today.answer', 'today.answer.field'))
        const verified = await ctx.persist(store, {
          origin: 'verify', validation: { passed: true, at: laterValidation },
        })
        expect(verified.manifest?.changed).toBe(true)
        const after = parseWalkManifest(await ctx.manifestText()).walks[0]
        expect(after.closureSha256).not.toBe(before.closureSha256)
        expect(after.assertionsSha256).toBe(before.assertionsSha256)
        const hit = await store.lookup(identity())
        expect(hit.status).toBe('hit')
        if (hit.status === 'hit') {
          expect(hit.walk.origin).toBe('verify')
          // History is keyed by the new closure sha, so the old closure's entries are not carried over.
          expect(hit.walk.validationHistory).toHaveLength(1)
        }
      } finally {
        await ctx.cleanup()
      }
    })

    test('a changed assertion is refused by verify and by remember, and only a reviewed remember accepts it', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        await ctx.persist(store)
        const manifestBefore = await ctx.manifestText()
        const metadataBefore = await implementation.snapshotMetadata(ctx.workspaceRoot)
        // Weakened: an assertion removed.
        await ctx.write(MAIN, MAIN_YAML.replace('- assertVisible:\n    id: "today.saved"\n', ''))
        await expect(ctx.persist(store, { origin: 'verify' })).rejects.toBeInstanceOf(AssertionHashChangedError)
        await expect(ctx.persist(store)).rejects.toBeInstanceOf(AssertionHashChangedError)
        await expect(ctx.persist(store, { assertionChange: 'accept', origin: 'verify' }))
          .rejects.toBeInstanceOf(AssertionHashChangedError)
        expect(await ctx.manifestText()).toBe(manifestBefore)
        expect((await implementation.snapshotMetadata(ctx.workspaceRoot)).equals(metadataBefore)).toBe(true)

        const accepted = await ctx.persist(store, { assertionChange: 'accept' })
        expect(accepted.manifest?.changed).toBe(true)
        expect(await ctx.manifestText()).not.toBe(manifestBefore)
        expect((await store.lookup(identity())).status).toBe('hit')
      } finally {
        await ctx.cleanup()
      }
    })

    test('the assertion hash covers whole commands, conditions and scripts', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        await ctx.persist(store)
        for (const edit of [
          MAIN_YAML.replace('    id: "today.saved"\n', '    id: "today.saved"\n    optional: true\n'),
          MAIN_YAML.replace('- runFlow:\n    file: ../subflows/type.yaml', '- runFlow:\n    when:\n      visible: "x"\n    file: ../subflows/type.yaml'),
        ]) {
          await ctx.write(MAIN, edit)
          await expect(ctx.persist(store, { origin: 'verify' })).rejects.toBeInstanceOf(AssertionHashChangedError)
        }
        await ctx.write(MAIN, MAIN_YAML)
        // The values an assertion depends on count too: env, and how often a repeat runs.
        for (const edit of [
          MAIN_YAML.replace('TEXT: "Popcorn"', 'TEXT: "Other"'),
          MAIN_YAML.replace('---\n', 'env:\n  NAME: x\n---\n'),
        ]) {
          await ctx.write(MAIN, edit)
          await expect(ctx.persist(store, { origin: 'verify' })).rejects.toBeInstanceOf(AssertionHashChangedError)
        }
        await ctx.write(MAIN, `${MAIN_YAML}- repeat:\n    times: 2\n    commands:\n      - tapOn: more\n`)
        await ctx.persist(store, { assertionChange: 'accept' })
        await ctx.write(MAIN, `${MAIN_YAML}- repeat:\n    times: 0\n    commands:\n      - tapOn: more\n`)
        await expect(ctx.persist(store, { origin: 'verify' })).rejects.toBeInstanceOf(AssertionHashChangedError)
        await ctx.write(MAIN, MAIN_YAML)
        await ctx.persist(store, { assertionChange: 'accept' })
        // A script's bytes count: assertTrue can read what it sets.
        await ctx.write('.maestro/subflows/s.js', 'output.ok = true\n')
        await ctx.write(MAIN, `${MAIN_YAML}- runScript: ../subflows/s.js\n`)
        await expect(ctx.persist(store, { origin: 'verify' })).rejects.toBeInstanceOf(AssertionHashChangedError)
        await ctx.persist(store, { assertionChange: 'accept' })
        await ctx.write('.maestro/subflows/s.js', 'output.ok = false\n')
        await expect(ctx.persist(store, { origin: 'verify' })).rejects.toBeInstanceOf(AssertionHashChangedError)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a malformed, unknown-version or unknown-field manifest fails closed on every read', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        for (const text of [
          '{not json',
          JSON.stringify({ manifestVersion: 2, hashVersion: 1, walks: [] }),
          JSON.stringify({ manifestVersion: 1, hashVersion: 2, walks: [] }),
          JSON.stringify({ manifestVersion: 1, hashVersion: 1, walks: [], extra: true }),
          JSON.stringify({ manifestVersion: 1, hashVersion: 1, walks: [{ goal: 'Today' }] }),
        ]) {
          await ctx.write(MANIFEST, text)
          await expect(store.lookup(identity())).rejects.toBeInstanceOf(InvalidWalkManifestError)
          await expect(ctx.persist(store)).rejects.toBeInstanceOf(InvalidWalkManifestError)
        }
        expect(existsSync(implementation.metadataPath(ctx.workspaceRoot))).toBe(false)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a flow edited between verification and save is refused, and a run that raced an edit is not recorded', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        const snapshot = await store.snapshotCandidate!(MAIN, '.deja/attempts/x/candidate')
        await ctx.write(SUB, `${SUB_YAML}- assertVisible: "unverified"\n`)
        await expect(ctx.persist(store, { expectedClosureSha256: snapshot.closureSha256 }))
          .rejects.toBeInstanceOf(ExpectedClosureMismatchError)
        expect(existsSync(join(ctx.workspaceRoot, MANIFEST))).toBe(false)
        expect(existsSync(implementation.metadataPath(ctx.workspaceRoot))).toBe(false)

        await ctx.write(SUB, SUB_YAML)
        await ctx.persist(store)
        await ctx.write(SUB, `${SUB_YAML}# edited during the run\n`)
        await expect(store.recordValidation(identity(), { passed: true, at: laterValidation }))
          .rejects.toBeInstanceOf(FlowChangedError)
        await expect(store.recordValidation(identity(), { passed: false, at: laterValidation }))
          .rejects.toBeInstanceOf(FlowChangedError)
      } finally {
        await ctx.cleanup()
      }
    })

    test('history keeps per-build entries for one closure, and a new build is stale until validated', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        await ctx.persist(store)
        const next = identity({ buildFingerprint: 'b'.repeat(64) })
        expect(await store.lookup(next)).toMatchObject({ status: 'miss', reason: 'stale-build' })
        const previous = await store.lookupPreviousSuccessful(next)
        expect(previous.status).toBe('hit')
        await ctx.persist(store, {
          identity: next, origin: 'replayed-on-new-build', validation: { passed: true, at: laterValidation },
        })
        const hit = await store.lookup(next)
        expect(hit.status).toBe('hit')
        if (hit.status === 'hit') {
          expect(hit.walk.origin).toBe('replayed-on-new-build')
          expect(hit.walk.validationHistory.map(entry => entry.at)).toEqual([initialValidation, laterValidation])
        }
        await store.recordValidation(next, { passed: true, at: '2026-11-22T00:02:00.000Z' })
        expect((await store.list())[0].validationHistory).toHaveLength(3)
        // Replaying on a new build never changes the manifest.
        expect(parseWalkManifest(await ctx.manifestText()).walks).toHaveLength(1)
      } finally {
        await ctx.cleanup()
      }
    })

    test('replayed-on-new-build refuses a flow that changed since it was verified', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        await ctx.persist(store)
        await ctx.write(MAIN, `${MAIN_YAML}# edited\n`)
        await expect(ctx.persist(store, {
          identity: identity({ buildFingerprint: 'b'.repeat(64) }), origin: 'replayed-on-new-build',
        })).rejects.toBeInstanceOf(FlowChangedError)
      } finally {
        await ctx.cleanup()
      }
    })

    test('device stores keep their own history and share one manifest', async () => {
      const ctx = await context()
      try {
        const first = ctx.open('device-a')
        const second = ctx.open('device-b')
        await ctx.persist(first)
        // A second device validates the same committed flow: the manifest does not change.
        const before = await ctx.manifestText()
        const again = await ctx.persist(second, {
          identity: identity({ buildFingerprint: 'c'.repeat(64) }), origin: 'verify',
        })
        expect(again.manifest?.changed).toBe(false)
        expect(await ctx.manifestText()).toBe(before)
        expect((await first.lookup(identity())).status).toBe('hit')
        expect((await second.lookup(identity({ buildFingerprint: 'c'.repeat(64) }))).status).toBe('hit')
        expect(await second.lookup(identity())).toMatchObject({ status: 'miss', reason: 'stale-build' })
        expect(existsSync(implementation.metadataPath(ctx.workspaceRoot, 'device-a'))).toBe(true)
        expect(existsSync(implementation.metadataPath(ctx.workspaceRoot, 'device-b'))).toBe(true)
      } finally {
        await ctx.cleanup()
      }
    })

    test('two stores adding different goals at once both land in the manifest', async () => {
      const ctx = await context()
      try {
        await ctx.write('.maestro/flows/smoke.yaml', 'appId: dev.deja.fixture\n---\n- assertVisible:\n    id: "screen.today"\n')
        const first = ctx.open('device-a')
        const second = ctx.open('device-b')
        await Promise.all([
          ctx.persist(first),
          ctx.persist(second, { identity: identity({ goal: 'Smoke' }), candidatePath: '.maestro/flows/smoke.yaml' }),
        ])
        expect(parseWalkManifest(await ctx.manifestText()).walks.map(walk => walk.goal).sort())
          .toEqual(['Smoke', 'Today'])
        // The same bytes whichever store wrote last: sorted, no timestamps.
        const text = await ctx.manifestText()
        expect(text.indexOf('"Smoke"')).toBeLessThan(text.indexOf('"Today"'))
        expect(text.endsWith('}\n')).toBe(true)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a schema 3 walk without a manifest entry is needs-migration, and manifest mode never reads its copy', async () => {
      const ctx = await context()
      try {
        const legacy = ctx.open('legacy', false)
        await legacy.persistVerifiedCandidate({
          identity: identity(), candidatePath: MAIN,
          validation: { passed: true, at: initialValidation }, origin: 'remember',
        })
        await implementation.closeStore(legacy)
        const store = ctx.open('legacy')
        expect(await store.lookup(identity())).toEqual({ status: 'miss', reason: 'needs-migration' })
        // The copy is not read, so removing it changes nothing.
        await rm(join(ctx.workspaceRoot, '.deja/flows'), { recursive: true, force: true })
        expect(await store.lookup(identity())).toEqual({ status: 'miss', reason: 'needs-migration' })
        expect(await store.list()).toHaveLength(1)
        // remember on the consumer's own file replaces the old walk.
        await ctx.persist(store)
        expect((await store.lookup(identity())).status).toBe('hit')
        expect((await store.list()).every(walk => walk.closureSha256 !== undefined)).toBe(true)
      } finally {
        await ctx.cleanup()
      }
    })

    test('schema 3 and schema 4 walks live in one store file and read back', async () => {
      const ctx = await context()
      try {
        await ctx.write('.maestro/flows/smoke.yaml', 'appId: dev.deja.fixture\n---\n- assertVisible:\n    id: "screen.today"\n')
        const legacy = ctx.open('mixed', false)
        await legacy.persistVerifiedCandidate({
          identity: identity({ goal: 'Smoke' }), candidatePath: '.maestro/flows/smoke.yaml',
          validation: { passed: true, at: initialValidation }, origin: 'remember',
        })
        await implementation.closeStore(legacy)
        const store = ctx.open('mixed')
        await ctx.persist(store)
        const walks = await store.list()
        expect(walks.map(walk => walk.goalKey).sort()).toEqual(['smoke', 'today'])
        expect(walks.find(walk => walk.goalKey === 'smoke')?.subflowFiles).toEqual([])
        expect(walks.find(walk => walk.goalKey === 'today')?.closureSha256).toBeDefined()
        // A store without a manifest reads the same file: the old walk still replays its copy.
        await implementation.closeStore(store)
        const plain = ctx.open('mixed', false)
        expect((await plain.lookup(identity({ goal: 'Smoke' }))).status).toBe('hit')
      } finally {
        await ctx.cleanup()
      }
    })

    test('only the consumer\'s own file can be kept: a candidate under .deja or outside the workspace is refused', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        await mkdir(join(ctx.workspaceRoot, '.deja/attempts'), { recursive: true })
        await ctx.write('.deja/attempts/candidate.yaml', MAIN_YAML)
        await expect(ctx.persist(store, { candidatePath: '.deja/attempts/candidate.yaml' }))
          .rejects.toBeInstanceOf(ConsumerFlowError)
        await expect(store.snapshotCandidate!('.deja/attempts/candidate.yaml', '.deja/x'))
          .rejects.toBeInstanceOf(ConsumerFlowError)
        expect(existsSync(join(ctx.workspaceRoot, MANIFEST))).toBe(false)
      } finally {
        await ctx.cleanup()
      }
    })

    test('verify and a new-build replay cannot add an entry, and repair is refused', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        await expect(ctx.persist(store, { origin: 'verify' })).rejects.toBeInstanceOf(ConsumerFlowError)
        await expect(ctx.persist(store, { origin: 'replayed-on-new-build' })).rejects.toBeInstanceOf(ConsumerFlowError)
        await ctx.persist(store)
        await expect(store.persistVerifiedRepair({
          identity: identity({ buildFingerprint: 'd'.repeat(64) }), candidatePath: MAIN,
          validation: { passed: true, at: laterValidation },
          previous: { buildFingerprint: 'a'.repeat(64), flowSha256: 'e'.repeat(64) },
          repair: {
            attemptId: 'x', patchPath: '.deja/patches/x.diff',
            previousBuildFingerprint: 'a'.repeat(64), previousFlowSha256: 'e'.repeat(64),
          },
        })).rejects.toThrow(/Repair is not supported with a manifest/)
      } finally {
        await ctx.cleanup()
      }
    })

    test('owner rules in the manifest are enforced and preserved across writes', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        await ctx.persist(store)
        const manifest = parseWalkManifest(await ctx.manifestText())
        manifest.walks[0].ownerRules = { minAssertions: 3, requiredAssertionIds: ['today.saved'] }
        manifest.walks[0].profile = { name: 'uiTesting', launchArguments: { uiTesting: true } }
        await ctx.write(MANIFEST, `${JSON.stringify(manifest, null, 2)}\n`)
        // Three assertions are required, but the flow has the two in its main file plus one in the subflow.
        await ctx.persist(store, { origin: 'verify' })
        manifest.walks[0].ownerRules = { minAssertions: 9 }
        await ctx.write(MANIFEST, `${JSON.stringify(manifest, null, 2)}\n`)
        await expect(ctx.persist(store, { origin: 'verify' })).rejects.toBeInstanceOf(OwnerRuleError)
        manifest.walks[0].ownerRules = { requiredAssertionIds: ['never.shown'] }
        await ctx.write(MANIFEST, `${JSON.stringify(manifest, null, 2)}\n`)
        await expect(ctx.persist(store, { origin: 'verify' })).rejects.toBeInstanceOf(OwnerRuleError)
        manifest.walks[0].ownerRules = { minAssertions: 1 }
        await ctx.write(MANIFEST, `${JSON.stringify(manifest, null, 2)}\n`)
        await ctx.write(MAIN, MAIN_YAML.replace('today.answer', 'today.answer.field'))
        await ctx.persist(store, { origin: 'verify' })
        const after = parseWalkManifest(await ctx.manifestText()).walks[0]
        expect(after.ownerRules).toEqual({ minAssertions: 1 })
        expect(after.profile).toEqual({ name: 'uiTesting', launchArguments: { uiTesting: true } })
      } finally {
        await ctx.cleanup()
      }
    })

    test('a goal the reader would refuse is refused before anything is written', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        for (const goal of ['open /private/tmp notes', '/abs goal', 'see ~/notes']) {
          await expect(ctx.persist(store, { identity: identity({ goal }) })).rejects.toThrow(/would not read back/)
        }
        await expect(ctx.persist(store, { identity: identity({ appId: '/Users/x/app' }) })).rejects.toBeInstanceOf(InvalidWalkManifestError)
        expect(existsSync(join(ctx.workspaceRoot, MANIFEST))).toBe(false)
        expect(existsSync(implementation.metadataPath(ctx.workspaceRoot))).toBe(false)
        // A goal with a slash that is not a path is fine, and the manifest it writes reads back.
        await ctx.persist(store, { identity: identity({ goal: 'checkout and/or pay' }) })
        expect(parseWalkManifest(await ctx.manifestText()).walks[0].goal).toBe('checkout and/or pay')
      } finally {
        await ctx.cleanup()
      }
    })

    test('a bare assertion counts toward minAssertions, and an optional runFlow does not satisfy a required id', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        const body = (extra: string) => `appId: dev.deja.fixture\n---\n- assertVisible:\n    id: "screen.today"\n${extra}`
        await ctx.write(MAIN, body('- assertNoDefectsWithAi\n'))
        await ctx.persist(store)
        const set = async (ownerRules: object) => {
          const manifest = parseWalkManifest(await ctx.manifestText())
          Object.assign(manifest.walks[0], { ownerRules })
          await ctx.write(MANIFEST, `${JSON.stringify(manifest, null, 2)}\n`)
        }
        await set({ minAssertions: 2 })
        await ctx.persist(store, { origin: 'verify' })
        await ctx.write(MAIN, body(''))
        await expect(ctx.persist(store, { origin: 'verify', assertionChange: 'accept' })).rejects.toBeInstanceOf(OwnerRuleError)
        for (const optional of ['true', 'yes']) {
          await set({ requiredAssertionIds: ['saved'] })
          await ctx.write(SUB, '- assertVisible:\n    id: saved\n')
          await ctx.write(MAIN, `appId: dev.deja.fixture\n---\n- runFlow:\n    file: ../subflows/type.yaml\n    optional: ${optional}\n`)
          await expect(ctx.persist(store, { origin: 'remember', assertionChange: 'accept' })).rejects.toBeInstanceOf(OwnerRuleError)
        }
        await ctx.write(MAIN, 'appId: dev.deja.fixture\n---\n- runFlow: ../subflows/type.yaml\n')
        await ctx.persist(store, { origin: 'remember', assertionChange: 'accept' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('required assertion ids count only when they always run', async () => {
      const ctx = await context()
      try {
        const store = ctx.open()
        await ctx.persist(store)
        const rules = async (requiredAssertionIds: string[]) => {
          const manifest = parseWalkManifest(await ctx.manifestText())
          manifest.walks[0].ownerRules = { requiredAssertionIds }
          await ctx.write(MANIFEST, `${JSON.stringify(manifest, null, 2)}\n`)
        }
        await rules(['screen.today', 'today.saved'])
        await ctx.persist(store, { origin: 'verify' })
        const main = (assertions: string) => `appId: dev.deja.fixture\n---\n${assertions}`
        const cases: [string, string][] = [
          ['optional', '- assertVisible:\n    id: "screen.today"\n    optional: true\n'],
          ['under a condition', '- runFlow:\n    when:\n      visible: x\n    commands:\n      - assertVisible:\n          id: "screen.today"\n'],
          ['in a repeat', '- repeat:\n    times: 1\n    commands:\n      - assertVisible:\n          id: "screen.today"\n'],
        ]
        for (const [, body] of cases) {
          await ctx.write(MAIN, main(`${body}- assertVisible:\n    id: "today.saved"\n`))
          await rules(['screen.today'])
          await expect(ctx.persist(store, { origin: 'verify', assertionChange: 'accept' })).rejects.toBeInstanceOf(OwnerRuleError)
        }
        // Behind a conditional call it does not always run either.
        await rules(['screen.today'])
        await ctx.write(MAIN, main('- runFlow:\n    file: ../subflows/type.yaml\n    when:\n      platform: iOS\n'))
        await ctx.write(SUB, '- assertVisible:\n    id: "screen.today"\n')
        await expect(ctx.persist(store, { origin: 'remember', assertionChange: 'accept' })).rejects.toBeInstanceOf(OwnerRuleError)
        await ctx.write(MAIN, main('- runFlow: ../subflows/type.yaml\n'))
        await ctx.persist(store, { origin: 'remember', assertionChange: 'accept' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('a manifest write that would hold a fixture credential is refused', async () => {
      const ctx = await context()
      try {
        const store = implementation.createStore(ctx.workspaceRoot, undefined, undefined, {
          path: MANIFEST, forbiddenValues: ['dev.deja.fixture'],
        })
        try {
          await expect(ctx.persist(store)).rejects.toBeInstanceOf(InvalidWalkManifestError)
          // Refused before anything was written: neither the manifest nor the store exists.
          expect(existsSync(join(ctx.workspaceRoot, MANIFEST))).toBe(false)
          expect(existsSync(implementation.metadataPath(ctx.workspaceRoot))).toBe(false)
        } finally {
          await implementation.closeStore(store)
        }
      } finally {
        await ctx.cleanup()
      }
    })
  })
}
