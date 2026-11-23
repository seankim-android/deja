import { describe, expect, test } from 'bun:test'
import { spawn } from 'node:child_process'
import { spawnSync } from 'node:child_process'
import { mkdir, symlink, mkdtemp, readdir, readFile, realpath, rm, stat, utimes, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { serializeWalkManifest, type WalkManifest } from '@deja-qa/core'
import { assertionsSha256, collectFlowClosure, ConsumerFlows } from '@deja-qa/store-local'
import { runCli } from '../src/main.ts'

const cli = resolve(import.meta.dir, '../deja.ts')
const fakeMaestro = resolve(import.meta.dir, '../../../packages/maestro/test/fixtures/fake-maestro.cjs')

async function remember(root: string, goal: string, flow: string) {
  const child = spawn(process.execPath, [cli, 'remember', goal, '--flow', flow, '--config', 'deja.config.json'], {
    cwd: root, env: { ...process.env, DEJA_FAKE_MAESTRO_MODE: '' }, stdio: ['ignore', 'pipe', 'pipe'],
  })
  child.stdout.resume()
  child.stderr.resume()
  return new Promise<number | null>(done => child.once('close', done))
}

async function check(root: string, ...args: string[]) {
  let stdout = ''
  let stderr = ''
  const exitCode = await runCli(['manifest', 'check', '--config', join(root, 'deja.config.json'), ...args], {
    stdout: text => { stdout += text },
    stderr: text => { stderr += text },
  })
  return { exitCode, stdout, stderr }
}

const TODAY = `appId: dev.deja.fixture
---
- launchApp:
    clearState: true
- assertVisible:
    id: "screen.today"
- addMedia:
    - ../media/pic.png
- runFlow:
    file: ../subflows/type.yaml
- assertVisible:
    id: "today.saved"
`
const TYPE = '- inputText: Popcorn\n- assertVisible:\n    id: "typed"\n'

/** A workspace with one remembered walk ("today"), made by the real `remember`, then `.deja/` removed. */
async function remembered(extra: Record<string, unknown> = {}) {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-manifest-check-')))
  const root = join(temporary, 'workspace')
  for (const folder of ['Fixture.app', '.maestro/flows', '.maestro/subflows', '.maestro/media']) {
    await mkdir(join(root, folder), { recursive: true })
  }
  await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-one')
  await writeFile(join(root, '.maestro/flows/today.yaml'), TODAY)
  await writeFile(join(root, '.maestro/subflows/type.yaml'), TYPE)
  await writeFile(join(root, '.maestro/media/pic.png'), Buffer.from([137, 80, 78, 71, 1]))
  await writeFile(join(root, 'deja.config.json'), JSON.stringify({
    workspaceRoot: '.', appId: 'dev.deja.fixture', platform: 'ios', adapter: 'maestro-ios/v1',
    device: 'DFC4D233-498B-4B33-B713-E59B239FA86A', buildPath: 'Fixture.app',
    fixtureCredentials: [{ label: 'Fixture password', value: 'hunter2-fixture' }],
    maestro: { executable: fakeMaestro, timeoutMs: 30_000 }, manifest: 'deja.walks.json', ...extra,
  }))
  expect(await remember(root, 'today', '.maestro/flows/today.yaml')).toBe(0)
  await rm(join(root, '.deja'), { recursive: true, force: true })
  return { temporary, root }
}

async function mtimes(root: string) {
  const found: Record<string, number> = {}
  const walk = async (folder: string) => {
    for (const entry of await readdir(join(root, folder), { withFileTypes: true })) {
      const path = folder === '' ? entry.name : `${folder}/${entry.name}`
      if (entry.isDirectory()) await walk(path)
      else found[path] = (await stat(join(root, path))).mtimeMs
    }
  }
  await walk('')
  return found
}

describe('deja manifest check', () => {
  test('agrees with the hashes the store wrote, is clean, and is read-only', async () => {
    const { temporary, root } = await remembered()
    try {
      const old = new Date(Date.now() - 3_600_000)
      for (const path of ['deja.walks.json', '.maestro/flows/today.yaml', 'deja.config.json']) await utimes(join(root, path), old, old)
      const before = await mtimes(root)
      const result = await check(root)
      expect(result.exitCode).toBe(0)
      expect(result.stdout).toContain('1 of 1 entries match')
      expect(result.stderr).toBe('')
      expect(await mtimes(root)).toEqual(before)
      // No store, lock or report directory was created.
      await expect(stat(join(root, '.deja'))).rejects.toBeDefined()

      // The same code computes both: the manifest the store wrote holds what the check computes.
      const walk = JSON.parse(await readFile(join(root, 'deja.walks.json'), 'utf8')).walks[0]
      const closure = await collectFlowClosure(root, join(root, '.maestro/flows/today.yaml'))
      expect(closure.sha256).toBe(walk.closureSha256)
      expect(assertionsSha256(closure)).toBe(walk.assertionsSha256)
      const json = JSON.parse((await check(root, '--json')).stdout)
      expect(json).toMatchObject({ ok: true, entries: [{
        status: 'ok', manifestClosureSha256: walk.closureSha256, currentClosureSha256: walk.closureSha256,
        currentAssertionsSha256: walk.assertionsSha256,
      }] })
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test.each([
    ['main flow', '.maestro/flows/today.yaml', `${TODAY}# edited\n`],
    ['subflow', '.maestro/subflows/type.yaml', `${TYPE}# edited\n`],
    ['addMedia file', '.maestro/media/pic.png', Buffer.from([137, 80, 78, 71, 2])],
  ])('a changed %s is flow-changed with the verify command', async (_name, path, contents) => {
    const { temporary, root } = await remembered()
    try {
      await writeFile(join(root, path), contents)
      const result = await check(root)
      expect(result.exitCode).toBe(8)
      expect(result.stdout).toContain('flow-changed')
      expect(result.stdout).toContain('fix: deja run today --verify --config ')
      const json = JSON.parse((await check(root, '--json')).stdout)
      expect(json.ok).toBe(false)
      expect(json.entries[0]).toMatchObject({ status: 'flow-changed', fix: expect.stringMatching(/^deja run today --verify --config \S+$/) })
      expect(json.entries[0].currentClosureSha256).not.toBe(json.entries[0].manifestClosureSha256)
      expect(result.stdout).not.toContain(root)
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('only the assertion hash differing is assertions-changed, with the remember command', async () => {
    const { temporary, root } = await remembered()
    try {
      // The closure hash is made to agree, so only the assertions differ.
      const file = join(root, 'deja.walks.json')
      const edited = TODAY.replace('"today.saved"', '"today.other"')
      await writeFile(join(root, '.maestro/flows/today.yaml'), edited)
      const closure = await collectFlowClosure(root, join(root, '.maestro/flows/today.yaml'))
      const manifest = JSON.parse(await readFile(file, 'utf8'))
      manifest.walks[0].closureSha256 = closure.sha256
      await writeFile(file, JSON.stringify(manifest))
      const result = await check(root)
      expect(result.exitCode).toBe(8)
      expect(result.stdout).toContain('assertions-changed')
      expect(result.stdout).toContain(
        'fix: deja remember today --flow .maestro/flows/today.yaml --accept-assertion-change --config ')
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('an edited assertion in the flow reports flow-changed and notes the assertions changed too', async () => {
    const { temporary, root } = await remembered()
    try {
      await writeFile(join(root, '.maestro/flows/today.yaml'), TODAY.replace('"today.saved"', '"today.other"'))
      const json = JSON.parse((await check(root, '--json')).stdout)
      expect(json.entries[0]).toMatchObject({ status: 'flow-changed', assertionsAlsoChanged: true })
      expect((await check(root)).stdout).toContain('--accept-assertion-change')
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('a missing flow or subflow is missing-file and names the file', async () => {
    const { temporary, root } = await remembered()
    try {
      await rm(join(root, '.maestro/subflows/type.yaml'))
      const sub = await check(root, '--json')
      expect(sub.exitCode).toBe(8)
      expect(JSON.parse(sub.stdout).entries[0]).toMatchObject({ status: 'missing-file', file: '.maestro/subflows/type.yaml' })
      await rm(join(root, '.maestro/flows/today.yaml'))
      const main = await check(root)
      expect(main.exitCode).toBe(8)
      expect(main.stdout).toContain('missing-file')
      expect(main.stdout).toContain('missing file: .maestro/flows/today.yaml')
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('a flow that cannot be read safely is closure-error', async () => {
    const { temporary, root } = await remembered()
    try {
      await writeFile(join(root, '.maestro/subflows/type.yaml'), '- runFlow: ${NAME}.yaml\n')
      const result = await check(root, '--json')
      expect(result.exitCode).toBe(8)
      expect(JSON.parse(result.stdout).entries[0]).toMatchObject({ status: 'closure-error' })
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('a malformed, unknown-version or missing manifest exits 2', async () => {
    const { temporary, root } = await remembered()
    try {
      const file = join(root, 'deja.walks.json')
      const good = await readFile(file, 'utf8')
      await writeFile(file, '{ not json')
      const malformed = await check(root)
      expect(malformed.exitCode).toBe(2)
      expect(malformed.stderr).toContain('Invalid walk manifest')
      await writeFile(file, good.replace('"manifestVersion": 1', '"manifestVersion": 2'))
      const unknown = await check(root)
      expect(unknown.exitCode).toBe(2)
      expect(unknown.stderr).toContain('unsupported manifestVersion')
      await rm(file)
      const missing = await check(root)
      expect(missing.exitCode).toBe(2)
      expect(missing.stderr).toContain('does not exist')
      await expect(stat(join(root, '.deja'))).rejects.toBeDefined()
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('--untracked lists yaml no entry uses, and never changes the exit code', async () => {
    const { temporary, root } = await remembered()
    try {
      await mkdir(join(root, '.maestro/flows/deja/nested'), { recursive: true })
      await writeFile(join(root, '.maestro/flows/orphan.yml'), '- launchApp\n')
      await writeFile(join(root, '.maestro/flows/deja/nested/new.yaml'), '- launchApp\n')
      await writeFile(join(root, '.maestro/flows/notes.txt'), 'x')
      const plain = await check(root)
      expect(plain.exitCode).toBe(0)
      expect(plain.stdout).not.toContain('orphan')
      const listed = await check(root, '--untracked')
      expect(listed.exitCode).toBe(0)
      expect(listed.stdout).toContain('.maestro/flows/orphan.yml')
      expect(listed.stdout).toContain('.maestro/flows/deja/nested/new.yaml')
      expect(listed.stdout).not.toContain('today.yaml\n  .maestro')
      expect(JSON.parse((await check(root, '--json', '--untracked')).stdout).untracked).toEqual([
        '.maestro/flows/deja/nested/new.yaml', '.maestro/flows/orphan.yml',
      ])
      // With a failing entry the code is 8 either way.
      await writeFile(join(root, '.maestro/subflows/type.yaml'), `${TYPE}# edited\n`)
      expect((await check(root, '--untracked')).exitCode).toBe(8)
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('usage errors exit 2 and a config with no manifest is refused', async () => {
    const { temporary, root } = await remembered()
    try {
      expect((await runCli(['manifest', '--config', join(root, 'deja.config.json')], { stdout: () => {}, stderr: () => {} })).valueOf()).toBe(2)
      expect((await runCli(['list', '--untracked', '--config', join(root, 'deja.config.json')], { stdout: () => {}, stderr: () => {} })).valueOf()).toBe(2)
      const config = JSON.parse(await readFile(join(root, 'deja.config.json'), 'utf8'))
      delete config.manifest
      await writeFile(join(root, 'deja.config.json'), JSON.stringify(config))
      expect((await check(root)).exitCode).toBe(2)
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('checks 8 flows well under a second', async () => {
    const { temporary, root } = await remembered()
    try {
      const walks: WalkManifest['walks'] = []
      for (let index = 0; index < 8; index += 1) {
        const flow = `.maestro/flows/walk-${index}.yaml`
        await writeFile(join(root, flow), TODAY.replace('today.saved', `saved.${index}`))
        const closure = await collectFlowClosure(root, join(root, flow))
        walks.push({
          goal: `walk ${index}`, appId: 'dev.deja.fixture', platform: 'ios', adapter: 'maestro-ios/v1', flow,
          closureSha256: closure.sha256, assertionsSha256: assertionsSha256(closure),
        })
      }
      await writeFile(join(root, 'deja.walks.json'), serializeWalkManifest({ manifestVersion: 1, hashVersion: 1, walks }))
      await check(root) // warm the module and the file cache
      const started = performance.now()
      const result = await check(root)
      const elapsed = performance.now() - started
      expect(result.exitCode).toBe(0)
      expect(result.stdout).toContain('8 of 8 entries match')
      // Measured in tens of milliseconds; the bound is generous so a slow CI disk does not flake.
      expect(elapsed).toBeLessThan(1000)
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test.each([
    ['appId', (walk: Record<string, unknown>) => { walk.appId = 'dev.other.app' }],
    ['platform', (walk: Record<string, unknown>) => { walk.platform = 'android' }],
    ['adapter', (walk: Record<string, unknown>) => { walk.adapter = 'maestro-ios/v0' }],
  ])('an entry whose %s differs from the config is identity-mismatch, exit 8', async (field, change) => {
    const { temporary, root } = await remembered()
    try {
      const file = join(root, 'deja.walks.json')
      const manifest = JSON.parse(await readFile(file, 'utf8'))
      change(manifest.walks[0])
      await writeFile(file, JSON.stringify(manifest))
      const result = await check(root, '--json')
      expect(result.exitCode).toBe(8)
      const entry = JSON.parse(result.stdout).entries[0]
      expect(entry.status).toBe('identity-mismatch')
      expect(entry.identity.field).toBe(field)
      expect(entry.fix).toBeUndefined()
      expect((await check(root)).stdout).toContain(`${field} differs`)
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('the browser is compared too, and an entry for another platform is fine when the goal has one for this config', async () => {
    const { temporary, root } = await remembered()
    try {
      const walk = JSON.parse(await readFile(join(root, 'deja.walks.json'), 'utf8')).walks[0]
      const other = { ...walk, platform: 'android', adapter: 'maestro-android/v1' }
      const web = { ...walk, platform: 'web', adapter: 'maestro-web/v1', browser: 'firefox' }
      const flows = new ConsumerFlows({ workspaceRoot: root, manifestPath: 'deja.walks.json' })
      const write = (walks: unknown[]) => writeFile(join(root, 'deja.walks.json'), JSON.stringify({ manifestVersion: 1, hashVersion: 1, walks }))
      const identity = { appId: walk.appId, platform: 'web', adapter: 'maestro-web/v1', browser: 'chromium' }
      await write([web])
      expect((await flows.check({ identity })).entries[0]).toMatchObject({ status: 'identity-mismatch', identity: { field: 'browser', manifest: 'firefox', config: 'chromium' } })
      await write([walk, other])
      const both = await flows.check({ identity: { ...identity, platform: 'ios', adapter: 'maestro-ios/v1', browser: null } })
      expect(both.entries.map(entry => entry.status)).toEqual(['ok', 'ok'])
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('the printed fix is safe to paste: quoted, escaped, `--` before a dash goal, and --config as typed', async () => {
    const { temporary, root } = await remembered()
    try {
      const goals = ['it\'s $(touch PWNED) `x` "q" *', '-rf', 'two\nlines\u001b[2K', 'a b']
      const walk = JSON.parse(await readFile(join(root, 'deja.walks.json'), 'utf8')).walks[0]
      const walks = goals.map(goal => ({ ...walk, goal, closureSha256: '0'.repeat(64) }))
      await writeFile(join(root, 'deja.walks.json'), JSON.stringify({ manifestVersion: 1, hashVersion: 1, walks }))
      const result = await check(root, '--json')
      const fixes: string[] = JSON.parse(result.stdout).entries.map((entry: { fix: string }) => entry.fix)
      expect(fixes).toHaveLength(4)
      for (const [index, goal] of goals.entries()) {
        const words = spawnSync('bash', ['-c', `printf '%s\\0' ${fixes[index].replace(/^deja /, '').replace(/ --config \S+/, '')}`], { cwd: root }).stdout.toString().split('\0').slice(0, -1)
        expect(words).toContain(goal)
        expect(words).toContain('--verify')
      }
      await expect(stat(join(root, 'PWNED'))).rejects.toBeDefined()
      expect(fixes[1]).toMatch(/^deja run --verify --config \S+ -- '-rf'$/)
      // The table shows no raw control character.
      const text = (await check(root)).stdout
      // eslint-disable-next-line no-control-regex
      expect(text).not.toMatch(/[\u0000-\u0008\u000b-\u001f\u007f]/)
      expect(text).toContain('two\\nlines\\x1b[2K')
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('the CLI reads goals after `--`, and the fix omits --config for the default config', async () => {
    const { temporary, root } = await remembered()
    try {
      const run = (args: string[]) => spawnSync(process.execPath, [cli, ...args], { cwd: root, encoding: 'utf8' })
      await writeFile(join(root, '.maestro/subflows/type.yaml'), `${TYPE}# edited\n`)
      const result = run(['manifest', 'check'])
      expect(result.status).toBe(8)
      expect(result.stdout).toContain('fix: deja run today --verify\n')
      expect(run(['run', '--', 'x', '--verify']).stderr).not.toContain('Unknown option')
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('a flowsDir that is a symlink out of the workspace is not listed', async () => {
    const { temporary, root } = await remembered({ flowsDir: '.maestro/linkflows' })
    try {
      const outside = join(temporary, 'outside')
      await mkdir(join(outside, 'secretdir'), { recursive: true })
      await writeFile(join(outside, 'OUTSIDE-SECRET-NAME.yaml'), '- launchApp\n')
      await writeFile(join(outside, 'secretdir/deep.yml'), '- launchApp\n')
      await symlink(outside, join(root, '.maestro/linkflows'))
      const result = await check(root, '--untracked', '--json')
      expect(result.exitCode).toBe(0)
      expect(result.stdout).not.toContain('OUTSIDE')
      expect(result.stdout).not.toContain('deep.yml')
      expect(JSON.parse(result.stdout).untracked).toEqual([])
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('retry and runScript files are part of the closure and assertions', async () => {
    const { temporary, root } = await remembered()
    try {
      await mkdir(join(root, '.maestro/scripts'), { recursive: true })
      await writeFile(join(root, '.maestro/subflows/again.yaml'), '- tapOn: Save\n- assertVisible:\n    id: "saved"\n')
      await writeFile(join(root, '.maestro/scripts/seed.js'), 'output.value = 1\n')
      const flow = '.maestro/flows/members.yaml'
      await writeFile(join(root, flow), '- retry:\n    maxRetries: 2\n    file: ../subflows/again.yaml\n- runScript: ../scripts/seed.js\n')
      const closure = await collectFlowClosure(root, join(root, flow))
      expect(closure.files.map(file => file.source).sort()).toEqual([
        '.maestro/flows/members.yaml', '.maestro/scripts/seed.js', '.maestro/subflows/again.yaml'])
      const walk = JSON.parse(await readFile(join(root, 'deja.walks.json'), 'utf8')).walks[0]
      await writeFile(join(root, 'deja.walks.json'), JSON.stringify({ manifestVersion: 1, hashVersion: 1, walks: [{
        ...walk, goal: 'members', flow, closureSha256: closure.sha256, assertionsSha256: assertionsSha256(closure),
      }] }))
      expect((await check(root)).exitCode).toBe(0)
      await writeFile(join(root, '.maestro/scripts/seed.js'), 'output.value = 2\n')
      expect(JSON.parse((await check(root, '--json')).stdout).entries[0].status).toBe('flow-changed')
      await writeFile(join(root, '.maestro/scripts/seed.js'), 'output.value = 1\n')
      await writeFile(join(root, '.maestro/subflows/again.yaml'), '- tapOn: Save\n')
      expect(JSON.parse((await check(root, '--json')).stdout).entries[0].status).toBe('flow-changed')
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('a closure-error reason has no store wording', async () => {
    const { temporary, root } = await remembered()
    try {
      await writeFile(join(root, '.maestro/subflows/type.yaml'), '- runFlow: ${NAME}.yaml\n')
      const entry = JSON.parse((await check(root, '--json')).stdout).entries[0]
      expect(entry.reason).toContain('reference uses a variable')
      expect(entry.reason).not.toContain('Cannot store flow')
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })
})
