import { describe, expect, test } from 'bun:test'
import { existsSync } from 'node:fs'
import { mkdir, mkdtemp, readFile, rename, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import {
  InvalidWalkMemoryError,
  LOCAL_MEMORY_PATH,
  LOCAL_MEMORY_SCHEMA_VERSION,
  LocalWalkStore,
  MAESTRO_FLOW_SCHEMA_VERSION,
  type WalkIdentity,
} from '../src/index.ts'

const initialValidation = '2026-11-22T00:00:00.000Z'
const laterValidation = '2026-11-22T00:01:00.000Z'

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

async function fixture() {
  const temporary = await mkdtemp(join(tmpdir(), 'walkcache-store-'))
  const workspaceRoot = join(temporary, 'workspace')
  await mkdir(join(workspaceRoot, '.walkcache/attempts'), { recursive: true })
  const candidatePath = '.walkcache/attempts/candidate.yaml'
  await writeFile(join(workspaceRoot, candidatePath), 'appId: dev.walkcache.checkout\n- assertVisible: Order confirmed\n')
  return { temporary, workspaceRoot, candidatePath }
}

async function cleanup(temporary: string) {
  await rm(temporary, { recursive: true, force: true })
}

async function seed(workspaceRoot: string, candidatePath: string, overrides: Partial<WalkIdentity> = {}) {
  const store = new LocalWalkStore({ workspaceRoot })
  const persisted = await store.persistVerifiedCandidate({
    identity: identity(overrides),
    candidatePath,
    validation: { passed: true, at: initialValidation },
    origin: 'explore',
  })
  return { store, persisted }
}

async function memory(workspaceRoot: string) {
  return JSON.parse(await readFile(join(workspaceRoot, LOCAL_MEMORY_PATH), 'utf8'))
}

async function writeMemory(workspaceRoot: string, value: unknown) {
  await writeFile(join(workspaceRoot, LOCAL_MEMORY_PATH), `${JSON.stringify(value, null, 2)}\n`)
}

describe('conservative local walk store', () => {
  test('returns a first-run miss when no memory exists', async () => {
    const { temporary, workspaceRoot } = await fixture()
    try {
      const result = await new LocalWalkStore({ workspaceRoot }).lookup(identity())
      expect(result).toEqual({ status: 'miss', reason: 'not-found' })
      expect(existsSync(join(workspaceRoot, LOCAL_MEMORY_PATH))).toBe(false)
    } finally {
      await cleanup(temporary)
    }
  })

  test('normalizes the goal and returns a valid hit with the complete exact identity', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { store, persisted } = await seed(workspaceRoot, candidatePath, { goal: '  Check   Out  ' })
      const result = await store.lookup(identity({ goal: '\tcheck  out\n' }))
      expect(result.status).toBe('hit')
      if (result.status !== 'hit') throw new Error('expected hit')
      expect(result.walk.goal).toBe('Check   Out')
      expect(result.walk.goalKey).toBe('check out')
      expect(result.walk.appId).toBe(identity().appId)
      expect(result.walk.platform).toBe('android')
      expect(result.walk.flowSchemaVersion).toBe(MAESTRO_FLOW_SCHEMA_VERSION)
      expect(result.walk.buildFingerprint).toBe('a'.repeat(64))
      expect(result.walk.flowSha256).toBe(persisted.flowSha256)
      expect(result.flowPath).toBe(resolve(workspaceRoot, persisted.flowPath))
      expect(result.walk.lastValidation.passed).toBe(true)
      expect(result.walk.validationHistory).toHaveLength(1)
      expect(await store.lookup(identity({ goal: 'check out', flowSchemaVersion: 'other/v1' }))).toEqual({
        status: 'miss', reason: 'wrong-flow-schema',
      })
    } finally {
      await cleanup(temporary)
    }
  })

  test('returns a stale-build miss instead of replaying another APK fingerprint', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { store } = await seed(workspaceRoot, candidatePath)
      expect(await store.lookup(identity({ buildFingerprint: 'b'.repeat(64) }))).toEqual({
        status: 'miss', reason: 'stale-build',
      })
    } finally {
      await cleanup(temporary)
    }
  })

  test('returns a wrong-app miss without loosening app identity', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { store } = await seed(workspaceRoot, candidatePath)
      expect(await store.lookup(identity({ appId: 'dev.walkcache.other' }))).toEqual({
        status: 'miss', reason: 'wrong-app',
      })
    } finally {
      await cleanup(temporary)
    }
  })

  test('returns a wrong-platform miss without sharing Android memory across platforms', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { store } = await seed(workspaceRoot, candidatePath)
      expect(await store.lookup(identity({ platform: 'ios' }))).toEqual({
        status: 'miss', reason: 'wrong-platform',
      })
    } finally {
      await cleanup(temporary)
    }
  })

  test('returns a missing-flow miss and never treats metadata alone as a pass', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { store, persisted } = await seed(workspaceRoot, candidatePath)
      await rm(join(workspaceRoot, persisted.flowPath))
      expect(await store.lookup(identity())).toEqual({ status: 'miss', reason: 'missing-flow' })
    } finally {
      await cleanup(temporary)
    }
  })

  test('rejects a flow checksum mismatch instead of executing changed bytes', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { store, persisted } = await seed(workspaceRoot, candidatePath)
      await writeFile(join(workspaceRoot, persisted.flowPath), '# tampered flow\n')
      await expect(store.lookup(identity())).rejects.toThrow('flow checksum does not match')
    } finally {
      await cleanup(temporary)
    }
  })

  test('rejects malformed JSON as invalid memory', async () => {
    const { temporary, workspaceRoot } = await fixture()
    try {
      await mkdir(join(workspaceRoot, '.walkcache/memory'), { recursive: true })
      await writeFile(join(workspaceRoot, LOCAL_MEMORY_PATH), '{not json\n')
      await expect(new LocalWalkStore({ workspaceRoot }).lookup(identity())).rejects.toThrow(
        'walks.json is not valid JSON',
      )
    } finally {
      await cleanup(temporary)
    }
  })

  test('rejects an unsupported memory schema', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      await seed(workspaceRoot, candidatePath)
      const value = await memory(workspaceRoot)
      value.schemaVersion = 999
      await writeMemory(workspaceRoot, value)
      await expect(new LocalWalkStore({ workspaceRoot }).lookup(identity())).rejects.toThrow(
        'unsupported memory schemaVersion: 999',
      )
    } finally {
      await cleanup(temporary)
    }
  })

  test('rejects invalid entries instead of filtering them out to produce a hit', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { store } = await seed(workspaceRoot, candidatePath)
      const value = await memory(workspaceRoot)
      value.walks.push({ goalKey: 'broken', lastValidation: { passed: true } })
      await writeMemory(workspaceRoot, value)
      await expect(store.lookup(identity())).rejects.toThrow('missing or malformed identity fields')
    } finally {
      await cleanup(temporary)
    }
  })

  test('rejects stored flow paths outside the canonical flow directory', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { store } = await seed(workspaceRoot, candidatePath)
      await writeFile(join(workspaceRoot, 'outside.yaml'), '# outside\n')
      const value = await memory(workspaceRoot)
      value.walks[0].flowPath = '../outside.yaml'
      await writeMemory(workspaceRoot, value)
      await expect(store.lookup(identity())).rejects.toThrow('must stay under .walkcache/flows')
    } finally {
      await cleanup(temporary)
    }
  })

  test('rejects a symlink whose canonical flow path escapes the workspace', async () => {
    if (process.platform === 'win32') return
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { store } = await seed(workspaceRoot, candidatePath)
      const outside = join(temporary, 'outside.yaml')
      await writeFile(outside, '# outside\n')
      const linkDirectory = join(workspaceRoot, '.walkcache/flows/escape')
      await mkdir(linkDirectory, { recursive: true })
      await symlink(outside, join(linkDirectory, 'android.yaml'))
      const value = await memory(workspaceRoot)
      value.walks[0].flowPath = '.walkcache/flows/escape/android.yaml'
      await writeMemory(workspaceRoot, value)
      await expect(store.lookup(identity())).rejects.toThrow(InvalidWalkMemoryError)
    } finally {
      await cleanup(temporary)
    }
  })

  test('an interruption before flow rename leaves only ignored temporary files and a first-run miss', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    let temporaryFlow = ''
    let finalFlow = ''
    const store = new LocalWalkStore({
      workspaceRoot,
      renameFile: async (from, to) => {
        temporaryFlow = from
        finalFlow = to
        throw new Error('simulated interruption before rename')
      },
    })
    try {
      await expect(store.persistVerifiedCandidate({
        identity: identity(), candidatePath,
        validation: { passed: true, at: initialValidation }, origin: 'explore',
      })).rejects.toThrow('simulated interruption before rename')
      expect(temporaryFlow.endsWith('.tmp')).toBe(true)
      expect(existsSync(temporaryFlow)).toBe(true)
      expect(existsSync(finalFlow)).toBe(false)
      expect(existsSync(join(workspaceRoot, LOCAL_MEMORY_PATH))).toBe(false)
      expect(await store.lookup(identity())).toEqual({ status: 'miss', reason: 'not-found' })
    } finally {
      await cleanup(temporary)
    }
  })

  test('failed candidate verification preserves the known-good flow and metadata byte for byte', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { persisted } = await seed(workspaceRoot, candidatePath)
      const flowPath = join(workspaceRoot, persisted.flowPath)
      const memoryPath = join(workspaceRoot, LOCAL_MEMORY_PATH)
      const beforeFlow = await readFile(flowPath)
      const beforeMemory = await readFile(memoryPath)
      let renameCalls = 0
      const store = new LocalWalkStore({
        workspaceRoot,
        renameFile: async () => { renameCalls += 1 },
      })
      await expect(store.persistVerifiedCandidate({
        identity: identity({ buildFingerprint: 'b'.repeat(64) }), candidatePath,
        validation: { passed: false, at: laterValidation }, origin: 'explore',
      })).rejects.toThrow('Only an independently passing candidate')
      expect(renameCalls).toBe(0)
      expect(await readFile(flowPath)).toEqual(beforeFlow)
      expect(await readFile(memoryPath)).toEqual(beforeMemory)
      expect((await store.lookup(identity())).status).toBe('hit')
    } finally {
      await cleanup(temporary)
    }
  })

  test('failed replacement preserves known-good flow and metadata and excludes the orphan and temp', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { persisted } = await seed(workspaceRoot, candidatePath)
      const oldFlowPath = join(workspaceRoot, persisted.flowPath)
      const memoryPath = join(workspaceRoot, LOCAL_MEMORY_PATH)
      const beforeFlow = await readFile(oldFlowPath)
      const beforeMemory = await readFile(memoryPath)
      await writeFile(join(workspaceRoot, candidatePath), '# independently verified replacement\n')
      let memoryTemporary = ''
      const store = new LocalWalkStore({
        workspaceRoot,
        renameFile: async (from, to) => {
          if (to === memoryPath) {
            memoryTemporary = from
            throw new Error('simulated metadata replacement failure')
          }
          await rename(from, to)
        },
      })
      await expect(store.persistVerifiedCandidate({
        identity: identity({ buildFingerprint: 'b'.repeat(64) }), candidatePath,
        validation: { passed: true, at: laterValidation }, origin: 'remember',
      })).rejects.toThrow('simulated metadata replacement failure')
      expect(await readFile(oldFlowPath)).toEqual(beforeFlow)
      expect(await readFile(memoryPath)).toEqual(beforeMemory)
      expect(memoryTemporary.endsWith('.tmp')).toBe(true)
      expect(existsSync(memoryTemporary)).toBe(true)
      expect((await store.lookup(identity())).status).toBe('hit')
      expect(await store.lookup(identity({ buildFingerprint: 'b'.repeat(64) }))).toEqual({
        status: 'miss', reason: 'stale-build',
      })
    } finally {
      await cleanup(temporary)
    }
  })

  test('failed validation remains in history and never remains a successful reusable hit', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { store, persisted } = await seed(workspaceRoot, candidatePath)
      const beforeFlow = await readFile(join(workspaceRoot, persisted.flowPath))
      const updated = await store.recordValidation(identity(), { passed: false, at: laterValidation })
      expect(updated.lastValidation).toEqual({ passed: false, at: laterValidation })
      expect(updated.validationHistory).toHaveLength(2)
      expect(updated.validationHistory[0].passed).toBe(true)
      expect(updated.validationHistory[1]).toEqual({
        passed: false, at: laterValidation, flowSha256: persisted.flowSha256,
      })
      expect(await store.lookup(identity())).toEqual({ status: 'miss', reason: 'failed-validation' })
      expect(await readFile(join(workspaceRoot, persisted.flowPath))).toEqual(beforeFlow)
      const stored = await memory(workspaceRoot)
      expect(stored.walks[0].lastValidation.passed).toBe(false)
      expect(stored.walks[0].validationHistory.map((entry: { passed: boolean }) => entry.passed)).toEqual([true, false])
    } finally {
      await cleanup(temporary)
    }
  })

  test('upgrades valid WALK-003 schema-one metadata without inventing validation success', async () => {
    const { temporary, workspaceRoot, candidatePath } = await fixture()
    try {
      const { store, persisted } = await seed(workspaceRoot, candidatePath)
      const oldPath = '.walkcache/flows/checkout/android.yaml'
      await rename(join(workspaceRoot, persisted.flowPath), join(workspaceRoot, oldPath))
      const value = await memory(workspaceRoot)
      value.schemaVersion = 1
      value.walks[0].flowPath = oldPath
      delete value.walks[0].flowSchemaVersion
      delete value.walks[0].validationHistory
      await writeMemory(workspaceRoot, value)
      expect((await store.lookup(identity())).status).toBe('hit')
      await store.recordValidation(identity(), { passed: false, at: laterValidation })
      const upgraded = await memory(workspaceRoot)
      expect(upgraded.schemaVersion).toBe(LOCAL_MEMORY_SCHEMA_VERSION)
      expect(upgraded.walks[0].validationHistory.map((entry: { passed: boolean }) => entry.passed)).toEqual([true, false])
      expect(await store.lookup(identity())).toEqual({ status: 'miss', reason: 'failed-validation' })
    } finally {
      await cleanup(temporary)
    }
  })
})
