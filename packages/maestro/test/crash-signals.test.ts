import { afterAll, beforeAll, describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, rm, symlink, utimes, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  classifyMaestroFailure,
  defaultCrashReportDirectory,
  detectCrashInOutput,
  findCrashReport,
  iosMaestroAdapter,
  MaestroWalkRunner,
} from '../src/index.ts'

const execution = [{ kind: 'execution' } as const]
const tapStep = 'Tap on id: checkout_button'

function classify(output: string, crashSignal?: Parameters<typeof classifyMaestroFailure>[0]['crashSignal']) {
  return classifyMaestroFailure({
    passed: false, timedOut: false, output, errors: execution, rawArtifactPath: 'output.log', crashSignal,
  })
}

describe('app_crash from Maestro output', () => {
  test('an explicit Maestro crash line labels the run app_crash and is never repair-eligible', () => {
    for (const line of [
      'App crashed', 'App crashed (SIGSEGV)', 'App crashed (signal 11)', 'App crashed (unknown cause)',
      'dev.deja.ios.checkout (pid 4242) was terminated by SIGABRT.',
    ]) {
      expect(classify(`${line}\n`)).toMatchObject({
        category: 'app_crash', repairEligible: false, advisory: true,
        crash: { source: 'maestro-output' },
      })
    }
  })

  test('a crash followed by "Element not found" is app_crash, not selector', () => {
    const output = `Launch app "dev.deja.ios.checkout"... COMPLETED\nApp crashed (SIGSEGV)\n${tapStep}... FAILED\nElement not found: Id matching regex: checkout_button\n`
    expect(classify(output)).toMatchObject({
      category: 'app_crash', failingStep: tapStep, repairEligible: false,
    })
    // The same output without the crash line is the repair-eligible selector failure it looks like.
    expect(classify(`${tapStep}... FAILED\nElement not found: Id matching regex: checkout_button\n`))
      .toMatchObject({ category: 'selector', repairEligible: true })
  })

  test('crash-like output without an explicit signal keeps its label', () => {
    for (const text of [
      'The app is not running. It may have crashed or been closed.',
      'The app crashed',
      'Element not found: Text matching regex: App crashed',
      'Assertion is false: "App crashed" is visible',
      'xApp crashed',
      'App crashed (SIGSEGV) twice',
    ]) {
      expect(classify(`${tapStep}... FAILED\n${text}\n`)?.category).not.toBe('app_crash')
    }
    expect(classify(`${tapStep}... FAILED\nThe app is not running. It may have crashed or been closed.\n`))
      .toMatchObject({ category: 'unknown' })
    expect(classify(`${tapStep}... FAILED\nElement not found: Id matching regex: x\n`))
      .toMatchObject({ category: 'selector' })
  })

  test('a timeout or a runner error still wins over a crash line', () => {
    expect(classifyMaestroFailure({
      passed: false, timedOut: true, output: 'App crashed\n', errors: [{ kind: 'timeout' }],
    })?.category).toBe('timeout')
    expect(classifyMaestroFailure({
      passed: false, timedOut: false, output: 'App crashed\n', errors: [{ kind: 'launch' }],
    })?.category).toBe('runner')
  })

  test('a crash-report signal from the runner labels app_crash and carries only file and process', () => {
    const result = classify(`${tapStep}... FAILED\nElement not found: id\n`, {
      source: 'crash-report', reportFile: 'DejaFixture-2026-11-23-010203.ips', process: 'DejaFixture',
    })
    expect(result).toEqual({
      category: 'app_crash', failingStep: tapStep, rawArtifactPath: 'output.log', advisory: true,
      repairEligible: false,
      crash: { source: 'crash-report', reportFile: 'DejaFixture-2026-11-23-010203.ips', process: 'DejaFixture' },
    })
  })

  test('detectCrashInOutput is whole-line only', () => {
    expect(detectCrashInOutput('  App crashed (SIGILL)  \r\n')).toEqual({ source: 'maestro-output' })
    expect(detectCrashInOutput('log: App crashed')).toBeNull()
    expect(detectCrashInOutput('')).toBeNull()
  })
})

describe('crash report finder', () => {
  let directory: string
  const appId = 'dev.deja.ios.checkout'
  const since = Date.parse('2026-11-23T01:00:00Z')
  const until = Date.parse('2026-11-23T01:05:00Z')

  async function report(name: string, body: string, when: string) {
    const path = join(directory, name)
    await writeFile(path, body)
    const time = new Date(when)
    await utimes(path, time, time)
  }
  const header = (bundle: string, proc = 'DejaFixture') =>
    `{"app_name":"${proc}","bug_type":"309"}\n{"procName":"${proc}","bundleInfo":{"CFBundleIdentifier":"${bundle}"},"secret":"patient-name"}`

  beforeAll(async () => {
    directory = await mkdtemp(join(tmpdir(), 'deja-ips-'))
  })
  afterAll(async () => {
    await rm(directory, { recursive: true, force: true })
  })

  test('matches a new report for the app ID written during the run, and keeps only its name and process', async () => {
    await report('DejaFixture-2026-11-23-010203.ips', header(appId), '2026-11-23T01:02:03Z')
    const found = await findCrashReport({ directory, appId, sinceMs: since, untilMs: until })
    expect(found).toEqual({
      source: 'crash-report', reportFile: 'DejaFixture-2026-11-23-010203.ips', process: 'DejaFixture',
    })
    expect(JSON.stringify(found)).not.toContain('patient-name')
  })

  test('ignores reports for another app, outside the run window, or not .ips', async () => {
    const other = await mkdtemp(join(tmpdir(), 'deja-ips-other-'))
    try {
      await writeFile(join(other, 'Other.ips'), header('com.example.other'))
      await writeFile(join(other, 'Old.ips'), header(appId))
      await utimes(join(other, 'Old.ips'), new Date('2026-11-22T00:00:00Z'), new Date('2026-11-22T00:00:00Z'))
      await writeFile(join(other, 'Late.ips'), header(appId))
      await utimes(join(other, 'Late.ips'), new Date('2026-11-23T02:00:00Z'), new Date('2026-11-23T02:00:00Z'))
      await writeFile(join(other, 'Note.txt'), header(appId))
      await utimes(join(other, 'Note.txt'), new Date('2026-11-23T01:02:00Z'), new Date('2026-11-23T01:02:00Z'))
      await utimes(join(other, 'Other.ips'), new Date('2026-11-23T01:02:00Z'), new Date('2026-11-23T01:02:00Z'))
      expect(await findCrashReport({ directory: other, appId, sinceMs: since, untilMs: until })).toBeNull()
    } finally {
      await rm(other, { recursive: true, force: true })
    }
  })

  test('an app ID is matched exactly, not as a prefix or a pattern', async () => {
    const other = await mkdtemp(join(tmpdir(), 'deja-ips-exact-'))
    try {
      await writeFile(join(other, 'A.ips'), header(`${appId}.extension`))
      await utimes(join(other, 'A.ips'), new Date('2026-11-23T01:02:00Z'), new Date('2026-11-23T01:02:00Z'))
      expect(await findCrashReport({ directory: other, appId, sinceMs: since, untilMs: until })).toBeNull()
      expect(await findCrashReport({ directory: other, appId: 'dev.deja.ios.checkout.extension', sinceMs: since, untilMs: until }))
        .toMatchObject({ source: 'crash-report', reportFile: 'A.ips' })
      expect(await findCrashReport({ directory: other, appId: '.*', sinceMs: since, untilMs: until })).toBeNull()
    } finally {
      await rm(other, { recursive: true, force: true })
    }
  })

  test('a missing directory is no evidence, and a hostile process name is dropped', async () => {
    expect(await findCrashReport({ directory: join(directory, 'nope'), appId, sinceMs: since, untilMs: until })).toBeNull()
    const other = await mkdtemp(join(tmpdir(), 'deja-ips-name-'))
    try {
      await mkdir(other, { recursive: true })
      await writeFile(join(other, 'B.ips'), header(appId, 'x\\u0000 outcome=passed'))
      await utimes(join(other, 'B.ips'), new Date('2026-11-23T01:02:00Z'), new Date('2026-11-23T01:02:00Z'))
      const found = await findCrashReport({ directory: other, appId, sinceMs: since, untilMs: until })
      expect(found).toEqual({ source: 'crash-report', reportFile: 'B.ips' })
    } finally {
      await rm(other, { recursive: true, force: true })
    }
  })

  test('the default directory is the macOS diagnostic reports folder', () => {
    expect(defaultCrashReportDirectory('/Users/example')).toBe('/Users/example/Library/Logs/DiagnosticReports')
  })
})

describe('crash report hostile input and attribution', () => {
  const appId = 'dev.deja.ios.checkout'
  const header = `{"app_name":"P"}\n{"procName":"P","bundleInfo":{"CFBundleIdentifier":"${appId}"}}`

  test('a symlinked .ips is never read, even when it points at a real matching report', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'deja-ips-link-'))
    try {
      const target = join(dir, 'target.txt')
      await writeFile(target, header)
      await symlink(target, join(dir, 'Link.ips'))
      const now = Date.now()
      expect(await findCrashReport({ directory: dir, appId, sinceMs: now - 60_000, untilMs: now + 60_000 })).toBeNull()
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  })

  test('an oversized report is matched from its head only, and an app ID past the head does not match', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'deja-ips-big-'))
    try {
      await writeFile(join(dir, 'Big.ips'), `${header}\n${'x'.repeat(5_000_000)}`)
      await writeFile(join(dir, 'Far.ips'), `{"app_name":"P"}\n${' '.repeat(100_000)}"CFBundleIdentifier":"${appId}"`)
      const now = Date.now()
      const found = await findCrashReport({ directory: dir, appId, sinceMs: now - 60_000, untilMs: now + 60_000 })
      expect(found).toEqual({ source: 'crash-report', reportFile: 'Big.ips', process: 'P' })
      await rm(join(dir, 'Big.ips'))
      expect(await findCrashReport({ directory: dir, appId, sinceMs: now - 60_000, untilMs: now + 60_000 })).toBeNull()
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  })

  test('a report over 8 MB is skipped', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'deja-ips-huge-'))
    try {
      await writeFile(join(dir, 'Huge.ips'), `${header}\n${'x'.repeat(9 * 1024 * 1024)}`)
      const now = Date.now()
      expect(await findCrashReport({ directory: dir, appId, sinceMs: now - 60_000, untilMs: now + 60_000 })).toBeNull()
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  })

  test('a report between two attempts belongs to the earlier one, not the next', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'deja-ips-attr-'))
    try {
      const end1 = Date.parse('2026-11-23T01:00:10Z')
      const start2 = Date.parse('2026-11-23T01:00:14Z')
      const end2 = Date.parse('2026-11-23T01:00:30Z')
      await writeFile(join(dir, 'Late.ips'), header)
      const landed = new Date('2026-11-23T01:00:12Z')
      await utimes(join(dir, 'Late.ips'), landed, landed)
      const grace = 10_000
      expect(await findCrashReport({ directory: dir, appId, sinceMs: Date.parse('2026-11-23T01:00:00Z'), untilMs: end1 + grace }))
        .toMatchObject({ reportFile: 'Late.ips' })
      expect(await findCrashReport({ directory: dir, appId, sinceMs: start2, untilMs: end2 + grace })).toBeNull()
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  })
})

describe('MaestroWalkRunner crash report written after the attempt ends', () => {
  test('a timeout or a runner error never waits for a report', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'deja-ips-nowait-'))
    try {
      const runner = new MaestroWalkRunner({
        workspaceRoot: dir, executable: '/unused', device: 'sim', timeoutMs: 1000,
        adapter: iosMaestroAdapter({ device: 'sim' }), appId: 'dev.deja.ios.checkout',
        crashReportDirectory: join(dir, 'empty'), crashReportGraceMs: 5000, crashReportPollMs: 25,
      })
      for (const [timedOut, kind] of [[true, 'timeout'], [false, 'launch']] as const) {
        const begun = Date.now()
        const adapted = await runner.adapt({
          platform: 'ios', device: 'sim', flowPath: join(dir, 'flow.yaml'),
          startedAt: new Date(begun - 1000).toISOString(), endedAt: new Date(begun).toISOString(),
          durationMs: 1000, monotonicDurationMs: 1000, exitCode: 1, signal: null, timedOut,
          passed: false, stdout: '', stderr: '', output: 'Tap on id: a... FAILED\n', errors: [{ kind, message: 'x' }],
        })
        expect(Date.now() - begun).toBeLessThan(1500)
        expect(adapted.failure?.category).toBe(timedOut ? 'timeout' : 'runner')
      }
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  })

  test('a report that lands a moment after a failed attempt gives app_crash, not selector', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'deja-ips-late-'))
    const reports = join(dir, 'reports')
    await mkdir(reports)
    try {
      const runner = new MaestroWalkRunner({
        workspaceRoot: dir, executable: '/unused', device: 'sim', timeoutMs: 1000,
        adapter: iosMaestroAdapter({ device: 'sim' }), appId: 'dev.deja.ios.checkout',
        crashReportDirectory: reports, crashReportGraceMs: 3000, crashReportPollMs: 25,
      })
      const ended = new Date()
      const output = 'Tap on id: checkout_button... FAILED\n\nElement not found: Id matching regex: checkout_button\n'
      setTimeout(() => {
        void writeFile(join(reports, 'Late-2026.ips'),
          '{"app_name":"P"}\n{"procName":"P","bundleInfo":{"CFBundleIdentifier":"dev.deja.ios.checkout"}}')
      }, 200)
      const adapted = await runner.adapt({
        platform: 'ios', device: 'sim', flowPath: join(dir, 'flow.yaml'),
        startedAt: new Date(ended.getTime() - 1000).toISOString(), endedAt: ended.toISOString(),
        durationMs: 1000, monotonicDurationMs: 1000, exitCode: 1, signal: null, timedOut: false,
        passed: false, stdout: output, stderr: '', output, errors: [{ kind: 'execution', message: 'x' }],
      })
      expect(adapted.failure).toMatchObject({
        category: 'app_crash', repairEligible: false, crash: { source: 'crash-report', reportFile: 'Late-2026.ips' },
      })

      // With no report in the grace window the same output stays the repair-eligible selector label.
      const quiet = new MaestroWalkRunner({
        workspaceRoot: dir, executable: '/unused', device: 'sim', timeoutMs: 1000,
        adapter: iosMaestroAdapter({ device: 'sim' }), appId: 'dev.deja.ios.checkout',
        crashReportDirectory: join(dir, 'empty'), crashReportGraceMs: 100, crashReportPollMs: 25,
      })
      const second = await quiet.adapt({
        platform: 'ios', device: 'sim', flowPath: join(dir, 'flow.yaml'),
        startedAt: new Date(Date.now() - 1000).toISOString(), endedAt: new Date().toISOString(),
        durationMs: 1000, monotonicDurationMs: 1000, exitCode: 1, signal: null, timedOut: false,
        passed: false, stdout: output, stderr: '', output, errors: [{ kind: 'execution', message: 'x' }],
      })
      expect(second.failure).toMatchObject({ category: 'selector', repairEligible: true })
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  })
})
