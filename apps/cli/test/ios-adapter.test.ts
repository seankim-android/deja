import { describe, expect, test } from 'bun:test'
import { spawn } from 'node:child_process'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

const cli = resolve(import.meta.dir, '../walkcache.ts')
const fakeMaestro = resolve(import.meta.dir, '../../../packages/maestro/test/fixtures/fake-maestro.cjs')

async function execute(root: string, args: string[], product = false) {
  const child = spawn(process.execPath, [cli, ...args, '--config', 'walkcache.config.json'], {
    cwd: root,
    env: { ...process.env, WALKCACHE_FAKE_MAESTRO_MODE: product ? 'product' : '' },
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
  try { report = JSON.parse(await readFile(join(root, '.walkcache/reports/latest.json'), 'utf8')) } catch {}
  return { exitCode, stdout: stdout.join(''), stderr: stderr.join(''), report }
}

describe('production iOS adapter CLI', () => {
  test('reviewed remember/warm replay works without a model; cold/stale/product failures stay honest', async () => {
    const temporary = await mkdtemp(join(tmpdir(), 'walkcache-ios-cli-'))
    const root = join(temporary, 'workspace')
    const udid = 'DFC4D233-498B-4B33-B713-E59B239FA86A'
    try {
      await mkdir(join(root, 'Fixture.app/_CodeSignature'), { recursive: true })
      await mkdir(join(root, 'flows'), { recursive: true })
      await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-one')
      await writeFile(join(root, 'Fixture.app/Info.plist'), '<plist>fixture</plist>')
      await writeFile(join(root, 'Fixture.app/_CodeSignature/CodeResources'), 'signature-one')
      await writeFile(join(root, 'flows/checkout.yaml'), `appId: dev.walkcache.ios.checkout
---
- launchApp:
    appId: dev.walkcache.ios.checkout
    clearState: true
- assertVisible: Order confirmed
- assertVisible: "Order #WC-1001"
`)
      await writeFile(join(root, 'walkcache.config.json'), JSON.stringify({
        workspaceRoot: '.', appId: 'dev.walkcache.ios.checkout', platform: 'ios',
        adapter: 'maestro-ios@2.12.0', device: udid,
        buildPath: 'Fixture.app', fixtureCredentials: [{ label: 'Fixture password', value: 'testing123' }],
        maestro: { executable: fakeMaestro, timeoutMs: 2000 },
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
    const temporary = await mkdtemp(join(tmpdir(), 'walkcache-ios-config-'))
    try {
      await mkdir(join(temporary, 'Fixture.app'))
      await writeFile(join(temporary, 'Fixture.app/Fixture'), 'binary')
      await writeFile(join(temporary, 'walkcache.config.json'), JSON.stringify({
        workspaceRoot: '.', appId: 'dev.walkcache.ios.checkout', platform: 'ios',
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
})
