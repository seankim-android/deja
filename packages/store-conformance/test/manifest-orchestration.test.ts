import { describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  createDeja,
  parseWalkManifest,
  type DejaAdapter,
  type ExecutionResult,
  type RunnerRequest,
  type RunResult,
  type WalkRunner,
} from '@deja-run/core'
import {
  localStoreContract,
  sqliteStoreContract,
  type WalkStoreContractImplementation,
} from '../src/index.ts'

const MANIFEST = 'deja.walks.json'
const MAIN = '.maestro/flows/today.yaml'
const SUB = '.maestro/subflows/type.yaml'
const MAIN_YAML = 'appId: dev.deja.fixture\n---\n- tapOn:\n    id: "today.answer"\n- runFlow: ../subflows/type.yaml\n- assertVisible:\n    id: "today.saved"\n'
const SUB_YAML = '- inputText: Popcorn\n'

let build = 'a'.repeat(64)
const adapter: DejaAdapter = {
  id: 'maestro-ios/v1',
  platform: 'ios',
  buildFingerprint: async () => build,
  cacheIdentity: ({ buildFingerprint }) => ({ buildFingerprint, adapter: 'maestro-ios/v1' }),
}

const passed = (at: string): ExecutionResult => ({
  passed: true, startedAt: at, endedAt: at, durationMs: 1, exitCode: 0, assertions: 1, failure: null, artifacts: [],
})

function manifestOrchestrationTests(implementation: WalkStoreContractImplementation) {
  async function setup(onRun?: (request: RunnerRequest) => Promise<void> | void) {
    build = 'a'.repeat(64)
    const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-manifest-orchestration-')))
    const workspaceRoot = join(temporary, 'repo')
    await mkdir(join(workspaceRoot, '.maestro/flows'), { recursive: true })
    await mkdir(join(workspaceRoot, '.maestro/subflows'), { recursive: true })
    await writeFile(join(workspaceRoot, MAIN), MAIN_YAML)
    await writeFile(join(workspaceRoot, SUB), SUB_YAML)
    const runs: RunnerRequest[] = []
    let clock = 0
    const runner: WalkRunner = {
      run: async request => {
        runs.push(request)
        await onRun?.(request)
        clock += 1
        return passed(new Date(Date.UTC(2026, 10, 22, 0, clock)).toISOString())
      },
    }
    let explorers = 0
    const stores: ReturnType<typeof implementation.createStore>[] = []
    const make = (extra: { staleBuild?: 'replay-last-verified' } = {}) => {
      const store = implementation.createStore(workspaceRoot, undefined, undefined, { path: MANIFEST })
      stores.push(store)
      const deja = createDeja({
        workspaceRoot,
        app: { appId: 'dev.deja.fixture', buildPath: 'build.app' },
        adapter,
        store,
        manifestPath: MANIFEST,
        runner,
        explorer: () => { explorers += 1; throw new Error('a manifest miss must not build an explorer') },
        reporter: { report: async () => ({ reportPath: '', latestPath: '' }) },
        artifactId: prefix => `${prefix}-fixed`,
        ...extra,
      })
      return { store, deja }
    }
    return {
      workspaceRoot, runs, make, explorers: () => explorers,
      write: (path: string, contents: string) => writeFile(join(workspaceRoot, path), contents),
      manifest: async () => parseWalkManifest(await readFile(join(workspaceRoot, MANIFEST), 'utf8')),
      cleanup: async () => {
        for (const store of stores.reverse()) await implementation.closeStore(store)
        await rm(temporary, { recursive: true, force: true })
      },
    }
  }

  const summary = (result: RunResult) => ({
    outcome: result.outcome, passed: result.passed, cache: result.cacheStatus, miss: result.cacheMissReason,
  })

  describe(`${implementation.name}: orchestration with a manifest`, () => {
    test('remember then run replays the consumer file in place, with zero model calls', async () => {
      const ctx = await setup()
      try {
        const { deja } = ctx.make()
        const remembered = await deja.remember('Today', MAIN)
        expect(summary(remembered)).toMatchObject({ outcome: 'remembered', passed: true })
        expect(remembered.artifacts.some(artifact => artifact.kind === 'manifest' && artifact.path === MANIFEST)).toBe(true)
        expect((await ctx.manifest()).walks).toHaveLength(1)
        const run = await deja.run('Today')
        expect(summary(run)).toEqual({ outcome: 'passed', passed: true, cache: 'hit', miss: undefined })
        expect(run.modelCalls).toBe(0)
        // Both the verification and the replay ran the consumer's path, not a copy under .deja.
        expect(ctx.runs.map(request => request.flowPath)).toEqual([MAIN, join(ctx.workspaceRoot, MAIN)])
        expect(run.yamlPath).toBe(MAIN)
        expect(ctx.explorers()).toBe(0)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a changed main flow or subflow gives flow-changed, replays nothing and never explores', async () => {
      const ctx = await setup()
      try {
        const { deja } = ctx.make()
        await deja.remember('Today', MAIN)
        for (const [path, contents] of [[MAIN, `${MAIN_YAML}# edited\n`], [SUB, `${SUB_YAML}# edited\n`]] as const) {
          const original = await readFile(join(ctx.workspaceRoot, path), 'utf8')
          await ctx.write(path, contents)
          const before = ctx.runs.length
          const result = await deja.run('Today')
          expect(summary(result)).toEqual({ outcome: 'flow-changed', passed: false, cache: 'miss', miss: 'flow-changed' })
          expect(result.errors[0]).toContain('--verify')
          expect(ctx.runs).toHaveLength(before)
          await ctx.write(path, original)
        }
        expect(ctx.explorers()).toBe(0)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a missing file is flow-changed without replay, a missing goal never explores', async () => {
      const ctx = await setup()
      try {
        const { deja } = ctx.make()
        await deja.remember('Today', MAIN)
        await rm(join(ctx.workspaceRoot, SUB))
        expect(summary(await deja.run('Today'))).toMatchObject({ outcome: 'flow-changed', miss: 'missing-flow' })
        const unknown = await deja.run('Something else')
        expect(unknown).toMatchObject({ outcome: 'exploration-rejected', passed: false, modelCalls: 0 })
        expect((await deja.explore('Today')).outcome).toBe('exploration-rejected')
        expect(ctx.explorers()).toBe(0)
      } finally {
        await ctx.cleanup()
      }
    })

    test('verify re-runs the changed file, updates the sha, and records history', async () => {
      const ctx = await setup()
      try {
        const { deja } = ctx.make()
        await deja.remember('Today', MAIN)
        const before = (await ctx.manifest()).walks[0]
        await ctx.write(MAIN, MAIN_YAML.replace('today.answer', 'today.answer.field'))
        const result = await deja.run('Today', { verify: true })
        expect(summary(result)).toMatchObject({ outcome: 'passed', passed: true, miss: 'flow-changed' })
        expect(ctx.runs.at(-1)?.flowPath).toBe(MAIN)
        const after = (await ctx.manifest()).walks[0]
        expect(after.closureSha256).not.toBe(before.closureSha256)
        expect(after.assertionsSha256).toBe(before.assertionsSha256)
        expect(summary(await deja.run('Today'))).toMatchObject({ outcome: 'passed', cache: 'hit' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('verify with a changed assertion fails and writes nothing; a reviewed remember accepts it', async () => {
      const ctx = await setup()
      try {
        const { deja } = ctx.make()
        await deja.remember('Today', MAIN)
        const manifestBefore = await readFile(join(ctx.workspaceRoot, MANIFEST), 'utf8')
        const weakened = MAIN_YAML.replace('- assertVisible:\n    id: "today.saved"\n', '')
        await ctx.write(MAIN, weakened)
        const verified = await deja.run('Today', { verify: true })
        expect(summary(verified)).toMatchObject({ outcome: 'assertions-changed', passed: false })
        expect(verified.errors[0]).toContain('--accept-assertion-change')
        expect(await readFile(join(ctx.workspaceRoot, MANIFEST), 'utf8')).toBe(manifestBefore)

        const refused = await deja.remember('Today', MAIN)
        expect(summary(refused)).toMatchObject({ outcome: 'assertions-changed', passed: false })
        expect(await readFile(join(ctx.workspaceRoot, MANIFEST), 'utf8')).toBe(manifestBefore)

        const accepted = await deja.remember('Today', MAIN, { acceptAssertionChange: true })
        expect(summary(accepted)).toMatchObject({ outcome: 'remembered', passed: true })
        expect(await readFile(join(ctx.workspaceRoot, MANIFEST), 'utf8')).not.toBe(manifestBefore)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a flow edited while verify runs is flow-changed and nothing is written', async () => {
      let edit: (() => Promise<void>) | null = null
      const ctx = await setup(async () => { await edit?.() })
      try {
        const { deja } = ctx.make()
        await deja.remember('Today', MAIN)
        await ctx.write(MAIN, MAIN_YAML.replace('today.answer', 'today.answer.field'))
        const manifestBefore = await readFile(join(ctx.workspaceRoot, MANIFEST), 'utf8')
        edit = () => ctx.write(SUB, `${SUB_YAML}# changed while running\n`)
        const result = await deja.run('Today', { verify: true })
        expect(summary(result)).toMatchObject({ outcome: 'flow-changed', passed: false })
        expect(await readFile(join(ctx.workspaceRoot, MANIFEST), 'utf8')).toBe(manifestBefore)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a replay that raced an edit is not recorded as a pass', async () => {
      let edit: (() => Promise<void>) | null = null
      const ctx = await setup(async () => { await edit?.() })
      try {
        const { deja, store } = ctx.make()
        await deja.remember('Today', MAIN)
        const historyBefore = (await store.list())[0].validationHistory.length
        edit = () => ctx.write(SUB, `${SUB_YAML}# changed while running\n`)
        const result = await deja.run('Today')
        expect(summary(result)).toMatchObject({ outcome: 'flow-changed', passed: false })
        expect((await store.list())[0].validationHistory).toHaveLength(historyBefore)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a fresh clone needs verify; verify then replays', async () => {
      const ctx = await setup()
      try {
        await ctx.make().deja.remember('Today', MAIN)
        await rm(join(ctx.workspaceRoot, '.deja'), { recursive: true, force: true })
        const { deja } = ctx.make()
        const refused = await deja.run('Today')
        expect(summary(refused)).toMatchObject({ outcome: 'not-validated', passed: false })
        expect(ctx.explorers()).toBe(0)
        const before = ctx.runs.length
        expect(summary(await deja.run('Today', { verify: true }))).toMatchObject({ outcome: 'passed', passed: true })
        expect(ctx.runs).toHaveLength(before + 1)
        expect(summary(await deja.run('Today'))).toMatchObject({ outcome: 'passed', cache: 'hit' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('a new build replays the consumer file once with the replay policy, and the manifest stays put', async () => {
      const ctx = await setup()
      try {
        const { deja } = ctx.make({ staleBuild: 'replay-last-verified' })
        await deja.remember('Today', MAIN)
        const manifestBefore = await readFile(join(ctx.workspaceRoot, MANIFEST), 'utf8')
        build = 'b'.repeat(64)
        const result = await deja.run('Today')
        expect(summary(result)).toMatchObject({ outcome: 'passed', passed: true })
        expect(result.replayedOnNewBuild).toMatchObject({ recordedForNewBuild: true, policy: 'replay-last-verified' })
        expect(ctx.runs.at(-1)?.flowPath).toBe(MAIN)
        expect(await readFile(join(ctx.workspaceRoot, MANIFEST), 'utf8')).toBe(manifestBefore)
        expect(summary(await deja.run('Today'))).toMatchObject({ outcome: 'passed', cache: 'hit' })
      } finally {
        await ctx.cleanup()
      }
    })

    test('a stale build without the policy is refused, and changing the flow beats stale-build', async () => {
      const ctx = await setup()
      try {
        const { deja } = ctx.make()
        await deja.remember('Today', MAIN)
        build = 'b'.repeat(64)
        const stale = await deja.run('Today')
        expect(summary(stale)).toMatchObject({ outcome: 'repair-rejected', miss: 'stale-build' })
        expect(stale.errors[0]).toContain('--verify')
        await ctx.write(MAIN, `${MAIN_YAML}# edited\n`)
        expect(summary(await deja.run('Today'))).toMatchObject({ outcome: 'flow-changed' })
        // verify covers a stale build too: it re-runs the file on this build and records it.
        await ctx.write(MAIN, MAIN_YAML)
        expect(summary(await deja.run('Today', { verify: true }))).toMatchObject({ outcome: 'passed' })
        expect(ctx.explorers()).toBe(0)
      } finally {
        await ctx.cleanup()
      }
    })

    test('remember refuses a flow under .deja, and createDeja refuses a mismatched manifest or a repairer', async () => {
      const ctx = await setup()
      try {
        const { deja, store } = ctx.make()
        await mkdir(join(ctx.workspaceRoot, '.deja/attempts'), { recursive: true })
        await ctx.write('.deja/attempts/candidate.yaml', MAIN_YAML)
        await expect(deja.remember('Today', '.deja/attempts/candidate.yaml')).rejects.toThrow(/not under \.deja/)
        expect(ctx.runs).toHaveLength(0)
        const base = {
          workspaceRoot: ctx.workspaceRoot, app: { appId: 'dev.deja.fixture', buildPath: 'build.app' },
          adapter, runner: { run: async () => passed('2026-11-22T00:00:00.000Z') },
          explorer: () => { throw new Error('no explorer') },
        }
        expect(() => createDeja({ ...base, store, manifestPath: 'other.json' })).toThrow(/needs a store built for the same manifest/)
        const plain = implementation.createStore(ctx.workspaceRoot, undefined, 'plain')
        try {
          expect(() => createDeja({ ...base, store: plain, manifestPath: MANIFEST })).toThrow(/refusing to replay a stored copy/)
        } finally {
          await implementation.closeStore(plain)
        }
        expect(() => createDeja({
          ...base, store, manifestPath: MANIFEST,
          repairer: () => { throw new Error('no repairer') },
        })).toThrow(/Repair is not supported with a manifest/)
      } finally {
        await ctx.cleanup()
      }
    })

    test('with a flows folder, a new goal is explored once and the result is a new file in the tree', async () => {
      const ctx = await setup()
      try {
        let explorers = 0
        const store = implementation.createStore(ctx.workspaceRoot, undefined, 'explore', {
          path: MANIFEST, flowsDir: '.maestro/flows/deja',
        })
        const deja = createDeja({
          workspaceRoot: ctx.workspaceRoot,
          app: { appId: 'dev.deja.fixture', buildPath: 'build.app' },
          adapter, store, manifestPath: MANIFEST,
          runner: { run: async () => passed('2026-11-22T00:00:00.000Z') },
          reporter: { report: async () => ({ reportPath: '', latestPath: '' }) },
          explorer: ({ identity }) => ({
            explore: async () => {
              explorers += 1
              await mkdir(join(ctx.workspaceRoot, '.deja/attempts/explore'), { recursive: true })
              await writeFile(join(ctx.workspaceRoot, '.deja/attempts/explore/candidate.yaml'), MAIN_YAML.replace('- runFlow: ../subflows/type.yaml\n', ''))
              const persisted = await store.persistVerifiedCandidate({
                identity, candidatePath: '.deja/attempts/explore/candidate.yaml', origin: 'explore',
                validation: { passed: true, at: '2026-11-22T00:00:00.000Z' },
              })
              return {
                passed: true, durationMs: 1, explorationDurationMs: 1, verificationDurationMs: 1, mcpCalls: 1,
                modelCalls: 2, modelCallBasis: 'fixture', model: 'm', provider: 'p', assertions: 1,
                flowPath: persisted.flowPath, flowSha256: persisted.flowSha256, failure: null,
                artifacts: [], errors: [],
              }
            },
          }),
        })
        try {
          const cold = await deja.run('Brand new goal')
          expect(summary(cold)).toMatchObject({ outcome: 'cold-recorded', passed: true })
          expect(explorers).toBe(1)
          expect(cold.yamlPath).toBe('.maestro/flows/deja/brand-new-goal.yaml')
          expect((await ctx.manifest()).walks.map(walk => walk.flow)).toEqual(['.maestro/flows/deja/brand-new-goal.yaml'])
          // The next run replays the new file, and a forced explore of a listed goal is refused up front.
          expect(summary(await deja.run('Brand new goal'))).toMatchObject({ outcome: 'passed', cache: 'hit' })
          expect(explorers).toBe(1)
          const forced = await deja.explore('Brand new goal')
          expect(forced).toMatchObject({ outcome: 'exploration-rejected', modelCalls: 0 })
          expect(forced.errors[0]).toContain('already lists this goal')
          expect(explorers).toBe(1)
          // A changed flow still never reaches the explorer.
          await ctx.write('.maestro/flows/deja/brand-new-goal.yaml', `${MAIN_YAML}# edited\n`)
          expect(summary(await deja.run('Brand new goal'))).toMatchObject({ outcome: 'flow-changed' })
          expect(explorers).toBe(1)
        } finally {
          await implementation.closeStore(store)
        }
      } finally {
        await ctx.cleanup()
      }
    })

    test('a goal that only has an old walk is needs-migration', async () => {
      const ctx = await setup()
      try {
        const legacy = implementation.createStore(ctx.workspaceRoot)
        await legacy.persistVerifiedCandidate({
          identity: {
            goal: 'Today', appId: 'dev.deja.fixture', platform: 'ios', flowSchemaVersion: 'maestro/v1',
            buildFingerprint: 'a'.repeat(64), adapter: 'maestro-ios/v1',
          },
          candidatePath: MAIN, validation: { passed: true, at: '2026-11-22T00:00:00.000Z' }, origin: 'remember',
        })
        await implementation.closeStore(legacy)
        const { deja } = ctx.make()
        expect(summary(await deja.run('Today'))).toMatchObject({ outcome: 'needs-migration', passed: false })
        expect(ctx.runs).toHaveLength(0)
        expect(ctx.explorers()).toBe(0)
      } finally {
        await ctx.cleanup()
      }
    })
  })
}

manifestOrchestrationTests(localStoreContract)
manifestOrchestrationTests(sqliteStoreContract)
