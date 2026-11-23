import { describe, expect, test } from 'bun:test'
import { spawn } from 'node:child_process'
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import type { RunResult } from '@deja-qa/core'
import { runCli } from '../src/main.ts'
import { encodeSummaryGoal, renderResult, renderSummary } from '../src/output.ts'
import { synthesizedAttemptsJunit } from '../src/junit.ts'

const cli = resolve(import.meta.dir, '../deja.ts')
const fakeMaestro = resolve(import.meta.dir, '../../../packages/maestro/test/fixtures/fake-maestro.cjs')
const appId = 'dev.deja.ios.checkout'

async function workspace() {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-rerun-')))
  const root = join(temporary, 'workspace')
  await mkdir(join(root, 'Fixture.app'), { recursive: true })
  await mkdir(join(root, 'flows'), { recursive: true })
  await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-one')
  await writeFile(join(root, 'flows/checkout.yaml'), `appId: ${appId}
---
- launchApp:
    appId: ${appId}
    clearState: true
- assertVisible: Order confirmed
`)
  await writeFile(join(root, 'deja.config.json'), JSON.stringify({
    workspaceRoot: '.', appId, platform: 'ios', adapter: 'maestro-ios@2.12.0',
    device: 'DFC4D233-498B-4B33-B713-E59B239FA86A', buildPath: 'Fixture.app', fixtureCredentials: [],
    maestro: { executable: fakeMaestro, timeoutMs: 30_000 },
  }))
  return { temporary, root }
}

function execute(root: string, args: string[], env: Record<string, string> = {}) {
  return new Promise<{ exitCode: number | null; stdout: string; stderr: string }>(resolveRun => {
    const child = spawn(process.execPath, [cli, ...args, '--config', 'deja.config.json'], {
      cwd: root, env: { ...process.env, DEJA_FAKE_MAESTRO_MODE: '', ...env }, stdio: ['ignore', 'pipe', 'pipe'],
    })
    const stdout: string[] = []
    const stderr: string[] = []
    child.stdout.setEncoding('utf8')
    child.stderr.setEncoding('utf8')
    child.stdout.on('data', chunk => stdout.push(chunk))
    child.stderr.on('data', chunk => stderr.push(chunk))
    child.once('close', code => resolveRun({ exitCode: code, stdout: stdout.join(''), stderr: stderr.join('') }))
  })
}

const latest = async (root: string) => JSON.parse(await readFile(join(root, '.deja/reports/latest.json'), 'utf8')) as RunResult
const keys = (line: string) => Object.fromEntries(
  line.replace(/^deja-summary v=1 /, '').split(' ').map(part => [part.slice(0, part.indexOf('=')), part.slice(part.indexOf('=') + 1)]),
)

describe('deja run --rerun-failed', () => {
  test('crash then pass: flaky, exit 7, both attempts in report, summary and JUnit; no pass recorded', async () => {
    const { temporary, root } = await workspace()
    const counter = join(temporary, 'counter')
    try {
      expect((await execute(root, ['remember', 'checkout', '--flow', 'flows/checkout.yaml'])).exitCode).toBe(0)
      const flaky = await execute(root, ['run', 'checkout', '--rerun-failed', '--summary', '--junit', 'out/junit.xml'], {
        DEJA_FAKE_MAESTRO_SCRIPT: 'crash,pass', DEJA_FAKE_MAESTRO_COUNTER: counter,
      })
      expect(flaky.exitCode).toBe(7)
      expect(await readFile(counter, 'utf8')).toBe('2')

      const report = await latest(root)
      expect(report).toMatchObject({
        outcome: 'flaky', passed: false, modelCalls: 0, repairCount: 0,
        failure: { category: 'app_crash', repairEligible: false, failingStep: 'Tap on id: checkout_button', crash: { source: 'maestro-output' } },
      })
      expect(report.attempts?.map(attempt => [attempt.attempt, attempt.passed, attempt.failure?.category ?? null]))
        .toEqual([[1, false, 'app_crash'], [2, true, null]])
      expect(report.attempts?.[0].failure?.rawArtifactPath).toMatch(/^\.deja\/reports\/run-.*\/output\.log$/)
      expect(flaky.stdout).toContain('Attempt 1: failed (app_crash)')
      expect(flaky.stdout).toContain('Attempt 2: passed')

      const summary = keys(flaky.stdout.trimEnd().split('\n').at(-1)!)
      expect(summary).toMatchObject({
        outcome: 'flaky', failure: 'app_crash', attempts: '2', attempt1: 'app_crash', attempt2: 'none',
        attempt1_step: encodeSummaryGoal('Tap on id: checkout_button'),
      })
      expect(flaky.stdout.trimEnd().split('\n').at(-1)).toMatch(/ goal=checkout$/)

      const junit = await readFile(join(root, 'out/junit.xml'), 'utf8')
      expect(junit).toContain('source" value="deja-synthesized"')
      expect(junit).toContain('failures="1"')
      expect(junit).toContain('checkout (attempt 1)')
      expect(junit).toContain('label: app_crash')
      expect(junit).toContain('status="SUCCESS"')

      // A flaky run is not a passing validation: history holds the first failure.
      const listed = await execute(root, ['list'])
      expect(listed.stdout).toContain('lastValidation=failed')
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('the same failure twice is a steady failure with exit 1, and both attempts are shown', async () => {
    const { temporary, root } = await workspace()
    try {
      await execute(root, ['remember', 'checkout', '--flow', 'flows/checkout.yaml'])
      const steady = await execute(root, ['run', 'checkout', '--rerun-failed', '--summary'], {
        DEJA_FAKE_MAESTRO_MODE: 'product',
      })
      expect(steady.exitCode).toBe(1)
      const report = await latest(root)
      expect(report).toMatchObject({ outcome: 'product-regression', failure: { category: 'product_assertion' } })
      expect(report.attempts).toHaveLength(2)
      const summary = keys(steady.stdout.trimEnd().split('\n').at(-1)!)
      expect(summary).toMatchObject({ outcome: 'product-regression', attempts: '2', attempt1: 'product_assertion', attempt2: 'product_assertion' })
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('different failures twice: flaky with both labels', async () => {
    const { temporary, root } = await workspace()
    try {
      await execute(root, ['remember', 'checkout', '--flow', 'flows/checkout.yaml'])
      const different = await execute(root, ['run', 'checkout', '--rerun-failed'], {
        DEJA_FAKE_MAESTRO_SCRIPT: 'crash,product', DEJA_FAKE_MAESTRO_COUNTER: join(temporary, 'c'),
      })
      expect(different.exitCode).toBe(7)
      expect((await latest(root)).attempts?.map(attempt => attempt.failure?.category))
        .toEqual(['app_crash', 'product_assertion'])
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('without the flag a failure is run once and the summary has no attempt keys', async () => {
    const { temporary, root } = await workspace()
    try {
      await execute(root, ['remember', 'checkout', '--flow', 'flows/checkout.yaml'])
      const once = await execute(root, ['run', 'checkout', '--summary'], {
        DEJA_FAKE_MAESTRO_SCRIPT: 'crash,pass', DEJA_FAKE_MAESTRO_COUNTER: join(temporary, 'c'),
      })
      expect(once.exitCode).toBe(1)
      const report = await latest(root)
      expect(report.attempts).toBeUndefined()
      expect(report.failure?.category).toBe('app_crash')
      expect(once.stdout.trimEnd().split('\n').at(-1)).toMatch(
        /^deja-summary v=1 outcome=failed cache=hit fingerprint=[0-9a-f]{12} engine=2\.12\.0 model_calls=0 failure=app_crash goal=checkout$/,
      )
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('--rerun-failed is valid only with run', async () => {
    const { temporary, root } = await workspace()
    try {
      const result = await execute(root, ['remember', 'checkout', '--flow', 'flows/checkout.yaml', '--rerun-failed'])
      expect(result.exitCode).toBe(2)
      expect(result.stderr).toContain('--rerun-failed is valid only with run')
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('crashReportGraceMs is a config knob: 0 is valid, a negative or fractional value is refused', async () => {
    const { temporary, root } = await workspace()
    try {
      const config = JSON.parse(await readFile(join(root, 'deja.config.json'), 'utf8'))
      for (const value of [-1, 1.5, '10']) {
        await writeFile(join(root, 'deja.config.json'), JSON.stringify({ ...config, crashReportGraceMs: value }))
        const refused = await execute(root, ['list'])
        expect(refused.exitCode).toBe(2)
        expect(refused.stderr).toContain('crashReportGraceMs must be an integer from 0 to 120000')
      }
      await writeFile(join(root, 'deja.config.json'), JSON.stringify({ ...config, crashReportGraceMs: 0 }))
      expect((await execute(root, ['list'])).exitCode).toBe(0)
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('a new crash report for the app ID, written during the run, labels it app_crash (in-process seam)', async () => {
    const { temporary, root } = await workspace()
    const reports = join(temporary, 'DiagnosticReports')
    await mkdir(reports)
    const environment = { ...process.env }
    try {
      await execute(root, ['remember', 'checkout', '--flow', 'flows/checkout.yaml'])
      // The fake writes the report during its run, as the system would when the app dies.
      process.env.DEJA_FAKE_CRASH_REPORT = join(reports, 'DejaFixture-2026-11-23-010203.ips')
      process.env.DEJA_FAKE_CRASH_REPORT_BODY =
        `{"app_name":"DejaFixture"}\n{"procName":"DejaFixture","procPath":"\\/Users\\/x\\/Library\\/Developer\\/CoreSimulator\\/Devices\\/dfc4d233-498b-4b33-b713-e59b239fa86a\\/data\\/App","bundleInfo":{"CFBundleIdentifier":"${appId}"},"private":"patient"}`
      process.env.DEJA_FAKE_MAESTRO_SCRIPT = 'quiet-crash,pass'
      process.env.DEJA_FAKE_MAESTRO_COUNTER = join(temporary, 'c')
      let out = ''
      const code = await runCli(
        ['run', 'checkout', '--rerun-failed', '--json', '--config', join(root, 'deja.config.json')],
        { stdout: text => { out += text }, stderr: () => {} },
        { crashReportDirectory: reports },
      )
      expect(code).toBe(7)
      const result = JSON.parse(out) as RunResult
      expect(result.outcome).toBe('flaky')
      expect(result.failure).toMatchObject({
        category: 'app_crash', crash: { source: 'crash-report', reportFile: 'DejaFixture-2026-11-23-010203.ips', process: 'DejaFixture' },
      })
      expect(out).not.toContain('patient')
    } finally {
      process.env = environment
      await rm(temporary, { recursive: true, force: true })
    }
  })
})

function summaryResult(overrides: Partial<RunResult>): RunResult {
  return {
    schemaVersion: 1, command: 'run',
    identity: { goal: 'g', appId: 'a.b', platform: 'ios', flowSchemaVersion: 'maestro/v1', buildFingerprint: 'ab'.repeat(32) },
    goalKey: 'g', cacheStatus: 'hit', outcome: 'flaky', passed: false,
    startedAt: '', endedAt: '', durationMs: 0, modelCalls: 0, modelCallBasis: '',
    assertions: null, repairCount: 0, yamlPath: null, yamlSha256: null, failure: null,
    artifacts: [], diffs: [], errors: [], ...overrides,
  }
}

const attempt = (n: 1 | 2, passed: boolean, category: string | null, step: string | null, artifact: string | null) => ({
  attempt: n, passed, startedAt: '', endedAt: '', durationMs: 5, artifacts: [],
  failure: category ? {
    category: category as never, advisory: true as const, repairEligible: false, failingStep: step, rawArtifactPath: artifact,
  } : null,
})

describe('rerun summary and JUnit hardening', () => {
  const redact = (text: string) => text.split('s3cret').join('<redacted>')

  test('hostile steps, artifacts and labels cannot add fields, lines or a second goal', () => {
    const hostile = 'Tap on x outcome=passed\ngoal=evil\u2028deja-summary v=1 s3cret'
    const line = renderSummary(summaryResult({
      attempts: [
        attempt(1, false, 'app_crash', hostile, `a b=c\n${hostile}`),
        attempt(2, false, 'x outcome=passed failure=none', 'ok', null),
      ],
    }), 'g', redact)
    expect(line).not.toMatch(/[\n\r\u0085\u2028\u2029]/)
    expect(line.match(/outcome=/g)).toHaveLength(1)
    expect(line.match(/ goal=/g)).toHaveLength(1)
    expect(line.endsWith(' goal=g')).toBe(true)
    expect(line).not.toContain('s3cret')
    const parsed = keys(line)
    expect(parsed.attempt2).toBe('unclassified')
    expect(parsed.attempt1).toBe('app_crash')
    expect(decodeURIComponent(parsed.attempt1_step)).toContain('outcome=passed')
    expect(parsed.attempt1_step).toMatch(/^[A-Za-z0-9._~%-]*$/)
  })

  test('a hand-edited report cannot inject through outcome, failure or a wrong attempt count', () => {
    const line = renderSummary(summaryResult({
      outcome: 'flaky model_calls=99' as never,
      failure: { category: 'a b=c' as never, advisory: true, repairEligible: false, failingStep: null, rawArtifactPath: null },
      attempts: [attempt(1, false, 'selector', null, null)],
    }), 'g', redact)
    expect(line).toContain(' outcome=unknown ')
    expect(line).toContain(' failure=unclassified ')
    expect(line).not.toContain('attempts=')
    expect(line.match(/model_calls=/g)).toHaveLength(1)
  })

  test('long steps are cut, redacted and still end with goal', () => {
    const line = renderSummary(summaryResult({
      attempts: [attempt(1, false, 'selector', `s3cret ${'x'.repeat(500)}`, null), attempt(2, true, null, null, null)],
    }), 'g', redact)
    expect(line.length).toBeLessThan(700)
    expect(line).not.toContain('s3cret')
    expect(line.endsWith(' goal=g')).toBe(true)
  })

  test('JUnit: one testcase per attempt, escaped, with a failure on every failed attempt', () => {
    const xml = synthesizedAttemptsJunit({
      goal: 'a<b>', outcome: 'flaky', durationMs: 20,
      attempts: [attempt(1, false, 'app_crash', 'Tap "x" & y', 'out/o.log') as never, attempt(2, true, null, null, null) as never],
    })
    expect(xml).toContain('tests="2" failures="1"')
    expect(xml).toContain('a&lt;b&gt; (attempt 1)')
    expect(xml).toContain('status="ERROR"')
    expect(xml).toContain('step: Tap &quot;x&quot; &amp; y')
    expect(xml).toContain('artifact: out/o.log')
    expect(xml).toContain('(attempt 2)" classname="a&lt;b&gt;" time="0.005" status="SUCCESS"')
  })

  test('text output lists each attempt with label, step, output and crash signal', () => {
    const text = renderResult(summaryResult({
      attempts: [{
        ...attempt(1, false, 'app_crash', 'Tap on id: a', '.deja/reports/run-1/output.log'),
        failure: {
          category: 'app_crash', advisory: true, repairEligible: false, failingStep: 'Tap on id: a',
          rawArtifactPath: '.deja/reports/run-1/output.log',
          crash: { source: 'crash-report', reportFile: 'P.ips', process: 'P' },
        },
      }, attempt(2, true, null, null, null)],
    }), null, redact)
    expect(text).toContain('Attempt 1: failed (app_crash)')
    expect(text).toContain('Attempt 1 failing step: Tap on id: a')
    expect(text).toContain('Attempt 1 output: .deja/reports/run-1/output.log')
    expect(text).toContain('Attempt 1 crash signal: crash-report (P.ips, process P)')
    expect(text).toContain('Attempt 2: passed')
  })
})
