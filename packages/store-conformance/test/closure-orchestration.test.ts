import { describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, readFile, readdir, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  createDeja,
  type CandidateSnapshot,
  type DejaAdapter,
  type ExecutionResult,
  type RunnerRequest,
  type RunResult,
  type WalkRunner,
  type WalkStore,
} from '@deja-qa/core'
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
const SWAPPED = '- inputText: Something else\n'
const VARIABLE = '- runFlow: ${NAME}.yaml\n'

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

/** Every file under `root` with its bytes, to show that a refused run wrote nothing. */
async function snapshotTree(root: string, skip: string[] = []): Promise<Record<string, string>> {
  const found: Record<string, string> = {}
  const visit = async (folder: string) => {
    for (const entry of await readdir(folder, { withFileTypes: true })) {
      const path = join(folder, entry.name)
      const relative = path.slice(root.length + 1)
      if (skip.some(item => relative.startsWith(item))) continue
      if (entry.isDirectory()) await visit(path)
      else found[relative] = (await readFile(path)).toString('base64')
    }
  }
  await visit(root)
  return found
}

function closureOrchestrationTests(implementation: WalkStoreContractImplementation, manifest: boolean) {
  const mode = manifest ? 'manifest' : 'classic'

  async function setup(onRun?: (request: RunnerRequest) => Promise<void> | void) {
    build = 'a'.repeat(64)
    const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-closure-orchestration-')))
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
    const store: WalkStore = implementation.createStore(
      workspaceRoot, undefined, undefined, manifest ? { path: MANIFEST } : undefined,
    )
    const deja = createDeja({
      workspaceRoot,
      app: { appId: 'dev.deja.fixture', buildPath: 'build.app' },
      adapter,
      store,
      ...(manifest ? { manifestPath: MANIFEST } : {}),
      staleBuild: 'replay-last-verified',
      runner,
      explorer: () => { throw new Error('a closure error must not build an explorer') },
      reporter: { report: async () => ({ reportPath: '', latestPath: '' }) },
      artifactId: prefix => `${prefix}-fixed`,
    })
    /** Run `change` right before the store snapshots the stored flow for a replay. */
    const beforeSnapshot = (change: () => Promise<void>) => {
      const mutable = store as { snapshotCandidate?: WalkStore['snapshotCandidate'] }
      const real = mutable.snapshotCandidate!.bind(store)
      mutable.snapshotCandidate = async (path: string, directory: string): Promise<CandidateSnapshot> => {
        await change()
        return real(path, directory)
      }
    }
    /** The subflow file the replay runs: the consumer's own, or the stored copy. */
    const subflowPath = async () => manifest
      ? join(workspaceRoot, SUB)
      : join(workspaceRoot, (await store.list())[0].subflowFiles![0].path)
    return {
      workspaceRoot, runs, deja, store, beforeSnapshot, subflowPath,
      write: (path: string, contents: string) => writeFile(join(workspaceRoot, path), contents),
      tree: () => snapshotTree(workspaceRoot, ['.deja/reports', '.deja/attempts', '.deja/store.lock', '.deja/manifest.lock']),
      cleanup: async () => {
        await implementation.closeStore(store)
        await rm(temporary, { recursive: true, force: true })
      },
    }
  }

  const summary = (result: RunResult) => ({ outcome: result.outcome, passed: result.passed })

  describe(`${implementation.name}: closure checks around a replay (${mode} mode)`, () => {
    test('a stored subflow swapped before the snapshot is refused, and nothing is written', async () => {
      const ctx = await setup()
      try {
        await ctx.deja.remember('Today', MAIN)
        build = 'b'.repeat(64)
        const sub = await ctx.subflowPath()
        ctx.beforeSnapshot(() => writeFile(sub, SWAPPED))
        const runsBefore = ctx.runs.length
        const history = JSON.stringify(await ctx.store.list())
        const treeBefore = await ctx.tree()
        treeBefore[manifest ? SUB : sub.slice(ctx.workspaceRoot.length + 1)] = Buffer.from(SWAPPED).toString('base64')

        const result = await ctx.deja.run('Today')
        expect(summary(result)).toEqual({ outcome: 'closure-error', passed: false })
        expect(result.errors[0]).toMatch(manifest ? /no longer matches the manifest/ : /no longer matches its recorded checksum/)
        expect(ctx.runs).toHaveLength(runsBefore)
        expect(JSON.stringify(await ctx.store.list())).toBe(history)
        // The only difference from before is the swap the test made itself.
        expect(await ctx.tree()).toEqual(treeBefore)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a subflow that became unreadable before the snapshot is a closure error, and nothing is written', async () => {
      const ctx = await setup()
      try {
        await ctx.deja.remember('Today', MAIN)
        build = 'b'.repeat(64)
        const sub = await ctx.subflowPath()
        ctx.beforeSnapshot(() => writeFile(sub, VARIABLE))
        const runsBefore = ctx.runs.length
        const history = JSON.stringify(await ctx.store.list())

        const result = await ctx.deja.run('Today')
        expect(summary(result)).toEqual({ outcome: 'closure-error', passed: false })
        expect(result.errors[0]).toContain('uses a variable')
        expect(ctx.runs).toHaveLength(runsBefore)
        expect(JSON.stringify(await ctx.store.list())).toBe(history)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a closure error while recording a replay is a failed result, and the history stays (manifest); classic keeps its hard error', async () => {
      let edit: (() => Promise<void>) | null = null
      const ctx = await setup(async () => { await edit?.() })
      try {
        await ctx.deja.remember('Today', MAIN)
        const history = JSON.stringify(await ctx.store.list())
        const sub = await ctx.subflowPath()
        edit = () => writeFile(sub, VARIABLE)
        if (!manifest) {
          // Classic mode keeps its long-standing answer: a stored file that changed is corrupt memory
          // (exit 4), not a closure error. DEJA-32 leaves it alone.
          await expect(ctx.deja.run('Today')).rejects.toThrow('stored subflow checksum does not match')
          return
        }
        const result = await ctx.deja.run('Today')
        expect(summary(result)).toEqual({ outcome: 'closure-error', passed: false })
        expect(result.execution?.passed).toBe(true)
        expect(JSON.stringify(await ctx.store.list())).toBe(history)
      } finally {
        await ctx.cleanup()
      }
    })

    test('a closure error from the first lookup is a failed result and nothing runs', async () => {
      const ctx = await setup()
      try {
        await ctx.deja.remember('Today', MAIN)
        if (!manifest) return
        await ctx.write(SUB, VARIABLE)
        const runsBefore = ctx.runs.length
        const result = await ctx.deja.run('Today')
        expect(summary(result)).toEqual({ outcome: 'closure-error', passed: false })
        expect(ctx.runs).toHaveLength(runsBefore)
      } finally {
        await ctx.cleanup()
      }
    })
  })
}

for (const manifest of [false, true]) {
  closureOrchestrationTests(localStoreContract, manifest)
  closureOrchestrationTests(sqliteStoreContract, manifest)
}
