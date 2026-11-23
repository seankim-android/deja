import { mkdir, readdir, readFile, rename, rm, stat, writeFile } from 'node:fs/promises'
import { dirname, relative, resolve } from 'node:path'
import {
  evaluateGate,
  gatePassed,
  normalizeGoal,
  parseWalkManifest,
  type GateFlakyFailure,
  type GateGoalResult,
  type RunResult,
  type WalkStore,
} from '@deja-qa/core'
import type { MaestroExecutionAdapter } from '@deja-qa/maestro'
import { CliConfigurationError, type ProductionConfiguration } from './config.ts'
import { redactJson, type Redactor } from './redaction.ts'

/** The gate is closed: at least one goal did not pass on this build. */
export const GATE_CLOSED_EXIT = 10
/** The gate could not decide: the store or the build could not be read, or the evidence not written. */
export const GATE_ERROR_EXIT = 11

export type GateOptions = {
  goalsPath: string
  buildPath?: string
  label?: string
  evidencePath?: string
}

export type GateEvidenceGoal = GateGoalResult & {
  /** The run report of the run that recorded this validation, when one is still on disk. */
  reportPath: string | null
}

/** The evidence file (JSON). The text table beside it carries the same facts. */
export type GateEvidence = {
  schemaVersion: 1
  command: 'gate'
  verdict: 'passed' | 'closed' | 'error'
  passed: boolean
  checkedAt: string
  /** Null when the build could not be fingerprinted. */
  buildFingerprint: string | null
  /** As the host gave it. The gate records it and never checks it. */
  label: string | null
  build: string
  identity: { appId: string; platform: string; adapter: string | null; browser: string | null }
  manifest: string | null
  goals: GateEvidenceGoal[]
  error?: string
}

const MAX_REPORT_BYTES = 2_000_000

/**
 * A plain list (one goal per line; blank lines and lines starting with `#` are skipped), a JSON
 * array of strings, or a walk manifest (`deja.walks.json`, whose entries name their goals).
 */
export function parseGoals(text: string, label: string): string[] {
  const trimmed = text.trim()
  let goals: string[]
  if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
    let value: unknown
    try { value = JSON.parse(trimmed) } catch (error) {
      throw new CliConfigurationError(`${label} looks like JSON but cannot be parsed`, { cause: error })
    }
    if (Array.isArray(value)) {
      if (!value.every(entry => typeof entry === 'string')) {
        throw new CliConfigurationError(`${label} must be an array of goal strings`)
      }
      goals = value as string[]
    } else {
      try {
        goals = parseWalkManifest(trimmed).walks.map(entry => entry.goal)
      } catch (error) {
        throw new CliConfigurationError(
          `${label} is JSON but neither an array of goals nor a walk manifest: ${error instanceof Error ? error.message : String(error)}`,
          { cause: error },
        )
      }
    }
  } else {
    goals = text.split(/\r?\n/).map(line => line.trim()).filter(line => line && !line.startsWith('#'))
  }
  const unique = new Map<string, string>()
  for (const goal of goals) {
    if (!goal.trim()) throw new CliConfigurationError(`${label} contains an empty goal`)
    const { key, text: normalized } = normalizeGoal(goal)
    if (!unique.has(key)) unique.set(key, normalized)
  }
  if (unique.size === 0) throw new CliConfigurationError(`${label} lists no goals; a gate with no goals never passes`)
  return [...unique.values()]
}

type ReportFact = { path: string; goalKey: string; buildFingerprint: string; outcome: string; at: string[] }

/** Read-only scan of `.deja/reports`. An unreadable or foreign file is skipped, never trusted. */
async function readReportFacts(workspaceRoot: string): Promise<ReportFact[]> {
  const directory = resolve(workspaceRoot, '.deja/reports')
  let names: string[]
  try { names = await readdir(directory) } catch { return [] }
  const facts: ReportFact[] = []
  for (const name of names.sort()) {
    if (!name.endsWith('.json') || name === 'latest.json') continue
    try {
      const path = resolve(directory, name)
      if ((await stat(path)).size > MAX_REPORT_BYTES) continue
      const report = JSON.parse(await readFile(path, 'utf8')) as Partial<RunResult> & {
        /** DEJA-7 (PR #21): both attempts of an opt-in rerun. Older reports omit it. */
        attempts?: Array<{ endedAt?: unknown }>
      }
      if (report.schemaVersion !== 1 || typeof report.goalKey !== 'string' ||
          typeof report.identity?.buildFingerprint !== 'string' || typeof report.outcome !== 'string') continue
      // A validation's time is the end of the execution it came from; with a rerun, the first attempt.
      const at = [report.attempts?.[0]?.endedAt, report.execution?.endedAt]
        .filter((value): value is string => typeof value === 'string')
      facts.push({
        path: `.deja/reports/${name}`, goalKey: report.goalKey,
        buildFingerprint: report.identity.buildFingerprint, outcome: report.outcome, at,
      })
    } catch { /* skip */ }
  }
  return facts
}

export function renderGateTable(evidence: GateEvidence, redact: Redactor) {
  const passedCount = evidence.goals.filter(goal => goal.outcome === 'passed').length
  const lines = [
    `Release gate: ${evidence.verdict === 'passed' ? 'OPEN' : evidence.verdict === 'closed' ? 'CLOSED' : 'ERROR'}` +
      ` (${passedCount} of ${evidence.goals.length} goals passed)`,
    `Build: ${evidence.buildFingerprint ?? 'unknown'}${evidence.label !== null ? `  label: ${evidence.label}` : ''}`,
  ]
  if (evidence.error) lines.push(`Error: ${evidence.error}`)
  const width = Math.max(4, ...evidence.goals.map(goal => goal.goal.length))
  lines.push(`${'GOAL'.padEnd(width)}  ${'OUTCOME'.padEnd(12)}  ${'VALIDATED'.padEnd(24)}  FLOW SHA256`)
  for (const goal of evidence.goals) {
    lines.push(
      `${goal.goal.padEnd(width)}  ${goal.outcome.padEnd(12)}  ${(goal.validatedAt ?? '-').padEnd(24)}  ${goal.flowSha256 ?? '-'}`,
    )
    if (goal.detail && goal.outcome !== 'passed') lines.push(`  ${goal.detail}`)
    if (goal.reportPath) lines.push(`  report: ${goal.reportPath}`)
  }
  return redact(`${lines.join('\n')}\n`)
}

async function atomicWrite(path: string, contents: string) {
  await mkdir(dirname(path), { recursive: true })
  const temporary = `${path}.${process.pid}.tmp`
  await writeFile(temporary, contents)
  await rename(temporary, path)
}

function evidencePaths(configuration: ProductionConfiguration, evidencePath: string | undefined) {
  const json = evidencePath
    ? resolve(process.cwd(), evidencePath)
    : resolve(configuration.workspaceRoot, '.deja/gate/evidence.json')
  return { json, text: json.endsWith('.json') ? `${json.slice(0, -5)}.txt` : `${json}.txt` }
}

export type GateIo = { stdout: (text: string) => void; stderr: (text: string) => void }

/**
 * `deja gate`: read the recorded validations and say whether every listed goal passed on this
 * build. It never runs Maestro, explores, repairs, calls a model or writes to the store. The store
 * is only read (`list`, `lookup`), so it takes no writer lock and can run beside a running deja.
 * Returns the exit code: 0 open, 10 closed, 11 could not decide.
 */
export async function runGate(input: {
  configuration: ProductionConfiguration
  store: WalkStore
  adapter: MaestroExecutionAdapter
  options: GateOptions
  redact: Redactor
  json: boolean
  io: GateIo
}): Promise<number> {
  const { configuration, store, adapter, options, redact, io } = input
  const paths = evidencePaths(configuration, options.evidencePath)
  // A file left by an earlier run must never be read as this run's evidence.
  await rm(paths.json, { force: true })
  await rm(paths.text, { force: true })

  const goalsFile = resolve(process.cwd(), options.goalsPath)
  let goalsText: string
  try { goalsText = await readFile(goalsFile, 'utf8') } catch (error) {
    throw new CliConfigurationError(`The goals file cannot be read: ${options.goalsPath}`, { cause: error })
  }
  const goals = parseGoals(goalsText, options.goalsPath)

  const buildPath = options.buildPath ?? configuration.explore.buildPath
  const evidence: GateEvidence = {
    schemaVersion: 1, command: 'gate', verdict: 'error', passed: false,
    checkedAt: new Date().toISOString(),
    buildFingerprint: null,
    label: options.label ?? null,
    build: relative(configuration.workspaceRoot, resolve(configuration.workspaceRoot, buildPath)) || buildPath,
    identity: {
      appId: configuration.explore.appId, platform: adapter.platform,
      adapter: null, browser: null,
    },
    manifest: store.manifestPath ?? null,
    goals: goals.map(goal => ({
      goal, goalKey: normalizeGoal(goal).key, outcome: 'not-run' as const,
      validatedAt: null, flowSha256: null, closureSha256: null, reportPath: null,
    })),
  }
  try {
    const buildFingerprint = await adapter.buildFingerprint(configuration.workspaceRoot, buildPath)
    const cache = adapter.cacheIdentity({ buildFingerprint })
    // The same identity a run builds, so the gate reads exactly what a run on this build recorded.
    const adapterId = cache.adapter ?? (store.manifestPath !== undefined ? adapter.id : undefined)
    evidence.buildFingerprint = buildFingerprint
    evidence.identity = {
      appId: configuration.explore.appId, platform: adapter.platform,
      adapter: adapterId ?? null, browser: cache.browser ?? null,
    }
    const facts = await readReportFacts(configuration.workspaceRoot)
    const flakyFailures: GateFlakyFailure[] = facts
      .filter(fact => fact.outcome === 'flaky' && fact.buildFingerprint === buildFingerprint && fact.at[0])
      .map(fact => ({ goalKey: fact.goalKey, buildFingerprint, at: fact.at[0] }))
    const results = await evaluateGate(store, goals, {
      appId: configuration.explore.appId,
      platform: adapter.platform,
      flowSchemaVersion: 'maestro/v1',
      buildFingerprint,
      ...(adapterId !== undefined ? { adapter: adapterId } : {}),
      ...(cache.browser !== undefined ? { browser: cache.browser } : {}),
    }, { flakyFailures })
    evidence.goals = results.map(result => ({
      ...result,
      reportPath: result.validatedAt
        ? facts.find(fact => fact.goalKey === result.goalKey && fact.buildFingerprint === buildFingerprint &&
          fact.at.includes(result.validatedAt!))?.path ?? null
        : null,
    }))
    evidence.passed = gatePassed(results)
    evidence.verdict = evidence.passed ? 'passed' : 'closed'
  } catch (error) {
    // Never passes by default: an unreadable store or build is a closed gate with its own exit code.
    evidence.verdict = 'error'
    evidence.passed = false
    evidence.error = error instanceof Error ? error.message : String(error)
  }

  // The same redaction as run reports and the JUnit file.
  const redacted = redactJson(evidence, redact)
  const table = renderGateTable(redacted, redact)
  try {
    await atomicWrite(paths.json, `${JSON.stringify(redacted, null, 2)}\n`)
    await atomicWrite(paths.text, table)
  } catch (error) {
    io.stderr(`${redact(`The evidence file cannot be written: ${error instanceof Error ? error.message : String(error)}`)}\n`)
    io.stdout(input.json ? `${JSON.stringify(redacted, null, 2)}\n` : table)
    return GATE_ERROR_EXIT
  }
  const shown = relative(configuration.workspaceRoot, paths.json)
  io.stdout(input.json
    ? `${JSON.stringify(redacted, null, 2)}\n`
    : `${table}Evidence: ${redact(shown.startsWith('..') ? paths.json : shown)}\n`)
  return evidence.verdict === 'passed' ? 0 : evidence.verdict === 'closed' ? GATE_CLOSED_EXIT : GATE_ERROR_EXIT
}

