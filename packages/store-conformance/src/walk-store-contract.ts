import { describe, expect, test } from 'bun:test'
import { createHash } from 'node:crypto'
import { existsSync } from 'node:fs'
import { mkdir, mkdtemp, realpath, readFile, readdir, rename, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { basename, join, resolve } from 'node:path'
import type { WalkIdentity, WalkStore } from '@deja-qa/core'

export const initialValidation = '2026-11-22T00:00:00.000Z'
export const laterValidation = '2026-11-22T00:01:00.000Z'

export function contractIdentity(overrides: Partial<WalkIdentity> = {}): WalkIdentity {
  return {
    goal: 'Checkout',
    appId: 'dev.deja.checkout',
    platform: 'android',
    flowSchemaVersion: 'maestro/v1',
    buildFingerprint: 'a'.repeat(64),
    ...overrides,
  }
}

export type StoreFailureInjection = {
  failFlowRename?: boolean
  failMetadataReplacement?: boolean
}

export type MetadataTamper = 'malformed' | 'unsupported-schema' | 'invalid-entry' |
  'duplicate-identity'

/** Backend-specific setup and tamper hooks; every behavioral assertion below is shared verbatim. */
export type WalkStoreContractImplementation = {
  name: string
  metadataPath(workspaceRoot: string): string
  createStore(workspaceRoot: string, failures?: StoreFailureInjection): WalkStore
  closeStore(store: WalkStore): void | Promise<void>
  snapshotMetadata(workspaceRoot: string): Promise<Buffer>
  tamperMetadata(workspaceRoot: string, kind: MetadataTamper): Promise<void>
  setFirstFlowPath(workspaceRoot: string, flowPath: string): Promise<void>
}

async function temporaryFiles(root: string): Promise<string[]> {
  if (!existsSync(root)) return []
  const found: string[] = []
  for (const entry of await readdir(root, { withFileTypes: true })) {
    const path = join(root, entry.name)
    if (entry.isDirectory()) found.push(...await temporaryFiles(path))
    else if (entry.name.endsWith('.tmp')) found.push(path)
  }
  return found
}

async function fixture() {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-store-contract-')))
  const workspaceRoot = join(temporary, 'workspace')
  await mkdir(join(workspaceRoot, '.deja/attempts'), { recursive: true })
  const candidatePath = '.deja/attempts/candidate.yaml'
  await writeFile(
    join(workspaceRoot, candidatePath),
    'appId: dev.deja.checkout\n- assertVisible: Order confirmed\n',
  )
  return { temporary, workspaceRoot, candidatePath }
}

export function runWalkStoreContractTests(implementation: WalkStoreContractImplementation) {
  async function context(failures?: StoreFailureInjection) {
    const value = await fixture()
    const stores: WalkStore[] = []
    const makeStore = (injectedFailures = failures) => {
      const store = implementation.createStore(value.workspaceRoot, injectedFailures)
      stores.push(store)
      return store
    }
    const cleanup = async () => {
      for (const store of stores.reverse()) await implementation.closeStore(store)
      await rm(value.temporary, { recursive: true, force: true })
    }
    const seed = async (overrides: Partial<WalkIdentity> = {}) => {
      const store = makeStore()
      const persisted = await store.persistVerifiedCandidate({
        identity: contractIdentity(overrides),
        candidatePath: value.candidatePath,
        validation: { passed: true, at: initialValidation },
        origin: 'explore',
      })
      return { store, persisted }
    }
    return { ...value, makeStore, cleanup, seed }
  }

  describe(`${implementation.name}: shared WalkStore behavioral contract`, () => {
    test('returns a first-run miss without creating metadata', async () => {
      const ctx = await context()
      try {
        const result = await ctx.makeStore().lookup(contractIdentity())
        expect(result).toEqual({ status: 'miss', reason: 'not-found' })
        expect(existsSync(implementation.metadataPath(ctx.workspaceRoot))).toBe(false)
      } finally {
        await ctx.cleanup()
      }
    })

    test('persists across instances, normalizes the goal, and returns the complete exact identity', async () => {
      const ctx = await context()
      try {
        const { persisted } = await ctx.seed({ goal: '  Check   Out  ' })
        const reopened = ctx.makeStore()
        const result = await reopened.lookup(contractIdentity({ goal: '\tcheck  out\n' }))
        expect(result.status).toBe('hit')
        if (result.status !== 'hit') throw new Error('expected hit')
        expect(result.walk.goal).toBe('Check   Out')
        expect(result.walk.goalKey).toBe('check out')
        expect(result.walk).toMatchObject({
          appId: contractIdentity().appId,
          platform: 'android',
          flowSchemaVersion: 'maestro/v1',
          buildFingerprint: 'a'.repeat(64),
          flowSha256: persisted.flowSha256,
        })
        expect(result.flowPath).toBe(resolve(ctx.workspaceRoot, persisted.flowPath))
        expect(result.walk.lastValidation.passed).toBe(true)
        expect(result.walk.validationHistory).toHaveLength(1)
        expect(await reopened.lookup(contractIdentity({ goal: 'check out', flowSchemaVersion: 'other/v1' })))
          .toEqual({ status: 'miss', reason: 'wrong-flow-schema' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('returns stale-build instead of replaying another build fingerprint', async () => {
      const ctx = await context()
      try {
        const { store } = await ctx.seed()
        expect(await store.lookup(contractIdentity({ buildFingerprint: 'b'.repeat(64) })))
          .toEqual({ status: 'miss', reason: 'stale-build' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('returns wrong-app without loosening app identity', async () => {
      const ctx = await context()
      try {
        const { store } = await ctx.seed()
        expect(await store.lookup(contractIdentity({ appId: 'dev.deja.other' })))
          .toEqual({ status: 'miss', reason: 'wrong-app' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('returns wrong-platform without sharing memory across platforms', async () => {
      const ctx = await context()
      try {
        const { store } = await ctx.seed()
        expect(await store.lookup(contractIdentity({ platform: 'ios' })))
          .toEqual({ status: 'miss', reason: 'wrong-platform' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('partitions Web identity by both adapter and browser', async () => {
      const ctx = await context()
      try {
        const web = contractIdentity({
          appId: 'http://127.0.0.1:4174', platform: 'web',
          adapter: 'maestro-web@2.12.0', browser: 'chromium',
        })
        const { store } = await ctx.seed(web)
        const hit = await store.lookup(web)
        expect(hit.status).toBe('hit')
        if (hit.status === 'hit') {
          expect(hit.walk).toMatchObject({
            platform: 'web', browser: 'chromium', adapter: 'maestro-web@2.12.0',
            buildFingerprint: web.buildFingerprint,
          })
        }
        expect(await store.lookup({ ...web, adapter: 'maestro-web-other' }))
          .toEqual({ status: 'miss', reason: 'wrong-adapter' })
        expect(await store.lookup({ ...web, browser: 'firefox' }))
          .toEqual({ status: 'miss', reason: 'wrong-browser' })
        expect(await store.lookup({ ...web, buildFingerprint: 'c'.repeat(64) }))
          .toEqual({ status: 'miss', reason: 'stale-build' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('returns missing-flow and never treats metadata alone as a pass', async () => {
      const ctx = await context()
      try {
        const { store, persisted } = await ctx.seed()
        await rm(join(ctx.workspaceRoot, persisted.flowPath))
        expect(await store.lookup(contractIdentity()))
          .toEqual({ status: 'miss', reason: 'missing-flow' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('rejects a tampered flow checksum instead of executing changed YAML', async () => {
      const ctx = await context()
      try {
        const { store, persisted } = await ctx.seed()
        await writeFile(join(ctx.workspaceRoot, persisted.flowPath), '# tampered flow\n')
        await expect(store.lookup(contractIdentity())).rejects.toThrow('flow checksum does not match')
      } finally {
        await ctx.cleanup()
      }
    })

    test('rejects malformed metadata as invalid memory', async () => {
      const ctx = await context()
      try {
        const { store } = await ctx.seed()
        await implementation.tamperMetadata(ctx.workspaceRoot, 'malformed')
        await expect(store.lookup(contractIdentity())).rejects.toThrow()
        await expect(store.list()).rejects.toThrow()
      } finally {
        await ctx.cleanup()
      }
    })

    test('rejects an unsupported metadata schema', async () => {
      const ctx = await context()
      try {
        const { store } = await ctx.seed()
        await implementation.tamperMetadata(ctx.workspaceRoot, 'unsupported-schema')
        await expect(store.lookup(contractIdentity())).rejects.toThrow('999')
      } finally {
        await ctx.cleanup()
      }
    })

    test('rejects an invalid entry instead of filtering it out to produce a hit', async () => {
      const ctx = await context()
      try {
        const { store } = await ctx.seed()
        await implementation.tamperMetadata(ctx.workspaceRoot, 'invalid-entry')
        await expect(store.lookup(contractIdentity())).rejects.toThrow('identity fields')
      } finally {
        await ctx.cleanup()
      }
    })

    test('rejects duplicate exact identities as ambiguous', async () => {
      const ctx = await context()
      try {
        const { store } = await ctx.seed()
        await implementation.tamperMetadata(ctx.workspaceRoot, 'duplicate-identity')
        await expect(store.lookup(contractIdentity())).rejects.toThrow('duplicate exact walk identities')
      } finally {
        await ctx.cleanup()
      }
    })

    test('rejects stored flow paths outside the canonical flow directory', async () => {
      const ctx = await context()
      try {
        await mkdir(ctx.workspaceRoot, { recursive: true })
        await writeFile(join(ctx.workspaceRoot, 'outside.yaml'), '# outside\n')
        const { store } = await ctx.seed()
        await implementation.setFirstFlowPath(ctx.workspaceRoot, '../outside.yaml')
        await expect(store.lookup(contractIdentity())).rejects.toThrow('must stay under .deja/flows')
      } finally {
        await ctx.cleanup()
      }
    })

    test('rejects a symlink whose canonical flow path escapes the workspace', async () => {
      if (process.platform === 'win32') return
      const ctx = await context()
      try {
        const { store } = await ctx.seed()
        const outside = join(ctx.temporary, 'outside.yaml')
        await writeFile(outside, '# outside\n')
        const linkDirectory = join(ctx.workspaceRoot, '.deja/flows/escape')
        await mkdir(linkDirectory, { recursive: true })
        await symlink(outside, join(linkDirectory, 'android.yaml'))
        await implementation.setFirstFlowPath(ctx.workspaceRoot, '.deja/flows/escape/android.yaml')
        await expect(store.lookup(contractIdentity())).rejects.toThrow()
      } finally {
        await ctx.cleanup()
      }
    })

    test('rejects a symlinked metadata-directory escape before creating outside files', async () => {
      if (process.platform === 'win32') return
      const ctx = await context()
      try {
        const outside = join(ctx.temporary, 'outside-memory')
        const memoryDirectory = join(ctx.workspaceRoot, '.deja/memory')
        await mkdir(outside, { recursive: true })
        await symlink(outside, memoryDirectory)
        const store = ctx.makeStore()
        await expect(store.lookup(contractIdentity())).rejects.toThrow('metadata path escapes workspace')
        await expect(store.persistVerifiedCandidate({
          identity: contractIdentity(), candidatePath: ctx.candidatePath,
          validation: { passed: true, at: initialValidation }, origin: 'explore',
        })).rejects.toThrow('metadata path escapes workspace')
        expect(await readdir(outside)).toEqual([])
        expect(existsSync(join(
          outside, basename(implementation.metadataPath(ctx.workspaceRoot)),
        ))).toBe(false)
      } finally {
        await ctx.cleanup()
      }
    })

    test('rejects metadata paths that traverse a non-directory on reads and writes', async () => {
      const ctx = await context()
      try {
        const metadataFile = join(ctx.workspaceRoot, '.deja/memory')
        await writeFile(metadataFile, '# not a directory\n')
        const store = ctx.makeStore()
        await expect(store.lookup(contractIdentity())).rejects.toThrow('metadata path')
        await expect(store.persistVerifiedCandidate({
          identity: contractIdentity(), candidatePath: ctx.candidatePath,
          validation: { passed: true, at: initialValidation }, origin: 'explore',
        })).rejects.toThrow('metadata path')
        expect(await readFile(metadataFile, 'utf8')).toBe('# not a directory\n')
      } finally {
        await ctx.cleanup()
      }
    })

    test('rejects an existing valid metadata directory swapped to an external symlink without modifying it', async () => {
      if (process.platform === 'win32') return
      const ctx = await context()
      try {
        const { store, persisted } = await ctx.seed()
        const outside = join(ctx.temporary, 'outside-valid-memory')
        const memoryDirectory = join(ctx.workspaceRoot, '.deja/memory')
        await rename(memoryDirectory, outside)
        await symlink(outside, memoryDirectory)
        const outsideMetadata = join(
          outside, basename(implementation.metadataPath(ctx.workspaceRoot)),
        )
        const beforeMetadata = await readFile(outsideMetadata)
        const beforeEntries = await readdir(outside)
        const beforeFlow = await readFile(join(ctx.workspaceRoot, persisted.flowPath))
        await expect(store.lookup(contractIdentity())).rejects.toThrow('metadata path escapes workspace')
        await expect(store.persistVerifiedCandidate({
          identity: contractIdentity({ buildFingerprint: 'b'.repeat(64) }),
          candidatePath: ctx.candidatePath,
          validation: { passed: true, at: laterValidation }, origin: 'remember',
        })).rejects.toThrow('metadata path escapes workspace')
        expect(await readFile(outsideMetadata)).toEqual(beforeMetadata)
        expect(await readdir(outside)).toEqual(beforeEntries)
        expect(await readFile(join(ctx.workspaceRoot, persisted.flowPath))).toEqual(beforeFlow)
      } finally {
        await ctx.cleanup()
      }
    })

    test('an interruption before flow rename leaves only ignored temporary files and a first-run miss', async () => {
      const ctx = await context({ failFlowRename: true })
      try {
        const store = ctx.makeStore()
        await expect(store.persistVerifiedCandidate({
          identity: contractIdentity(), candidatePath: ctx.candidatePath,
          validation: { passed: true, at: initialValidation }, origin: 'explore',
        })).rejects.toThrow('simulated interruption before flow rename')
        expect((await temporaryFiles(ctx.workspaceRoot)).length).toBeGreaterThan(0)
        expect(existsSync(implementation.metadataPath(ctx.workspaceRoot))).toBe(false)
        expect(await store.lookup(contractIdentity())).toEqual({ status: 'miss', reason: 'not-found' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('failed candidate verification preserves known-good YAML and metadata byte for byte', async () => {
      const ctx = await context()
      try {
        const { persisted } = await ctx.seed()
        const flowPath = join(ctx.workspaceRoot, persisted.flowPath)
        const beforeFlow = await readFile(flowPath)
        const beforeMetadata = await implementation.snapshotMetadata(ctx.workspaceRoot)
        const store = ctx.makeStore()
        await expect(store.persistVerifiedCandidate({
          identity: contractIdentity({ buildFingerprint: 'b'.repeat(64) }), candidatePath: ctx.candidatePath,
          validation: { passed: false, at: laterValidation }, origin: 'explore',
        })).rejects.toThrow('Only an independently passing candidate')
        expect(await readFile(flowPath)).toEqual(beforeFlow)
        expect(await implementation.snapshotMetadata(ctx.workspaceRoot)).toEqual(beforeMetadata)
        expect((await store.lookup(contractIdentity())).status).toBe('hit')
      } finally {
        await ctx.cleanup()
      }
    })

    test('failed metadata replacement preserves last-known-good bytes and ignores orphan and temporary YAML', async () => {
      const ctx = await context()
      try {
        const { persisted } = await ctx.seed()
        const oldFlowPath = join(ctx.workspaceRoot, persisted.flowPath)
        const beforeFlow = await readFile(oldFlowPath)
        const beforeMetadata = await implementation.snapshotMetadata(ctx.workspaceRoot)
        await writeFile(join(ctx.workspaceRoot, ctx.candidatePath), '# independently verified replacement\n')
        const store = ctx.makeStore({ failMetadataReplacement: true })
        await expect(store.persistVerifiedCandidate({
          identity: contractIdentity({ buildFingerprint: 'b'.repeat(64) }), candidatePath: ctx.candidatePath,
          validation: { passed: true, at: laterValidation }, origin: 'remember',
        })).rejects.toThrow('simulated metadata replacement failure')
        expect(await readFile(oldFlowPath)).toEqual(beforeFlow)
        expect(await implementation.snapshotMetadata(ctx.workspaceRoot)).toEqual(beforeMetadata)
        expect((await store.lookup(contractIdentity())).status).toBe('hit')
        expect(await store.lookup(contractIdentity({ buildFingerprint: 'b'.repeat(64) })))
          .toEqual({ status: 'miss', reason: 'stale-build' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('failed validation remains append-only history and blocks subsequent reuse without changing YAML', async () => {
      const ctx = await context()
      try {
        const { store, persisted } = await ctx.seed()
        const beforeFlow = await readFile(join(ctx.workspaceRoot, persisted.flowPath))
        const updated = await store.recordValidation(
          contractIdentity(), { passed: false, at: laterValidation },
        )
        expect(updated.lastValidation).toEqual({ passed: false, at: laterValidation })
        expect(updated.validationHistory).toHaveLength(2)
        expect(updated.validationHistory[0].passed).toBe(true)
        expect(updated.validationHistory[1]).toEqual({
          passed: false, at: laterValidation, flowSha256: persisted.flowSha256,
        })
        expect(await store.lookup(contractIdentity()))
          .toEqual({ status: 'miss', reason: 'failed-validation' })
        expect(await readFile(join(ctx.workspaceRoot, persisted.flowPath))).toEqual(beforeFlow)
        const stored = await ctx.makeStore().list()
        expect(stored[0].lastValidation.passed).toBe(false)
        expect(stored[0].validationHistory.map(entry => entry.passed)).toEqual([true, false])
      } finally {
        await ctx.cleanup()
      }
    })

    test('verified repair retains history and exploration, adds provenance, and selects its diagnostic base', async () => {
      const ctx = await context()
      try {
        const oldIdentity = contractIdentity()
        const newIdentity = contractIdentity({ buildFingerprint: 'b'.repeat(64) })
        const store = ctx.makeStore()
        const previous = await store.persistVerifiedCandidate({
          identity: oldIdentity, candidatePath: ctx.candidatePath,
          validation: { passed: true, at: initialValidation }, origin: 'explore',
          exploration: {
            explorationDurationMs: 10, verificationDurationMs: 5,
            mcpCalls: 2, modelCalls: 1, modelCallBasis: 'fixture turns',
          },
        })
        await writeFile(join(ctx.workspaceRoot, ctx.candidatePath), '# verified repaired flow\n')
        const diagnostic = await store.lookupPreviousSuccessful(newIdentity)
        expect(diagnostic.status).toBe('hit')
        if (diagnostic.status === 'hit') expect(diagnostic.walk.flowSha256).toBe(previous.flowSha256)
        const repaired = await store.persistVerifiedRepair({
          identity: newIdentity, candidatePath: ctx.candidatePath,
          validation: { passed: true, at: laterValidation },
          previous: { buildFingerprint: oldIdentity.buildFingerprint, flowSha256: previous.flowSha256 },
          repair: {
            attemptId: 'contract-repair', patchPath: '.deja/patches/contract-repair/proposed.patch',
            previousBuildFingerprint: oldIdentity.buildFingerprint, previousFlowSha256: previous.flowSha256,
          },
        })
        expect(repaired.walk.origin).toBe('repair')
        expect(repaired.walk.validationHistory).toHaveLength(2)
        expect(repaired.walk.exploration?.modelCalls).toBe(1)
        expect(repaired.walk.repairHistory?.[0]).toMatchObject({
          attemptId: 'contract-repair', previousBuildFingerprint: oldIdentity.buildFingerprint,
        })
        expect((await ctx.makeStore().lookup(newIdentity)).status).toBe('hit')
        expect(await store.lookup(oldIdentity)).toEqual({ status: 'miss', reason: 'stale-build' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('a changed repair base fails closed and preserves last-known-good metadata and YAML', async () => {
      const ctx = await context()
      try {
        const { store, persisted } = await ctx.seed()
        const beforeMetadata = await implementation.snapshotMetadata(ctx.workspaceRoot)
        const beforeFlow = await readFile(join(ctx.workspaceRoot, persisted.flowPath))
        await expect(store.persistVerifiedRepair({
          identity: contractIdentity({ buildFingerprint: 'b'.repeat(64) }), candidatePath: ctx.candidatePath,
          validation: { passed: true, at: laterValidation },
          previous: { buildFingerprint: 'c'.repeat(64), flowSha256: persisted.flowSha256 },
          repair: {
            attemptId: 'stale-repair', patchPath: '.deja/patches/stale-repair/proposed.patch',
            previousBuildFingerprint: 'c'.repeat(64), previousFlowSha256: persisted.flowSha256,
          },
        })).rejects.toThrow('previous successful repair base changed')
        expect(await implementation.snapshotMetadata(ctx.workspaceRoot)).toEqual(beforeMetadata)
        expect(await readFile(join(ctx.workspaceRoot, persisted.flowPath))).toEqual(beforeFlow)
        expect((await store.lookup(contractIdentity())).status).toBe('hit')
      } finally {
        await ctx.cleanup()
      }
    })

    describe('flows that run other files', () => {
      const main = [
        'appId: dev.deja.checkout',
        'onFlowStart:',
        '  - runScript: ../scripts/seed.js',
        '---',
        '- runFlow: ../subflows/sign-in.yaml',
        '- runFlow:',
        '    file: ../subflows/note.yaml',
        '    env: { NOTE: hi }',
        '- repeat:',
        '    times: 2',
        '    commands:',
        '      - retry:',
        '          maxRetries: 1',
        '          commands:',
        '            - runFlow: ../subflows/note.yaml',
        '- assertVisible: Order confirmed',
        '',
      ].join('\n')
      const files: Record<string, string> = {
        '.maestro/flows/main.yaml': main,
        '.maestro/scripts/seed.js': 'output.seeded = true\n',
        '.maestro/subflows/sign-in.yaml': '- runFlow: nested/type.yaml\n- assertVisible: Signed in\n',
        '.maestro/subflows/nested/type.yaml': '- inputText: hello\n- runScript:\n    file: ../../scripts/seed.js\n',
        '.maestro/subflows/note.yaml': '- tapOn: Note\n',
      }

      async function withFlows(
        extra: Record<string, string>,
        run: (ctx: Awaited<ReturnType<typeof context>>, store: WalkStore) => Promise<void>,
      ) {
        const ctx = await context()
        try {
          for (const [path, contents] of Object.entries({ ...files, ...extra })) {
            await mkdir(join(ctx.workspaceRoot, path, '..'), { recursive: true })
            await writeFile(join(ctx.workspaceRoot, path), contents)
          }
          await run(ctx, ctx.makeStore())
        } finally {
          await ctx.cleanup()
        }
      }

      const remember = (store: WalkStore, candidatePath: string, at = initialValidation) =>
        store.persistVerifiedCandidate({
          identity: contractIdentity(),
          candidatePath,
          validation: { passed: true, at },
          origin: 'remember',
        })

      test('stores nested subflows and scripts with their relative layout, and replays them', async () => {
        await withFlows({}, async (ctx, store) => {
          const persisted = await remember(store, join(ctx.workspaceRoot, '.maestro/flows/main.yaml'))
          const stored = resolve(ctx.workspaceRoot, persisted.flowPath)
          expect(persisted.flowPath).toMatch(/^\.deja\/flows\/checkout\/android-[a-f0-9]{64}\/flows\/main\.yaml$/)
          expect(persisted.flowSha256).toBe(
            createHash('sha256').update(main).digest('hex'),
          )
          expect(persisted.walk.subflowFiles?.map(file => file.path.split('/').slice(4).join('/')).sort())
            .toEqual(['scripts/seed.js', 'subflows/nested/type.yaml', 'subflows/note.yaml', 'subflows/sign-in.yaml'])
          // The paths written in the main flow still resolve from where the copy lives.
          for (const [relative, original] of [
            ['../scripts/seed.js', '.maestro/scripts/seed.js'],
            ['../subflows/sign-in.yaml', '.maestro/subflows/sign-in.yaml'],
            ['../subflows/nested/type.yaml', '.maestro/subflows/nested/type.yaml'],
            ['../subflows/note.yaml', '.maestro/subflows/note.yaml'],
          ]) {
            expect(await readFile(resolve(stored, '..', relative), 'utf8')).toBe(files[original])
          }
          const reopened = ctx.makeStore()
          const hit = await reopened.lookup(contractIdentity())
          expect(hit).toMatchObject({ status: 'hit', flowPath: stored })
          if (hit.status !== 'hit') throw new Error('expected hit')
          expect(hit.walk.subflowFiles).toHaveLength(4)
          expect((await reopened.list())[0].subflowFiles).toEqual(persisted.walk.subflowFiles)
          expect((await reopened.lookupPreviousSuccessful(contractIdentity())).status).toBe('hit')
        })
      })

      test('a flow that runs nothing keeps the single-file layout and has no subflow metadata', async () => {
        const ctx = await context()
        try {
          const persisted = await remember(ctx.makeStore(), ctx.candidatePath)
          expect(persisted.flowPath)
            .toBe(`.deja/flows/checkout/android-${persisted.flowSha256}.yaml`)
          expect(persisted.walk.subflowFiles).toBeUndefined()
        } finally {
          await ctx.cleanup()
        }
      })

      test('a subflow that changed on disk is a new closure and never reuses the old bytes', async () => {
        await withFlows({}, async (ctx, store) => {
          const candidate = join(ctx.workspaceRoot, '.maestro/flows/main.yaml')
          const first = await remember(store, candidate)
          await writeFile(join(ctx.workspaceRoot, '.maestro/subflows/note.yaml'), '- tapOn: Edited note\n')
          const second = await remember(store, candidate, laterValidation)
          expect(second.flowSha256).toBe(first.flowSha256)
          expect(second.flowPath).not.toBe(first.flowPath)
          expect(await store.list()).toHaveLength(1)
          const hit = await ctx.makeStore().lookup(contractIdentity())
          if (hit.status !== 'hit') throw new Error('expected hit')
          expect(hit.walk.flowPath).toBe(second.flowPath)
          expect(await readFile(resolve(hit.flowPath, '../../subflows/note.yaml'), 'utf8'))
            .toBe('- tapOn: Edited note\n')
          // The first closure is still on disk, unchanged and unreferenced.
          expect(await readFile(resolve(ctx.workspaceRoot, first.flowPath, '../../subflows/note.yaml'), 'utf8'))
            .toBe('- tapOn: Note\n')
        })
      })

      test('rejects a tampered stored subflow like a tampered main flow', async () => {
        await withFlows({}, async (ctx, store) => {
          const persisted = await remember(store, join(ctx.workspaceRoot, '.maestro/flows/main.yaml'))
          const stored = resolve(ctx.workspaceRoot, persisted.flowPath)
          await writeFile(resolve(stored, '../../subflows/nested/type.yaml'), '- tapOn: Delete everything\n')
          await expect(store.lookup(contractIdentity())).rejects.toThrow('stored subflow checksum does not match')
          await expect(store.lookupPreviousSuccessful(contractIdentity()))
            .rejects.toThrow('stored subflow checksum does not match')
        })
      })

      test('returns missing-flow when a stored subflow is gone', async () => {
        await withFlows({}, async (ctx, store) => {
          const persisted = await remember(store, join(ctx.workspaceRoot, '.maestro/flows/main.yaml'))
          await rm(resolve(ctx.workspaceRoot, persisted.flowPath, '../../scripts/seed.js'))
          expect(await store.lookup(contractIdentity())).toEqual({ status: 'miss', reason: 'missing-flow' })
        })
      })

      const rejected: [string, Record<string, string>, string][] = [
        ['a reference that leaves the workspace',
          { '.maestro/flows/bad.yaml': '- runFlow: ../../../outside.yaml\n' }, 'escapes the workspace'],
        ['a nested reference that leaves the workspace',
          {
            '.maestro/flows/bad.yaml': '- runFlow: ../subflows/deep.yaml\n',
            '.maestro/subflows/deep.yaml': '- runFlow:\n    file: ../../../outside.yaml\n',
          }, 'escapes the workspace'],
        ['an absolute path',
          { '.maestro/flows/bad.yaml': '- runFlow: /etc/hosts\n' }, 'must be a relative path'],
        ['a path built from a variable',
          { '.maestro/flows/bad.yaml': '- runFlow: ../subflows/${NAME}.yaml\n' }, 'uses a variable'],
        ['a reference to a file that does not exist',
          { '.maestro/flows/bad.yaml': '- runScript: ../scripts/none.js\n' }, 'does not exist'],
        ['flows that run each other',
          {
            '.maestro/flows/bad.yaml': '- runFlow: ../subflows/a.yaml\n',
            '.maestro/subflows/a.yaml': '- runFlow: b.yaml\n',
            '.maestro/subflows/b.yaml': '- runFlow: a.yaml\n',
          }, 'in a cycle'],
      ]
      for (const [name, extra, message] of rejected) {
        test(`rejects ${name} and writes nothing`, async () => {
          await withFlows(extra, async (ctx, store) => {
            await writeFile(join(ctx.workspaceRoot, '..', 'outside.yaml'), '- tapOn: Outside\n')
            await expect(remember(store, join(ctx.workspaceRoot, '.maestro/flows/bad.yaml')))
              .rejects.toThrow(message)
            expect(existsSync(join(ctx.workspaceRoot, '.deja/flows'))).toBe(false)
            expect(existsSync(implementation.metadataPath(ctx.workspaceRoot))).toBe(false)
          })
        })
      }

      test('rejects a symlink inside the workspace that points outside it', async () => {
        await withFlows({ '.maestro/flows/bad.yaml': '- runFlow: ../subflows/link.yaml\n' }, async (ctx, store) => {
          const outside = join(ctx.temporary, 'outside.yaml')
          await writeFile(outside, '- tapOn: Outside\n')
          await symlink(outside, join(ctx.workspaceRoot, '.maestro/subflows/link.yaml'))
          await expect(remember(store, join(ctx.workspaceRoot, '.maestro/flows/bad.yaml')))
            .rejects.toThrow('escapes the workspace')
          expect(existsSync(join(ctx.workspaceRoot, '.deja/flows'))).toBe(false)
        })
      })
    })
  })
}
