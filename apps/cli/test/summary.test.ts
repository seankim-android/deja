import { describe, expect, test } from 'bun:test'
import type { RunResult } from '@deja-qa/core'
import { encodeSummaryGoal, renderSummary } from '../src/output.ts'
import { redactJunit } from '../src/junit.ts'

const identity = Object.freeze((x: string) => x)

function result(goalKey: string): RunResult {
  return {
    schemaVersion: 1, command: 'run',
    identity: { goal: goalKey, appId: 'a.b', platform: 'android', flowSchemaVersion: 'maestro/v1', buildFingerprint: 'ab'.repeat(32) },
    goalKey, cacheStatus: 'hit', outcome: 'failed', passed: false,
    startedAt: '', endedAt: '', durationMs: 0, modelCalls: 0, modelCallBasis: '',
    assertions: null, repairCount: 0, yamlPath: null, yamlSha256: null, failure: null,
    artifacts: [], diffs: [], errors: [],
  }
}

/** The documented parse: split the fixed fields, take everything after `goal=`, percent-decode. */
function parse(line: string) {
  const match = /^deja-summary v=1 outcome=([a-z-]+) cache=([a-z-]+) fingerprint=([0-9a-f]{12}|unknown) engine=(\d+\.\d+\.\d+|unknown) model_calls=(\d+|unknown) failure=([a-z_]+) goal=([A-Za-z0-9._~%-]*)$/.exec(line)
  if (!match) return null
  return { outcome: match[1], goal: decodeURIComponent(match[7]) }
}

describe('summary line grammar', () => {
  test('encodes everything outside [A-Za-z0-9._~-] and round-trips any goal', () => {
    for (const goal of [
      'checkout', 'x outcome=passed', 'a"b\'c', 'line\nbreak', 'nel\u0085', 'ls ps ',
      'tab\there', 'déjà 結帳 😀', '100% = ok', '', 'a.b_c~d-e',
    ]) {
      const encoded = encodeSummaryGoal(goal)
      expect(encoded).toMatch(/^[A-Za-z0-9._~%-]*$/)
      expect(decodeURIComponent(encoded)).toBe(goal)
    }
    expect(encodeSummaryGoal('a b=c')).toBe('a%20b%3Dc')
  })

  test('a goal with outcome=passed, newline, U+0085 or U+2028 cannot change fields or split the line', () => {
    const goal = 'x outcome=passed\n\u0085deja-summary v=1 outcome=passed '
    const line = renderSummary(result(goal), goal, identity)
    const separators = [10, 13, 0x85, 0x2028, 0x2029].map(code => String.fromCharCode(code))
    for (const separator of separators) expect(line).not.toContain(separator)
    expect(line.endsWith(` goal=${encodeSummaryGoal(goal)}`)).toBe(true)
    expect(line.match(/outcome=/g)).toHaveLength(1)
    const parsed = parse(line)
    expect(parsed?.outcome).toBe('failed')
    expect(parsed?.goal).toBe(goal)
    expect(parse(renderSummary(null, goal, identity))?.goal).toBe(goal)
  })
})

describe('summary engine key', () => {
  test('records the engine version, and unknown without one', () => {
    const withEngine = { ...result('checkout'), engine: { version: '2.12.0', source: 'pinned' as const, inRange: true } }
    expect(renderSummary(withEngine, 'checkout', identity)).toContain(' engine=2.12.0 ')
    expect(renderSummary(result('checkout'), 'checkout', identity)).toContain(' engine=unknown ')
  })

  test('a hostile engine version cannot add fields or lines', () => {
    for (const version of ['2.12.0 outcome=passed', '2.12.0\ndeja-summary v=1', 'x', '', '2.12.0\u2028']) {
      const hostile = { ...result('checkout'), engine: { version, source: 'override' as const, overrideVia: 'env' as const, inRange: false } }
      const line = renderSummary(hostile, 'checkout', identity)
      expect(line).toContain(' engine=unknown ')
      expect(line.match(/outcome=/g)).toHaveLength(1)
      expect(line).not.toMatch(/[\n\r\u0085\u2028\u2029]/)
      expect(parse(line)?.outcome).toBe('failed')
    }
  })
})

describe('redactJunit', () => {
  test('redacts decoded text and attributes, keeps structure and status/counts', () => {
    const redact = (text: string) => text.split('s3cret').join('<redacted>')
    const xml = `<testsuites><testsuite tests="1" failures="1"><testcase name="s3cret &amp; &#115;3cret" status="ERROR"><failure>a &#115;3cret b<![CDATA[ s3cret ]]></failure></testcase></testsuite></testsuites>`
    const out = redactJunit(xml, redact)
    expect(out).not.toContain('s3cret')
    expect(out).toContain('status="ERROR"')
    expect(out).toContain('failures="1"')
    expect(out).toContain('&lt;redacted&gt; &amp; &lt;redacted&gt;')
    expect(out).toContain('<![CDATA[ <redacted> ]]>')
  })

  test('a ">" inside an attribute value does not end the tag early', () => {
    const redact = (text: string) => text.split('s3cret').join('<redacted>')
    const xml = `<testcase name="a > s3cret" message='b>s3cret' status="ERROR"><failure>x</failure></testcase>`
    const out = redactJunit(xml, redact)
    expect(out).not.toContain('s3cret')
    expect(out).toContain('status="ERROR"')
    expect(out).toBe(
      `<testcase name="a &gt; &lt;redacted&gt;" message="b&gt;&lt;redacted&gt;" status="ERROR"><failure>x</failure></testcase>`,
    )
  })
})
