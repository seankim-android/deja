import { describe, expect, test } from 'bun:test'
import { lstat, mkdir, mkdtemp, readFile, readdir, realpath, rename, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { spawn } from 'node:child_process'
import { join, resolve } from 'node:path'
import { iosMaestroAdapter } from '@deja-run/maestro'
import { LocalWalkStore } from '@deja-run/store-local'
import { runCli } from '../src/main.ts'
import { atomicWrite, parseGoals, removeChecked, renderGateTable, type GateEvidence } from '../src/gate.ts'

const FLOW = 'appId: dev.deja.fixture\n---\n- launchApp\n- assertVisible:\n    id: "screen.today"\n'
const T1 = '2026-11-22T00:00:00.000Z'
const T2 = '2026-11-22T00:01:00.000Z'
const SECRET = 'hunter2-fixture'
const adapter = iosMaestroAdapter({ device: 'DFC4D233-498B-4B33-B713-E59B239FA86A' })

async function workspace(config: Record<string, unknown> = {}) {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-gate-cli-')))
  const root = join(temporary, 'workspace')
  await mkdir(join(root, 'Fixture.app'), { recursive: true })
  await mkdir(join(root, '.maestro/flows'), { recursive: true })
  await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-one')
  await writeFile(join(root, '.maestro/flows/today.yaml'), FLOW)
  await writeFile(join(root, '.maestro/flows/pay.yaml'), FLOW)
  await writeFile(join(root, 'deja.config.json'), JSON.stringify({
    workspaceRoot: '.', appId: 'dev.deja.fixture', platform: 'ios', adapter: 'maestro-ios/v1',
    device: 'DFC4D233-498B-4B33-B713-E59B239FA86A', buildPath: 'Fixture.app',
    fixtureCredentials: [{ label: 'Fixture password', value: SECRET }],
    ...config,
  }))
  const fingerprint = () => adapter.buildFingerprint(root, 'Fixture.app')
  const store = (manifest = false) => new LocalWalkStore({
    workspaceRoot: root, ...(manifest ? { manifest: { path: 'deja.walks.json' } } : {}),
  })
  const identity = async (goal: string) => ({
    goal, appId: 'dev.deja.fixture', platform: 'ios', flowSchemaVersion: 'maestro/v1',
    buildFingerprint: await fingerprint(), adapter: 'maestro-ios/v1',
  })
  const remember = async (goal: string, at = T1, manifest = false, path = '.maestro/flows/today.yaml') => {
    const walks = store(manifest)
    try {
      return await walks.persistVerifiedCandidate({
        identity: await identity(goal), candidatePath: manifest ? path : path,
        validation: { passed: true, at }, origin: 'remember',
      })
    } finally { walks.close() }
  }
  const fail = async (goal: string, at: string, manifest = false) => {
    const walks = store(manifest)
    try { await walks.recordValidation(await identity(goal), { passed: false, at }) } finally { walks.close() }
  }
  const goals = (text: string) => writeFile(join(root, 'goals.txt'), text)
  const gate = async (...args: string[]) => {
    const stdout: string[] = []
    const stderr: string[] = []
    const exit = await runCli(
      ['gate', '--config', join(root, 'deja.config.json'), ...args],
      { stdout: text => stdout.push(text), stderr: text => stderr.push(text) },
    )
    return { exit, stdout: stdout.join(''), stderr: stderr.join('') }
  }
  const evidence = async () => JSON.parse(await readFile(join(root, '.deja/gate/evidence.json'), 'utf8'))
  return { temporary, root, remember, fail, goals, gate, evidence, fingerprint, store }
}

describe('deja gate', () => {
  test('opens only when every goal passed on this build, and writes JSON plus a text table', async () => {
    const ws = await workspace()
    try {
      await ws.goals('# core flows\nToday\n\nPay\n')
      await ws.remember('Today')
      await ws.remember('Pay')
      const result = await ws.gate('--goals', join(ws.root, 'goals.txt'), '--label', 'abc1234 simulator')
      expect(result.exit).toBe(0)
      expect(result.stdout).toContain('Release gate: OPEN (2 of 2 goals passed)')
      expect(result.stdout).toContain('label: abc1234 simulator')
      const evidence = await ws.evidence()
      expect(evidence).toMatchObject({
        schemaVersion: 1, command: 'gate', verdict: 'passed', passed: true, label: 'abc1234 simulator',
        buildFingerprint: await ws.fingerprint(), manifest: null, build: 'Fixture.app',
      })
      expect(evidence.goals).toMatchObject([
        { goal: 'Today', outcome: 'passed', validatedAt: T1, reportPath: null },
        { goal: 'Pay', outcome: 'passed', validatedAt: T1 },
      ])
      expect(evidence.goals[0].flowSha256).toMatch(/^[0-9a-f]{64}$/)
      expect(await readFile(join(ws.root, '.deja/gate/evidence.txt'), 'utf8')).toContain('OPEN')
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'), '--json')).stdout).toContain('"verdict": "passed"')
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('closes with exit 10 for failed, not-run and a missing goal, and a new build has no passes', async () => {
    const ws = await workspace()
    try {
      await ws.goals('Today\nPay\nMissing goal\n')
      await ws.remember('Today')
      await ws.remember('Pay')
      await ws.fail('Pay', T2)
      const closed = await ws.gate('--goals', join(ws.root, 'goals.txt'))
      expect(closed.exit).toBe(10)
      expect((await ws.evidence()).goals.map((goal: { outcome: string }) => goal.outcome)).toEqual(['passed', 'failed', 'not-run'])
      expect(closed.stdout).toContain('CLOSED (1 of 3 goals passed)')

      await ws.goals('Today\n')
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'))).exit).toBe(0)
      // The next build: the old pass does not count.
      await writeFile(join(ws.root, 'Fixture.app/Fixture'), 'binary-two')
      const next = await ws.gate('--goals', join(ws.root, 'goals.txt'))
      expect(next.exit).toBe(10)
      expect((await ws.evidence()).goals[0]).toMatchObject({ outcome: 'not-run', validatedAt: null, flowSha256: null })
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('--build fingerprints the named build, not the configured one', async () => {
    const ws = await workspace()
    try {
      await mkdir(join(ws.root, 'Other.app'), { recursive: true })
      await writeFile(join(ws.root, 'Other.app/Fixture'), 'binary-one')
      await ws.goals('Today\n')
      await ws.remember('Today')
      // Same bytes under another name: same fingerprint, because the hash covers paths inside the tree only.
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'), '--build', 'Other.app')).exit).toBe(0)
      await writeFile(join(ws.root, 'Other.app/Fixture'), 'binary-three')
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'), '--build', 'Other.app')).exit).toBe(10)
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('a failure that a run report names as flaky reads flaky, and the report is linked', async () => {
    const ws = await workspace()
    try {
      await ws.goals('Today\nPay\n')
      await ws.remember('Today')
      await ws.remember('Pay')
      await ws.fail('Pay', T2)
      const fingerprint = await ws.fingerprint()
      const report = (outcome: string, goalKey: string, endedAt: string, shape: 'attempts' | 'execution') => JSON.stringify({
        schemaVersion: 1, command: 'run', goalKey, outcome, passed: false, cacheStatus: 'hit',
        identity: { buildFingerprint: fingerprint }, artifacts: [], diffs: [],
        ...(shape === 'attempts' ? { attempts: [{ attempt: 1, endedAt }, { attempt: 2, endedAt: '2026-11-22T00:09:00.000Z' }] } : { execution: { endedAt } }),
      })
      await mkdir(join(ws.root, '.deja/reports'), { recursive: true })
      await writeFile(join(ws.root, '.deja/reports/run-1.json'), report('flaky', 'pay', T2, 'attempts'))
      await writeFile(join(ws.root, '.deja/reports/run-2.json'), report('passed', 'today', T1, 'execution'))
      await writeFile(join(ws.root, '.deja/reports/run-3.json'), '{not json')
      const result = await ws.gate('--goals', join(ws.root, 'goals.txt'))
      expect(result.exit).toBe(10)
      expect((await ws.evidence()).goals).toMatchObject([
        { goal: 'Today', outcome: 'passed', reportPath: '.deja/reports/run-2.json' },
        { goal: 'Pay', outcome: 'flaky', reportPath: '.deja/reports/run-1.json' },
      ])
      // A flaky report for another build or another time never turns a failure into anything but closed.
      await writeFile(join(ws.root, '.deja/reports/run-1.json'), report('flaky', 'pay', '2026-11-22T00:05:00.000Z', 'attempts'))
      await ws.gate('--goals', join(ws.root, 'goals.txt'))
      expect((await ws.evidence()).goals[1].outcome).toBe('failed')
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('manifest mode: the manifest is the goals file, and a changed flow closes the gate with flow-changed', async () => {
    const ws = await workspace({ manifest: 'deja.walks.json' })
    try {
      await ws.remember('Today', T1, true)
      await ws.remember('Pay', T1, true, '.maestro/flows/pay.yaml')
      const open = await ws.gate('--goals', join(ws.root, 'deja.walks.json'))
      expect(open.exit).toBe(0)
      expect((await ws.evidence())).toMatchObject({ manifest: 'deja.walks.json', verdict: 'passed' })
      expect((await ws.evidence()).goals[0]).toMatchObject({ goal: 'Pay' })
      expect((await ws.evidence()).goals[0].closureSha256).toMatch(/^[0-9a-f]{64}$/)

      await writeFile(join(ws.root, '.maestro/flows/pay.yaml'), `${FLOW}- assertVisible: Receipt\n`)
      const changed = await ws.gate('--goals', join(ws.root, 'deja.walks.json'))
      expect(changed.exit).toBe(10)
      expect((await ws.evidence()).goals.map((goal: { outcome: string }) => goal.outcome)).toEqual(['flow-changed', 'passed'])
      expect(changed.stdout).toContain('flow-changed')
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('a corrupt store exits 11, writes error evidence and replaces an older passing file', async () => {
    const ws = await workspace()
    try {
      await ws.goals('Today\n')
      await ws.remember('Today')
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'))).exit).toBe(0)
      expect((await ws.evidence()).verdict).toBe('passed')
      await writeFile(join(ws.root, '.deja/memory/walks.json'), '{not json\n')
      const result = await ws.gate('--goals', join(ws.root, 'goals.txt'))
      expect(result.exit).toBe(11)
      expect(result.stdout).toContain('Release gate: ERROR')
      expect(await ws.evidence()).toMatchObject({ verdict: 'error', passed: false })
      expect((await ws.evidence()).error).toMatch(/walks\.json/)
      expect((await ws.evidence()).goals[0].outcome).toBe('not-run')
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('a build that cannot be fingerprinted exits 11 and never passes', async () => {
    const ws = await workspace()
    try {
      await ws.goals('Today\n')
      await ws.remember('Today')
      const result = await ws.gate('--goals', join(ws.root, 'goals.txt'), '--build', 'Missing.app')
      expect(result.exit).toBe(11)
      expect(await ws.evidence()).toMatchObject({ verdict: 'error', buildFingerprint: null })
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('usage errors exit 2 and delete an older evidence file', async () => {
    const ws = await workspace()
    try {
      await ws.goals('Today\n')
      await ws.remember('Today')
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'))).exit).toBe(0)
      expect((await ws.gate('--goals', join(ws.root, 'nope.txt'))).exit).toBe(2)
      await expect(readFile(join(ws.root, '.deja/gate/evidence.json'), 'utf8')).rejects.toThrow()
      await ws.goals('# nothing\n\n')
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'))).exit).toBe(2)
      expect((await ws.gate()).exit).toBe(2)
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'), '--verify')).exit).toBe(2)
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'), 'extra')).exit).toBe(2)
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'), '--allow-failed')).exit).toBe(2)
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('it only reads: no store change, no writer lock file, and it runs beside a held writer lock', async () => {
    const ws = await workspace()
    try {
      await ws.goals('Today\n')
      await ws.remember('Today')
      const files = async () => (await readdir(join(ws.root, '.deja/memory'))).sort()
      const before = await readFile(join(ws.root, '.deja/memory/walks.json'))
      const listed = await files()
      const writer = ws.store()
      writer.acquireWriter()
      try {
        expect((await ws.gate('--goals', join(ws.root, 'goals.txt'))).exit).toBe(0)
      } finally { writer.close() }
      expect((await readFile(join(ws.root, '.deja/memory/walks.json'))).equals(before)).toBe(true)
      expect(await files()).toEqual(listed)
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('the evidence goes through the run-report redaction', async () => {
    const ws = await workspace()
    try {
      await ws.goals('Today\n')
      await ws.remember('Today')
      const result = await ws.gate('--goals', join(ws.root, 'goals.txt'), '--label', `sha ${SECRET} ${ws.root}`)
      expect(result.exit).toBe(0)
      for (const text of [
        result.stdout, await readFile(join(ws.root, '.deja/gate/evidence.json'), 'utf8'),
        await readFile(join(ws.root, '.deja/gate/evidence.txt'), 'utf8'),
      ]) {
        expect(text).not.toContain(SECRET)
        expect(text).not.toContain(ws.root)
      }
      expect((await ws.evidence()).label).toBe('sha <redacted-fixture-value> <workspace>')
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('the gate options are valid only with gate', async () => {
    const ws = await workspace()
    try {
      const stderr: string[] = []
      const exit = await runCli(['list', '--goals', 'x', '--config', join(ws.root, 'deja.config.json')],
        { stdout: () => {}, stderr: text => stderr.push(text) })
      expect(exit).toBe(2)
      expect(stderr.join('')).toContain('valid only with gate')
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })
})

const cli = resolve(import.meta.dir, '../deja.ts')
const fakeMaestro = resolve(import.meta.dir, '../../../packages/maestro/test/fixtures/fake-maestro.cjs')

function execute(root: string, args: string[], env: Record<string, string> = {}) {
  return new Promise<{ exitCode: number | null }>(done => {
    const child = spawn(process.execPath, [cli, ...args, '--config', 'deja.config.json'], {
      cwd: root, env: { ...process.env, DEJA_FAKE_MAESTRO_MODE: '', ...env }, stdio: 'ignore',
    })
    child.once('close', code => done({ exitCode: code }))
  })
}

describe('deja gate on a real flaky run (DEJA-7)', () => {
  test('a run that failed, then passed on a rerun reads flaky', async () => {
    const ws = await workspace({ maestro: { executable: fakeMaestro, timeoutMs: 30_000 } })
    try {
      await ws.goals('Today\n')
      expect((await execute(ws.root, ['remember', 'today', '--flow', '.maestro/flows/today.yaml'])).exitCode).toBe(0)
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'))).exit).toBe(0)
      const flaky = await execute(ws.root, ['run', 'today', '--rerun-failed'], {
        DEJA_FAKE_MAESTRO_SCRIPT: 'crash,pass', DEJA_FAKE_MAESTRO_COUNTER: join(ws.temporary, 'counter'),
      })
      expect(flaky.exitCode).toBe(7)
      const closed = await ws.gate('--goals', join(ws.root, 'goals.txt'))
      expect(closed.exit).toBe(10)
      const [goal] = (await ws.evidence()).goals
      expect(goal).toMatchObject({ outcome: 'flaky', detail: expect.stringContaining('rerun') })
      expect(goal.reportPath).toMatch(/^\.deja\/reports\/run-.*\.json$/)
      expect(JSON.parse(await readFile(join(ws.root, goal.reportPath), 'utf8')).outcome).toBe('flaky')
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })
})

describe('deja gate --evidence safety (B1)', () => {
  async function opened() {
    const ws = await workspace()
    await ws.goals('Today\n')
    await ws.remember('Today')
    return ws
  }
  const refused = async (ws: Awaited<ReturnType<typeof opened>>, target: string) => {
    const result = await ws.gate('--goals', join(ws.root, 'goals.txt'), '--evidence', target)
    expect(result.exit).toBe(2)
    expect(result.stderr).toContain('is refused')
  }

  test('the goals file, the store file, the manifest and a flow file are never deleted or overwritten', async () => {
    const ws = await opened()
    try {
      await writeFile(join(ws.root, 'deja.walks.json'), '{"keep":"manifest"}\n')
      const keep = async (path: string) => readFile(join(ws.root, path), 'utf8')
      const targets = ['goals.txt', '.deja/memory/walks.json', 'deja.walks.json', '.maestro/flows/today.yaml']
      const before = await Promise.all(targets.map(keep))
      for (const target of targets) await refused(ws, join(ws.root, target))
      expect(await Promise.all(targets.map(keep))).toEqual(before)
      // The store still works.
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'))).exit).toBe(0)
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('a foreign file, a foreign .txt sibling, a file inside .deja/ and a symlink are refused and left alone', async () => {
    const ws = await opened()
    try {
      await writeFile(join(ws.root, 'notes.json'), 'important\n')
      await refused(ws, join(ws.root, 'notes.json'))
      expect(await readFile(join(ws.root, 'notes.json'), 'utf8')).toBe('important\n')

      await writeFile(join(ws.root, 'out.txt'), 'my notes\n')
      await refused(ws, join(ws.root, 'out.json'))
      expect(await readFile(join(ws.root, 'out.txt'), 'utf8')).toBe('my notes\n')
      await expect(readFile(join(ws.root, 'out.json'), 'utf8')).rejects.toThrow()

      await refused(ws, join(ws.root, '.deja/reports/new.json'))
      await expect(readFile(join(ws.root, '.deja/reports/new.json'), 'utf8')).rejects.toThrow()

      await symlink(join(ws.root, 'notes.json'), join(ws.root, 'link.json'))
      await refused(ws, join(ws.root, 'link.json'))
      expect(await readFile(join(ws.root, 'notes.json'), 'utf8')).toBe('important\n')

      await mkdir(join(ws.root, 'dir.json'))
      await refused(ws, join(ws.root, 'dir.json'))
      // A symlinked folder that leads to .deja/memory resolves first, then is refused.
      await symlink(join(ws.root, '.deja/memory'), join(ws.root, 'shortcut'))
      await refused(ws, join(ws.root, 'shortcut/walks.json'))
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'))).exit).toBe(0)
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('a previous gate evidence file, and a new path, are replaced and written', async () => {
    const ws = await opened()
    try {
      const target = join(ws.root, 'out/gate.json')
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'), '--evidence', target)).exit).toBe(0)
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'), '--evidence', target)).exit).toBe(0)
      expect(JSON.parse(await readFile(target, 'utf8'))).toMatchObject({ kind: 'deja-gate-evidence', command: 'gate' })
      expect(await readFile(join(ws.root, 'out/gate.txt'), 'utf8')).toContain('Release gate: OPEN')
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('a symlink planted at the temporary name is never followed (F1)', async () => {
    const ws = await opened()
    try {
      await writeFile(join(ws.root, 'victim.txt'), 'keep me\n')
      await mkdir(join(ws.root, 'o'))
      // The old predictable name, for any pid, and the exact name a write is about to use.
      for (const pid of [process.pid, process.pid + 1, 1, 2, 3]) {
        await symlink(join(ws.root, 'victim.txt'), join(ws.root, `o/e.json.${pid}.tmp`))
      }
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'), '--evidence', join(ws.root, 'o/e.json'))).exit).toBe(0)
      expect(await readFile(join(ws.root, 'victim.txt'), 'utf8')).toBe('keep me\n')

      await symlink(join(ws.root, 'victim.txt'), join(ws.root, `o/f.json.${process.pid}.fixed.tmp`))
      await expect(atomicWrite(join(ws.root, 'o/f.json'), 'evidence', 'fixed')).rejects.toThrow()
      expect(await readFile(join(ws.root, 'victim.txt'), 'utf8')).toBe('keep me\n')
      // The planted link is left as it was; nothing was written to the destination.
      expect((await lstat(join(ws.root, `o/f.json.${process.pid}.fixed.tmp`))).isSymbolicLink()).toBe(true)
      await expect(readFile(join(ws.root, 'o/f.json'), 'utf8')).rejects.toThrow()
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('an old .txt is replaced only when it is the exact table of the evidence beside it (F3)', async () => {
    const ws = await opened()
    try {
      const target = join(ws.root, 'out/gate.json')
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'), '--evidence', target)).exit).toBe(0)
      const table = await readFile(join(ws.root, 'out/gate.txt'), 'utf8')
      // Starts like a table, has a Build line and the header row, but is not the table.
      await writeFile(join(ws.root, 'out/gate.txt'), `${table}my notes\n`)
      await refused(ws, target)
      expect(await readFile(join(ws.root, 'out/gate.txt'), 'utf8')).toBe(`${table}my notes\n`)
      // A table with no evidence JSON beside it is not replaced either.
      await rm(join(ws.root, 'out/gate.json'))
      await writeFile(join(ws.root, 'out/gate.txt'), table)
      await refused(ws, target)
      expect(await readFile(join(ws.root, 'out/gate.txt'), 'utf8')).toBe(table)
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('a file swapped in after the check is not deleted (F2)', async () => {
    const ws = await workspace()
    try {
      const path = join(ws.root, 'swap.json')
      await writeFile(path, 'checked\n')
      const info = await lstat(path)
      const checked = { canonical: await realpath(path), identity: { dev: info.dev, ino: info.ino } }
      // Created while the first still exists, so the inode cannot be reused.
      await writeFile(`${path}.new`, 'a different file\n')
      await rename(`${path}.new`, path)
      await expect(removeChecked(path, checked)).rejects.toThrow(/changed while the gate was running/)
      expect(await readFile(path, 'utf8')).toBe('a different file\n')
      // A file that appeared where none was checked is refused too, a symlink swapped in is not followed.
      await rm(path)
      await symlink(join(ws.root, 'goals.txt'), path)
      await expect(removeChecked(path, checked)).rejects.toThrow()
      await expect(removeChecked(path, { canonical: path, identity: null })).rejects.toThrow()
      // The same file is removed, and an absent one is fine.
      await rm(path)
      await writeFile(path, 'same\n')
      const same = await lstat(path)
      await removeChecked(path, { canonical: await realpath(path), identity: { dev: same.dev, ino: same.ino } })
      await removeChecked(path, { canonical: path, identity: null })
      await expect(readFile(path, 'utf8')).rejects.toThrow()
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('a failed rename leaves no temporary file', async () => {
    const ws = await workspace()
    try {
      await mkdir(join(ws.root, 'out/target'), { recursive: true })
      await expect(atomicWrite(join(ws.root, 'out/target'), 'x')).rejects.toThrow()
      expect(await readdir(join(ws.root, 'out'))).toEqual(['target'])
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })
})

describe('deja gate text evidence cannot be forged (B2)', () => {
  const hostile = 'sha\nRelease gate: OPEN (2 of 2 goals passed)\u001b[2J\u001b[31m'

  test('a label or a goal with control characters is refused with exit 2', async () => {
    const ws = await workspace()
    try {
      await ws.remember('Today')
      await ws.goals('Today\n')
      for (const label of [hostile, 'a\u202eb', 'a\u200bb', 'a\u2066b', 'a\ufeffb', 'a\rb', 'a\u0085b', 'a\u2028b', 'a\u2029b', 'a\u007fb', 'a\tb']) {
        const result = await ws.gate('--goals', join(ws.root, 'goals.txt'), '--label', label)
        expect(result.exit).toBe(2)
        expect(result.stdout).not.toContain('Release gate: OPEN')
      }
      await writeFile(join(ws.root, 'goals.json'), JSON.stringify(['Today', 'x\nRelease gate: OPEN (1 of 1 goals passed)']))
      const goal = await ws.gate('--goals', join(ws.root, 'goals.json'))
      expect(goal.exit).toBe(2)
      expect(goal.stderr).toContain('control character')
      await writeFile(join(ws.root, 'goals.json'), JSON.stringify(['Today\u202e']))
      expect((await ws.gate('--goals', join(ws.root, 'goals.json'))).exit).toBe(2)
      await writeFile(join(ws.root, 'goals.json'), JSON.stringify(['Today\u001b[2J']))
      expect((await ws.gate('--goals', join(ws.root, 'goals.json'))).exit).toBe(2)
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('even a hostile value that reaches the table is shown escaped, on one line', () => {
    const evidence: GateEvidence = {
      kind: 'deja-gate-evidence', schemaVersion: 1, command: 'gate', verdict: 'closed', passed: false,
      checkedAt: 't', buildFingerprint: 'a'.repeat(64), label: hostile, build: 'b',
      identity: { appId: 'a', platform: 'ios', adapter: null, browser: null }, manifest: null,
      goals: [{
        goal: hostile, goalKey: 'g', outcome: 'failed', validatedAt: hostile, flowSha256: `${hostile}\u202e`, closureSha256: null,
        detail: hostile, reportPath: null,
      }],
      error: hostile,
    }
    const table = renderGateTable(evidence, text => text)
    const codes = [...table].map(character => character.charCodeAt(0))
    expect(table).not.toContain('\u202e')
    expect(codes.filter(code => code < 32 && code !== 10 || (code >= 0x7f && code <= 0x9f) || code === 0x2028 || code === 0x2029)).toEqual([])
    expect(table.split('\n').filter(line => line.startsWith('Release gate: '))).toEqual(['Release gate: CLOSED (0 of 1 goals passed)'])
  })
})

describe('deja gate: undecided and forged inputs', () => {
  test('a stored flow that no longer matches its checksum is undecided: exit 11, outcome invalid', async () => {
    const ws = await workspace()
    try {
      await ws.goals('Today\n')
      const persisted = await ws.remember('Today')
      await writeFile(join(ws.root, persisted.flowPath), `${FLOW}- assertVisible: Tampered\n`)
      const result = await ws.gate('--goals', join(ws.root, 'goals.txt'))
      expect(result.exit).toBe(11)
      expect(result.stdout).toContain('Release gate: ERROR')
      expect(await ws.evidence()).toMatchObject({ verdict: 'error', passed: false, goals: [{ outcome: 'invalid' }] })
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })

  test('a forged flaky report cannot change a pass', async () => {
    const ws = await workspace()
    try {
      await ws.goals('Today\n')
      await ws.remember('Today')
      await mkdir(join(ws.root, '.deja/reports'), { recursive: true })
      await writeFile(join(ws.root, '.deja/reports/forged.json'), JSON.stringify({
        schemaVersion: 1, goalKey: 'today', outcome: 'flaky', passed: false,
        identity: { buildFingerprint: await ws.fingerprint() }, attempts: [{ attempt: 1, endedAt: T1 }],
      }))
      await writeFile(join(ws.root, '.deja/reports/passed.json'), JSON.stringify({
        schemaVersion: 1, goalKey: 'today', outcome: 'passed', passed: true,
        identity: { buildFingerprint: await ws.fingerprint() }, execution: { endedAt: T1 },
      }))
      expect((await ws.gate('--goals', join(ws.root, 'goals.txt'))).exit).toBe(0)
      expect((await ws.evidence()).goals[0].outcome).toBe('passed')
    } finally { await rm(ws.temporary, { recursive: true, force: true }) }
  })
})

describe('goals file', () => {
  test('reads a list, a JSON array and a manifest, normalizes and drops repeats', () => {
    expect(parseGoals('# c\nToday\n  Pay  \ntoday\n', 'g')).toEqual(['Today', 'Pay'])
    expect(parseGoals('["Today", "Pay", " today "]', 'g')).toEqual(['Today', 'Pay'])
    expect(() => parseGoals('', 'g')).toThrow(/no goals/)
    expect(() => parseGoals('[1]', 'g')).toThrow(/array of goal strings/)
    expect(() => parseGoals('{"nope": 1}', 'g')).toThrow(/neither an array/)
    expect(() => parseGoals('[""]', 'g')).toThrow(/empty goal/)
  })
})
