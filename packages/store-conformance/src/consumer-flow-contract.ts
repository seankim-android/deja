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

  const FLOWS = '.maestro/flows/deja'
  const SMOKE = '.maestro/flows/smoke.yaml'
  const SMOKE_YAML = 'appId: dev.deja.fixture\n---\n- assertVisible:\n    id: "screen.today"\n'

  describe(`${implementation.name}: explore output and migration`, () => {
    async function withFlows() {
      const ctx = await context()
      await ctx.write(SMOKE, SMOKE_YAML)
      const stores: WalkStore[] = []
      const openFlows = (
        storeId?: string,
        failures?: Parameters<typeof implementation.createStore>[1],
        forbiddenValues?: string[],
      ) => {
        const store = implementation.createStore(ctx.workspaceRoot, failures, storeId, {
          path: MANIFEST, flowsDir: FLOWS, ...(forbiddenValues ? { forbiddenValues } : {}),
        })
        stores.push(store)
        return store
      }
      const legacy = async (storeId: string | undefined, walks: { goal: string; path: string; build?: string; at?: string }[]) => {
        const store = ctx.open(storeId, false)
        for (const walk of walks) {
          await store.persistVerifiedCandidate({
            identity: identity({ goal: walk.goal, buildFingerprint: walk.build ?? 'a'.repeat(64) }),
            candidatePath: walk.path, validation: { passed: true, at: walk.at ?? initialValidation }, origin: 'remember',
          })
        }
        await implementation.closeStore(store)
      }
      return {
        ...ctx, openFlows, legacy,
        cleanup: async () => {
          for (const store of stores.reverse()) await implementation.closeStore(store)
          await ctx.cleanup()
        },
      }
    }

    test('explore output becomes a new file of the consumer\'s, a manifest entry and history', async () => {
      const ctx = await withFlows()
      try {
        const store = ctx.openFlows()
        expect(store.manifestFlowsDir).toBe(FLOWS)
        await mkdir(join(ctx.workspaceRoot, '.deja/attempts/x'), { recursive: true })
        await ctx.write('.deja/attempts/x/candidate.yaml', SMOKE_YAML)
        const persisted = await ctx.persist(store, {
          identity: identity({ goal: 'Smoke Test' }), candidatePath: '.deja/attempts/x/candidate.yaml', origin: 'explore',
        })
        expect(persisted.flowPath).toBe(`${FLOWS}/smoke-test.yaml`)
        expect(persisted.manifest).toEqual({ path: MANIFEST, changed: true, filesWritten: [`${FLOWS}/smoke-test.yaml`] })
        expect(await readFile(join(ctx.workspaceRoot, persisted.flowPath), 'utf8')).toBe(SMOKE_YAML)
        const walk = parseWalkManifest(await ctx.manifestText()).walks[0]
        expect(walk).toMatchObject({ goal: 'Smoke Test', flow: `${FLOWS}/smoke-test.yaml` })
        const hit = await store.lookup(identity({ goal: 'Smoke Test' }))
        expect(hit.status).toBe('hit')
        if (hit.status === 'hit') expect(hit.walk.origin).toBe('explore')
        expect(existsSync(join(ctx.workspaceRoot, '.deja/flows'))).toBe(false)
        // A second exploration of the same goal is refused, and the file is not touched.
        await expect(ctx.persist(store, {
          identity: identity({ goal: 'Smoke Test' }), candidatePath: '.deja/attempts/x/candidate.yaml', origin: 'explore',
        })).rejects.toBeInstanceOf(ConsumerFlowError)
      } finally {
        await ctx.cleanup()
      }
    })

    test('explore output with subflows keeps its layout, and never overwrites a different file', async () => {
      const ctx = await withFlows()
      try {
        const store = ctx.openFlows()
        await mkdir(join(ctx.workspaceRoot, '.deja/attempts/y/flows'), { recursive: true })
        await mkdir(join(ctx.workspaceRoot, '.deja/attempts/y/subflows'), { recursive: true })
        await ctx.write('.deja/attempts/y/flows/main.yaml', MAIN_YAML)
        await ctx.write('.deja/attempts/y/subflows/type.yaml', SUB_YAML)
        const candidate = '.deja/attempts/y/flows/main.yaml'
        // Something else already sits where the subflow would go.
        await mkdir(join(ctx.workspaceRoot, FLOWS, 'today/subflows'), { recursive: true })
        await ctx.write(`${FLOWS}/today/subflows/type.yaml`, '- inputText: other\n')
        await expect(ctx.persist(store, { candidatePath: candidate, origin: 'explore' }))
          .rejects.toBeInstanceOf(ConsumerFlowError)
        expect(existsSync(join(ctx.workspaceRoot, FLOWS, 'today/flows/main.yaml'))).toBe(false)
        expect(existsSync(join(ctx.workspaceRoot, MANIFEST))).toBe(false)
        await rm(join(ctx.workspaceRoot, FLOWS, 'today'), { recursive: true })

        const persisted = await ctx.persist(store, { candidatePath: candidate, origin: 'explore' })
        expect(persisted.flowPath).toBe(`${FLOWS}/today/flows/main.yaml`)
        expect(persisted.manifest?.filesWritten?.sort()).toEqual([
          `${FLOWS}/today/flows/main.yaml`, `${FLOWS}/today/subflows/type.yaml`,
        ])
        expect((await store.lookup(identity())).status).toBe('hit')
        // An edit to the new subflow is a flow-changed, like any consumer file.
        await ctx.write(`${FLOWS}/today/subflows/type.yaml`, `${SUB_YAML}# edited\n`)
        expect(await store.lookup(identity())).toMatchObject({ status: 'miss', reason: 'flow-changed' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('explore output with a fixture value, or a goal named by one, is refused before anything is written', async () => {
      const ctx = await withFlows()
      try {
        const store = ctx.openFlows(undefined, undefined, ['hunter2-fixture'])
        await mkdir(join(ctx.workspaceRoot, '.deja/attempts/x'), { recursive: true })
        await ctx.write('.deja/attempts/x/candidate.yaml', `${SMOKE_YAML}- inputText: hunter2-fixture\n`)
        const absolute = join(ctx.workspaceRoot, '.deja/attempts/x/candidate.yaml')
        await expect(ctx.persist(store, { identity: identity({ goal: 'Login' }), candidatePath: absolute, origin: 'explore' }))
          .rejects.toThrow(/fixture credential value/)
        await ctx.write('.deja/attempts/x/clean.yaml', SMOKE_YAML)
        await expect(ctx.persist(store, {
          identity: identity({ goal: 'hunter2-fixture' }), candidatePath: '.deja/attempts/x/clean.yaml', origin: 'explore',
        })).rejects.toBeInstanceOf(InvalidWalkManifestError)
        for (const path of [FLOWS, MANIFEST]) expect(existsSync(join(ctx.workspaceRoot, path))).toBe(false)
        expect(existsSync(implementation.metadataPath(ctx.workspaceRoot))).toBe(false)
        // The same flow without the value is kept, from an absolute path as the explorer gives it.
        const clean = await ctx.persist(store, {
          identity: identity({ goal: 'Login' }), candidatePath: join(ctx.workspaceRoot, '.deja/attempts/x/clean.yaml'),
          origin: 'explore',
        })
        expect(clean.flowPath).toBe(`${FLOWS}/login.yaml`)
      } finally {
        await ctx.cleanup()
      }
    })

    test('migrate leaves a walk whose goal the manifest reader would refuse, and moves the rest', async () => {
      const ctx = await withFlows()
      try {
        await ctx.legacy('dev', [{ goal: 'open /private/tmp notes', path: SMOKE }, { goal: 'Today', path: MAIN }])
        const store = ctx.openFlows('dev')
        const report = await store.migrateToManifest!({})
        expect(report.migrated.map(walk => walk.goal)).toEqual(['Today'])
        expect(report.skipped.map(item => [item.goal, item.reason])).toEqual([['open /private/tmp notes', 'manifest-invalid']])
        expect(existsSync(join(ctx.workspaceRoot, FLOWS, 'open-private-tmp-notes.yaml'))).toBe(false)
        expect(parseWalkManifest(await ctx.manifestText()).walks.map(walk => walk.goal)).toEqual(['Today'])
        expect((await store.lookup(identity())).status).toBe('hit')
      } finally {
        await ctx.cleanup()
      }
    })

    test('migrate does not copy a flow that holds a fixture value, and a goal named by one stops the run', async () => {
      const ctx = await withFlows()
      try {
        await ctx.write('.maestro/flows/login.yaml', `${SMOKE_YAML}- inputText: hunter2-fixture\n`)
        await ctx.legacy('dev', [{ goal: 'Login', path: '.maestro/flows/login.yaml' }, { goal: 'Smoke', path: SMOKE }])
        const store = ctx.openFlows('dev', undefined, ['hunter2-fixture'])
        for (const dryRun of [true, false]) {
          const report = await store.migrateToManifest!({ dryRun })
          expect(report.skipped.map(item => [item.goal, item.reason])).toEqual([['Login', 'holds-fixture-value']])
          expect(JSON.stringify(report)).not.toContain('hunter2-fixture')
          expect(existsSync(join(ctx.workspaceRoot, FLOWS, 'login.yaml'))).toBe(false)
          if (dryRun) expect(existsSync(join(ctx.workspaceRoot, MANIFEST))).toBe(false)
        }
        expect(await readFile(join(ctx.workspaceRoot, MANIFEST), 'utf8')).not.toContain('hunter2-fixture')
        await ctx.legacy('named', [{ goal: 'hunter2-fixture', path: SMOKE }])
        const named = ctx.openFlows('named', undefined, ['hunter2-fixture'])
        await expect(named.migrateToManifest!({})).rejects.toBeInstanceOf(InvalidWalkManifestError)
        expect(existsSync(join(ctx.workspaceRoot, FLOWS, 'hunter2-fixture.yaml'))).toBe(false)
        expect((await named.list()).every(walk => walk.closureSha256 === undefined)).toBe(true)
      } finally {
        await ctx.cleanup()
      }
    })

    test('without a flows folder explore output is refused, and remember never copies', async () => {
      const ctx = await withFlows()
      try {
        const store = ctx.open()
        await mkdir(join(ctx.workspaceRoot, '.deja/attempts/x'), { recursive: true })
        await ctx.write('.deja/attempts/x/candidate.yaml', SMOKE_YAML)
        expect(store.manifestFlowsDir).toBeUndefined()
        await expect(ctx.persist(store, { candidatePath: '.deja/attempts/x/candidate.yaml', origin: 'explore' }))
          .rejects.toBeInstanceOf(ConsumerFlowError)
        const withFlows = ctx.openFlows('other')
        await expect(ctx.persist(withFlows, { candidatePath: '.deja/attempts/x/candidate.yaml', origin: 'remember' }))
          .rejects.toBeInstanceOf(ConsumerFlowError)
      } finally {
        await ctx.cleanup()
      }
    })

    test('migrate moves schema 3 walks into the tree, with a dry run first and a backup', async () => {
      const ctx = await withFlows()
      try {
        await ctx.legacy('dev', [
          { goal: 'Today', path: MAIN },
          { goal: 'Smoke', path: SMOKE, at: laterValidation },
        ])
        const store = ctx.openFlows('dev')
        const metadataBefore = await implementation.snapshotMetadata(ctx.workspaceRoot).catch(() => null)

        const dry = await store.migrateToManifest!({ dryRun: true })
        expect(dry).toMatchObject({ dryRun: true, manifestChanged: true, backupPath: null, skipped: [] })
        expect(dry.migrated.map(walk => walk.flow).sort()).toEqual([
          `${FLOWS}/smoke.yaml`, `${FLOWS}/today/flows/today.yaml`,
        ])
        expect(existsSync(join(ctx.workspaceRoot, FLOWS))).toBe(false)
        expect(existsSync(join(ctx.workspaceRoot, MANIFEST))).toBe(false)
        expect(await store.lookup(identity())).toEqual({ status: 'miss', reason: 'needs-migration' })

        const report = await store.migrateToManifest!({})
        expect(report.dryRun).toBe(false)
        expect(report.skipped).toEqual([])
        expect(report.backupPath).toBe(`${implementation.metadataPath(ctx.workspaceRoot, 'dev').slice(ctx.workspaceRoot.length + 1)}.schema3.bak`)
        expect(existsSync(join(ctx.workspaceRoot, report.backupPath!))).toBe(true)
        for (const walk of report.migrated) {
          expect(walk.filesWritten.length).toBeGreaterThan(0)
          expect(walk.historyEntries).toBe(1)
        }
        // The files are the verified bytes, in the consumer's tree, and the originals are untouched.
        expect(await readFile(join(ctx.workspaceRoot, `${FLOWS}/today/flows/today.yaml`), 'utf8')).toBe(MAIN_YAML)
        expect(await readFile(join(ctx.workspaceRoot, `${FLOWS}/today/subflows/type.yaml`), 'utf8')).toBe(SUB_YAML)
        expect(await readFile(join(ctx.workspaceRoot, MAIN), 'utf8')).toBe(MAIN_YAML)
        const manifest = parseWalkManifest(await ctx.manifestText())
        expect(manifest.walks.map(walk => walk.goal).sort()).toEqual(['Smoke', 'Today'])
        // The migrated assertion hash is the one a fresh remember of the same flow would record.
        const direct = ctx.open('fresh')
        await ctx.persist(direct, { candidatePath: MAIN })
        const fresh = parseWalkManifest(await ctx.manifestText()).walks.find(walk => walk.goal === 'Today')!
        expect(fresh.assertionsSha256).toBe(manifest.walks.find(walk => walk.goal === 'Today')!.assertionsSha256)

        const walks = await store.list()
        expect(walks.every(walk => walk.closureSha256 !== undefined && walk.subflowFiles === undefined)).toBe(true)
        expect(walks.find(walk => walk.goalKey === 'today')?.validationHistory).toHaveLength(1)
        expect((await store.lookup(identity({ goal: 'Today' }))).status).toBe('hit')
        expect((await store.lookup(identity({ goal: 'Smoke' }))).status).toBe('hit')
        // The old copies are not read any more: removing them changes nothing.
        await rm(join(ctx.workspaceRoot, '.deja/flows'), { recursive: true, force: true })
        expect((await store.lookup(identity({ goal: 'Today' }))).status).toBe('hit')
        void metadataBefore
      } finally {
        await ctx.cleanup()
      }
    })

    test('migrate is safe to run again, and finishes after a crash before the store was rewritten', async () => {
      const ctx = await withFlows()
      try {
        await ctx.legacy(undefined, [{ goal: 'Today', path: MAIN }])
        const crashing = ctx.openFlows(undefined, { failMetadataReplacement: true })
        await expect(crashing.migrateToManifest!({})).rejects.toThrow()
        // Files and manifest are in place; the store is still schema 3.
        expect(existsSync(join(ctx.workspaceRoot, `${FLOWS}/today/flows/today.yaml`))).toBe(true)
        const manifestAfterCrash = await ctx.manifestText()
        expect(await crashing.lookup(identity())).toMatchObject({ status: 'miss', reason: 'not-validated' })
        await implementation.closeStore(crashing)

        const store = ctx.openFlows()
        const report = await store.migrateToManifest!({})
        expect(report.migrated).toHaveLength(1)
        expect(report.migrated[0].filesWritten).toEqual([])
        expect(report.migrated[0].filesUnchanged.length).toBe(2)
        expect(report.manifestChanged).toBe(false)
        expect(await ctx.manifestText()).toBe(manifestAfterCrash)
        expect((await store.lookup(identity())).status).toBe('hit')

        const again = await store.migrateToManifest!({})
        expect(again).toMatchObject({ migrated: [], skipped: [], manifestChanged: false, backupPath: null })
      } finally {
        await ctx.cleanup()
      }
    })

    test('migrate leaves what it cannot move in the old store and says why', async () => {
      const ctx = await withFlows()
      try {
        await ctx.write('.maestro/flows/other.yaml', 'appId: dev.deja.fixture\n---\n- assertVisible:\n    id: "other"\n')
        await ctx.write('.maestro/flows/gone.yaml', 'appId: dev.deja.fixture\n---\n- assertVisible:\n    id: "gone"\n')
        await ctx.write('.maestro/flows/blocked.yaml', 'appId: dev.deja.fixture\n---\n- assertVisible:\n    id: "blocked"\n')
        await ctx.legacy('dev', [
          { goal: 'Today', path: MAIN },
          { goal: 'Gone', path: '.maestro/flows/gone.yaml' },
          { goal: 'Blocked', path: '.maestro/flows/blocked.yaml' },
          { goal: 'Other', path: '.maestro/flows/other.yaml' },
        ])
        // The copy of one walk is deleted, a subflow copy of another is edited, one destination is taken.
        const store = ctx.openFlows('dev')
        const before = await store.list()
        const gone = before.find(walk => walk.goalKey === 'gone')!
        await rm(join(ctx.workspaceRoot, gone.flowPath))
        const today = before.find(walk => walk.goalKey === 'today')!
        await ctx.write(today.subflowFiles![0].path, `${SUB_YAML}# tampered\n`)
        await mkdir(join(ctx.workspaceRoot, FLOWS), { recursive: true })
        await ctx.write(`${FLOWS}/blocked.yaml`, '- tapOn: somewhere else\n')

        const report = await store.migrateToManifest!({})
        expect(report.migrated.map(walk => walk.goal)).toEqual(['Other'])
        expect(Object.fromEntries(report.skipped.map(item => [item.goal, item.reason]))).toEqual({
          Today: 'copy-changed', Gone: 'copy-missing', Blocked: 'would-overwrite',
        })
        expect(await readFile(join(ctx.workspaceRoot, `${FLOWS}/blocked.yaml`), 'utf8')).toBe('- tapOn: somewhere else\n')
        const after = await store.list()
        expect(after.filter(walk => walk.closureSha256 === undefined).map(walk => walk.goalKey).sort())
          .toEqual(['blocked', 'gone', 'today'])
        expect(await store.lookup(identity())).toEqual({ status: 'miss', reason: 'needs-migration' })
        expect(parseWalkManifest(await ctx.manifestText()).walks.map(walk => walk.goal)).toEqual(['Other'])
      } finally {
        await ctx.cleanup()
      }
    })

    test('migrate keeps the newest walk of a goal and its adapter', async () => {
      const ctx = await withFlows()
      try {
        // The same goal remembered on two builds: a store keeps one walk per goal, the newest.
        await ctx.legacy('dev', [
          { goal: 'Smoke', path: SMOKE, build: 'a'.repeat(64), at: initialValidation },
          { goal: 'Smoke', path: SMOKE, build: 'b'.repeat(64), at: laterValidation },
        ])
        const store = ctx.openFlows('dev')
        const before = await store.list()
        const report = await store.migrateToManifest!({})
        expect(report.migrated).toHaveLength(1)
        expect(report.migrated[0].olderWalksDropped).toBe(before.filter(walk => walk.goalKey === 'smoke').length - 1)
        const kept = (await store.list()).filter(walk => walk.goalKey === 'smoke')
        expect(kept).toHaveLength(1)
        expect(kept[0].adapter).toBe('maestro-ios/v1')
        expect(kept[0].buildFingerprint).toBe('b'.repeat(64))
      } finally {
        await ctx.cleanup()
      }
    })

    test('migrate skips walks with no adapter, no passing run, or subflows the old store never kept', async () => {
      const ctx = await withFlows()
      try {
        await ctx.write('.maestro/flows/nopass.yaml', 'appId: dev.deja.fixture\n---\n- assertVisible:\n    id: "nopass"\n')
        const plain = ctx.open(undefined, false)
        await plain.persistVerifiedCandidate({
          identity: identity({ goal: 'No Adapter', adapter: undefined }), candidatePath: SMOKE,
          validation: { passed: true, at: initialValidation }, origin: 'remember',
        })
        await plain.persistVerifiedCandidate({
          identity: identity({ goal: 'No Pass' }), candidatePath: '.maestro/flows/nopass.yaml',
          validation: { passed: true, at: initialValidation }, origin: 'remember',
        })
        await plain.recordValidation(identity({ goal: 'No Pass' }), { passed: false, at: laterValidation })
        await plain.persistVerifiedCandidate({
          identity: identity({ goal: 'Today' }), candidatePath: MAIN,
          validation: { passed: true, at: initialValidation }, origin: 'remember',
        })
        await implementation.closeStore(plain)
        // A walk written before schema 3 kept only its main flow, so its subflows are unknown.
        await implementation.downgradeToSchema2(ctx.workspaceRoot)

        const store = ctx.openFlows()
        const first = await store.migrateToManifest!({})
        expect(Object.fromEntries(first.skipped.map(item => [item.goal, item.reason]))).toEqual({
          'No Adapter': 'adapter-unknown', 'No Pass': 'not-passing', Today: 'subflows-not-stored',
        })
        expect(first.migrated).toEqual([])
        expect(first.backupPath).toBeNull()
        expect(existsSync(join(ctx.workspaceRoot, MANIFEST))).toBe(false)

        // Given the adapter id, the walk without one moves; the others still cannot.
        const second = await store.migrateToManifest!({ adapter: 'maestro-android' })
        expect(second.migrated.map(walk => walk.goal)).toEqual(['No Adapter'])
        expect(second.skipped.map(item => item.goal).sort()).toEqual(['No Pass', 'Today'])
        expect(parseWalkManifest(await ctx.manifestText()).walks[0].adapter).toBe('maestro-android')
        expect((await store.list()).filter(walk => walk.closureSha256 === undefined)).toHaveLength(2)
      } finally {
        await ctx.cleanup()
      }
    })

    test('migrate without a manifest store, and a dry run that has nowhere to write, are refused', async () => {
      const ctx = await withFlows()
      try {
        const plain = ctx.open(undefined, false)
        await expect(plain.migrateToManifest!({})).rejects.toThrow(/needs a store built with a manifest/)
        await ctx.legacy('dev', [{ goal: 'Today', path: MAIN }])
        const noFolder = ctx.open('dev')
        await expect(noFolder.migrateToManifest!({ dryRun: true })).resolves.toMatchObject({
          migrated: [], skipped: [{ goal: 'Today', reason: 'closure-error' }],
        })
      } finally {
        await ctx.cleanup()
      }
    })
  })
}
