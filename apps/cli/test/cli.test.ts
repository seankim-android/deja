import { describe, expect, test } from 'bun:test'
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

const cli = resolve(import.meta.dir, '../deja.ts')
const fakeMaestro = resolve(import.meta.dir, '../../../packages/maestro/test/fixtures/fake-maestro.cjs')
const fakeOpenClaw = resolve(import.meta.dir, '../../../packages/agent-openclaw/test/fixtures/fake-openclaw.cjs')
async function workspace() {
  const temporary = await mkdtemp(join(tmpdir(), 'deja-cli-'))
  const root = join(temporary, 'workspace')
  await mkdir(join(root, 'app/build'), { recursive: true })
  await mkdir(join(root, 'flows'), { recursive: true })
  await mkdir(join(root, 'scripts'), { recursive: true })
  await mkdir(join(root, 'fixture-sdk/platform-tools'), { recursive: true })
  await writeFile(join(root, 'app/build/app.apk'), 'deterministic test build')
  await writeFile(join(root, 'scripts/maestro-mcp-proxy.ts'), '// controlled fixture: no MCP process is started\n')
  await writeFile(join(root, 'fixture-sdk/platform-tools/adb'), `#!/bin/sh
printf '%s\\n' "$*" >> "${join(root, 'adb-calls.log')}"
exit 0
`, { mode: 0o755 })
  await writeFile(join(root, 'flows/candidate.yaml'), `appId: dev.deja.checkout
---
- launchApp:
    clearState: true
- assertVisible: Déjà Shop
- assertVisible: Order confirmed
`)
  await writeFile(join(root, 'deja.config.json'), `${JSON.stringify({
    workspaceRoot: '.',
    appId: 'dev.deja.checkout',
    platform: 'android',
    buildPath: 'app/build/app.apk',
    device: 'fixture-device',
    openClawModel: 'cerebras/moonshotai-kimi-k3.1-pro-trial',
    openClawBaseUrl: 'https://api.cerebras.ai/v1',
    explorationTimeoutSeconds: 30,
    verificationTimeoutMs: 2000,
    fixtureCredentials: [{ label: 'Fixture password', value: 'testing123' }],
    maestro: { executable: fakeMaestro, timeoutMs: 2000 },
    openClaw: { executable: fakeOpenClaw, mcpProxy: 'scripts/maestro-mcp-proxy.ts' },
  }, null, 2)}\n`)
  return { temporary, root }
}

async function execute(root: string, args: string[], modes: {
  maestro?: 'product' | 'selector'
  explorer?: 'invalid'
  capture?: string
} = {}) {
  const cliArguments = args.includes('--config') ? args : [...args, '--config', 'deja.config.json']
  const child = spawn(process.execPath, [cli, ...cliArguments], {
    cwd: root,
    env: {
      ...process.env,
      ANDROID_HOME: join(root, 'fixture-sdk'),
      DEJA_FAKE_MAESTRO_MODE: modes.maestro ?? '',
      DEJA_FAKE_OPENCLAW_MODE: modes.explorer ?? '',
      ...(modes.capture ? { DEJA_FAKE_OPENCLAW_CAPTURE: modes.capture } : {}),
    },
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
  try {
    report = JSON.parse(await readFile(join(root, '.deja/reports/latest.json'), 'utf8'))
  } catch {
    // Usage/help/config failures intentionally have no run report.
  }
  return { exitCode, stdout: stdout.join(''), stderr: stderr.join(''), report }
}

async function cleanup(temporary: string) {
  await rm(temporary, recursiveOptions)
}
const recursiveOptions = { recursive: true, force: true }

async function configureSemantic(root: string, semantic: Record<string, unknown>) {
  const configPath = join(root, 'deja.config.json')
  const config = JSON.parse(await readFile(configPath, 'utf8'))
  config.semantic = semantic
  await writeFile(configPath, `${JSON.stringify(config, null, 2)}\n`)
}

describe('production headless CLI', () => {
  test('miss enters real cold lifecycle, then warm bypasses model construction and supports list/inspect', async () => {
    const context = await workspace()
    const capture = join(context.temporary, 'model-invocation.json')
    try {
      const cold = await execute(context.root, ['run', 'checkout'], { capture })
      expect(cold.exitCode).toBe(0)
      expect(cold.report).toMatchObject({
        command: 'run', cacheStatus: 'miss', cacheMissReason: 'not-found',
        outcome: 'cold-recorded', passed: true, modelCalls: 2, repairCount: 0, assertions: 2,
      })
      expect(cold.stdout).toContain('Cache status: miss (not-found)')
      expect(cold.stdout).toContain('Model calls: 2')
      expect(cold.stdout).toContain('Duration:')
      expect(cold.stdout).toContain('Assertions: 2')
      expect(cold.stdout).toContain('Repair count: 0')
      expect(cold.stdout).toContain('Report: .deja/reports/run-')
      expect(cold.stdout).toContain('YAML: .deja/flows/checkout/')
      expect(existsSync(capture)).toBe(true)
      expect((await readFile(join(context.root, 'adb-calls.log'), 'utf8')).split('\n').filter(Boolean)).toHaveLength(2)
      expect(JSON.stringify(cold.report)).not.toContain(context.root)
      expect(JSON.stringify(cold.report)).not.toContain('testing123')
      const modelCapture = await readFile(capture, 'utf8')

      const warm = await execute(context.root, ['run', 'checkout'], { capture })
      expect(warm.exitCode).toBe(0)
      expect(warm.report).toMatchObject({
        cacheStatus: 'hit', outcome: 'passed', modelCalls: 0, repairCount: 0,
      })
      expect(await readFile(capture, 'utf8')).toBe(modelCapture)
      expect(warm.stdout).toContain('exact local hit')
      expect(warm.stdout).toContain('Source diffs: none')

      const listed = await execute(context.root, ['list', '--json'])
      expect(listed.exitCode).toBe(0)
      const listJson = JSON.parse(listed.stdout)
      expect(listJson.walks).toHaveLength(1)
      expect(listJson.walks[0]).toMatchObject({ goalKey: 'checkout', origin: 'explore' })
      const inspected = await execute(context.root, ['inspect', '.deja/reports/latest.json'])
      expect(inspected.exitCode).toBe(0)
      expect(inspected.stdout).toContain('Outcome: passed')
      expect(inspected.stdout).toContain('Cache status: hit')
      expect(inspected.stdout).toContain('Artifacts:')

      // The persisted YAML remains independently executable without Déjà or a model client.
      const flowPath = String(warm.report!.yamlPath)
      const standalone = spawn(fakeMaestro, [
        '--no-ansi', '--device', 'fixture-device', 'test', join(context.root, flowPath),
      ], {
        cwd: context.root,
        env: { ...process.env, DEJA_OPENCLAW: join(context.root, 'must-not-exist') },
        stdio: 'ignore',
      })
      expect(await new Promise<number | null>(resolveExit => standalone.once('close', resolveExit))).toBe(0)
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('remember verifies and persists; a genuine classified product failure stops with failed validation', async () => {
    const context = await workspace()
    const forbiddenModel = join(context.temporary, 'model-must-not-run.json')
    try {
      const remembered = await execute(context.root, [
        'remember', 'checkout', '--flow', 'flows/candidate.yaml',
      ], { capture: forbiddenModel })
      expect(remembered.exitCode).toBe(0)
      expect(remembered.report).toMatchObject({
        command: 'remember', outcome: 'remembered', modelCalls: 0,
        cacheStatus: 'not-checked', passed: true,
      })
      expect(existsSync(forbiddenModel)).toBe(false)

      const regression = await execute(context.root, ['run', 'checkout'], { maestro: 'product', capture: forbiddenModel })
      expect(regression.exitCode).toBe(1)
      expect(regression.report).toMatchObject({
        outcome: 'product-regression', passed: false, modelCalls: 0, repairCount: 0,
        failure: { category: 'product_assertion', repairEligible: false },
      })
      expect(regression.stdout).toContain('repair was not attempted')
      expect(regression.stdout).toContain('Source diffs: none')
      expect(existsSync(forbiddenModel)).toBe(false)
      const memory = JSON.parse(await readFile(join(context.root, '.deja/memory/walks.json'), 'utf8'))
      expect(memory.walks[0].lastValidation.passed).toBe(false)
      expect(memory.walks[0].validationHistory.map((entry: { passed: boolean }) => entry.passed)).toEqual([true, false])

      const inspected = await execute(context.root, ['inspect', '.deja/reports/latest.json'])
      expect(inspected.exitCode).toBe(1)
      expect(inspected.stdout).toContain('Outcome: product-regression')
      expect(inspected.stdout).not.toContain('testing123')
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('rejected cold verification exits 3 and never creates successful memory', async () => {
    const context = await workspace()
    try {
      const rejected = await execute(context.root, ['explore', 'checkout'], {
        explorer: 'invalid', maestro: 'product',
      })
      expect(rejected.exitCode).toBe(3)
      expect(rejected.report).toMatchObject({
        command: 'explore', outcome: 'exploration-rejected', passed: false,
        repairCount: 0, yamlPath: null,
      })
      expect(rejected.stdout).toContain('Cache status: not-checked')
      expect(rejected.stdout).toContain('Source diffs: none')
      const list = await execute(context.root, ['list', '--json'])
      expect(JSON.parse(list.stdout).walks).toHaveLength(0)
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('opt-in local semantic fallback is exact-first, reports selection, and replays stored YAML', async () => {
    const context = await workspace()
    const forbiddenModel = join(context.temporary, 'semantic-model-must-not-run.json')
    try {
      await configureSemantic(context.root, { provider: 'local-lexical-v1' })
      const remembered = await execute(context.root, [
        'remember', 'complete checkout', '--flow', 'flows/candidate.yaml',
      ], { capture: forbiddenModel })
      expect(remembered.exitCode).toBe(0)

      const exact = await execute(context.root, ['run', 'complete checkout', '--json'], {
        capture: forbiddenModel,
      })
      expect(exact.exitCode).toBe(0)
      expect(exact.report).toMatchObject({
        outcome: 'passed', modelCalls: 0,
        semantic: { status: 'skipped-exact-hit', embeddingCalls: 0 },
      })

      const semantic = await execute(context.root, ['run', 'finish checkout'], {
        capture: forbiddenModel,
      })
      expect(semantic.exitCode).toBe(0)
      const report = JSON.parse(await readFile(
        join(context.root, '.deja/reports/latest.json'), 'utf8',
      ))
      expect(report).toMatchObject({
        command: 'run', cacheStatus: 'hit', outcome: 'passed', modelCalls: 0,
        identity: { goal: 'finish checkout' },
        semantic: {
          status: 'hit', provider: 'local-lexical-v1', embeddingCalls: 2,
          candidates: 1, eligibleCandidates: 1, goalSimilarity: 1,
          minimumSimilarity: 0.9, minimumMargin: 0.15,
          selectedWalk: { goal: 'complete checkout', goalKey: 'complete checkout' },
        },
      })
      expect(report.semantic.selectedWalk.flowSha256).toBe(report.yamlSha256)
      expect(semantic.stdout).toContain('Semantic lookup: hit')
      expect(semantic.stdout).toContain('Semantic embedding calls: 2')
      expect(semantic.stdout).toContain('Goal similarity: 1.000000')
      expect(semantic.stdout).toContain('Selected walk: complete checkout')
      expect(existsSync(forbiddenModel)).toBe(false)

      const memory = JSON.parse(await readFile(
        join(context.root, '.deja/memory/walks.json'), 'utf8',
      ))
      expect(memory.walks).toHaveLength(1)
      expect(memory.walks[0].goalKey).toBe('complete checkout')
      expect(memory.walks[0].validationHistory.map((entry: { passed: boolean }) => entry.passed))
        .toEqual([true, true, true])

      const inspected = await execute(context.root, ['inspect', '.deja/reports/latest.json'])
      expect(inspected.exitCode).toBe(0)
      expect(inspected.stdout).toContain('Selected walk: complete checkout')
      expect(inspected.stdout).toContain('Goal similarity: 1.000000')
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('a configured but missing embedding provider uses ordinary cold then exact behavior', async () => {
    const context = await workspace()
    const capture = join(context.temporary, 'missing-embedding-provider.json')
    try {
      await configureSemantic(context.root, { provider: 'not-installed-v1' })
      const cold = await execute(context.root, ['run', 'checkout', '--json'], { capture })
      expect(cold.exitCode).toBe(0)
      expect(cold.report).toMatchObject({
        outcome: 'cold-recorded', cacheStatus: 'miss', cacheMissReason: 'not-found', modelCalls: 2,
        semantic: {
          status: 'unavailable', reason: 'provider-unavailable',
          embeddingCalls: 0, selectedWalk: null,
        },
      })
      expect(cold.stdout).toContain('"semantic"')
      expect(existsSync(capture)).toBe(true)

      const exact = await execute(context.root, ['run', 'checkout'], { capture })
      expect(exact.exitCode).toBe(0)
      expect(exact.report).toMatchObject({
        outcome: 'passed', modelCalls: 0,
        semantic: { status: 'skipped-exact-hit', embeddingCalls: 0 },
      })
      const callsAfterCold = await readFile(capture, 'utf8')
      const repeatedExact = await execute(context.root, ['run', 'checkout'], { capture })
      expect(repeatedExact.exitCode).toBe(0)
      expect(await readFile(capture, 'utf8')).toBe(callsAfterCold)

      await configureSemantic(context.root, {
        provider: 'local-lexical-v1', minimumSimilarity: null,
      })
      const nullThreshold = await execute(context.root, ['list'])
      expect(nullThreshold.exitCode).toBe(2)
      expect(nullThreshold.stderr).toContain('between 0.9 and 1')

      await configureSemantic(context.root, {
        provider: 'local-lexical-v1', minimumMargin: 0,
      })
      const disabledMargin = await execute(context.root, ['list'])
      expect(disabledMargin.exitCode).toBe(2)
      expect(disabledMargin.stderr).toContain('between 0.15 and 1')
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('Maestro executable launch failure is a documented runner failure with exit 1', async () => {
    const context = await workspace()
    try {
      const configPath = join(context.root, 'deja.config.json')
      const config = JSON.parse(await readFile(configPath, 'utf8'))
      config.maestro.executable = join(context.root, 'missing', 'maestro')
      await writeFile(configPath, `${JSON.stringify(config, null, 2)}\n`)

      const result = await execute(context.root, [
        'remember', 'checkout', '--flow', 'flows/candidate.yaml',
      ])
      expect(result.exitCode).toBe(1)
      expect(result.stdout).toContain('Outcome: rejected')
      expect(result.stdout).toContain('Failure: runner')
      expect(result.report).toMatchObject({
        outcome: 'rejected', passed: false, failure: { category: 'runner', repairEligible: false },
        execution: { exitCode: null },
      })
    } finally {
      await cleanup(context.temporary)
    }
  })

  test('help, command errors, bad config, and inspection failures use documented exits', async () => {
    const context = await workspace()
    try {
      const help = await execute(context.root, ['--help', '--config', 'does-not-exist.json'])
      expect(help.exitCode).toBe(0)
      expect(help.stdout).toContain('deja explore <goal>')
      expect(help.stdout).toContain('Exit codes:')
      expect(help.stdout).toContain('including executable launch')
      expect(help.report).toBeNull()

      const unknown = await execute(context.root, ['unknown-command'])
      expect(unknown.exitCode).toBe(2)
      expect(unknown.stderr).toContain('Unknown command')
      const missingFlow = await execute(context.root, ['remember', 'checkout'])
      expect(missingFlow.exitCode).toBe(2)
      expect(missingFlow.stderr).toContain('requires --flow')
      const missingReport = await execute(context.root, ['inspect', 'missing-report.json'])
      expect(missingReport.exitCode).toBe(2)
      const badConfig = await execute(context.root, ['list', '--config', 'missing-config.json'])
      expect(badConfig.exitCode).toBe(2)
      expect(badConfig.stderr).toContain('Cannot read Déjà config')

      await mkdir(join(context.root, '.deja/memory'), { recursive: true })
      await writeFile(join(context.root, '.deja/memory/walks.json'), '{not json\n')
      const corrupt = await execute(context.root, ['list'])
      expect(corrupt.exitCode).toBe(4)
      expect(corrupt.stderr).toContain('Invalid walk memory')
      expect(corrupt.stderr).toContain('quarantine')
      expect(corrupt.stderr).not.toContain(context.root)
    } finally {
      await cleanup(context.temporary)
    }
  })

  describe('JUnit and summary output', () => {
    const summaryPattern =
      /^deja-summary v=1 goal="[^"\n]*" outcome=[a-z-]+ cache=[a-z-]+ fingerprint=(?:[0-9a-f]{12}|unknown) model_calls=(?:\d+|unknown) failure=[a-z_]+$/m

    test('a passing run writes Maestro\'s own report, lists it as an artifact, and prints one summary line', async () => {
      const context = await workspace()
      try {
        const junit = join(context.temporary, 'out', 'report.xml')
        const remembered = await execute(context.root, [
          'remember', 'checkout', '--flow', 'flows/candidate.yaml', '--junit', junit, '--summary',
        ])
        expect(remembered.exitCode).toBe(0)
        const passReport = await readFile(junit, 'utf8')
        expect(passReport).toContain('device="fake"')
        expect(passReport).toContain('status="SUCCESS"')
        expect(passReport).not.toContain('deja-synthesized')
        expect(JSON.stringify(remembered.report!.artifacts)).toContain('"kind":"junit"')
        expect(remembered.stdout).toMatch(summaryPattern)
        expect(remembered.stdout.trimEnd().split('\n').at(-1)).toMatch(
          /^deja-summary v=1 goal="checkout" outcome=remembered cache=not-checked fingerprint=[0-9a-f]{12} model_calls=0 failure=none$/,
        )

        // The config field works too, and the flag is optional.
        const config = JSON.parse(await readFile(join(context.root, 'deja.config.json'), 'utf8'))
        config.junit = 'reports/config.xml'
        await writeFile(join(context.root, 'deja.config.json'), JSON.stringify(config))
        const warm = await execute(context.root, ['run', 'checkout', '--json', '--summary'])
        expect(warm.exitCode).toBe(0)
        expect(await readFile(join(context.root, 'reports/config.xml'), 'utf8')).toContain('status="SUCCESS"')
        expect(JSON.parse(warm.stdout).artifacts.map((a: { kind: string }) => a.kind)).toContain('junit')
        expect(warm.stderr.trim()).toMatch(
          /^deja-summary v=1 goal="checkout" outcome=passed cache=hit fingerprint=[0-9a-f]{12} model_calls=0 failure=none$/,
        )
      } finally {
        await cleanup(context.temporary)
      }
    })

    test('a failing run keeps Maestro\'s failed report and names the failure label', async () => {
      const context = await workspace()
      try {
        const junit = join(context.temporary, 'report.xml')
        await execute(context.root, ['remember', 'checkout', '--flow', 'flows/candidate.yaml'])
        const failed = await execute(context.root, ['run', 'checkout', '--junit', junit, '--summary'], {
          maestro: 'product',
        })
        expect(failed.exitCode).toBe(1)
        const xml = await readFile(junit, 'utf8')
        expect(xml).toContain('status="ERROR"')
        expect(xml).toContain('fake maestro failure')
        expect(xml).not.toContain('deja-synthesized')
        expect(failed.stdout.trimEnd().split('\n').at(-1)).toMatch(
          /^deja-summary v=1 goal="checkout" outcome=product-regression cache=hit fingerprint=[0-9a-f]{12} model_calls=0 failure=product_assertion$/,
        )
      } finally {
        await cleanup(context.temporary)
      }
    })

    test('runs where Maestro never ran write a failed report and clear any stale one', async () => {
      const context = await workspace()
      try {
        const junit = join(context.temporary, 'report.xml')
        await execute(context.root, ['remember', 'checkout', '--flow', 'flows/candidate.yaml'])
        // A new build with no repairer: the run is refused before Maestro starts.
        await writeFile(join(context.root, 'app/build/app.apk'), 'a different build')
        await mkdir(join(context.temporary), { recursive: true })
        await writeFile(junit, '<testsuites><testsuite tests="1" failures="0"><testcase name="checkout" status="SUCCESS"/></testsuite></testsuites>')
        const refused = await execute(context.root, ['run', 'checkout', '--junit', junit, '--summary'])
        expect(refused.exitCode).toBe(1)
        expect(refused.report).toMatchObject({ outcome: 'repair-rejected', passed: false })
        const xml = await readFile(junit, 'utf8')
        expect(xml).toContain('deja-synthesized')
        expect(xml).toContain('status="ERROR"')
        expect(xml).toContain('outcome: repair-rejected')
        expect(xml).not.toContain('status="SUCCESS"')
        expect(xml).not.toContain(context.root)
        expect(JSON.stringify(refused.report!.artifacts)).toContain('"kind":"junit"')
        expect(refused.stdout.trimEnd().split('\n').at(-1)).toMatch(
          /^deja-summary v=1 goal="checkout" outcome=repair-rejected cache=miss fingerprint=[0-9a-f]{12} model_calls=0 failure=unclassified$/,
        )

        // Maestro cannot launch: no report from Maestro, so a failed one is written.
        const config = JSON.parse(await readFile(join(context.root, 'deja.config.json'), 'utf8'))
        config.maestro.executable = join(context.root, 'missing', 'maestro')
        await writeFile(join(context.root, 'deja.config.json'), JSON.stringify(config))
        const launch = await execute(context.root, [
          'remember', 'checkout', '--flow', 'flows/candidate.yaml', '--junit', junit,
        ])
        expect(launch.exitCode).toBe(1)
        expect(await readFile(junit, 'utf8')).toContain('deja-synthesized')
      } finally {
        await cleanup(context.temporary)
      }
    })

    test('invalid memory (exit 4) and usage errors after parsing still leave an errored report', async () => {
      const context = await workspace()
      try {
        const junit = join(context.temporary, 'report.xml')
        await mkdir(join(context.root, '.deja/memory'), { recursive: true })
        await writeFile(join(context.root, '.deja/memory/walks.json'), '{not json\n')
        const corrupt = await execute(context.root, ['run', 'checkout', '--junit', junit, '--summary'])
        expect(corrupt.exitCode).toBe(4)
        const xml = await readFile(junit, 'utf8')
        expect(xml).toContain('status="ERROR"')
        expect(xml).toContain('Invalid walk memory')
        expect(corrupt.stdout.trim()).toBe(
          'deja-summary v=1 goal="checkout" outcome=error cache=unknown fingerprint=unknown model_calls=unknown failure=error',
        )

        const wrong = await execute(context.root, ['list', '--junit', junit])
        expect(wrong.exitCode).toBe(2)
        expect(wrong.stderr).toContain('--junit is valid only')
      } finally {
        await cleanup(context.temporary)
      }
    })
  })
})
