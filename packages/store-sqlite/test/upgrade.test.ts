import { Database } from 'bun:sqlite'
import { afterEach, beforeEach, describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { SQLITE_MEMORY_PATH, SQLiteWalkStore } from '../src/index.ts'

let root = ''

beforeEach(async () => {
  root = await realpath(await mkdtemp(join(tmpdir(), 'deja-sqlite-upgrade-')))
  await mkdir(join(root, '.maestro/flows'), { recursive: true })
  await writeFile(join(root, '.maestro/flows/today.yaml'), 'appId: a\n---\n- assertVisible: "x"\n')
})

afterEach(async () => {
  await rm(root, { recursive: true, force: true })
})

const identity = {
  goal: 'today', appId: 'a', platform: 'ios', flowSchemaVersion: 'maestro/v1',
  buildFingerprint: 'a'.repeat(64), adapter: 'maestro-ios/v1',
}

describe('SQLite memory upgrade, schema 3 to 4', () => {
  test('a schema 3 database without closure_sha256 gains it, keeps its walks, and becomes schema 4 on the first manifest write', async () => {
    const old = new SQLiteWalkStore({ workspaceRoot: root })
    await old.persistVerifiedCandidate({
      identity: { ...identity, goal: 'legacy' }, candidatePath: '.maestro/flows/today.yaml',
      validation: { passed: true, at: '2026-11-22T00:00:00.000Z' }, origin: 'remember',
    })
    old.close()
    const path = join(root, SQLITE_MEMORY_PATH)
    // Make it look like a database written before this column existed.
    const raw = new Database(path)
    raw.exec('ALTER TABLE walks DROP COLUMN closure_sha256')
    expect((raw.query('PRAGMA user_version').get() as { user_version: number }).user_version).toBe(3)
    expect((raw.query('PRAGMA table_info(walks)').all() as { name: string }[]).some(column => column.name === 'closure_sha256')).toBe(false)
    raw.close()

    const store = new SQLiteWalkStore({ workspaceRoot: root, manifest: { path: 'deja.walks.json' } })
    try {
      // Reading a schema 3 file needs no upgrade, and the old walk is not read through.
      expect(await store.lookup({ ...identity, goal: 'legacy' })).toEqual({ status: 'miss', reason: 'needs-migration' })
      await store.persistVerifiedCandidate({
        identity, candidatePath: '.maestro/flows/today.yaml',
        validation: { passed: true, at: '2026-11-22T00:01:00.000Z' }, origin: 'remember',
      })
      expect((await store.lookup(identity)).status).toBe('hit')
      const walks = await store.list()
      expect(walks.map(walk => walk.goalKey).sort()).toEqual(['legacy', 'today'])
      expect(walks.find(walk => walk.goalKey === 'legacy')?.subflowFiles).toEqual([])
      expect(walks.find(walk => walk.goalKey === 'today')?.closureSha256).toMatch(/^[a-f0-9]{64}$/)
    } finally {
      store.close()
    }
    const after = new Database(path, { readonly: true })
    expect((after.query('PRAGMA user_version').get() as { user_version: number }).user_version).toBe(4)
    expect((after.query('PRAGMA table_info(walks)').all() as { name: string }[]).some(column => column.name === 'closure_sha256')).toBe(true)
    after.close()
  })
})
