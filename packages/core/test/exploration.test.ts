import { describe, expect, test } from 'bun:test'
import {
  findChromeIds,
  fingerprintScreenV2,
  isDynamicId,
  matchScreenV2,
  normalizeElements,
  normalizedSnapshot,
  observationReport,
  type ElementObservation,
  type FingerprintRules,
  type RawElement,
  type StoredScreen,
} from '../src/index.ts'

const rules: FingerprintRules = {
  normalizeId: raw => raw.startsWith('com.app:id/') ? raw.slice('com.app:id/'.length) : raw || null,
  isOverlay: element => element.rawId === 'keyboard.key' || element.rawId === 'toast',
  isSystemUi: element => element.rawId === 'ActivityListView',
}

const raw = (rawId: string | null, label: string | null = null): RawElement => ({
  rawId, label, bounds: null, enabled: true, focused: false, selected: false, checked: false,
  role: 'other', interactive: false,
})

const el = (id: string | null, overlay = false): ElementObservation => ({
  id, role: 'other', enabled: true, interactive: false, label: null, overlay,
})

const config = { screenMarkerPattern: '^screen\\.' }

describe('normalizeElements', () => {
  test('normalizes IDs, flags overlays and system UI, and drops elements with no ID and no label', () => {
    const { elements, truncated } = normalizeElements([
      raw('com.app:id/screen.today'), raw('keyboard.key'), raw('ActivityListView'), raw(null), raw(null, 'Just text'), raw('', null),
    ], rules)
    expect(truncated).toBe(false)
    expect(elements.map(e => [e.id, e.overlay])).toEqual([
      ['screen.today', false], ['keyboard.key', true], ['ActivityListView', true], [null, false],
    ])
  })

  test('bounds the list and sets truncated', () => {
    const many = Array.from({ length: 450 }, (_, index) => raw(`row.x${index}`))
    const { elements, truncated } = normalizeElements(many, rules)
    expect(elements).toHaveLength(400)
    expect(truncated).toBe(true)
  })
})

describe('fingerprint v2', () => {
  test('is the same for any element order and never reads labels', () => {
    const a = fingerprintScreenV2([el('screen.today'), el('today.save'), el('today.answer')], config)
    const b = fingerprintScreenV2([el('today.answer'), el('screen.today'), el('today.save')], config)
    expect(a.fingerprint).toBe(b.fingerprint)
    const labelled = [{ ...el('today.save'), label: 'Save' }, el('screen.today')]
    const relabelled = [{ ...el('today.save'), label: 'Guardar' }, el('screen.today')]
    expect(fingerprintScreenV2(labelled, config).fingerprint)
      .toBe(fingerprintScreenV2(relabelled, config).fingerprint)
  })

  test('drops overlays and system UI', () => {
    const plain = fingerprintScreenV2([el('screen.today'), el('today.answer')], config)
    const withOverlays = fingerprintScreenV2([
      el('screen.today'), el('today.answer'), el('space', true), el('ActivityListView', true),
    ], config)
    expect(withOverlays.fingerprint).toBe(plain.fingerprint)
  })

  test('drops config.ignore IDs, exact and prefix', () => {
    const base = fingerprintScreenV2([el('screen.today'), el('today.answer')], config)
    const ignoring = fingerprintScreenV2(
      [el('screen.today'), el('today.answer'), el('debug.banner'), el('ads.slot1')],
      { ...config, ignore: ['debug.banner', 'ads.*'] },
    )
    expect(ignoring.fingerprint).toBe(base.fingerprint)
  })

  test('drops dynamic IDs (numbers, UUIDs, dates, trailing counters)', () => {
    for (const id of [
      'calendar.noAnswer.2026-11-08', 'row.42', 'user.0b9f2c4e-1d2a-4b5c-8e9f-1a2b3c4d5e6f',
      'item_7', 'feed.card-3', 'day.2026-11-08T10:00',
    ]) expect(isDynamicId(id)).toBe(true)
    for (const id of ['screen.today', 'today.answer', 'mood.good', 'settings.category.privacyAndData']) {
      expect(isDynamicId(id)).toBe(false)
    }
    const a = fingerprintScreenV2([el('screen.journal'), el('calendar.noAnswer.2026-11-08')], config)
    const b = fingerprintScreenV2([el('screen.journal'), el('calendar.noAnswer.2026-11-09')], config)
    expect(a.fingerprint).toBe(b.fingerprint)
  })

  test('drops chrome IDs found on most screens, and only removes IDs', () => {
    const screens = [
      { anchors: ['tab.today', 'a'] }, { anchors: ['tab.today', 'b'] },
      { anchors: ['tab.today', 'c'] }, { anchors: ['d'] },
    ]
    expect(findChromeIds(screens)).toEqual(['tab.today'])
    expect(findChromeIds(screens.slice(0, 2))).toEqual([]) // too few screens to call anything chrome
    const withChrome = fingerprintScreenV2([el('screen.x'), el('tab.today'), el('a')], { ...config, chromeIds: ['tab.today'] })
    expect(withChrome.anchors).toEqual(['a'])
  })

  test('marker rule: one marker is the marker; zero or several give null; anchors exclude markers', () => {
    const one = fingerprintScreenV2([el('screen.feed'), el('feed.fillIn')], config)
    expect(one.marker).toBe('screen.feed')
    expect(one.anchors).toEqual(['feed.fillIn'])
    const none = fingerprintScreenV2([el('feed.fillIn')], config)
    expect(none.marker).toBeNull()
    const two = fingerprintScreenV2([el('screen.feed'), el('screen.today'), el('x')], config)
    expect(two.marker).toBeNull()
    expect(two.markers).toEqual(['screen.feed', 'screen.today'])
    expect(two.fingerprint).not.toBe(one.fingerprint)
    expect(two).toMatchObject({ stable: false, unstableReason: 'marker-conflict' })
    expect(matchScreenV2([], two).status).toBe('gap')
  })

  test('markerScope last-marker keeps the foreground screen: a hidden page that comes first is dropped', () => {
    const clean = fingerprintScreenV2([el('screen.today'), el('today.answer'), el('tab.today')], config)
    const leaked = [el('screen.journal'), el('journal.you'), el('screen.today'), el('today.answer'), el('tab.today')]
    expect(fingerprintScreenV2(leaked, config).stable).toBe(false)
    const scoped = fingerprintScreenV2(leaked, { ...config, markerScope: 'last-marker' })
    expect(scoped).toMatchObject({ stable: true, marker: 'screen.today' })
    expect(scoped.fingerprint).toBe(clean.fingerprint)
    // One marker: nothing before it is dropped.
    const single = [el('toolbar.x'), el('screen.today')]
    expect(fingerprintScreenV2(single, { ...config, markerScope: 'last-marker' }).anchors).toEqual(['toolbar.x'])
  })

  test('markers are found after the dynamic, ignore and chrome filters', () => {
    // `screen.step_2` is a dynamic ID, so it is no marker and cannot drive last-marker slicing.
    const withDynamic = [el('screen.a'), el('a.x'), el('screen.step_2'), el('a.y')]
    const scoped = fingerprintScreenV2(withDynamic, { ...config, markerScope: 'last-marker' })
    expect(scoped).toMatchObject({ marker: 'screen.a', stable: true })
    expect(scoped.anchors).toEqual(['a.x', 'a.y'])
    const ignored = fingerprintScreenV2([el('screen.old'), el('screen.new'), el('n.x')], { ...config, ignore: ['screen.old'] })
    expect(ignored).toMatchObject({ marker: 'screen.new', stable: true })
    const chrome = fingerprintScreenV2([el('screen.shell'), el('screen.new')], { ...config, chromeIds: ['screen.shell'] })
    expect(chrome.markers).toEqual(['screen.new'])
  })

  test('a screen with no stable IDs is not stable', () => {
    expect(fingerprintScreenV2([el(null), el('calendar.noAnswer.2026-11-08')], config))
      .toMatchObject({ stable: false, unstableReason: 'no-stable-ids' })
  })
})

describe('snapshots', () => {
  test('keep IDs, roles and flags, and labels only when allowed', () => {
    const observation = {
      elements: [{ ...el('today.save'), label: 'Save my answer' }], keyboardVisible: false, truncated: false,
    }
    expect(normalizedSnapshot(observation).json).not.toContain('Save my answer')
    expect(normalizedSnapshot(observation, { includeLabels: true }).json).toContain('Save my answer')
    expect(normalizedSnapshot(observation).sha256).not.toBe(normalizedSnapshot(observation, { includeLabels: true }).sha256)
  })
})

describe('matchScreenV2', () => {
  const stored = (nodeId: string, ids: string[]): StoredScreen => {
    const fp = fingerprintScreenV2(ids.map(id => el(id)), config)
    return { nodeId, marker: fp.marker, anchors: fp.anchors, fingerprint: fp.fingerprint }
  }
  const view = (ids: string[]) => fingerprintScreenV2(ids.map(id => el(id)), config)
  const nodes = [
    stored('settings', ['screen.settings', 'settings.a', 'settings.b', 'settings.c']),
    stored('today', ['screen.today', 'today.answer']),
  ]

  test('exact fingerprint is matched', () => {
    const match = matchScreenV2(nodes, view(['screen.today', 'today.answer']))
    expect(match).toMatchObject({ status: 'matched', candidateNodeIds: ['today'], ambiguous: false })
  })

  test('same marker with a subset of anchors is matched-scrolled, never matched', () => {
    const match = matchScreenV2(nodes, view(['screen.settings', 'settings.a']))
    expect(match.status).toBe('matched-scrolled')
    expect(match.candidateNodeIds).toEqual(['settings'])
  })

  test('a different anchor under the same marker is not scrolled', () => {
    expect(matchScreenV2(nodes, view(['screen.settings', 'settings.z'])).status).toBe('unknown')
  })

  test('two nodes with one fingerprint are ambiguous and pick none', () => {
    const twin = { ...nodes[1], nodeId: 'today-copy' }
    const match = matchScreenV2([...nodes, twin], view(['screen.today', 'today.answer']))
    expect(match).toMatchObject({ status: 'ambiguous', ambiguous: true, candidateNodeIds: ['today', 'today-copy'] })
    const report = observationReport(match, [...nodes, twin], false)
    expect(report).toEqual({
      status: 'ambiguous', ambiguous: true, truncated: false,
      candidates: [{ nodeId: 'today', marker: 'screen.today' }, { nodeId: 'today-copy', marker: 'screen.today' }],
    })
  })

  test('two scrolled candidates are ambiguous', () => {
    const second = stored('settings2', ['screen.settings', 'settings.a', 'settings.d'])
    expect(matchScreenV2([...nodes, second], view(['screen.settings', 'settings.a'])).status).toBe('ambiguous')
  })

  test('unknown without an expectation, changed with one', () => {
    const elsewhere = view(['screen.other', 'other.x'])
    expect(matchScreenV2(nodes, elsewhere).status).toBe('unknown')
    expect(matchScreenV2(nodes, elsewhere, 'today').status).toBe('changed')
    expect(matchScreenV2(nodes, view(['screen.today', 'today.answer']), 'settings').status).toBe('changed')
  })

  test('an unstable screen is a gap, never a node', () => {
    const match = matchScreenV2(nodes, fingerprintScreenV2([el(null)], config))
    expect(match).toMatchObject({ status: 'gap', candidateNodeIds: [], ambiguous: false })
  })

  test('the report never says ok for a gap, an unknown or a changed screen', () => {
    const elsewhere = view(['screen.other', 'other.x'])
    expect(observationReport(matchScreenV2(nodes, elsewhere), nodes, false).status).toBe('unknown')
    expect(observationReport(matchScreenV2(nodes, elsewhere, 'today'), nodes, false).status).toBe('changed')
    expect(observationReport(matchScreenV2(nodes, fingerprintScreenV2([el(null)], config)), nodes, false).status).toBe('gap')
    expect(observationReport(matchScreenV2(nodes, view(['screen.settings', 'settings.a'])), nodes, false).status).toBe('matched-scrolled')
  })

  test('a non-ambiguous report carries no candidates', () => {
    const match = matchScreenV2(nodes, view(['screen.today', 'today.answer']))
    expect(observationReport(match, nodes, true)).toEqual({ status: 'matched', ambiguous: false, candidates: [], truncated: true })
  })
})
