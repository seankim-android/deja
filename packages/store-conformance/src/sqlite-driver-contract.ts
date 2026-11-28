import { mkdir, mkdtemp, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { openSqlite, SQLITE_MEMORY_PATH, SQLiteWalkStore } from '@deja-run/store-sqlite'
import { resolveHarness, type ContractHarness } from './harness.ts'

const identity = {
  goal: 'today', appId: 'a', platform: 'ios' as const, flowSchemaVersion: 'maestro/v1',
  buildFingerprint: 'a'.repeat(64), adapter: 'maestro-ios/v1',
}

/**
 * The `store-sqlite` driver seam must behave the same on `bun:sqlite` and `node:sqlite`: statement
 * results, transactions, integers, blobs, and the schema 3 to 4 upgrade of a database on disk.
 */
export function runSqliteDriverContractTests(harness?: ContractHarness) {
  const { describe, test, expect } = resolveHarness(harness)

  async function temporary() {
    const root = await realpath(await mkdtemp(join(tmpdir(), 'deja-sqlite-driver-')))
    return { root, cleanup: () => rm(root, { recursive: true, force: true }) }
  }

  describe('store-sqlite driver seam', () => {
    test('run, get and all return the same shapes: plain rows, null for no row, numbers, text, blobs and NULL', async () => {
      const { root, cleanup } = await temporary()
      const database = openSqlite(join(root, 'shapes.sqlite'))
      try {
        database.exec('CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT, b BLOB, z TEXT)')
        database.query('INSERT INTO t (n, s, b, z) VALUES (?, ?, ?, ?)').run(2 ** 40, 'héllo', new Uint8Array([0, 1, 255]), null)
        expect(database.query('SELECT * FROM t WHERE id = ?').get(999)).toBeNull()
        const row = database.query('SELECT n, s, b, z FROM t').get()!
        expect(Object.getPrototypeOf(row)).toBe(Object.prototype)
        expect(row.n).toBe(2 ** 40)
        expect(typeof row.n).toBe('number')
        expect(row.s).toBe('héllo')
        expect(row.z).toBeNull()
        expect(row.b).toBeInstanceOf(Uint8Array)
        expect([...row.b as Uint8Array]).toEqual([0, 1, 255])
        const inserted = database.query('INSERT INTO t (n) VALUES (?) RETURNING id').get(7)!
        expect(inserted.id).toBe(2)
        const rows = database.query('SELECT id, n FROM t ORDER BY id').all()
        expect(rows).toEqual([{ id: 1, n: 2 ** 40 }, { id: 2, n: 7 }])
        expect(Object.getPrototypeOf(rows[0])).toBe(Object.prototype)
        expect(database.query('SELECT id FROM t WHERE id > ?').all(99)).toEqual([])
        expect(database.query('PRAGMA user_version').get()).toEqual({ user_version: 0 })
      } finally {
        database.close()
        await cleanup()
      }
    })

    test('a transaction commits or rolls back as a whole', async () => {
      const { root, cleanup } = await temporary()
      const path = join(root, 'transaction.sqlite')
      const database = openSqlite(path)
      try {
        database.exec('CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)')
        database.exec('BEGIN IMMEDIATE')
        database.query('INSERT INTO t (v) VALUES (?)').run('kept')
        database.exec('COMMIT')
        database.exec('BEGIN IMMEDIATE')
        database.query('INSERT INTO t (v) VALUES (?)').run('discarded')
        database.exec('ROLLBACK')
        expect(database.query('SELECT v FROM t ORDER BY id').all()).toEqual([{ v: 'kept' }])
        expect(() => database.query('INSERT INTO t (v) VALUES (?)').run(null)).toThrow()
      } finally {
        database.close()
      }
      const reopened = openSqlite(path)
      try {
        expect(reopened.query('SELECT v FROM t').all()).toEqual([{ v: 'kept' }])
      } finally {
        reopened.close()
        await cleanup()
      }
    })

    test('a read-only open never creates a file and refuses writes', async () => {
      const { root, cleanup } = await temporary()
      const path = join(root, 'readonly.sqlite')
      try {
        expect(() => openSqlite(path, { readonly: true })).toThrow()
        const writable = openSqlite(path)
        writable.exec('CREATE TABLE t (v TEXT)')
        writable.close()
        const database = openSqlite(path, { readonly: true })
        try {
          expect(database.query('SELECT COUNT(*) AS c FROM t').get()).toEqual({ c: 0 })
          expect(() => database.exec('INSERT INTO t (v) VALUES (\'x\')')).toThrow()
        } finally {
          database.close()
        }
      } finally {
        await cleanup()
      }
    })

    test('a schema 3 database gains closure_sha256, keeps its walks, and becomes schema 4 on the first manifest write', async () => {
      const { root, cleanup } = await temporary()
      await mkdir(join(root, '.maestro/flows'), { recursive: true })
      await writeFile(join(root, '.maestro/flows/today.yaml'), 'appId: a\n---\n- assertVisible: "x"\n')
      const path = join(root, SQLITE_MEMORY_PATH)
      try {
        const old = new SQLiteWalkStore({ workspaceRoot: root })
        await old.persistVerifiedCandidate({
          identity: { ...identity, goal: 'legacy' }, candidatePath: '.maestro/flows/today.yaml',
          validation: { passed: true, at: '2026-11-22T00:00:00.000Z' }, origin: 'remember',
        })
        old.close()
        // Make it look like a database written before this column existed.
        const raw = openSqlite(path)
        raw.exec('ALTER TABLE walks DROP COLUMN closure_sha256')
        expect(raw.query('PRAGMA user_version').get()).toEqual({ user_version: 3 })
        expect(raw.query('PRAGMA table_info(walks)').all().some(column => column.name === 'closure_sha256')).toBe(false)
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
        const after = openSqlite(path, { readonly: true })
        expect(after.query('PRAGMA user_version').get()).toEqual({ user_version: 4 })
        expect(after.query('PRAGMA table_info(walks)').all().some(column => column.name === 'closure_sha256')).toBe(true)
        after.close()
      } finally {
        await cleanup()
      }
    })
  })
}
