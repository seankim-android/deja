import { describe, expect, test } from 'bun:test'
import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { MAESTRO_FLOW_SCHEMA_VERSION, LocalWalkStore } from '../src/index.ts'

function identity() {
  return {
    goal: 'checkout',
    appId: 'http://127.0.0.1:4174',
    platform: 'web',
    flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION,
    buildFingerprint: 'b'.repeat(64),
    adapter: 'maestro-web@2.12.0',
    browser: 'chromium',
  }
}

describe('Web walk identity persistence', () => {
  test('persists browser/adapter identity and partitions exact lookup by both fields', async () => {
    const root = await mkdtemp(join(tmpdir(), 'walkcache-web-store-'))
    try {
      await writeFile(join(root, 'candidate.yaml'), 'url: http://127.0.0.1:4174\n---\n- assertVisible: Ready\n')
      const store = new LocalWalkStore({ workspaceRoot: root })
      const current = identity()
      await store.persistVerifiedCandidate({
        identity: current,
        candidatePath: 'candidate.yaml',
        validation: { passed: true, at: new Date().toISOString() },
        origin: 'remember',
      })
      const hit = await store.lookup(current)
      expect(hit.status).toBe('hit')
      if (hit.status === 'hit') {
        expect(hit.walk).toMatchObject({
          platform: 'web', browser: 'chromium', adapter: 'maestro-web@2.12.0',
          buildFingerprint: current.buildFingerprint,
        })
      }
      expect(await store.lookup({ ...current, adapter: 'maestro-web-other' })).toEqual({
        status: 'miss', reason: 'wrong-adapter',
      })
      expect(await store.lookup({ ...current, browser: 'firefox' })).toEqual({
        status: 'miss', reason: 'wrong-browser',
      })
      expect(await store.lookup({ ...current, buildFingerprint: 'c'.repeat(64) })).toEqual({
        status: 'miss', reason: 'stale-build',
      })
      expect((await store.list())[0].validationHistory).toHaveLength(1)
    } finally {
      await rm(root, { recursive: true, force: true })
    }
  })
})
