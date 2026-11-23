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
  const match = /^deja-summary v=1 outcome=([a-z-]+) cache=([a-z-]+) fingerprint=([0-9a-f]{12}|unknown) model_calls=(\d+|unknown) failure=([a-z_]+) goal=([A-Za-z0-9._~%-]*)$/.exec(line)
  if (!match) return null
  return { outcome: match[1], goal: decodeURIComponent(match[6]) }
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
})
