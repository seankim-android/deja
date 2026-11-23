import { randomUUID } from 'node:crypto'
import { lstat, mkdir, readdir, readFile, realpath, rename, rm, stat, writeFile } from 'node:fs/promises'
import { basename, dirname, join, relative, resolve, sep } from 'node:path'
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
  /** Marks a file as gate evidence, so only a previous evidence file is ever replaced. */
  kind: typeof EVIDENCE_KIND
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
const EVIDENCE_KIND = 'deja-gate-evidence'
const TABLE_HEADER = 'Release gate: '
/** C0, DEL, C1, the Unicode line and paragraph separators, and format characters (bidi, zero-width). */
function isControl(character: string) {
  const code = character.codePointAt(0)!
  // Cf covers bidi controls (U+202A to U+202E, U+2066 to U+2069) and zero-width characters.
  return code < 0x20 || (code >= 0x7f && code <= 0x9f) || code === 0x2028 || code === 0x2029 || /^\p{Cf}$/u.test(character)
}
const hasControl = (value: string) => [...value].some(isControl)

/** Shown, never trusted: control characters become visible escapes, so a value cannot add a line or an escape code. */
function plain(value: string) {
  return [...value].map(character => isControl(character)
    ? (character.codePointAt(0)! > 0xffff
      ? `\\u{${character.codePointAt(0)!.toString(16)}}`
      : `\\u${character.codePointAt(0)!.toString(16).padStart(4, '0')}`)
    : character).join('')
}

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
    if (hasControl(goal)) throw new CliConfigurationError(`${label} contains a goal with a control character`)
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
    `${TABLE_HEADER}${evidence.verdict === 'passed' ? 'OPEN' : evidence.verdict === 'closed' ? 'CLOSED' : 'ERROR'}` +
      ` (${passedCount} of ${evidence.goals.length} goals passed)`,
    `Build: ${evidence.buildFingerprint ?? 'unknown'}${evidence.label !== null ? `  label: ${plain(evidence.label)}` : ''}`,
  ]
  if (evidence.error) lines.push(`Error: ${plain(evidence.error)}`)
  const width = Math.max(4, ...evidence.goals.map(goal => plain(goal.goal).length))
  lines.push(`${'GOAL'.padEnd(width)}  ${'OUTCOME'.padEnd(12)}  ${'VALIDATED'.padEnd(24)}  FLOW SHA256`)
  for (const goal of evidence.goals) {
    lines.push(
      `${plain(goal.goal).padEnd(width)}  ${goal.outcome.padEnd(12)}  ${plain(goal.validatedAt ?? '-').padEnd(24)}  ${plain(goal.flowSha256 ?? '-')}`,
    )
    if (goal.detail && goal.outcome !== 'passed') lines.push(`  ${plain(goal.detail)}`)
    if (goal.reportPath) lines.push(`  report: ${plain(goal.reportPath)}`)
  }
  return redact(`${lines.join('\n')}\n`)
}

/**
 * Write through a temporary file in the same folder, then rename it into place. The temporary name
 * is random and created with O_EXCL (`wx`), which fails on any existing path, a planted symlink
 * included, so the write can never follow a link to another file. A rename replaces a symlink at
 * the destination, never writes through it.
 */
export async function atomicWrite(path: string, contents: string, suffix: string = randomUUID()) {
  await mkdir(dirname(path), { recursive: true })
  const temporary = `${path}.${process.pid}.${suffix}.tmp`
  try {
    await writeFile(temporary, contents, { flag: 'wx' })
    await rename(temporary, path)
  } catch (error) {
    // Only a file this call created: a failed `wx` means the name was not ours, so leave it.
    if ((error as NodeJS.ErrnoException).code !== 'EEXIST') await rm(temporary, { force: true })
    throw error
  }
}

type CheckedFile = { canonical: string; identity: { dev: number; ino: number } | null }

/**
 * Delete a file `evidencePaths` approved, only if it is still the same file: not a symlink, same
 * device and inode, same resolved location, or still absent. A swap after the check is refused.
 * Remaining window: between this check and the unlink itself (microseconds), a local adversary who
 * can write the folder could still swap the file. The rename into place has the same window.
 */
export async function removeChecked(path: string, checked: CheckedFile) {
  const refuse = (): never => {
    throw new CliConfigurationError(`--evidence ${path} changed while the gate was running; refused. Nothing was deleted.`)
  }
  let now = null
  try { now = await lstat(path) } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
  }
  if (!now) {
    if (checked.identity) refuse()
    return
  }
  if (!checked.identity || now.isSymbolicLink() || !now.isFile() ||
      now.dev !== checked.identity.dev || now.ino !== checked.identity.ino ||
      await canonicalPath(path) !== checked.canonical) refuse()
  await rm(path, { force: true })
}

/** The nearest existing ancestor resolved through symlinks, plus the rest of the path. */
async function canonicalPath(path: string): Promise<string> {
  const rest: string[] = []
  let current = path
  for (;;) {
    try {
      return join(await realpath(current), ...rest.reverse())
    } catch (error) {
      const parent = dirname(current)
      if (parent === current || (error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
      rest.push(basename(current))
      current = parent
    }
  }
}

/**
 * Where the evidence goes, and proof that writing it can only replace a previous gate evidence
 * file. Nothing is deleted or written before this returns. A path that names the goals file, the
 * manifest, anything under `.deja/` but `.deja/gate/`, a symlink, a directory or any file that is not
 * gate evidence (a flow, the store's walks.json) is refused with exit 2.
 */
async function evidencePaths(
  configuration: ProductionConfiguration,
  evidencePath: string | undefined,
  goalsFile: string,
  manifestPath: string | undefined,
  redact: Redactor,
) {
  const requested = evidencePath
    ? resolve(process.cwd(), evidencePath)
    : resolve(configuration.workspaceRoot, '.deja/gate/evidence.json')
  const json = requested
  const text = json.endsWith('.json') ? `${json.slice(0, -5)}.txt` : `${json}.txt`
  const root = await realpath(configuration.workspaceRoot)
  const protectedFiles = new Set<string>()
  for (const file of [goalsFile, ...(manifestPath ? [resolve(configuration.workspaceRoot, manifestPath)] : [])]) {
    protectedFiles.add(await canonicalPath(file))
  }
  const gateDirectory = join(root, '.deja', 'gate') + sep
  const deja = join(root, '.deja') + sep
  const refuse = (target: string, reason: string): never => {
    throw new CliConfigurationError(`--evidence ${relative(process.cwd(), target) || target} is refused: ${reason}. Nothing was changed.`)
  }
  const checked = new Map<string, CheckedFile>()
  let previous: GateEvidence | null = null
  for (const [target, isText] of [[json, false], [text, true]] as const) {
    let existing = null
    try { existing = await lstat(target) } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
    }
    if (existing?.isSymbolicLink()) refuse(target, 'it is a symbolic link')
    const canonical = await canonicalPath(target)
    if (protectedFiles.has(canonical)) refuse(target, 'it is the goals file or the manifest')
    if (`${canonical}${sep}`.startsWith(deja) && !canonical.startsWith(gateDirectory)) {
      refuse(target, 'it is inside .deja/ but not .deja/gate/')
    }
    checked.set(target, {
      canonical, identity: existing ? { dev: existing.dev, ino: existing.ino } : null,
    })
    if (!existing) {
      continue
    }
    if (!existing.isFile()) refuse(target, 'it is not a regular file')
    if (existing.nlink > 1) refuse(target, 'it has more than one hard link')
    if (existing.size > MAX_REPORT_BYTES) refuse(target, 'it is not a gate evidence file')
    const contents = await readFile(target, 'utf8')
    if (isText) {
      // Replaced only when the JSON beside it is valid gate evidence and this is exactly its table.
      if (!previous || contents !== renderGateTable(previous, redact)) {
        refuse(target, 'it is not the table of the gate evidence beside it')
      }
      continue
    }
    let parsed: Partial<GateEvidence> | null = null
    try { parsed = JSON.parse(contents) as Partial<GateEvidence> } catch { /* refused below */ }
    if (parsed?.kind !== EVIDENCE_KIND || parsed.command !== 'gate' || parsed.schemaVersion !== 1) {
      refuse(target, 'it is not a gate evidence file')
    }
    previous = parsed as GateEvidence
  }
  return { json, text, checked }
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
  const goalsFile = resolve(process.cwd(), options.goalsPath)
  if (options.label !== undefined && hasControl(options.label)) {
    throw new CliConfigurationError('--label cannot contain control characters or line breaks')
  }
  const paths = await evidencePaths(configuration, options.evidencePath, goalsFile, store.manifestPath, redact)
  // A file left by an earlier run must never be read as this run's evidence. Only an old gate
  // evidence file gets here (evidencePaths checked).
  await removeChecked(paths.json, paths.checked.get(paths.json)!)
  await removeChecked(paths.text, paths.checked.get(paths.text)!)

  let goalsText: string
  try { goalsText = await readFile(goalsFile, 'utf8') } catch (error) {
    throw new CliConfigurationError(`The goals file cannot be read: ${options.goalsPath}`, { cause: error })
  }
  const goals = parseGoals(goalsText, options.goalsPath)

  const buildPath = options.buildPath ?? configuration.explore.buildPath
  const evidence: GateEvidence = {
    kind: EVIDENCE_KIND, schemaVersion: 1, command: 'gate', verdict: 'error', passed: false,
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
    // A goal the gate could not read or trust is undecided (exit 11), the same as a failing list().
    const undecided = results.some(result => result.outcome === 'invalid')
    evidence.verdict = evidence.passed ? 'passed' : undecided ? 'error' : 'closed'
    if (undecided) evidence.error = 'at least one goal has a stored walk or flow that cannot be read or trusted'
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

