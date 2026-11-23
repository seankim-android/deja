import { describe, expect, test } from 'bun:test'
import { spawn } from 'node:child_process'
import { mkdir, mkdtemp, readdir, readFile, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

const cli = resolve(import.meta.dir, '../deja.ts')
const fakeMaestro = resolve(import.meta.dir, '../../../packages/maestro/test/fixtures/fake-maestro.cjs')

async function execute(root: string, args: string[], product = false) {
  const child = spawn(process.execPath, [cli, ...args, '--config', 'deja.config.json'], {
    cwd: root,
    env: { ...process.env, DEJA_FAKE_MAESTRO_MODE: product ? 'product' : '' },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  const stdout: string[] = []
  const stderr: string[] = []
  child.stdout.setEncoding('utf8')
  child.stderr.setEncoding('utf8')
  child.stdout.on('data', chunk => stdout.push(chunk))
  child.stderr.on('data', chunk => stderr.push(chunk))
  const exitCode = await new Promise<number | null>(resolveExit => {
    child.once('error', error => stderr.push(String(error)))
    child.once('close', code => resolveExit(code))
  })
  let report: Record<string, unknown> | null = null
  try { report = JSON.parse(await readFile(join(root, '.deja/reports/latest.json'), 'utf8')) } catch {}
  return { exitCode, stdout: stdout.join(''), stderr: stderr.join(''), report }
}

describe('production iOS adapter CLI', () => {
  test('reviewed remember/warm replay works without a model; cold/stale/product failures stay honest', async () => {
    const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-ios-cli-')))
    const root = join(temporary, 'workspace')
    const udid = 'DFC4D233-498B-4B33-B713-E59B239FA86A'
    try {
      await mkdir(join(root, 'Fixture.app/_CodeSignature'), { recursive: true })
      await mkdir(join(root, 'flows'), { recursive: true })
      await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-one')
      await writeFile(join(root, 'Fixture.app/Info.plist'), '<plist>fixture</plist>')
      await writeFile(join(root, 'Fixture.app/_CodeSignature/CodeResources'), 'signature-one')
      await writeFile(join(root, 'flows/checkout.yaml'), `appId: dev.deja.ios.checkout
---
- launchApp:
    appId: dev.deja.ios.checkout
    clearState: true
- assertVisible: Order confirmed
- assertVisible: "Order #WC-1001"
`)
      await writeFile(join(root, 'deja.config.json'), JSON.stringify({
        workspaceRoot: '.', appId: 'dev.deja.ios.checkout', platform: 'ios',
        adapter: 'maestro-ios@2.12.0', device: udid,
        buildPath: 'Fixture.app', fixtureCredentials: [{ label: 'Fixture password', value: 'testing123' }],
        maestro: { executable: fakeMaestro, timeoutMs: 30_000 },
      }))
      const remembered = await execute(root, ['remember', 'checkout', '--flow', 'flows/checkout.yaml'])
      expect(remembered.exitCode).toBe(0)
      expect(remembered.report).toMatchObject({
        outcome: 'remembered', passed: true, modelCalls: 0, assertions: 2,
        identity: { platform: 'ios', adapter: 'maestro-ios@2.12.0' },
      })
      expect(JSON.stringify(remembered.report)).not.toContain('testing123')

      const warm = await execute(root, ['run', 'checkout'])
      expect(warm.exitCode).toBe(0)
      expect(warm.report).toMatchObject({ cacheStatus: 'hit', passed: true, modelCalls: 0, repairCount: 0 })
      expect(warm.stdout).toContain('exact local hit')

      const cold = await execute(root, ['run', 'a different iOS journey'])
      expect(cold.exitCode).toBe(3)
      expect(cold.report).toMatchObject({ outcome: 'exploration-rejected', modelCalls: 0, passed: false })
      expect(cold.stdout).toContain('Cold iOS exploration is not supported')

      await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-two')
      const stale = await execute(root, ['run', 'checkout'])
      expect(stale.exitCode).toBe(1)
      expect(stale.report).toMatchObject({
        cacheMissReason: 'stale-build', outcome: 'repair-rejected', modelCalls: 0, repairCount: 0,
      })
      expect(stale.stdout).toContain('cold exploration was not substituted')

      await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-one')
      const regression = await execute(root, ['run', 'checkout'], true)
      expect(regression.exitCode).toBe(1)
      expect(regression.report).toMatchObject({
        cacheStatus: 'hit', outcome: 'product-regression', modelCalls: 0, repairCount: 0,
        failure: { category: 'product_assertion', repairEligible: false },
      })
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  test('iOS config never accepts the Android repair plugin', async () => {
    const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-ios-config-')))
    try {
      await mkdir(join(temporary, 'Fixture.app'))
      await writeFile(join(temporary, 'Fixture.app/Fixture'), 'binary')
      await writeFile(join(temporary, 'deja.config.json'), JSON.stringify({
        workspaceRoot: '.', appId: 'dev.deja.ios.checkout', platform: 'ios',
        buildPath: 'Fixture.app', device: 'test-simulator', fixtureCredentials: [],
        repair: {},
      }))
      const result = await execute(temporary, ['list'])
      expect(result.exitCode).toBe(2)
      expect(result.stderr).toContain('iOS does not configure the Android-only repair plugin')
    } finally {
      await rm(temporary, { recursive: true, force: true })
    }
  })

  describe('WALK-021 staleBuild: replay-last-verified', () => {
    async function workspace(staleBuild: unknown = 'replay-last-verified') {
      const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-ios-new-build-')))
      const root = join(temporary, 'workspace')
      await mkdir(join(root, 'Fixture.app/_CodeSignature'), { recursive: true })
      await mkdir(join(root, 'flows'), { recursive: true })
      await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-one')
      await writeFile(join(root, 'Fixture.app/Info.plist'), '<plist>fixture</plist>')
      await writeFile(join(root, 'Fixture.app/_CodeSignature/CodeResources'), 'signature-one')
      await writeFile(join(root, 'flows/smoke.yaml'), `appId: dev.deja.ios.checkout
---
- launchApp:
    appId: dev.deja.ios.checkout
- assertVisible: Order confirmed
`)
      await writeFile(join(root, 'deja.config.json'), JSON.stringify({
        workspaceRoot: '.', appId: 'dev.deja.ios.checkout', platform: 'ios',
        adapter: 'maestro-ios@2.12.0', device: 'DFC4D233-498B-4B33-B713-E59B239FA86A',
        buildPath: 'Fixture.app', fixtureCredentials: [],
        maestro: { executable: fakeMaestro, timeoutMs: 30_000 },
        ...(staleBuild === undefined ? {} : { staleBuild }),
      }))
      return { temporary, root }
    }

    async function walks(root: string) {
      return JSON.parse(await readFile(join(root, '.deja/memory/walks.json'), 'utf8')).walks as
        Array<{ buildFingerprint: string, flowSha256: string, lastValidation: { passed: boolean } }>
    }

    test('a pass on a new build is reported as such and makes the next run an exact hit', async () => {
      const { temporary, root } = await workspace()
      try {
        const remembered = await execute(root, ['remember', 'smoke', '--flow', 'flows/smoke.yaml'])
        expect(remembered.exitCode).toBe(0)
        const [first] = await walks(root)

        await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-two')
        const replayed = await execute(root, ['run', 'smoke'])
        expect(replayed.exitCode).toBe(0)
        expect(replayed.report).toMatchObject({
          cacheStatus: 'miss', cacheMissReason: 'stale-build', outcome: 'passed', passed: true,
          modelCalls: 0, repairCount: 0, yamlSha256: first.flowSha256,
          replayedOnNewBuild: {
            policy: 'replay-last-verified', previousBuildFingerprint: first.buildFingerprint,
            previousFlowSha256: first.flowSha256, recordedForNewBuild: true,
          },
        })
        expect(replayed.stdout).toContain('Replayed on a new build: this build was not verified')
        const [second] = await walks(root)
        expect(second.buildFingerprint).not.toBe(first.buildFingerprint)
        expect(second.flowSha256).toBe(first.flowSha256)
        expect(second.lastValidation.passed).toBe(true)

        const warm = await execute(root, ['run', 'smoke'])
        expect(warm.exitCode).toBe(0)
        expect(warm.report).toMatchObject({ cacheStatus: 'hit', outcome: 'passed', modelCalls: 0 })
        expect(warm.report?.replayedOnNewBuild).toBeUndefined()
      } finally {
        await rm(temporary, { recursive: true, force: true })
      }
    })

    test('a fail on a new build reports the real Maestro failure and stores nothing for it', async () => {
      const { temporary, root } = await workspace()
      try {
        expect((await execute(root, ['remember', 'smoke', '--flow', 'flows/smoke.yaml'])).exitCode).toBe(0)
        const before = await readFile(join(root, '.deja/memory/walks.json'), 'utf8')

        await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-two')
        const failed = await execute(root, ['run', 'smoke'], true)
        expect(failed.exitCode).toBe(1)
        expect(failed.report).toMatchObject({
          cacheMissReason: 'stale-build', outcome: 'product-regression', passed: false,
          modelCalls: 0, repairCount: 0, diffs: [],
          failure: { category: 'product_assertion', repairEligible: false },
          replayedOnNewBuild: { recordedForNewBuild: false },
        })
        expect(await readFile(join(root, '.deja/memory/walks.json'), 'utf8')).toBe(before)
        expect(await readdir(join(root, '.deja'))).not.toContain('patches')
      } finally {
        await rm(temporary, { recursive: true, force: true })
      }
    })

    test('no previous verified walk keeps the stale-build rejection', async () => {
      const { temporary, root } = await workspace()
      try {
        expect((await execute(root, ['remember', 'smoke', '--flow', 'flows/smoke.yaml'])).exitCode).toBe(0)
        // A failed replay on build one leaves no verified walk to carry to a new build.
        expect((await execute(root, ['run', 'smoke'], true)).exitCode).toBe(1)
        await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-two')
        const stale = await execute(root, ['run', 'smoke'])
        expect(stale.exitCode).toBe(1)
        expect(stale.report).toMatchObject({
          cacheMissReason: 'stale-build', outcome: 'repair-rejected', modelCalls: 0,
        })
        expect(stale.report?.replayedOnNewBuild).toBeUndefined()
      } finally {
        await rm(temporary, { recursive: true, force: true })
      }
    })

    test('tampered stored YAML is invalid memory, never a replay', async () => {
      const { temporary, root } = await workspace()
      try {
        expect((await execute(root, ['remember', 'smoke', '--flow', 'flows/smoke.yaml'])).exitCode).toBe(0)
        const before = await readFile(join(root, '.deja/memory/walks.json'), 'utf8')
        const directory = join(root, '.deja/flows/smoke')
        const [stored] = await readdir(directory)
        await writeFile(join(directory, stored), 'appId: dev.deja.ios.checkout\n---\n- launchApp\n')
        await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-two')
        const reportsBefore = await readdir(join(root, '.deja/reports'))
        const tampered = await execute(root, ['run', 'smoke'])
        expect(tampered.exitCode).toBe(4)
        expect(tampered.stderr).toContain('stored flow checksum does not match')
        expect(await readdir(join(root, '.deja/reports'))).toEqual(reportsBefore)
        expect(await readFile(join(root, '.deja/memory/walks.json'), 'utf8')).toBe(before)
      } finally {
        await rm(temporary, { recursive: true, force: true })
      }
    })

    test('config accepts only the one value, and only for iOS', async () => {
      const bad = await workspace('replay-anything')
      try {
        const result = await execute(bad.root, ['list'])
        expect(result.exitCode).toBe(2)
        expect(result.stderr).toContain('staleBuild must be "replay-last-verified"')
      } finally {
        await rm(bad.temporary, { recursive: true, force: true })
      }
      const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-web-stale-')))
      try {
        await writeFile(join(temporary, 'index.html'), '<html></html>')
        await writeFile(join(temporary, 'deja.config.json'), JSON.stringify({
          workspaceRoot: '.', appId: 'http://127.0.0.1:4173/', platform: 'web',
          buildPath: 'index.html', browser: 'chromium', fixtureCredentials: [],
          staleBuild: 'replay-last-verified',
        }))
        const result = await execute(temporary, ['list'])
        expect(result.exitCode).toBe(2)
        expect(result.stderr).toContain('staleBuild is supported only for platform: ios')
      } finally {
        await rm(temporary, { recursive: true, force: true })
      }
    })
  })
})
