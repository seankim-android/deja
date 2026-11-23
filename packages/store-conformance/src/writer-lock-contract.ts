import { resolveHarness, type ContractHarness } from './harness.ts'
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process'
import { existsSync } from 'node:fs'
import { mkdir, mkdtemp, readFile, readdir, realpath, rm, writeFile } from 'node:fs/promises'
import { hostname, tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { InvalidStoreIdError, StoreLockedError } from '@deja-qa/store-local'
import { contractIdentity, initialValidation, type WalkStoreContractImplementation } from './walk-store-contract.ts'

async function temporaryFilesUnder(root: string): Promise<string[]> {
  const found: string[] = []
  for (const entry of await readdir(root, { withFileTypes: true })) {
    const path = join(root, entry.name)
    if (entry.isDirectory()) found.push(...await temporaryFilesUnder(path))
    else if (entry.name.endsWith('.tmp')) found.push(path)
  }
  return found
}

const writerProcess = resolve(import.meta.dirname, '../test/fixtures/writer-process.mjs')

type Child = {
  process: ChildProcessWithoutNullStreams
  /** The first line it printed: ACQUIRED or LOCKED:<code>. */
  first: Promise<string>
  exited: Promise<number | null>
  /** The signal that ended it, if any. */
  signal: Promise<string | null>
}

function startWriter(
  implementation: WalkStoreContractImplementation,
  workspaceRoot: string,
  storeId: string | undefined,
  mode?: 'exit-now',
): Child {
  const child = spawn(process.execPath, [
    writerProcess, implementation.kind, workspaceRoot, storeId ?? '-', ...(mode ? [mode] : []),
  ], { stdio: 'pipe' })
  const first = new Promise<string>((resolveFirst, reject) => {
    let text = ''
    child.stdout.on('data', chunk => {
      text += String(chunk)
      if (text.includes('\n')) resolveFirst(text.split('\n')[0])
    })
    child.once('close', () => resolveFirst(text.split('\n')[0] || 'NO-OUTPUT'))
    child.once('error', reject)
  })
  const exited = new Promise<number | null>(resolveExit => child.once('close', code => resolveExit(code)))
  const signal = new Promise<string | null>(resolveSignal => child.once('close', (_code, ended) => resolveSignal(ended)))
  return { process: child, first, exited, signal }
}

async function workspace() {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja4-lock-')))
  const workspaceRoot = join(temporary, 'workspace')
  await mkdir(join(workspaceRoot, 'flows'), { recursive: true })
  await writeFile(
    join(workspaceRoot, 'flows/candidate.yaml'),
    'appId: dev.deja.checkout\n---\n- assertVisible: Order confirmed\n',
  )
  return { temporary, workspaceRoot }
}

function persist(store: ReturnType<WalkStoreContractImplementation['createStore']>, goal = 'Checkout') {
  return store.persistVerifiedCandidate({
    identity: contractIdentity({ goal }),
    candidatePath: 'flows/candidate.yaml',
    validation: { passed: true, at: initialValidation },
    origin: 'remember',
  })
}

/** The store's own lock path, taken from a store that holds it. */
function lockPathOf(implementation: WalkStoreContractImplementation, root: string, storeId?: string) {
  return `${implementation.metadataPath(root, storeId)}.lock`
}

export function runWriterLockContractTests(implementation: WalkStoreContractImplementation, harness?: ContractHarness) {
  const { describe, test, expect } = resolveHarness(harness)
  describe(`${implementation.name}: one writer per store, one store per device`, () => {
    test('a second writer in the same process is refused with a typed error, and reads still work', async () => {
      const { temporary, workspaceRoot } = await workspace()
      const first = implementation.createStore(workspaceRoot)
      const second = implementation.createStore(workspaceRoot)
      try {
        await persist(first)
        const error = await persist(second, 'Other').catch(caught => caught)
        expect(error).toBeInstanceOf(StoreLockedError)
        expect(error.code).toBe('STORE_LOCKED')
        expect(error.holder.pid).toBe(process.pid)
        expect(error.message).toContain('storeId')
        // Nothing of the refused write reached the store, and the lock does not block readers.
        expect((await second.list()).map(walk => walk.goalKey)).toEqual(['checkout'])
        expect((await second.lookup(contractIdentity())).status).toBe('hit')
        // The refused writer cannot record a validation either.
        await expect(second.recordValidation(contractIdentity(), { passed: true, at: initialValidation }))
          .rejects.toBeInstanceOf(StoreLockedError)
        // Closing the holder frees the store.
        await implementation.closeStore(first)
        await persist(second, 'Other')
        expect((await second.list()).map(walk => walk.goalKey).sort()).toEqual(['checkout', 'other'])
      } finally {
        await implementation.closeStore(first)
        await implementation.closeStore(second)
        await rm(temporary, { recursive: true, force: true })
      }
    })

    test('acquireWriter refuses before any work, and a read-only store never takes the lock', async () => {
      const { temporary, workspaceRoot } = await workspace()
      const reader = implementation.createStore(workspaceRoot)
      const writer = implementation.createStore(workspaceRoot)
      try {
        await reader.list()
        await reader.lookup(contractIdentity())
        expect(existsSync(lockPathOf(implementation, workspaceRoot))).toBe(false)
        ;(writer as unknown as { acquireWriter(): void }).acquireWriter()
        expect(existsSync(lockPathOf(implementation, workspaceRoot))).toBe(true)
        expect(() => (reader as unknown as { acquireWriter(): void }).acquireWriter())
          .toThrow(StoreLockedError)
      } finally {
        await implementation.closeStore(reader)
        await implementation.closeStore(writer)
        await rm(temporary, { recursive: true, force: true })
      }
    })

    test('two processes racing for one store: exactly one wins, every time, and history stays valid', async () => {
      const { temporary, workspaceRoot } = await workspace()
      try {
        for (let round = 0; round < 5; round += 1) {
          const children = [
            startWriter(implementation, workspaceRoot, undefined),
            startWriter(implementation, workspaceRoot, undefined),
          ]
          const firsts = await Promise.all(children.map(child => child.first))
          expect(firsts.filter(line => line === 'ACQUIRED')).toHaveLength(1)
          expect(firsts.filter(line => line === 'LOCKED:STORE_LOCKED')).toHaveLength(1)
          // Only the winner is waiting for a line; the loser has already exited with LOCKED.
          children[firsts.indexOf('ACQUIRED')].process.stdin.write('go\n')
          const codes = await Promise.all(children.map(child => child.exited))
          expect(codes.sort()).toEqual([0, 3])
          expect(existsSync(lockPathOf(implementation, workspaceRoot))).toBe(false)
        }
        const store = implementation.createStore(workspaceRoot)
        try {
          // One walk per winning process; none lost, none torn.
          expect(await store.list()).toHaveLength(5)
        } finally {
          await implementation.closeStore(store)
        }
      } finally {
        await rm(temporary, { recursive: true, force: true })
      }
    })

    test('a process that is killed leaves a stale lock, and the next writer takes over', async () => {
      const { temporary, workspaceRoot } = await workspace()
      const store = implementation.createStore(workspaceRoot)
      try {
        const child = startWriter(implementation, workspaceRoot, undefined)
        expect(await child.first).toBe('ACQUIRED')
        const lockPath = lockPathOf(implementation, workspaceRoot)
        expect(existsSync(lockPath)).toBe(true)
        child.process.kill('SIGKILL')
        await child.exited
        // SIGKILL runs no exit hook: the lock file is still there.
        expect(existsSync(lockPath)).toBe(true)
        await persist(store)
        const holder = JSON.parse(await readFile(lockPath, 'utf8'))
        expect(holder.pid).toBe(process.pid)
        expect((await store.list()).map(walk => walk.goalKey).sort()).toEqual(['checkout', expect.stringMatching(/^race \d+$/)])
      } finally {
        await implementation.closeStore(store)
        await rm(temporary, { recursive: true, force: true })
      }
    })

    for (const name of ['SIGINT', 'SIGTERM'] as const) {
      test(`${name} releases the lock and still ends the process`, async () => {
        const { temporary, workspaceRoot } = await workspace()
        try {
          const child = startWriter(implementation, workspaceRoot, undefined)
          expect(await child.first).toBe('ACQUIRED')
          const lockPath = lockPathOf(implementation, workspaceRoot)
          expect(existsSync(lockPath)).toBe(true)
          child.process.kill(name)
          await child.exited
          expect(await child.signal).toBe(name)
          expect(existsSync(lockPath)).toBe(false)
          const store = implementation.createStore(workspaceRoot)
          try {
            await persist(store, 'After signal')
          } finally {
            await implementation.closeStore(store)
          }
        } finally {
          await rm(temporary, { recursive: true, force: true })
        }
      })
    }

    test('stores with different ids in one process write the same flow in parallel without errors or torn reads', async () => {
      for (let round = 0; round < 8; round += 1) {
        const { temporary, workspaceRoot } = await workspace()
        const stores = ['a', 'b', 'c', 'd', 'e', 'f'].map(id => implementation.createStore(workspaceRoot, {}, id))
        try {
          const results = await Promise.allSettled(stores.map(async store => {
            const persisted = await persist(store)
            // A reader that arrives while other stores rewrite the same flow sees a whole file.
            const hit = await store.lookup(contractIdentity())
            expect(hit.status).toBe('hit')
            return persisted.flowPath
          }))
          expect(results.filter(result => result.status === 'rejected')).toEqual([])
          const paths = new Set(results.map(result => (result as PromiseFulfilledResult<string>).value))
          expect(paths.size).toBe(1)
          expect(await readFile(join(workspaceRoot, [...paths][0]), 'utf8'))
            .toBe('appId: dev.deja.checkout\n---\n- assertVisible: Order confirmed\n')
          expect((await temporaryFilesUnder(join(workspaceRoot, '.deja')))).toEqual([])
        } finally {
          for (const store of stores) await implementation.closeStore(store)
          await rm(temporary, { recursive: true, force: true })
        }
      }
    })

    test('a lock whose pid now belongs to a different process is stale', async () => {
      const { temporary, workspaceRoot } = await workspace()
      const store = implementation.createStore(workspaceRoot)
      try {
        const lockPath = lockPathOf(implementation, workspaceRoot)
        await mkdir(dirname(lockPath), { recursive: true })
        await writeFile(lockPath, `${JSON.stringify({
          version: 1, pid: process.pid, startedAt: 'Mon Jan  1 00:00:00 2001',
          host: hostname(), token: 'reused-pid', acquiredAt: '2001-01-01T00:00:00.000Z',
        })}\n`)
        await persist(store)
        expect((await store.list())).toHaveLength(1)
      } finally {
        await implementation.closeStore(store)
        await rm(temporary, { recursive: true, force: true })
      }
    })

    test('a lock from another host, or one that cannot be read, is never taken over', async () => {
      const { temporary, workspaceRoot } = await workspace()
      const store = implementation.createStore(workspaceRoot)
      try {
        const lockPath = lockPathOf(implementation, workspaceRoot)
        await mkdir(dirname(lockPath), { recursive: true })
        await writeFile(lockPath, `${JSON.stringify({
          version: 1, pid: 999999, startedAt: null, host: 'some-other-machine.invalid',
          token: 'remote', acquiredAt: '2026-11-22T00:00:00.000Z',
        })}\n`)
        await expect(persist(store)).rejects.toBeInstanceOf(StoreLockedError)
        await writeFile(lockPath, 'garbage')
        await expect(persist(store)).rejects.toBeInstanceOf(StoreLockedError)
        expect(existsSync(lockPath)).toBe(true)
        expect(await store.list()).toEqual([])
      } finally {
        await implementation.closeStore(store)
        await rm(temporary, { recursive: true, force: true })
      }
    })

    test('a process that exits normally releases its lock', async () => {
      const { temporary, workspaceRoot } = await workspace()
      try {
        const child = startWriter(implementation, workspaceRoot, undefined, 'exit-now')
        expect(await child.first).toBe('ACQUIRED')
        expect(await child.exited).toBe(0)
        expect(existsSync(lockPathOf(implementation, workspaceRoot))).toBe(false)
      } finally {
        await rm(temporary, { recursive: true, force: true })
      }
    })

    test('two processes on two store ids run side by side, each with its own history', async () => {
      const { temporary, workspaceRoot } = await workspace()
      try {
        const children = [
          startWriter(implementation, workspaceRoot, 'udid-A'),
          startWriter(implementation, workspaceRoot, 'udid-B'),
        ]
        expect(await Promise.all(children.map(child => child.first))).toEqual(['ACQUIRED', 'ACQUIRED'])
        expect(existsSync(lockPathOf(implementation, workspaceRoot, 'udid-A'))).toBe(true)
        expect(existsSync(lockPathOf(implementation, workspaceRoot, 'udid-B'))).toBe(true)
        // Each store can be listed while its writer runs.
        const a = implementation.createStore(workspaceRoot, {}, 'udid-A')
        const b = implementation.createStore(workspaceRoot, {}, 'udid-B')
        try {
          expect(await a.list()).toHaveLength(1)
          expect(await b.list()).toHaveLength(1)
          expect((await a.list())[0].goalKey).not.toBe((await b.list())[0].goalKey)
        } finally {
          await implementation.closeStore(a)
          await implementation.closeStore(b)
        }
        for (const child of children) child.process.stdin.write('go\n')
        expect(await Promise.all(children.map(child => child.exited))).toEqual([0, 0])
        // The default store was never touched.
        expect(existsSync(implementation.metadataPath(workspaceRoot))).toBe(false)
        // The same flow written by both stores is one content-addressed file.
        const flows = implementation.createStore(workspaceRoot, {}, 'udid-A')
        try {
          const [walk] = await flows.list()
          expect(existsSync(join(workspaceRoot, walk.flowPath))).toBe(true)
        } finally {
          await implementation.closeStore(flows)
        }
      } finally {
        await rm(temporary, { recursive: true, force: true })
      }
    })

    test('a store id is a plain name; anything that could leave its folder is refused', () => {
      for (const bad of ['..', '.hidden', 'a/b', 'a\\b', '../x', 'x y', 'x'.repeat(129)]) {
        expect(() => implementation.createStore('/nonexistent', {}, bad)).toThrow(InvalidStoreIdError)
      }
      expect(() => implementation.createStore('/nonexistent', {}, 'A1B2C3D4-0000-1111-2222-333344445555')).not.toThrow()
    })
  })
}
