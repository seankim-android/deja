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

describe('production Web adapter CLI', () => {
  test('verified remember/warm replay works without a model; unsupported cold and stale/product failures stay honest', async () => {
    const temporary = await mkdtemp(join(tmpdir(), 'walkcache-web-cli-'))
    const root = join(temporary, 'workspace')
    try {
      await mkdir(join(root, 'public'), { recursive: true })
      await mkdir(join(root, 'flows'), { recursive: true })
      await writeFile(join(root, 'public/index.html'), '<main>fixture one</main>')
      await writeFile(join(root, 'public/app.js'), 'const fixture = 1')
      await writeFile(join(root, 'flows/checkout.yaml'), `url: http://127.0.0.1:4174
---
- openLink: http://127.0.0.1:4174
- assertVisible: Order confirmed
`)
      await writeFile(join(root, 'walkcache.config.json'), JSON.stringify({
        workspaceRoot: '.', appId: 'http://127.0.0.1:4174', platform: 'web',
        browser: 'chromium', adapter: 'maestro-web@2.12.0', device: 'chromium',
        buildPath: 'public', fixtureCredentials: [{ label: 'Fixture password', value: 'testing123' }],
        maestro: { executable: fakeMaestro, timeoutMs: 2000 },
      }))
      const remembered = await execute(root, ['remember', 'checkout', '--flow', 'flows/checkout.yaml'])
      expect(remembered.exitCode).toBe(0)
      expect(remembered.report).toMatchObject({
        outcome: 'remembered', passed: true, modelCalls: 0,
        identity: { platform: 'web', browser: 'chromium', adapter: 'maestro-web@2.12.0' },
      })
      expect(JSON.stringify(remembered.report)).not.toContain('testing123')

      const warm = await execute(root, ['run', 'checkout'])
      expect(warm.exitCode).toBe(0)
      expect(warm.report).toMatchObject({ cacheStatus: 'hit', passed: true, modelCalls: 0, repairCount: 0 })
      expect(warm.stdout).toContain('exact local hit')

      const cold = await execute(root, ['run', 'a different web journey'])
      expect(cold.exitCode).toBe(3)
      expect(cold.report).toMatchObject({ outcome: 'exploration-rejected', modelCalls: 0, passed: false })
      expect(cold.stdout).toContain('Cold Web exploration is not supported')

      await writeFile(join(root, 'public/app.js'), 'const fixture = 2')
      const stale = await execute(root, ['run', 'checkout'])
      expect(stale.exitCode).toBe(1)
      expect(stale.report).toMatchObject({
        cacheMissReason: 'stale-build', outcome: 'repair-rejected', modelCalls: 0, repairCount: 0,
      })
      expect(stale.stdout).toContain('cold exploration was not substituted')

      await writeFile(join(root, 'public/app.js'), 'const fixture = 1')
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
})
