import { describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, realpath, readFile, rename, rm, writeFile } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { LocalVerifiedSubflowStore } from '../src/verified-subflow-store.ts'
import { InvalidSubflowMemoryError } from '../src/verified-subflow-store.ts'
import type { SubflowContract } from '@deja-qa/core'

const at = '2026-11-22T00:00:00.000Z'

function contract(): SubflowContract {
  return {
    schemaVersion: 1, segmentId: 'login-v1',
    identity: {
      appId: 'fixture.app', platform: 'android', flowSchemaVersion: 'maestro/v1',
      buildFingerprint: 'a'.repeat(64),
    },
    inputs: [{ name: 'password', kind: 'credential', required: true }],
    outputs: [{ name: 'catalog', kind: 'screen', resourceId: 'screen_catalog' }],
    preconditions: [{ kind: 'clean-app-launch' }],
    postconditions: [{ kind: 'screen-visible', resourceId: 'screen_catalog' }],
    stepCount: 1, assertionCount: 1,
    provenance: {
      kind: 'deterministic-fixture', fixtureId: 'store-fixture', newDeviceRunRecorded: false,
    },
  }
}

async function fixture() {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'walk018-segment-store-')))
  const workspaceRoot = join(temporary, 'workspace')
  await mkdir(join(workspaceRoot, '.deja/attempts'), { recursive: true })
  const candidatePath = '.deja/attempts/login.yaml'
  const bytes = 'appId: fixture.app\n---\n- assertVisible:\n    id: screen_catalog\n'
  await writeFile(resolve(workspaceRoot, candidatePath), bytes)
  return { temporary, workspaceRoot, candidatePath, bytes }
}

async function save(store: LocalVerifiedSubflowStore, candidatePath: string) {
  return store.persistVerified({
    contract: contract(), candidatePath, validation: { passed: true, at },
  })
}

describe('local verified reusable segment store', () => {
  test('persists actual YAML only with passing validation and reopens an exact hit', async () => {
    const ctx = await fixture()
    try {
      const store = new LocalVerifiedSubflowStore({ workspaceRoot: ctx.workspaceRoot })
      const saved = await save(store, ctx.candidatePath)
      expect(saved.lastValidation).toEqual({ passed: true, at })
      expect(saved.flowSha256).toMatch(/^[a-f0-9]{64}$/)
      expect(await readFile(resolve(ctx.workspaceRoot, saved.flowPath), 'utf8')).toBe(ctx.bytes)
      const reopened = new LocalVerifiedSubflowStore({ workspaceRoot: ctx.workspaceRoot })
      const hit = await reopened.lookup('login-v1', contract().identity)
      expect(hit.status).toBe('hit')
      if (hit.status === 'hit') {
        expect(hit.segment).toEqual(saved)
        expect(hit.flowPath).toBe(resolve(ctx.workspaceRoot, saved.flowPath))
      }
    } finally {
      await rm(ctx.temporary, { recursive: true, force: true })
    }
  })

  test('rejects failed validation before creating segment memory', async () => {
    const ctx = await fixture()
    try {
      const store = new LocalVerifiedSubflowStore({ workspaceRoot: ctx.workspaceRoot })
      await expect(store.persistVerified({
        contract: contract(), candidatePath: ctx.candidatePath, validation: { passed: false, at },
      })).rejects.toThrow('independently passing validation')
      expect(await store.list()).toEqual([])
      expect(existsSync(store.indexPath)).toBe(false)
    } finally {
      await rm(ctx.temporary, { recursive: true, force: true })
    }
  })

  test('partitions lookup by exact app, build, platform, and schema identity', async () => {
    const ctx = await fixture()
    try {
      const store = new LocalVerifiedSubflowStore({ workspaceRoot: ctx.workspaceRoot })
      await save(store, ctx.candidatePath)
      for (const [identity, reason] of [
        [{ ...contract().identity, appId: 'other.app' }, 'wrong-app'],
        [{ ...contract().identity, platform: 'ios' }, 'wrong-platform'],
        [{ ...contract().identity, flowSchemaVersion: 'maestro/v2' }, 'wrong-flow-schema'],
        [{ ...contract().identity, buildFingerprint: 'b'.repeat(64) }, 'stale-build'],
      ] as const) {
        expect(await store.lookup('login-v1', identity)).toEqual({ status: 'miss', reason })
      }
      expect(await store.lookup('unknown', contract().identity))
        .toEqual({ status: 'miss', reason: 'not-found' })
    } finally {
      await rm(ctx.temporary, { recursive: true, force: true })
    }
  })

  test('reports missing YAML and rejects checksum tampering', async () => {
    const ctx = await fixture()
    try {
      const store = new LocalVerifiedSubflowStore({ workspaceRoot: ctx.workspaceRoot })
      const saved = await save(store, ctx.candidatePath)
      await rm(resolve(ctx.workspaceRoot, saved.flowPath))
      expect(await store.lookup('login-v1', contract().identity))
        .toEqual({ status: 'miss', reason: 'missing-flow' })
      await save(store, ctx.candidatePath)
      await writeFile(resolve(ctx.workspaceRoot, saved.flowPath), '# changed bytes\n')
      await expect(store.lookup('login-v1', contract().identity))
        .rejects.toThrow('checksum does not match')
    } finally {
      await rm(ctx.temporary, { recursive: true, force: true })
    }
  })

  test('fails closed on malformed or duplicate segment metadata', async () => {
    const ctx = await fixture()
    try {
      const store = new LocalVerifiedSubflowStore({ workspaceRoot: ctx.workspaceRoot })
      const saved = await save(store, ctx.candidatePath)
      const original = await readFile(store.indexPath, 'utf8')
      await writeFile(store.indexPath, '{malformed')
      await expect(store.lookup('login-v1', contract().identity)).rejects.toThrow(InvalidSubflowMemoryError)
      const parsed = JSON.parse(original)
      parsed.segments.push(saved)
      await writeFile(store.indexPath, JSON.stringify(parsed))
      await expect(store.list()).rejects.toThrow('duplicate segment IDs')
    } finally {
      await rm(ctx.temporary, { recursive: true, force: true })
    }
  })

  test('metadata replacement failure retains the previous segment and index byte-for-byte', async () => {
    const ctx = await fixture()
    try {
      const originalStore = new LocalVerifiedSubflowStore({ workspaceRoot: ctx.workspaceRoot })
      const previous = await save(originalStore, ctx.candidatePath)
      const indexBefore = await readFile(originalStore.indexPath)
      const flowBefore = await readFile(resolve(ctx.workspaceRoot, previous.flowPath))
      const replacementPath = '.deja/attempts/replacement.yaml'
      await writeFile(resolve(ctx.workspaceRoot, replacementPath),
        '# replacement candidate\nappId: fixture.app\n---\n- assertVisible:\n    id: screen_catalog\n')
      const failingStore = new LocalVerifiedSubflowStore({
        workspaceRoot: ctx.workspaceRoot,
        renameFile: async (temporary, destination) => {
          if (destination === originalStore.indexPath) throw new Error('injected index failure')
          await rename(temporary, destination)
        },
      })
      await expect(save(failingStore, replacementPath)).rejects.toThrow('injected index failure')
      expect(await readFile(originalStore.indexPath)).toEqual(indexBefore)
      expect(await readFile(resolve(ctx.workspaceRoot, previous.flowPath))).toEqual(flowBefore)
      const hit = await originalStore.lookup('login-v1', contract().identity)
      expect(hit.status).toBe('hit')
      if (hit.status === 'hit') expect(hit.segment.flowSha256).toBe(previous.flowSha256)
    } finally {
      await rm(ctx.temporary, { recursive: true, force: true })
    }
  })

  test('flow write failure creates no successful index entry', async () => {
    const ctx = await fixture()
    try {
      const store = new LocalVerifiedSubflowStore({
        workspaceRoot: ctx.workspaceRoot,
        renameFile: async (_temporary, destination) => {
          if (destination.endsWith('.yaml')) throw new Error('injected flow failure')
          await rename(_temporary, destination)
        },
      })
      await expect(save(store, ctx.candidatePath)).rejects.toThrow('injected flow failure')
      expect(existsSync(store.indexPath)).toBe(false)
      expect(await store.list()).toEqual([])
    } finally {
      await rm(ctx.temporary, { recursive: true, force: true })
    }
  })
})
