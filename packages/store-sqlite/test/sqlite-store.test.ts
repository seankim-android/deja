import { expect, test } from 'bun:test'
import { Database } from 'bun:sqlite'
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  MAESTRO_FLOW_SCHEMA_VERSION,
  SQLITE_MEMORY_PATH,
  SQLiteWalkStore,
  type WalkIdentity,
} from '../src/index.ts'

function identity(overrides: Partial<WalkIdentity> = {}): WalkIdentity {
  return {
    goal: 'Checkout',
    appId: 'dev.walkcache.checkout',
    platform: 'android',
    flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION,
    buildFingerprint: 'a'.repeat(64),
    ...overrides,
  }
}

test('a real SQLite error rolls back the entire metadata replacement and retains the known-good walk', async () => {
  const temporary = await mkdtemp(join(tmpdir(), 'walkcache-sqlite-rollback-'))
  const workspaceRoot = join(temporary, 'workspace')
  const store = new SQLiteWalkStore({ workspaceRoot })
  try {
    await mkdir(join(workspaceRoot, 'flows'), { recursive: true })
    const candidatePath = 'flows/candidate.yaml'
    await writeFile(join(workspaceRoot, candidatePath), '# original verified flow\n')
    const previous = await store.persistVerifiedCandidate({
      identity: identity(), candidatePath,
      validation: { passed: true, at: '2026-11-22T00:00:00.000Z' }, origin: 'remember',
    })
    const triggerDatabase = new Database(join(workspaceRoot, SQLITE_MEMORY_PATH))
    triggerDatabase.exec(`
      CREATE TRIGGER reject_replacement
      BEFORE INSERT ON walks
      WHEN NEW.goal_key = 'second goal'
      BEGIN
        SELECT RAISE(ABORT, 'simulated mid-transaction rejection');
      END;
    `)
    triggerDatabase.close()

    await writeFile(join(workspaceRoot, candidatePath), '# independently verified second flow\n')
    await expect(store.persistVerifiedCandidate({
      identity: identity({
        goal: 'Second goal',
        buildFingerprint: 'b'.repeat(64),
      }),
      candidatePath,
      validation: { passed: true, at: '2026-11-22T00:01:00.000Z' },
      origin: 'remember',
    })).rejects.toThrow('transaction rolled back')

    expect((await store.lookup(identity())).status).toBe('hit')
    expect(await store.lookup(identity({ goal: 'Second goal' }))).toEqual({
      status: 'miss', reason: 'not-found',
    })
    expect(await store.list()).toHaveLength(1)
    expect((await store.list())[0].flowSha256).toBe(previous.flowSha256)
  } finally {
    store.close()
    await rm(temporary, { recursive: true, force: true })
  }
})
