import { describe, expect, test } from 'bun:test'
import { spawn } from 'node:child_process'
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

const cli = resolve(import.meta.dir, '../deja.ts')
const fakeMaestro = resolve(import.meta.dir, '../../../packages/maestro/test/fixtures/fake-maestro.cjs')

async function execute(root: string, args: string[]) {
  const child = spawn(process.execPath, [cli, ...args, '--config', 'deja.config.json'], {
    cwd: root, env: { ...process.env, DEJA_FAKE_MAESTRO_MODE: '' }, stdio: ['ignore', 'pipe', 'pipe'],
  })
  const stdout: string[] = []
  const stderr: string[] = []
  child.stdout.setEncoding('utf8')
  child.stderr.setEncoding('utf8')
  child.stdout.on('data', chunk => stdout.push(chunk))
  child.stderr.on('data', chunk => stderr.push(chunk))
  const exitCode = await new Promise<number | null>(done => {
    child.once('error', error => stderr.push(String(error)))
    child.once('close', code => done(code))
  })
  let report: Record<string, unknown> | null = null
  try { report = JSON.parse(await readFile(join(root, '.deja/reports/latest.json'), 'utf8')) } catch {}
  return { exitCode, stdout: stdout.join(''), stderr: stderr.join(''), report }
}

const TODAY = `appId: dev.deja.fixture
---
- launchApp:
    clearState: true
- assertVisible:
    id: "screen.today"
- runFlow:
    file: ../subflows/type.yaml
- assertVisible:
    id: "today.saved"
`
const TYPE = '- inputText: Popcorn\n'

async function workspace(config: Record<string, unknown> = {}) {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-manifest-cli-')))
  const root = join(temporary, 'workspace')
  await mkdir(join(root, 'Fixture.app'), { recursive: true })
  await mkdir(join(root, '.maestro/flows'), { recursive: true })
  await mkdir(join(root, '.maestro/subflows'), { recursive: true })
  await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-one')
  await writeFile(join(root, '.maestro/flows/today.yaml'), TODAY)
  await writeFile(join(root, '.maestro/subflows/type.yaml'), TYPE)
  await writeFile(join(root, 'deja.config.json'), JSON.stringify({
    workspaceRoot: '.', appId: 'dev.deja.fixture', platform: 'ios', adapter: 'maestro-ios/v1',
    device: 'DFC4D233-498B-4B33-B713-E59B239FA86A', buildPath: 'Fixture.app',
    fixtureCredentials: [{ label: 'Fixture password', value: 'hunter2-fixture' }],
    maestro: { executable: fakeMaestro, timeoutMs: 30_000 },
    manifest: 'deja.walks.json',
    ...config,
  }))
  return { temporary, root, manifest: () => readFile(join(root, 'deja.walks.json'), 'utf8') }
}

describe('manifest mode on the CLI', () => {
  test('remember writes the manifest, run replays the consumer file, and a change needs --verify', async () => {
    const { temporary, root, manifest } = await workspace()
    try {
      const remembered = await execute(root, ['remember', 'today', '--flow', '.maestro/flows/today.yaml'])
      expect(remembered.exitCode).toBe(0)
      expect(remembered.report).toMatchObject({ outcome: 'remembered', passed: true, yamlPath: '.maestro/flows/today.yaml' })
      expect(remembered.stdout).toContain('[manifest] deja.walks.json')
      const written = await manifest()
      expect(JSON.parse(written)).toMatchObject({
        manifestVersion: 1, hashVersion: 1,
        walks: [{ goal: 'today', platform: 'ios', adapter: 'maestro-ios/v1', flow: '.maestro/flows/today.yaml' }],
      })
      // Nothing machine-specific or secret is committed.
      for (const forbidden of [root, 'hunter2-fixture', '.deja/']) expect(written).not.toContain(forbidden)
      await expect(readFile(join(root, '.deja/flows'))).rejects.toBeDefined()

      const warm = await execute(root, ['run', 'today'])
      expect(warm.exitCode).toBe(0)
      expect(warm.report).toMatchObject({ cacheStatus: 'hit', passed: true, modelCalls: 0 })

      await writeFile(join(root, '.maestro/subflows/type.yaml'), `${TYPE}# edited\n`)
      const changed = await execute(root, ['run', 'today', '--summary'])
      expect(changed.exitCode).toBe(6)
      expect(changed.report).toMatchObject({ outcome: 'flow-changed', passed: false, cacheMissReason: 'flow-changed' })
      expect(changed.stdout).toContain('outcome=flow-changed')
      expect(await manifest()).toBe(written)

      const verified = await execute(root, ['run', 'today', '--verify'])
      expect(verified.exitCode).toBe(0)
      expect(verified.stdout).toContain('[manifest] deja.walks.json')
      expect(await manifest()).not.toBe(written)
      expect((await execute(root, ['run', 'today'])).exitCode).toBe(0)

      // Assertions: a weakened flow is refused by --verify and by a plain remember.
      await writeFile(join(root, '.maestro/flows/today.yaml'), TODAY.replace('- assertVisible:\n    id: "today.saved"\n', ''))
      const afterVerify = await manifest()
      const weakened = await execute(root, ['run', 'today', '--verify'])
      expect(weakened.exitCode).toBe(6)
      expect(weakened.report).toMatchObject({ outcome: 'assertions-changed', passed: false })
      expect((await execute(root, ['remember', 'today', '--flow', '.maestro/flows/today.yaml'])).exitCode).toBe(6)
      expect(await manifest()).toBe(afterVerify)
      const accepted = await execute(root, [
        'remember', 'today', '--flow', '.maestro/flows/today.yaml', '--accept-assertion-change',
      ])
      expect(accepted.exitCode).toBe(0)
      expect(await manifest()).not.toBe(afterVerify)
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('flow-changed writes a failing JUnit report, and explore and unknown goals never run a model', async () => {
    const { temporary, root } = await workspace()
    try {
      expect((await execute(root, ['remember', 'today', '--flow', '.maestro/flows/today.yaml'])).exitCode).toBe(0)
      await writeFile(join(root, '.maestro/flows/today.yaml'), `${TODAY}# edited\n`)
      const result = await execute(root, ['run', 'today', '--junit', 'out/junit.xml'])
      expect(result.exitCode).toBe(6)
      const junit = await readFile(join(root, 'out/junit.xml'), 'utf8')
      expect(junit).toContain('<failure')
      expect(junit).toContain('flow-changed')
      const unknown = await execute(root, ['run', 'something new'])
      expect(unknown.exitCode).toBe(3)
      expect(unknown.report).toMatchObject({ outcome: 'exploration-rejected', modelCalls: 0 })
      expect((await execute(root, ['explore', 'today'])).exitCode).toBe(3)
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('migrate moves an old store into the tree as reviewable files, and then run replays them', async () => {
    const { temporary, root, manifest } = await workspace({ manifest: undefined })
    try {
      // Before: the old way. remember copies the flow into .deja/flows.
      expect((await execute(root, ['remember', 'today', '--flow', '.maestro/flows/today.yaml'])).exitCode).toBe(0)
      expect((await readFile(join(root, '.deja/memory/walks.json'), 'utf8'))).toContain('"schemaVersion": 3')
      const config = JSON.parse(await readFile(join(root, 'deja.config.json'), 'utf8'))
      await writeFile(join(root, 'deja.config.json'), JSON.stringify({ ...config, manifest: 'deja.walks.json' }))

      const blocked = await execute(root, ['run', 'today'])
      expect(blocked.exitCode).toBe(6)
      expect(blocked.report).toMatchObject({ outcome: 'needs-migration', passed: false, modelCalls: 0 })

      const dry = await execute(root, ['migrate', '--dry-run'])
      expect(dry.exitCode).toBe(0)
      expect(dry.stdout).toContain('dry run, nothing written')
      expect(dry.stdout).toContain('.maestro/flows/deja/today/flows/today.yaml')
      await expect(readFile(join(root, 'deja.walks.json'))).rejects.toBeDefined()
      await expect(readFile(join(root, '.maestro/flows/deja/today/flows/today.yaml'))).rejects.toBeDefined()

      const migrated = await execute(root, ['migrate', '--json'])
      expect(migrated.exitCode).toBe(0)
      const report = JSON.parse(migrated.stdout).migration
      expect(report).toMatchObject({ dryRun: false, manifestPath: 'deja.walks.json', skipped: [] })
      expect(report.migrated[0]).toMatchObject({ goal: 'today', flow: '.maestro/flows/deja/today/flows/today.yaml' })
      expect(report.backupPath).toBe('.deja/memory/walks.json.schema3.bak')
      expect(await readFile(join(root, '.maestro/flows/deja/today/flows/today.yaml'), 'utf8')).toBe(TODAY)
      expect(await readFile(join(root, '.maestro/flows/deja/today/subflows/type.yaml'), 'utf8')).toBe(TYPE)
      const written = await manifest()
      expect(JSON.parse(written).walks[0]).toMatchObject({ goal: 'today', flow: '.maestro/flows/deja/today/flows/today.yaml' })
      expect(written).not.toContain(root)

      const warm = await execute(root, ['run', 'today'])
      expect(warm.exitCode).toBe(0)
      expect(warm.report).toMatchObject({ cacheStatus: 'hit', passed: true, yamlPath: '.maestro/flows/deja/today/flows/today.yaml' })
      const again = await execute(root, ['migrate'])
      expect(again.exitCode).toBe(0)
      expect(again.stdout).toContain('0 moved, 0 left')
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('migrate needs a manifest, takes no goal, and flowsDir must be a safe folder', async () => {
    const without = await workspace({ manifest: undefined })
    try {
      const result = await execute(without.root, ['migrate'])
      expect(result.exitCode).toBe(2)
      expect(result.stderr).toContain('migrate needs a "manifest"')
      expect((await execute(without.root, ['run', 'today', '--dry-run'])).exitCode).toBe(2)
    } finally {
      await rm(without.temporary, { recursive: true, force: true })
    }
    for (const flowsDir of ['/abs', '../out', '.deja/flows', 'a//b']) {
      const bad = await workspace({ flowsDir })
      try {
        const result = await execute(bad.root, ['migrate', '--dry-run'])
        expect(result.exitCode).toBe(2)
        expect(result.stderr).toContain('flowsDir')
      } finally {
        await rm(bad.temporary, { recursive: true, force: true })
      }
    }
    const orphan = await workspace({ manifest: undefined, flowsDir: '.maestro/x' })
    try {
      expect((await execute(orphan.root, ['list'])).stderr).toContain('flowsDir needs a manifest')
    } finally {
      await rm(orphan.temporary, { recursive: true, force: true })
    }
    const extra = await workspace()
    try {
      expect((await execute(extra.root, ['migrate', 'today'])).exitCode).toBe(2)
    } finally {
      await rm(extra.temporary, { recursive: true, force: true })
    }
  })

  test('a bad manifest, a bad config and a fixture credential in the manifest all fail closed', async () => {
    const { temporary, root } = await workspace()
    try {
      for (const text of ['{nope', JSON.stringify({ manifestVersion: 9, hashVersion: 1, walks: [] })]) {
        await writeFile(join(root, 'deja.walks.json'), text)
        const result = await execute(root, ['run', 'today'])
        expect(result.exitCode).toBe(2)
        expect(result.stderr).toContain('Invalid walk manifest')
      }
      await rm(join(root, 'deja.walks.json'))
      // The goal text itself must not smuggle a fixture value into a committed file.
      const leaked = await execute(root, ['remember', 'hunter2-fixture', '--flow', '.maestro/flows/today.yaml'])
      expect(leaked.exitCode).toBe(2)
      await expect(readFile(join(root, 'deja.walks.json'))).rejects.toBeDefined()
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
    for (const config of [
      { manifest: '/abs/deja.walks.json' }, { manifest: '../deja.walks.json' }, { manifest: '.deja/walks.json' },
      { manifest: 'walks.yaml' },
    ]) {
      const bad = await workspace(config)
      try {
        const result = await execute(bad.root, ['list'])
        expect(result.exitCode).toBe(2)
        expect(result.stderr).toContain('manifest')
      } finally {
        await rm(bad.temporary, { recursive: true, force: true })
      }
    }
    const without = await workspace({ manifest: undefined })
    try {
      const result = await execute(without.root, ['run', 'today', '--verify'])
      expect(result.exitCode).toBe(2)
      expect(result.stderr).toContain('needs a "manifest"')
      expect((await execute(without.root, ['remember', 'today', '--flow', '.maestro/flows/today.yaml'])).exitCode).toBe(0)
      await expect(readFile(join(without.root, 'deja.walks.json'))).rejects.toBeDefined()
    } finally {
      await rm(without.temporary, { recursive: true, force: true })
    }
  })
})
