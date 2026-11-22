import { spawn } from 'node:child_process'
import { appendFileSync, existsSync, readFileSync } from 'node:fs'
import { mkdir, rename, writeFile } from 'node:fs/promises'
import { dirname, relative, resolve } from 'node:path'

export type FixtureCredential = { label: string; value: string }

export type ExploreConfiguration = {
  workspaceRoot: string
  appId: string
  platform: 'android'
  buildPath: string
  device: string
  fixtureCredentials: FixtureCredential[]
  openClawModel: string
  openClawBaseUrl: string
  explorationTimeoutSeconds: number
  verificationTimeoutMs: number
}

export type OpenClawEnvelope = {
  ok?: boolean
  status?: string
  final?: string
  assistantTurns?: number
  model?: string | null
  provider?: string | null
  sessionId?: string
  error?: { message?: string; kind?: string }
}

export type VerificationDetails = {
  passed: boolean
  [key: string]: unknown
}

export type PersistenceDetails = {
  flowPath: string
  flowSha256: string
  [key: string]: unknown
}

export type ExplorationMetrics = {
  explorationDurationMs: number | null
  verificationDurationMs: number | null
  mcpCalls: number | null
  modelCalls: number | null
  modelCallBasis: string
}

export type ColdExploreHooks = {
  clearAppState: (phase: 'exploration' | 'verification') => Promise<void>
  verifyCandidate: (candidatePath: string, attemptDirectory: string) => Promise<VerificationDetails>
  persistVerified: (
    candidatePath: string,
    verification: VerificationDetails,
    metrics: ExplorationMetrics,
  ) => Promise<PersistenceDetails>
}

export type ColdExploreOptions = {
  goal: string
  configuration: ExploreConfiguration
  buildFingerprint: string
  attemptDirectory: string
  openClawExecutable: string
  maestroMcpProxy: string
  maestroMcpEnvironment: Record<string, string>
}

export type ColdExploreReport = {
  goalKey: string
  goal: string
  appId: string
  platform: 'android'
  buildFingerprint: string
  buildPath: string
  workspaceRoot: string
  cacheStatus: 'recorded' | 'rejected' | 'error'
  passed: boolean
  durationMs: number
  explorationDurationMs: number | null
  verificationDurationMs: number | null
  mcpCalls: number | null
  mcpCallBasis: string
  modelCalls: number | null
  modelCallBasis: string
  model: string
  provider: string | null
  explorerExitCode: number | null
  attemptDirectory: string
  candidatePath: string | null
  activityPath: string
  mcpActivityPath: string
  openClawResultPath: string | null
  flowPath: string | null
  flowSha256: string | null
  transport: 'maestro-mcp-stdio'
  accessPolicy: {
    allowedTools: string[]
    referenceFlow: 'denied'
    appSource: 'denied'
    candidateDestination: 'attempt-only'
  }
  cleanState: {
    exploration: boolean
    independentVerification: boolean
    candidateClearStateLaunch: boolean
  }
  verification: VerificationDetails | null
  persistence: PersistenceDetails | null
  errors: string[]
}

function insideWorkspace(root: string, candidate: string) {
  const path = resolve(root, candidate)
  return path === root || path.startsWith(`${resolve(root)}/`)
}

export function loadExploreConfiguration(configPath: string): ExploreConfiguration {
  const raw = JSON.parse(readFileSync(resolve(configPath), 'utf8')) as Record<string, unknown>
  const workspaceRoot = resolve(dirname(resolve(configPath)), String(raw.workspaceRoot ?? ''))
  const appId = typeof raw.appId === 'string' ? raw.appId.trim() : ''
  const buildPath = typeof raw.buildPath === 'string' ? raw.buildPath : ''
  if (raw.platform !== 'android') throw new Error('WALK-003 supports only platform: android')
  if (!appId) throw new Error('Explore config requires an explicit appId')
  if (!raw.workspaceRoot || !buildPath || !insideWorkspace(workspaceRoot, buildPath)) {
    throw new Error('Explore config requires explicit workspaceRoot and a buildPath inside it')
  }
  if (!existsSync(workspaceRoot) || !existsSync(resolve(workspaceRoot, buildPath))) {
    throw new Error('Explore workspaceRoot and buildPath must already exist')
  }
  if (typeof raw.device !== 'string' || !raw.device.trim()) {
    throw new Error('Explore config requires an explicit Android device')
  }
  if (typeof raw.openClawModel !== 'string' || !raw.openClawModel.includes('/')) {
    throw new Error('Explore config requires an explicit provider/model OpenClaw model')
  }
  if (typeof raw.openClawBaseUrl !== 'string' || !raw.openClawBaseUrl.startsWith('https://')) {
    throw new Error('Explore config requires an explicit HTTPS OpenClaw provider base URL')
  }
  const explorationTimeoutSeconds = Number(raw.explorationTimeoutSeconds)
  const verificationTimeoutMs = Number(raw.verificationTimeoutMs)
  if (!Number.isInteger(explorationTimeoutSeconds) || explorationTimeoutSeconds <= 0) {
    throw new Error('explorationTimeoutSeconds must be a positive integer')
  }
  if (!Number.isInteger(verificationTimeoutMs) || verificationTimeoutMs <= 0) {
    throw new Error('verificationTimeoutMs must be a positive integer')
  }
  if (!Array.isArray(raw.fixtureCredentials)) throw new Error('fixtureCredentials must be an explicit array')
  const fixtureCredentials = raw.fixtureCredentials.map((entry, index) => {
    const credential = entry as Record<string, unknown>
    if (typeof credential.label !== 'string' || typeof credential.value !== 'string' ||
        !credential.label.trim() || credential.value.length > 4096) {
      throw new Error(`fixtureCredentials[${index}] requires a label and a bounded fixture value`)
    }
    return { label: credential.label, value: credential.value }
  })
  return {
    workspaceRoot,
    appId,
    platform: 'android',
    buildPath,
    device: raw.device,
    fixtureCredentials,
    openClawModel: raw.openClawModel,
    openClawBaseUrl: raw.openClawBaseUrl,
    explorationTimeoutSeconds,
    verificationTimeoutMs,
  }
}

export function explorerPrompt(goal: string, configuration: ExploreConfiguration) {
  const credentials = configuration.fixtureCredentials
    .map(({ label, value }) => `- ${label}: ${value}`)
    .join('\n')
  return `You are the WalkCache cold Android explorer. Explore only through the provided Maestro MCP tools.\n\nGOAL:\n${goal.trim()}\n\nALLOWED CONFIGURATION:\n- platform: android\n- appId: ${configuration.appId}\n- device: ${configuration.device}\n${credentials}\n\nThe values above are deterministic fixture credentials, not personal credentials. Do not seek or use any other credentials.\n\nDENIED INPUTS AND TOOLS:\n- Do not read, search, infer, or request app source, repository files, existing/reference flows, successful cache memory, reports, or prior attempt artifacts.\n- Filesystem, shell, web, session, memory, and subagent tools are denied. Only the Maestro MCP tools are available.\n- For Maestro run, use inline yaml only. Never use files, dir, cloud, viewer, or external-flow features.\n- The only writable candidate is your final response; WalkCache writes it to a fresh attempt, never to successful storage.\n\nMETHOD:\n1. Use Maestro MCP inspect_screen and inline run calls on the configured device to discover and validate the journey empirically.\n2. Start the app with launchApp using the exact appId and clearState: true. Begin from clean state and include meaningful visible assertions around each transition, including the final product outcome.\n3. Keep failed branches in MCP activity; do not hide them. Use stable observed Android IDs when available and visible text otherwise.\n4. Author one complete standalone Maestro YAML flow with appId: ${configuration.appId}, a launchApp clearState: true step, and assertVisible assertions. Do not copy or refer to any external flow.\n5. Run your candidate inline through MCP before finishing. Then return ONLY the complete candidate YAML (no Markdown fence, explanation, paths, or extra text).\n`
}

export function extractCandidateYaml(finalText: string) {
  const trimmed = finalText.trim()
  const fenced = /^```(?:yaml|yml)?\s*\n([\s\S]*?)\n```$/m.exec(trimmed)
  const candidate = (fenced?.[1] ?? trimmed).trim()
  if (!candidate) throw new Error('OpenClaw returned no candidate YAML')
  if (!candidate.startsWith('appId:')) throw new Error('Candidate response must contain only YAML beginning with appId')
  return `${candidate}\n`
}

export function validateCandidate(candidate: string, appId: string) {
  const appDeclaration = new RegExp(`^appId:\\s*${appId.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\s*$`, 'm')
  if (!appDeclaration.test(candidate)) throw new Error('Candidate appId does not match the explicit fixture app')
  if (!/-\s+launchApp:\s*\n(?:(?:\s+.*\n))*?\s+clearState:\s*true\s*$/m.test(candidate) &&
      !/-\s+launchApp:\s*[^\n]*clearState:\s*true/m.test(candidate)) {
    throw new Error('Candidate must launch the app with clearState: true')
  }
  if (!/-\s+assertVisible:/m.test(candidate)) throw new Error('Candidate has no meaningful assertVisible assertion')
  if (/^\s*-\s+(?:runScript|runFlow):/m.test(candidate)) {
    throw new Error('Candidate scripts and external flows are denied; use only self-contained inline exploration')
  }
}

export function openClawConfig(options: ColdExploreOptions) {
  const [provider, ...modelParts] = options.configuration.openClawModel.split('/')
  const modelId = modelParts.join('/')
  if (!provider || !modelId) throw new Error('OpenClaw model must be an explicit provider/model')
  return {
    models: {
      providers: {
        [provider]: {
          baseUrl: options.configuration.openClawBaseUrl,
          api: 'openai-completions',
          models: [{
            id: modelId,
            name: modelId,
            reasoning: true,
            input: ['text'],
            contextWindow: 262_144,
            maxTokens: 32_768,
            compat: { supportsTools: true },
          }],
        },
      },
    },
    agents: {
      defaults: {
        workspace: resolve(options.attemptDirectory, 'workspace'),
        skills: [],
        contextInjection: 'never',
      },
    },
    tools: {
      allow: ['maestro__*'],
      fs: { workspaceOnly: true },
      codeMode: { enabled: false },
    },
    mcp: {
      servers: {
        maestro: {
          command: process.execPath,
          args: [options.maestroMcpProxy],
          cwd: resolve(options.attemptDirectory, 'workspace'),
          transport: 'stdio',
          connectionTimeoutMs: 15_000,
          requestTimeoutMs: 180_000,
          toolFilter: {
            include: ['list_devices', 'take_screenshot', 'run', 'inspect_screen', 'cheat_sheet'],
          },
          env: {
            ...options.maestroMcpEnvironment,
            WALKCACHE_MCP_LOG: resolve(options.attemptDirectory, 'mcp-activity.jsonl'),
          },
        },
      },
    },
  }
}

async function atomicWrite(path: string, contents: string) {
  await mkdir(dirname(path), { recursive: true })
  const temporary = `${path}.${process.pid}.tmp`
  await writeFile(temporary, contents)
  await rename(temporary, path)
}

function activity(attemptDirectory: string, event: string, details: Record<string, unknown> = {}) {
  appendFileSync(resolve(attemptDirectory, 'activity.jsonl'), `${JSON.stringify({
    timestamp: new Date().toISOString(), event, ...details,
  })}\n`)
}

function relativePath(root: string, path: string) {
  return relative(root, path)
}

async function invokeOpenClaw(
  options: ColdExploreOptions,
  promptPath: string,
  configPath: string,
): Promise<{
  exitCode: number | null
  timedOut: boolean
  durationMs: number
  envelope: OpenClawEnvelope | null
}> {
  const stdoutPath = resolve(options.attemptDirectory, 'openclaw.stdout.log')
  const stderrPath = resolve(options.attemptDirectory, 'openclaw.stderr.log')
  const started = performance.now()
  const child = spawn(options.openClawExecutable, [
    'agent', 'exec',
    '--message-file', promptPath,
    '--cwd', resolve(options.attemptDirectory, 'workspace'),
    '--config', configPath,
    '--model', options.configuration.openClawModel,
    '--thinking', 'off',
    '--code-mode', 'direct',
    '--timeout', String(options.configuration.explorationTimeoutSeconds),
    '--json',
  ], {
    cwd: resolve(options.attemptDirectory, 'workspace'),
    stdio: ['ignore', 'pipe', 'pipe'],
    detached: process.platform !== 'win32',
    windowsHide: true,
  })
  const stdout: string[] = []
  const stderr: string[] = []
  child.stdout.setEncoding('utf8')
  child.stderr.setEncoding('utf8')
  child.stdout.on('data', chunk => stdout.push(chunk))
  child.stderr.on('data', chunk => stderr.push(chunk))
  let timedOut = false
  const timer = setTimeout(() => {
    timedOut = true
    if (child.pid) {
      try {
        process.kill(process.platform === 'win32' ? child.pid : -child.pid, 'SIGTERM')
      } catch {
        // The child close event remains the source of truth.
      }
    }
  }, options.configuration.explorationTimeoutSeconds * 1_000 + 15_000)
  const exitCode = await new Promise<number | null>(resolveExit => {
    let settled = false
    child.once('error', () => {
      if (!settled) {
        settled = true
        resolveExit(null)
      }
    })
    child.once('close', code => {
      if (!settled) {
        settled = true
        resolveExit(code)
      }
    })
  })
  clearTimeout(timer)
  await writeFile(stdoutPath, stdout.join(''))
  await writeFile(stderrPath, stderr.join(''))
  let envelope: OpenClawEnvelope | null = null
  try {
    envelope = JSON.parse(stdout.join('')) as OpenClawEnvelope
    await atomicWrite(
      resolve(options.attemptDirectory, 'openclaw-result.json'),
      `${JSON.stringify(envelope, null, 2)}\n`,
    )
  } catch {
    // Raw stdout/stderr are retained in ignored attempt artifacts for diagnosis.
  }
  return { exitCode, timedOut, durationMs: Math.round(performance.now() - started), envelope }
}

function mcpCallCount(path: string): number | null {
  if (!existsSync(path)) return null
  const contents = readFileSync(path, 'utf8')
  return contents.split('\n').filter(Boolean).filter(line => {
    try {
      const entry = JSON.parse(line) as { direction?: string; method?: string }
      return entry.direction === 'request' && entry.method === 'tools/call'
    } catch {
      return false
    }
  }).length
}

export async function runColdExploration(options: ColdExploreOptions, hooks: ColdExploreHooks): Promise<ColdExploreReport> {
  const root = options.configuration.workspaceRoot
  const attempt = resolve(options.attemptDirectory)
  if (!insideWorkspace(root, attempt) || attempt === root) throw new Error('Attempt directory must be inside the workspace')
  await mkdir(resolve(attempt, 'workspace'), { recursive: true })
  await writeFile(resolve(attempt, 'activity.jsonl'), '')
  const started = performance.now()
  const errors: string[] = []
  let explorationDurationMs: number | null = null
  let verificationDurationMs: number | null = null
  let envelope: OpenClawEnvelope | null = null
  let explorerExitCode: number | null = null
  let candidatePath: string | null = null
  let verification: VerificationDetails | null = null
  let persistence: PersistenceDetails | null = null
  let mcpCalls: number | null = null
  let explorationClean = false
  let verificationClean = false
  let candidateClearState = false
  let cacheStatus: ColdExploreReport['cacheStatus'] = 'error'
  let resultPath: string | null = null
  const mcpPath = resolve(attempt, 'mcp-activity.jsonl')

  try {
    activity(attempt, 'exploration_started', { platform: 'android', appId: options.configuration.appId })
    await hooks.clearAppState('exploration')
    explorationClean = true
    activity(attempt, 'clean_state_succeeded', { phase: 'exploration' })

    const promptPath = resolve(attempt, 'prompt.txt')
    const configPath = resolve(attempt, 'openclaw-config.json')
    await atomicWrite(promptPath, explorerPrompt(options.goal, options.configuration))
    await atomicWrite(configPath, `${JSON.stringify(openClawConfig(options), null, 2)}\n`)
    activity(attempt, 'openclaw_started', {
      model: options.configuration.openClawModel,
      invocation: 'openclaw agent exec --json',
      allowedTools: ['maestro__*'],
    })
    const invocation = await invokeOpenClaw(options, promptPath, configPath)
    explorationDurationMs = invocation.durationMs
    explorerExitCode = invocation.exitCode
    envelope = invocation.envelope
    resultPath = envelope ? relativePath(root, resolve(attempt, 'openclaw-result.json')) : null
    activity(attempt, 'openclaw_finished', {
      exitCode: invocation.exitCode,
      timedOut: invocation.timedOut,
      resultParsed: envelope !== null,
    })
    if (invocation.timedOut) throw new Error('OpenClaw exploration exceeded its process timeout')
    if (invocation.exitCode !== 0 || !envelope?.ok || !envelope.final) {
      throw new Error(envelope?.error?.message ?? `OpenClaw exploration exited with ${String(invocation.exitCode)}`)
    }

    const candidate = extractCandidateYaml(envelope.final)
    validateCandidate(candidate, options.configuration.appId)
    candidateClearState = true
    candidatePath = resolve(attempt, 'candidate.yaml')
    await atomicWrite(candidatePath, candidate)
    activity(attempt, 'candidate_authored', { destination: relativePath(root, candidatePath) })
    cacheStatus = 'rejected'

    await hooks.clearAppState('verification')
    verificationClean = true
    activity(attempt, 'clean_state_succeeded', { phase: 'verification' })
    const verificationStarted = performance.now()
    verification = await hooks.verifyCandidate(candidatePath, attempt)
    verificationDurationMs = Math.round(performance.now() - verificationStarted)
    activity(attempt, 'independent_verification_finished', { passed: verification.passed })
    if (verification.passed) {
      mcpCalls = mcpCallCount(mcpPath)
      const modelCalls = typeof envelope?.assistantTurns === 'number' && Number.isInteger(envelope.assistantTurns)
        ? envelope.assistantTurns
        : null
      persistence = await hooks.persistVerified(candidatePath, verification, {
        explorationDurationMs,
        verificationDurationMs,
        mcpCalls,
        modelCalls,
        modelCallBasis: modelCalls === null ? 'unknown' : 'openclaw agent exec assistantTurns (completed round trips)',
      })
      cacheStatus = 'recorded'
      activity(attempt, 'successful_memory_persisted', {
        flowPath: persistence.flowPath,
        flowSha256: persistence.flowSha256,
      })
    } else {
      errors.push('Independent WALK-002 Maestro verification rejected the candidate')
      activity(attempt, 'candidate_rejected', { reason: 'independent_verification_failed' })
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    errors.push(message)
    cacheStatus = candidatePath ? 'rejected' : 'error'
    activity(attempt, 'exploration_failed', { message })
  }

  mcpCalls = mcpCallCount(mcpPath)
  const modelCalls = typeof envelope?.assistantTurns === 'number' && Number.isInteger(envelope.assistantTurns)
    ? envelope.assistantTurns
    : null
  const report: ColdExploreReport = {
    goalKey: options.goal.trim().toLowerCase().replace(/\s+/g, ' '),
    goal: options.goal,
    appId: options.configuration.appId,
    platform: 'android',
    buildFingerprint: options.buildFingerprint,
    buildPath: options.configuration.buildPath,
    workspaceRoot: root,
    cacheStatus,
    passed: cacheStatus === 'recorded' && verification?.passed === true && persistence !== null,
    durationMs: Math.round(performance.now() - started),
    explorationDurationMs,
    verificationDurationMs,
    mcpCalls,
    mcpCallBasis: mcpCalls === null ? 'unknown' : 'maestro-mcp-proxy tools/call requests',
    modelCalls,
    modelCallBasis: modelCalls === null ? 'unknown' : 'openclaw agent exec assistantTurns (completed round trips)',
    model: envelope?.model ?? options.configuration.openClawModel,
    provider: envelope?.provider ?? null,
    explorerExitCode,
    attemptDirectory: relativePath(root, attempt),
    candidatePath: candidatePath ? relativePath(root, candidatePath) : null,
    activityPath: relativePath(root, resolve(attempt, 'activity.jsonl')),
    mcpActivityPath: relativePath(root, mcpPath),
    openClawResultPath: resultPath,
    flowPath: persistence?.flowPath ?? null,
    flowSha256: persistence?.flowSha256 ?? null,
    transport: 'maestro-mcp-stdio',
    accessPolicy: {
      allowedTools: ['maestro__*'],
      referenceFlow: 'denied',
      appSource: 'denied',
      candidateDestination: 'attempt-only',
    },
    cleanState: {
      exploration: explorationClean,
      independentVerification: verificationClean,
      candidateClearStateLaunch: candidateClearState,
    },
    verification,
    persistence,
    errors,
  }
  activity(attempt, 'exploration_reported', {
    passed: report.passed,
    cacheStatus,
    modelCalls,
    mcpCalls,
    explorationDurationMs,
    verificationDurationMs,
  })
  await atomicWrite(resolve(attempt, 'report.json'), `${JSON.stringify(report, null, 2)}\n`)
  return report
}
