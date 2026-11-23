import { describe, expect, test } from 'bun:test'
import { existsSync, readFileSync } from 'node:fs'
import { emitSnapshot, packageName, PUBLIC_PACKAGES, snapshotPath } from './api-snapshot.ts'

describe('public API snapshots', () => {
  for (const directory of PUBLIC_PACKAGES) {
    const name = packageName(directory)
    test(`${name} matches api/${name}.d.ts (run bun scripts/api-snapshot.ts --update to accept a change)`, () => {
      expect(existsSync(snapshotPath(directory))).toBe(true)
      expect(emitSnapshot(directory)).toBe(readFileSync(snapshotPath(directory), 'utf8'))
    }, 60_000)
  }

  test('the snapshot lists only exports, not internal files', () => {
    const maestro = readFileSync(snapshotPath('packages/maestro'), 'utf8')
    expect(maestro).toContain('// exports: ')
    expect(maestro).toMatch(/^\/\/ typescript \d+\.\d+\.\d+/)
  })

  test('core exports createDeja and CONTRACT_VERSION', () => {
    const core = readFileSync(snapshotPath('packages/core'), 'utf8')
    expect(core).toContain('export declare function createDeja(')
    expect(core).toContain('export declare const CONTRACT_VERSION')
  })
})
