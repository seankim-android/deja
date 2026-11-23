import { describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  androidMaestroAdapter,
  iosMaestroAdapter,
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

  test('iOS identity and command include adapter, simulator, build, and explicit platform', () => {
    const udid = 'DFC4D233-498B-4B33-B713-E59B239FA86A'
    const adapter = iosMaestroAdapter({ device: udid, adapterId: 'maestro-ios@2.12.0' })
    expect(adapter.cacheIdentity({ buildFingerprint: fingerprint })).toEqual({
      buildFingerprint: fingerprint,
      adapter: 'maestro-ios@2.12.0',
    })
    expect(adapter.commandArguments('/workspace/flow.yaml')).toEqual([
      '--no-ansi', '--platform', 'ios', '--device', udid, 'test', '/workspace/flow.yaml',
    ])
    expect(() => iosMaestroAdapter({ device: ' ' })).toThrow('simulator UDID')
  })

  test('iOS app fingerprint covers bundle paths and contents', async () => {
    const temporary = await mkdtemp(join(tmpdir(), 'deja-ios-build-'))
    try {
      const app = join(temporary, 'Fixture.app')
      await mkdir(join(app, '_CodeSignature'), { recursive: true })
      await writeFile(join(app, 'Info.plist'), '<plist>one</plist>')
      await writeFile(join(app, 'Fixture'), 'binary-one')
      await writeFile(join(app, '_CodeSignature/CodeResources'), 'signature-one')
      const adapter = iosMaestroAdapter({ device: 'test-simulator' })
      const before = await adapter.buildFingerprint(temporary, 'Fixture.app')
      expect(before).toMatch(/^[a-f0-9]{64}$/)
      expect(await adapter.buildFingerprint(temporary, 'Fixture.app')).toBe(before)
      await writeFile(join(app, 'Fixture'), 'binary-two')
      expect(await adapter.buildFingerprint(temporary, 'Fixture.app')).not.toBe(before)
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('Web build fingerprint covers paths and every deterministic static file', async () => {
    const temporary = await mkdtemp(join(tmpdir(), 'deja-web-build-'))
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

  test('JUnit options add Maestro\'s own report flags before the flow, and only when asked', () => {
    const options = { junitPath: '/host/report.xml' }
    const report = ['--format=JUNIT', '--output=/host/report.xml']
    expect(androidMaestroAdapter({ device: 'emulator-5554' }).commandArguments('/w/flow.yaml', options))
      .toEqual(['--no-ansi', '--device', 'emulator-5554', 'test', ...report, '/w/flow.yaml'])
    expect(iosMaestroAdapter({ device: 'UDID-1' }).commandArguments('/w/flow.yaml', options))
      .toEqual(['--no-ansi', '--platform', 'ios', '--device', 'UDID-1', 'test', ...report, '/w/flow.yaml'])
    const web = webMaestroAdapter().commandArguments('/w/flow.yaml', options)
    expect(web.slice(-3)).toEqual([...report.slice(0, 2), '/w/flow.yaml'].slice(0, 3))
    expect(web).toContain('--screen-size')
    expect(androidMaestroAdapter({ device: 'emulator-5554' }).commandArguments('/w/flow.yaml', {}))
      .toEqual(['--no-ansi', '--device', 'emulator-5554', 'test', '/w/flow.yaml'])
  })
})
