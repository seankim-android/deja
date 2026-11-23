import { describe, expect, test } from 'bun:test'
import { existsSync, readFileSync } from 'node:fs'
import { emitSnapshot, PUBLIC_PACKAGES, snapshotPath } from './api-snapshot.ts'

describe('public API snapshots', () => {
  for (const name of PUBLIC_PACKAGES) {
    test(`${name} matches api/${name}.d.ts (run bun scripts/api-snapshot.ts --update to accept a change)`, () => {
      expect(existsSync(snapshotPath(name))).toBe(true)
      expect(emitSnapshot(name)).toBe(readFileSync(snapshotPath(name), 'utf8'))
    }, 60_000)
  }

  test('core exports createDeja and CONTRACT_VERSION', () => {
    const core = readFileSync(snapshotPath('core'), 'utf8')
    expect(core).toContain('export declare function createDeja(')
    expect(core).toContain('export declare const CONTRACT_VERSION')
  })
})
