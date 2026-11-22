import { expect, test } from 'bun:test'
import { mkdir, mkdtemp, readFile, rename, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  LOCAL_MEMORY_PATH,
  LOCAL_MEMORY_SCHEMA_VERSION,
  LocalWalkStore,
  MAESTRO_FLOW_SCHEMA_VERSION,
  type WalkIdentity,
} from '../src/index.ts'

function identity(): WalkIdentity {
  return {
    goal: 'Checkout',
    appId: 'dev.walkcache.checkout',
    platform: 'android',
    flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION,
    buildFingerprint: 'a'.repeat(64),
  }
}

test('upgrades valid WALK-003 schema-one metadata without inventing validation success', async () => {
  const temporary = await mkdtemp(join(tmpdir(), 'walkcache-local-legacy-'))
  const workspaceRoot = join(temporary, 'workspace')
  try {
    await mkdir(join(workspaceRoot, '.walkcache/attempts'), { recursive: true })
    const candidatePath = '.walkcache/attempts/candidate.yaml'
    await writeFile(join(workspaceRoot, candidatePath), 'appId: dev.walkcache.checkout\n')
    const store = new LocalWalkStore({ workspaceRoot })
    const persisted = await store.persistVerifiedCandidate({
      identity: identity(), candidatePath,
      validation: { passed: true, at: '2026-11-22T00:00:00.000Z' }, origin: 'explore',
    })
    const oldPath = '.walkcache/flows/checkout/android.yaml'
    await rename(join(workspaceRoot, persisted.flowPath), join(workspaceRoot, oldPath))
    const memory = JSON.parse(await readFile(join(workspaceRoot, LOCAL_MEMORY_PATH), 'utf8'))
    memory.schemaVersion = 1
    memory.walks[0].flowPath = oldPath
    delete memory.walks[0].flowSchemaVersion
    delete memory.walks[0].validationHistory
    await writeFile(join(workspaceRoot, LOCAL_MEMORY_PATH), `${JSON.stringify(memory, null, 2)}\n`)
    expect((await store.lookup(identity())).status).toBe('hit')
    await store.recordValidation(identity(), {
      passed: false, at: '2026-11-22T00:01:00.000Z',
    })
    const upgraded = JSON.parse(await readFile(join(workspaceRoot, LOCAL_MEMORY_PATH), 'utf8'))
    expect(upgraded.schemaVersion).toBe(LOCAL_MEMORY_SCHEMA_VERSION)
    expect(upgraded.walks[0].validationHistory.map(
      (entry: { passed: boolean }) => entry.passed,
    )).toEqual([true, false])
    expect(await store.lookup(identity())).toEqual({ status: 'miss', reason: 'failed-validation' })
  } finally {
    await rm(temporary, { recursive: true, force: true })
  }
})
