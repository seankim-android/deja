import { describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  androidMaestroAdapter,
  webMaestroAdapter,
} from '../src/index.ts'

const fingerprint = 'a'.repeat(64)

describe('Maestro execution platform adapters', () => {
  test('Web identity and command include Chromium, adapter, build, and bounded browser settings', () => {
    const adapter = webMaestroAdapter({ adapterId: 'maestro-web@2.12.0' })
    expect(adapter.cacheIdentity({ buildFingerprint: fingerprint })).toEqual({
      buildFingerprint: fingerprint,
      adapter: 'maestro-web@2.12.0',
      browser: 'chromium',
    })
    expect(adapter.commandArguments('/workspace/flow.yaml')).toEqual([
      '--no-ansi', '--platform', 'web', '--device', 'chromium', 'test', '--headless',
      '--screen-size', '1280x800', '/workspace/flow.yaml',
    ])
    expect(() => webMaestroAdapter({ device: 'firefox' })).toThrow('chromium')
    expect(() => webMaestroAdapter({ screenSize: 'huge' })).toThrow('WIDTHxHEIGHT')
  })

  test('Android runner and cache identity remain compatible with validated pre-Web walks', () => {
    const adapter = androidMaestroAdapter({ device: 'emulator-5554' })
    expect(adapter.cacheIdentity({ buildFingerprint: fingerprint })).toEqual({
      buildFingerprint: fingerprint,
    })
    expect(adapter.commandArguments('/workspace/flow.yaml')).toEqual([
      '--no-ansi', '--device', 'emulator-5554', 'test', '/workspace/flow.yaml',
    ])
  })

  test('Web build fingerprint covers paths and every deterministic static file', async () => {
    const temporary = await mkdtemp(join(tmpdir(), 'walkcache-web-build-'))
    try {
      await mkdir(join(temporary, 'public', 'assets'), { recursive: true })
      await writeFile(join(temporary, 'public', 'index.html'), '<main>one</main>')
      await writeFile(join(temporary, 'public', 'assets', 'app.js'), 'const value = 1')
      const adapter = webMaestroAdapter()
      const before = await adapter.buildFingerprint(temporary, 'public')
      expect(before).toMatch(/^[a-f0-9]{64}$/)
      expect(await adapter.buildFingerprint(temporary, 'public')).toBe(before)
      await writeFile(join(temporary, 'public', 'assets', 'app.js'), 'const value = 2')
      expect(await adapter.buildFingerprint(temporary, 'public')).not.toBe(before)
      const afterScript = await adapter.buildFingerprint(temporary, 'public')
      await writeFile(join(temporary, 'public', 'styles.css'), 'main { color: green }')
      expect(await adapter.buildFingerprint(temporary, 'public')).not.toBe(afterScript)
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })
})
