import { mkdir, readFile, rename, rm, writeFile } from 'node:fs/promises'
import { dirname, relative, sep } from 'node:path'
import type { ReportReceipt, RunAttempt, RunReporter, RunResult } from '@deja-qa/core'
import type { Redactor } from './redaction.ts'

/**
 * Opt-in JUnit output for CI hosts.
 *
 * When Maestro ran, its own report is passed through, with Déjà's redaction applied. Maestro never ran, or the run
 * did not pass and Maestro's report shows no failure: a minimal report is written instead, with
 * one failed `<testcase>` (`status="ERROR"`, the status Maestro uses), so a missing or stale file
 * is never read as a pass. Synthesized files carry `<property name="source" value="deja-synthesized"/>`.
 */

function escapeXml(text: string) {
  // XML 1.0 forbids most control characters, even escaped.
  const printable = [...text].filter(character => {
    const code = character.charCodeAt(0)
    return code >= 0x20 || code === 0x09 || code === 0x0a || code === 0x0d
  }).join('')
  return printable.replace(/[&<>"']/g, character => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&apos;',
  })[character]!)
}

export function synthesizedJunit(input: {
  goal: string
  passed: boolean
  reason: string
  durationMs: number
}) {
  const goal = escapeXml(input.goal)
  const seconds = (input.durationMs / 1000).toFixed(3)
  const testcase = input.passed
    ? `<testcase id="${goal}" name="${goal}" classname="${goal}" time="${seconds}" status="SUCCESS"/>`
    : `<testcase id="${goal}" name="${goal}" classname="${goal}" time="${seconds}" status="ERROR">
      <failure message="${escapeXml(input.reason.replace(/\s+/g, ' ').slice(0, 300))}">${escapeXml(input.reason)}</failure>
    </testcase>`
  return `<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="Test Suite" tests="1" failures="${input.passed ? 0 : 1}" time="${seconds}">
    <properties>
      <property name="source" value="deja-synthesized"/>
    </properties>
    ${testcase}
  </testsuite>
</testsuites>
`
}

/**
 * Two attempts (opt-in rerun): one testcase per attempt, so a host sees both. An attempt that
 * failed carries its label, failing step and output path. A flaky run always has a failed testcase,
 * so a host that reads only this file never sees a pass.
 */
export function synthesizedAttemptsJunit(input: {
  goal: string
  outcome: string
  attempts: RunAttempt[]
  durationMs: number
}) {
  const goal = escapeXml(input.goal)
  const failures = input.attempts.filter(attempt => !attempt.passed).length
  const cases = input.attempts.map(attempt => {
    const name = `${goal} (attempt ${attempt.attempt})`
    const seconds = (attempt.durationMs / 1000).toFixed(3)
    if (attempt.passed) {
      return `<testcase id="${name}" name="${name}" classname="${goal}" time="${seconds}" status="SUCCESS"/>`
    }
    const failure = attempt.failure
    const detail = [
      `outcome: ${input.outcome}`,
      `label: ${failure?.category ?? 'unclassified'}`,
      ...(failure?.failingStep ? [`step: ${failure.failingStep}`] : []),
      ...(failure?.rawArtifactPath ? [`artifact: ${failure.rawArtifactPath}`] : []),
      ...(failure?.crash ? [`crash signal: ${failure.crash.source}${failure.crash.reportFile ? ` ${failure.crash.reportFile}` : ''}`] : []),
    ].join('; ')
    return `<testcase id="${name}" name="${name}" classname="${goal}" time="${seconds}" status="ERROR">
      <failure message="${escapeXml(detail.replace(/\s+/g, ' ').slice(0, 300))}">${escapeXml(detail)}</failure>
    </testcase>`
  })
  const seconds = (input.durationMs / 1000).toFixed(3)
  return `<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="Test Suite" tests="${input.attempts.length}" failures="${failures}" time="${seconds}">
    <properties>
      <property name="source" value="deja-synthesized"/>
      <property name="outcome" value="${escapeXml(input.outcome)}"/>
    </properties>
    ${cases.join('\n    ')}
  </testsuite>
</testsuites>
`
}

/** True when a Maestro JUnit report shows at least one case that did not succeed. */
export function reportsFailure(xml: string) {
  return /<(?:failure|error)[\s>/]/.test(xml) || /\sstatus="(?!SUCCESS")/.test(xml)
}

async function atomicWrite(path: string, contents: string) {
  await mkdir(dirname(path), { recursive: true })
  const temporary = `${path}.${process.pid}.tmp`
  try {
    await writeFile(temporary, contents)
    await rename(temporary, path)
  } finally {
    await rm(temporary, { force: true })
  }
}

function decodeXml(text: string) {
  return text
    .replace(/&#x([0-9a-fA-F]+);/g, (_, hex: string) => codePoint(Number.parseInt(hex, 16)))
    .replace(/&#([0-9]+);/g, (_, decimal: string) => codePoint(Number.parseInt(decimal, 10)))
    .replace(/&(lt|gt|quot|apos);/g, (_, name: string) => (
      { lt: '<', gt: '>', quot: '"', apos: "'" } as Record<string, string>)[name])
    .replace(/&amp;/g, '&')
}

function codePoint(value: number) {
  try {
    return String.fromCodePoint(value)
  } catch {
    return ''
  }
}

// Counts and status stay as Maestro wrote them, so a short fixture value cannot change them.
const STRUCTURAL_ATTRIBUTES = new Set(['status', 'failures', 'errors', 'tests', 'skipped', 'time'])

/**
 * Runs the redactor over the text a person can read in a JUnit file: attribute values and text
 * nodes (decoded first, so `&amp;` and numeric-entity forms of a value cannot slip through, then
 * escaped again), CDATA and comments. Element and attribute names stay, so the XML stays well
 * formed and the status and count attributes keep their values.
 */
export function redactJunit(xml: string, redact: Redactor) {
  return xml.replace(
    /<!\[CDATA\[([\s\S]*?)\]\]>|<!--([\s\S]*?)-->|<(?:[^>"']|"[^"]*"|'[^']*')*>|[^<]+/g,
    (segment, cdata: string | undefined, comment: string | undefined) => {
      if (cdata !== undefined) return `<![CDATA[${redact(cdata)}]]>`
      if (comment !== undefined) return `<!--${redact(comment)}-->`
      if (!segment.startsWith('<')) return escapeXml(redact(decodeXml(segment)))
      return segment.replace(
        /(\s([\w:.-]+)\s*=\s*)(?:"([^"]*)"|'([^']*)')/g,
        (whole, prefix: string, name: string, double: string | undefined, single: string | undefined) => {
          if (STRUCTURAL_ATTRIBUTES.has(name)) return whole
          return `${prefix}"${escapeXml(redact(decodeXml(double ?? single ?? '')))}"`
        },
      )
    },
  )
}

/** Remove any earlier file so a stale report can never stand in for this run; create its folder. */
export async function prepareJunit(path: string) {
  await rm(path, { force: true })
  await mkdir(dirname(path), { recursive: true })
}

export async function writeErroredJunit(path: string, goal: string, message: string) {
  await atomicWrite(path, synthesizedJunit({
    goal, passed: false, reason: message, durationMs: 0,
  }))
}

function reasonFor(result: RunResult, redact: Redactor) {
  const parts = [`outcome: ${result.outcome}`]
  if (result.failure) parts.push(`failure: ${result.failure.category}`)
  for (const error of result.errors) parts.push(error)
  return redact(parts.join('; '))
}

export function displayPath(workspaceRoot: string, path: string) {
  const inside = relative(workspaceRoot, path)
  return inside && !inside.startsWith('..') && !inside.startsWith(sep)
    ? inside.split(sep).join('/')
    : path
}

/** Finalizes the host's JUnit file for every command result, then adds it to `artifacts`. */
export class JunitReporter implements RunReporter {
  private readonly inner: RunReporter
  private readonly path: string
  private readonly workspaceRoot: string
  private readonly redact: Redactor

  constructor(inner: RunReporter, path: string, workspaceRoot: string, redact: Redactor) {
    this.inner = inner
    this.path = path
    this.workspaceRoot = workspaceRoot
    this.redact = redact
  }

  async report(result: RunResult): Promise<ReportReceipt> {
    let maestroReport: string | null = null
    try {
      maestroReport = await readFile(this.path, 'utf8')
    } catch {
      // Maestro never ran, or did not get as far as writing a report.
    }
    // A rerun overwrote Maestro's file with the second attempt only, so write both attempts.
    const rerun = result.attempts !== undefined && result.attempts.length === 2
    const keep = !rerun && maestroReport !== null && (result.passed || reportsFailure(maestroReport))
    if (rerun) {
      await atomicWrite(this.path, redactJunit(synthesizedAttemptsJunit({
        goal: result.goalKey, outcome: result.outcome, attempts: result.attempts!, durationMs: result.durationMs,
      }), this.redact))
    } else if (keep) {
      const redacted = redactJunit(maestroReport!, this.redact)
      if (redacted !== maestroReport) await atomicWrite(this.path, redacted)
    } else {
      await atomicWrite(this.path, synthesizedJunit({
        goal: result.goalKey,
        passed: result.passed,
        reason: reasonFor(result, this.redact),
        durationMs: result.durationMs,
      }))
    }
    result.artifacts = [
      ...result.artifacts,
      { kind: 'junit', path: displayPath(this.workspaceRoot, this.path) },
    ]
    return this.inner.report(result)
  }
}
