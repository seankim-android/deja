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
// Made-up UDIDs. A real report carries `procPath` inside CoreSimulator/Devices/<UDID>/ with the
// slashes escaped, which is what the fixtures copy.
const MINE = '11111111-AAAA-4BBB-8CCC-222222222222'
const OTHER = '33333333-DDDD-4EEE-8FFF-444444444444'
const procPath = (udid: string) =>
  `\\/Users\\/x\\/Library\\/Developer\\/CoreSimulator\\/Devices\\/${udid}\\/data\\/Containers\\/Bundle\\/Application\\/A\\/App.app\\/App`

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
    `{"app_name":"${proc}","bug_type":"309"}\n{"procName":"${proc}","procPath":"${procPath(MINE)}","bundleInfo":{"CFBundleIdentifier":"${bundle}"},"secret":"patient-name"}`

  beforeAll(async () => {
    directory = await mkdtemp(join(tmpdir(), 'deja-ips-'))
  })
  afterAll(async () => {
    await rm(directory, { recursive: true, force: true })
  })

  test('matches a new report for the app ID written during the run, and keeps only its name and process', async () => {
    await report('DejaFixture-2026-11-23-010203.ips', header(appId), '2026-11-23T01:02:03Z')
    const found = await findCrashReport({ directory, appId, sinceMs: since, untilMs: until, deviceUdid: MINE })
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
      expect(await findCrashReport({ directory: other, appId, sinceMs: since, untilMs: until, deviceUdid: MINE })).toBeNull()
    } finally {
      await rm(other, { recursive: true, force: true })
    }
  })

  test('an app ID is matched exactly, not as a prefix or a pattern', async () => {
    const other = await mkdtemp(join(tmpdir(), 'deja-ips-exact-'))
    try {
      await writeFile(join(other, 'A.ips'), header(`${appId}.extension`))
      await utimes(join(other, 'A.ips'), new Date('2026-11-23T01:02:00Z'), new Date('2026-11-23T01:02:00Z'))
      expect(await findCrashReport({ directory: other, appId, sinceMs: since, untilMs: until, deviceUdid: MINE })).toBeNull()
      expect(await findCrashReport({ directory: other, appId: 'dev.deja.ios.checkout.extension', sinceMs: since, untilMs: until, deviceUdid: MINE }))
        .toMatchObject({ source: 'crash-report', reportFile: 'A.ips' })
      expect(await findCrashReport({ directory: other, appId: '.*', sinceMs: since, untilMs: until, deviceUdid: MINE })).toBeNull()
    } finally {
      await rm(other, { recursive: true, force: true })
    }
  })

  test('a missing directory is no evidence, and a hostile process name is dropped', async () => {
    expect(await findCrashReport({ directory: join(directory, 'nope'), appId, sinceMs: since, untilMs: until, deviceUdid: MINE })).toBeNull()
    const other = await mkdtemp(join(tmpdir(), 'deja-ips-name-'))
    try {
      await mkdir(other, { recursive: true })
      await writeFile(join(other, 'B.ips'), header(appId, 'x\\u0000 outcome=passed'))
      await utimes(join(other, 'B.ips'), new Date('2026-11-23T01:02:00Z'), new Date('2026-11-23T01:02:00Z'))
      const found = await findCrashReport({ directory: other, appId, sinceMs: since, untilMs: until, deviceUdid: MINE })
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
  const header = `{"app_name":"P"}\n{"procName":"P","procPath":"${procPath(MINE)}","bundleInfo":{"CFBundleIdentifier":"${appId}"}}`

  test('a symlinked .ips is never read, even when it points at a real matching report', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'deja-ips-link-'))
    try {
      const target = join(dir, 'target.txt')
      await writeFile(target, header)
      await symlink(target, join(dir, 'Link.ips'))
      const now = Date.now()
      expect(await findCrashReport({ directory: dir, appId, sinceMs: now - 60_000, untilMs: now + 60_000, deviceUdid: MINE })).toBeNull()
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
      const found = await findCrashReport({ directory: dir, appId, sinceMs: now - 60_000, untilMs: now + 60_000, deviceUdid: MINE })
      expect(found).toEqual({ source: 'crash-report', reportFile: 'Big.ips', process: 'P' })
      await rm(join(dir, 'Big.ips'))
      expect(await findCrashReport({ directory: dir, appId, sinceMs: now - 60_000, untilMs: now + 60_000, deviceUdid: MINE })).toBeNull()
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  })

  test('a report over 8 MB is skipped', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'deja-ips-huge-'))
    try {
      await writeFile(join(dir, 'Huge.ips'), `${header}\n${'x'.repeat(9 * 1024 * 1024)}`)
      const now = Date.now()
      expect(await findCrashReport({ directory: dir, appId, sinceMs: now - 60_000, untilMs: now + 60_000, deviceUdid: MINE })).toBeNull()
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
      expect(await findCrashReport({ directory: dir, appId, sinceMs: Date.parse('2026-11-23T01:00:00Z'), untilMs: end1 + grace, deviceUdid: MINE }))
        .toMatchObject({ reportFile: 'Late.ips' })
      expect(await findCrashReport({ directory: dir, appId, sinceMs: start2, untilMs: end2 + grace, deviceUdid: MINE })).toBeNull()
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
        workspaceRoot: dir, executable: '/unused', device: MINE, timeoutMs: 1000,
        adapter: iosMaestroAdapter({ device: MINE }), appId: 'dev.deja.ios.checkout',
        crashReportDirectory: join(dir, 'empty'), crashReportGraceMs: 5000, crashReportPollMs: 25,
      })
      for (const [timedOut, kind] of [[true, 'timeout'], [false, 'launch']] as const) {
        const begun = Date.now()
        const adapted = await runner.adapt({
          platform: 'ios', device: MINE, flowPath: join(dir, 'flow.yaml'),
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
        workspaceRoot: dir, executable: '/unused', device: MINE, timeoutMs: 1000,
        adapter: iosMaestroAdapter({ device: MINE }), appId: 'dev.deja.ios.checkout',
        crashReportDirectory: reports, crashReportGraceMs: 3000, crashReportPollMs: 25,
      })
      const ended = new Date()
      const output = 'Tap on id: checkout_button... FAILED\n\nElement not found: Id matching regex: checkout_button\n'
      setTimeout(() => {
        void writeFile(join(reports, 'Late-2026.ips'),
          `{"app_name":"P"}\n{"procName":"P","procPath":"${procPath(MINE)}","bundleInfo":{"CFBundleIdentifier":"dev.deja.ios.checkout"}}`)
      }, 200)
      const adapted = await runner.adapt({
        platform: 'ios', device: MINE, flowPath: join(dir, 'flow.yaml'),
        startedAt: new Date(ended.getTime() - 1000).toISOString(), endedAt: ended.toISOString(),
        durationMs: 1000, monotonicDurationMs: 1000, exitCode: 1, signal: null, timedOut: false,
        passed: false, stdout: output, stderr: '', output, errors: [{ kind: 'execution', message: 'x' }],
      })
      expect(adapted.failure).toMatchObject({
        category: 'app_crash', repairEligible: false, crash: { source: 'crash-report', reportFile: 'Late-2026.ips' },
      })

      // With no report in the grace window the same output stays the repair-eligible selector label.
      const quiet = new MaestroWalkRunner({
        workspaceRoot: dir, executable: '/unused', device: MINE, timeoutMs: 1000,
        adapter: iosMaestroAdapter({ device: MINE }), appId: 'dev.deja.ios.checkout',
        crashReportDirectory: join(dir, 'empty'), crashReportGraceMs: 100, crashReportPollMs: 25,
      })
      const second = await quiet.adapt({
        platform: 'ios', device: MINE, flowPath: join(dir, 'flow.yaml'),
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

describe('crash report belongs to the run simulator', () => {
  const appId = 'dev.deja.ios.checkout'
  const since = Date.parse('2026-11-23T01:00:00Z')
  const until = Date.parse('2026-11-23T01:05:00Z')
  const when = new Date('2026-11-23T01:02:00Z')
  const body = (path: string | null) =>
    `{"app_name":"P"}\n{"procName":"P",${path === null ? '' : `"procPath":"${path}",`}"bundleInfo":{"CFBundleIdentifier":"${appId}"}}`

  async function find(files: Record<string, string>, deviceUdid: string | undefined) {
    const dir = await mkdtemp(join(tmpdir(), 'deja-ips-udid-'))
    const ignored: string[] = []
    try {
      for (const [name, text] of Object.entries(files)) {
        await writeFile(join(dir, name), text)
        await utimes(join(dir, name), when, when)
      }
      const found = await findCrashReport({
        directory: dir, appId, sinceMs: since, untilMs: until,
        ...(deviceUdid === undefined ? {} : { deviceUdid }),
        onIgnored: (file, reason) => ignored.push(`${reason}: ${file}`),
      })
      return { found, ignored }
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  }

  test('a report from this simulator is accepted', async () => {
    const { found, ignored } = await find({ 'Mine.ips': body(procPath(MINE)) }, MINE)
    expect(found).toEqual({ source: 'crash-report', reportFile: 'Mine.ips', process: 'P' })
    expect(ignored).toEqual([])
  })

  test('a report from another simulator is ignored and the reason is recorded', async () => {
    const { found, ignored } = await find({ 'Other.ips': body(procPath(OTHER)) }, MINE)
    expect(found).toBeNull()
    expect(ignored).toEqual(['other simulator: Other.ips'])
  })

  test('another simulator newer than this one does not hide this one', async () => {
    const { found } = await find({ 'Mine.ips': body(procPath(MINE)), 'Other.ips': body(procPath(OTHER)) }, MINE)
    expect(found).toMatchObject({ reportFile: 'Mine.ips' })
  })

  test('a report with no simulator is ignored: a host path, no procPath, or a UDID in another field', async () => {
    const { found, ignored } = await find({
      'Host.ips': body('\\/Applications\\/Xcode.app\\/Contents\\/usr\\/bin\\/swiftpm-testing-helper'),
      'None.ips': body(null),
      'Field.ips': `{"app_name":"P"}\n{"procName":"P","note":"${procPath(MINE)}","bundleInfo":{"CFBundleIdentifier":"${appId}"}}`,
    }, MINE)
    expect(found).toBeNull()
    expect(ignored.sort()).toEqual([
      'no simulator in report: Field.ips', 'no simulator in report: Host.ips', 'no simulator in report: None.ips',
    ])
  })

  test('the UDID is compared case-insensitively and unescaped slashes work', async () => {
    const plain = '/Users/x/Library/Developer/CoreSimulator/Devices/' + MINE.toLowerCase() + '/data/App'
    expect((await find({ 'Lower.ips': body(procPath(MINE.toLowerCase())) }, MINE)).found).toMatchObject({ reportFile: 'Lower.ips' })
    expect((await find({ 'Upper.ips': body(procPath(MINE.toUpperCase())) }, MINE.toLowerCase())).found).toMatchObject({ reportFile: 'Upper.ips' })
    expect((await find({ 'Plain.ips': body(plain) }, MINE)).found).toMatchObject({ reportFile: 'Plain.ips' })
  })

  test('an unknown run UDID accepts nothing, even a report that names a simulator', async () => {
    for (const unknown of [undefined, '', 'booted', 'iPhone 17', 'sim']) {
      const { found, ignored } = await find({ 'Any.ips': body(procPath(MINE)) }, unknown)
      expect(found).toBeNull()
      expect(ignored).toEqual(['run simulator unknown: Any.ips'])
    }
  })

  test('a procPath beyond the 32 KB head is not seen, so the report is ignored', async () => {
    const far = `{"app_name":"P"}\n{"procName":"P","bundleInfo":{"CFBundleIdentifier":"${appId}"},"pad":"${'x'.repeat(40_000)}","procPath":"${procPath(MINE)}"}`
    const { found, ignored } = await find({ 'Far.ips': far }, MINE)
    expect(found).toBeNull()
    expect(ignored).toEqual(['no simulator in report: Far.ips'])
  })

  test('a crash report from another simulator never labels the run app_crash', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'deja-ips-run-'))
    try {
      await writeFile(join(dir, 'Other.ips'), body(procPath(OTHER)))
      const runner = new MaestroWalkRunner({
        workspaceRoot: dir, executable: '/unused', device: MINE, timeoutMs: 1000,
        adapter: iosMaestroAdapter({ device: MINE }), appId,
        crashReportDirectory: dir, crashReportGraceMs: 0, crashReportPollMs: 25,
      })
      const output = 'Tap on id: checkout_button... FAILED\n\nElement not found: Id matching regex: checkout_button\n'
      const adapted = await runner.adapt({
        platform: 'ios', device: MINE, flowPath: join(dir, 'flow.yaml'),
        startedAt: new Date(Date.now() - 1000).toISOString(), endedAt: new Date().toISOString(),
        durationMs: 1000, monotonicDurationMs: 1000, exitCode: 1, signal: null, timedOut: false,
        passed: false, stdout: output, stderr: '', output, errors: [{ kind: 'execution', message: 'x' }],
      })
      expect(adapted.failure).toMatchObject({ category: 'selector', repairEligible: true })
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  })
})
