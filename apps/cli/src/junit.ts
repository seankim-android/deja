import { mkdir, readFile, rename, rm, writeFile } from 'node:fs/promises'
import { dirname, relative, sep } from 'node:path'
import type { ReportReceipt, RunReporter, RunResult } from '@deja-qa/core'
import type { Redactor } from './redaction.ts'

/**
 * Opt-in JUnit output for CI hosts.
 *
 * When Maestro ran, its own report is passed through untouched. Maestro never ran, or the run
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
      <failure>${escapeXml(input.reason)}</failure>
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

/** True when a Maestro JUnit report shows at least one case that did not succeed. */
export function reportsFailure(xml: string) {
  return /<(?:failure|error)[\s>/]/.test(xml) || /\sstatus="(?!SUCCESS")/.test(xml)
}

async function atomicWrite(path: string, contents: string) {
  await mkdir(dirname(path), { recursive: true })
  const temporary = `${path}.${process.pid}.tmp`
  await writeFile(temporary, contents)
  await rename(temporary, path)
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
    const keep = maestroReport !== null && (result.passed || reportsFailure(maestroReport))
    if (!keep) {
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
