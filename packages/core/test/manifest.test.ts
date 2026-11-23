import { describe, expect, test } from 'bun:test'
import {
  InvalidWalkManifestError,
  MANIFEST_HASH_VERSION,
  WALK_MANIFEST_VERSION,
  emptyWalkManifest,
  parseWalkManifest,
  serializeWalkManifest,
  type ManifestWalk,
} from '../src/index.ts'

const sha = (letter: string) => letter.repeat(64)

function entry(overrides: Partial<ManifestWalk> = {}): ManifestWalk {
  return {
    goal: 'Today', appId: 'com.example.app', platform: 'ios', adapter: 'maestro-ios/v1',
    flow: '.maestro/flows/today.yaml', closureSha256: sha('a'), assertionsSha256: sha('b'),
    ...overrides,
  }
}

function manifestText(walks: unknown[], extra: Record<string, unknown> = {}) {
  return JSON.stringify({ manifestVersion: 1, hashVersion: 1, walks, ...extra })
}

describe('walk manifest', () => {
  test('round-trips through the fixed serialization, sorted, with no timestamps', () => {
    const walks = [
      entry({ goal: 'Today' }),
      entry({
        goal: 'Smoke', flow: '.maestro/flows/smoke.yaml',
        ownerRules: { minAssertions: 2, requiredAssertionIds: ['screen.today'] },
        profile: { name: 'uiTesting', launchArguments: { uiTesting: true, count: 2, mode: 'fast' }, clearState: true },
      }),
    ]
    const text = serializeWalkManifest({ ...emptyWalkManifest(), walks })
    expect(text.endsWith('}\n')).toBe(true)
    expect(text.indexOf('"Smoke"')).toBeLessThan(text.indexOf('"Today"'))
    expect(text).toContain(`"manifestVersion": ${WALK_MANIFEST_VERSION}`)
    expect(text).toContain(`"hashVersion": ${MANIFEST_HASH_VERSION}`)
    const parsed = parseWalkManifest(text)
    expect(parsed.walks.map(walk => walk.goal)).toEqual(['Smoke', 'Today'])
    expect(serializeWalkManifest(parsed)).toBe(text)
  })

  test('fails closed on an unknown version, unknown field or non-object', () => {
    for (const text of [
      '',
      '[]',
      JSON.stringify({ manifestVersion: 2, hashVersion: 1, walks: [] }),
      JSON.stringify({ hashVersion: 1, walks: [] }),
      JSON.stringify({ manifestVersion: 1, hashVersion: 2, walks: [] }),
      manifestText([], { extra: 1 }),
      manifestText([{ ...entry(), extra: 1 }]),
      manifestText([{ ...entry(), profile: { name: 'x', secret: 'y' } }]),
      manifestText([{ ...entry(), ownerRules: { other: 1 } }]),
    ]) {
      expect(() => parseWalkManifest(text)).toThrow(InvalidWalkManifestError)
    }
  })

  test('refuses a field that appears twice in one object', () => {
    const good = serializeWalkManifest({ ...emptyWalkManifest(), walks: [entry()] })
    const twice = good.replace('"closureSha256"', `"assertionsSha256": "${sha('c')}",\n      "closureSha256"`)
    expect(twice).not.toBe(good)
    expect(() => parseWalkManifest(twice)).toThrow(/appears twice/)
    expect(() => parseWalkManifest('{"manifestVersion":1,"manifestVersion":1,"hashVersion":1,"walks":[]}')).toThrow(/appears twice/)
    // Equal keys in different objects, and braces inside strings, are fine.
    const text = serializeWalkManifest({ ...emptyWalkManifest(), walks: [entry({ goal: 'a {"x": 1, "x": 2} b' }), entry({ goal: 'Other' })] })
    expect(parseWalkManifest(text).walks).toHaveLength(2)
  })

  test('refuses unsafe flow paths', () => {
    for (const flow of [
      '/abs/today.yaml', '../today.yaml', '.maestro/../../today.yaml', '.maestro\\flows\\today.yaml',
      'C:/today.yaml', '.deja/flows/today.yaml', '.DEJA/flows/today.yaml', '.Deja/x.yaml', '.maestro//today.yaml', './today.yaml', 'today.json', '',
    ]) {
      expect(() => parseWalkManifest(manifestText([entry({ flow })]))).toThrow(InvalidWalkManifestError)
    }
  })

  test('refuses machine data: home paths, secret-looking names, bad hashes, duplicates', () => {
    const bad: unknown[] = [
      entry({ goal: '/Users/someone/work' }),
      entry({ appId: '~/app' }),
      entry({ profile: { name: 'C:\\Users\\x' } }),
      entry({ profile: { launchArguments: { apiKey: 'x' } } }),
      entry({ profile: { launchArguments: { path: '/home/someone/a' } } }),
      entry({ profile: { launchArguments: { nested: { a: 1 } as never } } }),
      entry({ closureSha256: 'ABC' }),
      entry({ assertionsSha256: sha('b').slice(1) }),
      entry({ platform: 'tv' as never }),
      entry({ platform: 'web' }),
      entry({ browser: 'chromium' }),
      entry({ goal: '   ' }),
    ]
    for (const walk of bad) {
      expect(() => parseWalkManifest(manifestText([walk]))).toThrow(InvalidWalkManifestError)
    }
    expect(() => parseWalkManifest(manifestText([entry(), entry({ goal: ' TODAY ' })]))).toThrow(/duplicate/)
    // The same goal on another platform or adapter is a different walk.
    expect(parseWalkManifest(manifestText([entry(), entry({ platform: 'android', adapter: 'maestro-android' })])).walks)
      .toHaveLength(2)
    expect(parseWalkManifest(manifestText([
      entry({ platform: 'web', adapter: 'maestro-web@2', browser: 'chromium', appId: 'http://127.0.0.1:4173' }),
    ])).walks[0].browser).toBe('chromium')
  })
})
