import { afterEach, describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import {
  extractCandidateYaml,
  runColdExploration,
  validateCandidate,
  type ColdExploreHooks,
  type ColdExploreOptions,
  type ExplorationMetrics,
} from '../src/index.ts'

const fakeOpenClaw = resolve(import.meta.dir, 'fixtures/fake-openclaw.cjs')
const environmentKeys = [
  'WALKCACHE_FAKE_OPENCLAW_CAPTURE',
  'WALKCACHE_FAKE_OPENCLAW_MCP_CALLS',
  'WALKCACHE_FAKE_OPENCLAW_MODE',
  'WALKCACHE_FAKE_OPENCLAW_NO_TURNS',
  'WALKCACHE_FAKE_OPENCLAW_SKIP_MCP_LOG',
  'WALKCACHE_FAKE_OPENCLAW_TURNS',
  'WALKCACHE_FAKE_OPENCLAW_FENCED',
] as const
const originalEnvironment = Object.fromEntries(environmentKeys.map(key => [key, process.env[key]]))

afterEach(() => {
  for (const key of environmentKeys) {
    const value = originalEnvironment[key]
    if (value === undefined) delete process.env[key]
    else process.env[key] = value
  }
})

async function fixture(goal = 'checkout') {
  const temporary = await mkdtemp(join(tmpdir(), 'walkcache-openclaw-'))
  const workspaceRoot = join(temporary, 'workspace')
  const buildPath = 'app/build/app-debug.apk'
  await mkdir(join(workspaceRoot, 'app/build'), { recursive: true })
  await writeFile(join(workspaceRoot, buildPath), 'fixture apk')
  const configuration = {
    workspaceRoot,
    appId: 'dev.walkcache.checkout',
    platform: 'android' as const,
    buildPath,
    device: 'emulator-5554',
    fixtureCredentials: [
      { label: 'Fixture email', value: 'qa@walkcache.dev' },
      { label: 'Fixture password', value: 'testing123' },
    ],
    openClawModel: 'cerebras/moonshotai-kimi-k3.1-pro-trial',
    openClawBaseUrl: 'https://api.cerebras.ai/v1',
    explorationTimeoutSeconds: 30,
    verificationTimeoutMs: 120_000,
  }
  const options: ColdExploreOptions = {
    goal,
    configuration,
    buildFingerprint: 'a'.repeat(64),
    attemptDirectory: join(workspaceRoot, '.walkcache/attempts/explore-test'),
    openClawExecutable: fakeOpenClaw,
    maestroMcpProxy: join(workspaceRoot, 'unused-proxy.ts'),
    maestroMcpEnvironment: {
      WALKCACHE_MAESTRO: '/fixture/maestro',
      WALKCACHE_DEVICE: configuration.device,
    },
  }
  return { temporary, workspaceRoot, options }
}

async function cleanup(temporary: string) {
  await rm(temporary, { recursive: true, force: true })
}

function verification(passed: boolean) {
  return {
    runner: 'WALK-002 runMaestro',
    passed,
    endedAt: '2026-11-22T00:00:00.000Z',
    durationMs: 25,
  }
}

describe('self-contained OpenClaw cold exploration', () => {
  test('pins an MCP-only explorer and persists only after independent verification', async () => {
    const { temporary, workspaceRoot, options } = await fixture()
    const capturePath = join(temporary, 'invocation.json')
    process.env.WALKCACHE_FAKE_OPENCLAW_CAPTURE = capturePath
    process.env.WALKCACHE_FAKE_OPENCLAW_MCP_CALLS = '2'
    process.env.WALKCACHE_FAKE_OPENCLAW_TURNS = '3'
    const phases: string[] = []
    let observedMetrics: ExplorationMetrics | null = null
    const hooks: ColdExploreHooks = {
      clearAppState: async phase => { phases.push(phase) },
      verifyCandidate: async () => {
        phases.push('verify')
        return verification(true)
      },
      persistVerified: async (_candidate, _verified, metrics) => {
        phases.push('persist')
        observedMetrics = metrics
        await mkdir(join(workspaceRoot, '.walkcache/flows/checkout'), { recursive: true })
        await writeFile(join(workspaceRoot, '.walkcache/flows/checkout/android.yaml'), 'verified flow\n')
        return {
          flowPath: '.walkcache/flows/checkout/android.yaml',
          flowSha256: 'b'.repeat(64),
          storedMetadataPath: '.walkcache/memory/walks.json',
        }
      },
    }
    try {
      const report = await runColdExploration(options, hooks)
      expect(report.passed).toBe(true)
      expect(report.cacheStatus).toBe('recorded')
      expect(report.modelCalls).toBe(3)
      expect(report.modelCallBasis).toContain('assistantTurns')
      expect(report.mcpCalls).toBe(2)
      expect(report.explorationDurationMs).not.toBeNull()
      expect(report.verificationDurationMs).not.toBeNull()
      expect(report.cleanState).toEqual({
        exploration: true,
        independentVerification: true,
        candidateClearStateLaunch: true,
      })
      expect(phases).toEqual(['exploration', 'verification', 'verify', 'persist'])
      expect(observedMetrics).toMatchObject({ modelCalls: 3, mcpCalls: 2 })
      expect(await readFile(join(options.attemptDirectory, 'candidate.yaml'), 'utf8')).toContain('clearState: true')

      const captured = JSON.parse(await readFile(capturePath, 'utf8'))
      expect(captured.args).toContain('agent')
      expect(captured.args).toContain('exec')
      expect(captured.args).toContain('--config')
      expect(captured.config.tools.allow).toEqual(['maestro__*'])
      expect(captured.config.tools.fs.workspaceOnly).toBe(true)
      expect(captured.config.models.providers.cerebras.baseUrl).toBe('https://api.cerebras.ai/v1')
      expect(captured.config.agents.defaults.contextInjection).toBe('never')
      expect(captured.config.mcp.servers.maestro.toolFilter.include).not.toContain('run_on_cloud')
      expect(captured.config.mcp.servers.maestro.cwd).toBe(join(options.attemptDirectory, 'workspace'))
      expect(captured.config.mcp.servers.maestro.env.WALKCACHE_MCP_LOG).toBe(
        join(options.attemptDirectory, 'mcp-activity.jsonl'),
      )
      expect(JSON.stringify(captured.config)).not.toContain('testing123')
    } finally {
      await cleanup(temporary)
    }
  })

  test('a rejected candidate cannot create or replace known-good memory', async () => {
    const { temporary, workspaceRoot, options } = await fixture()
    process.env.WALKCACHE_FAKE_OPENCLAW_MODE = 'invalid'
    process.env.WALKCACHE_FAKE_OPENCLAW_NO_TURNS = '1'
    const flowPath = join(workspaceRoot, '.walkcache/flows/checkout/android.yaml')
    const memoryPath = join(workspaceRoot, '.walkcache/memory/walks.json')
    await mkdir(join(workspaceRoot, '.walkcache/flows/checkout'), { recursive: true })
    await mkdir(join(workspaceRoot, '.walkcache/memory'), { recursive: true })
    await writeFile(flowPath, 'last known good flow\n')
    await writeFile(memoryPath, '{"schemaVersion":1,"walks":["known-good"]}\n')
    let persistCalled = false
    const hooks: ColdExploreHooks = {
      clearAppState: async () => {},
      verifyCandidate: async () => verification(false),
      persistVerified: async () => {
        persistCalled = true
        throw new Error('must not persist')
      },
    }
    try {
      const report = await runColdExploration(options, hooks)
      expect(report.passed).toBe(false)
      expect(report.cacheStatus).toBe('rejected')
      expect(report.modelCalls).toBeNull()
      expect(report.modelCallBasis).toBe('unknown')
      expect(report.mcpCalls).toBe(0)
      expect(report.verification?.passed).toBe(false)
      expect(persistCalled).toBe(false)
      expect(report.candidatePath).toContain('.walkcache/attempts/')
      expect(await readFile(flowPath, 'utf8')).toBe('last known good flow\n')
      expect(await readFile(memoryPath, 'utf8')).toBe('{"schemaVersion":1,"walks":["known-good"]}\n')
      expect(await readFile(join(options.attemptDirectory, 'candidate.yaml'), 'utf8')).toContain('WC-9999')
    } finally {
      await cleanup(temporary)
    }
  })

  test('extracts fenced YAML and enforces the app plus a fresh clearState launch', async () => {
    const { temporary, options } = await fixture()
    process.env.WALKCACHE_FAKE_OPENCLAW_FENCED = '1'
    try {
      const fenced = await runColdExploration(options, {
        clearAppState: async () => {},
        verifyCandidate: async () => verification(true),
        persistVerified: async () => ({
          flowPath: '.walkcache/flows/checkout/android.yaml',
          flowSha256: 'b'.repeat(64),
        }),
      })
      expect(fenced.passed).toBe(true)
      expect(extractCandidateYaml('```yaml\nappId: dev.walkcache.checkout\n```')).toBe(
        'appId: dev.walkcache.checkout\n',
      )
      expect(() => validateCandidate('appId: dev.walkcache.checkout\n- assertVisible: Hello\n', options.configuration.appId))
        .toThrow('clearState: true')
      expect(() => validateCandidate('appId: other.app\n- launchApp:\n    clearState: true\n- assertVisible: Hello\n', options.configuration.appId))
        .toThrow('appId')
      expect(() => validateCandidate('appId: dev.walkcache.checkout\n- launchApp:\n    clearState: true\n- assertVisible: Hello\n- runFlow: reference.yaml\n', options.configuration.appId))
        .toThrow('external flows')
    } finally {
      await cleanup(temporary)
    }
  })

  test('leaves model and MCP metrics unknown when instrumentation is absent', async () => {
    const { temporary, options } = await fixture()
    process.env.WALKCACHE_FAKE_OPENCLAW_NO_TURNS = '1'
    process.env.WALKCACHE_FAKE_OPENCLAW_SKIP_MCP_LOG = '1'
    try {
      const report = await runColdExploration(options, {
        clearAppState: async () => {},
        verifyCandidate: async () => verification(false),
        persistVerified: async () => {
          throw new Error('must not persist')
        },
      })
      expect(report.modelCalls).toBeNull()
      expect(report.mcpCalls).toBeNull()
      expect(report.mcpCallBasis).toBe('unknown')
      expect(report.modelCallBasis).toBe('unknown')
      expect(report.explorationDurationMs).toBeGreaterThanOrEqual(0)
      expect(report.verificationDurationMs).toBeGreaterThanOrEqual(0)
    } finally {
      await cleanup(temporary)
    }
  })
})
