import type {
  MigrationReport,
  ReportReceipt,
  RunResult,
  StoredWalk,
} from '@deja-qa/core'
import { redactJson, type Redactor } from './redaction.ts'

export const HELP = `Déjà — explore once, replay without a model, repair only bounded staleness.

Usage:
  deja explore <goal> [--config <file>] [--json] [--junit <file>] [--summary]
  deja run <goal> [--verify] [--rerun-failed] [--config <file>] [--json] [--junit <file>] [--summary]
  deja remember <goal> --flow <candidate.yaml> [--accept-assertion-change] [--config <file>] [--json] [--junit <file>] [--summary]
  deja list [--config <file>] [--json]
  deja migrate [--dry-run] [--store <id>] [--config <file>] [--json]
  deja inspect <report> [--config <file>] [--json] [--summary]
  deja engine install [--json]
  deja doctor [--config <file>] [--json]
  deja help

Commands:
  explore   Force verified cold exploration and store only a passing candidate.
  run       Check exact local memory first. Hits never construct an explorer/model client.
            Opt-in semantic fallback applies only to not-found misses and replays verified YAML.
            Other misses explore cold; stale-build memory enters only bounded selector repair,
            or, with iOS {"staleBuild":"replay-last-verified"}, replays the last verified YAML.
  remember  Independently verify an existing YAML candidate before storing it.
  migrate   With a manifest: move walks from the old store (memory schema 3) into your own flow
            files plus manifest entries. Writes new files and the manifest; never commits.
  list      List local walks without constructing an explorer or runner invocation.
  inspect   Summarize a saved run result, artifact paths, failures, and source diffs.
  engine install
            Download Déjà's pinned Maestro into $DEJA_HOME/maestro/<version> (default
            ~/.local/share/deja), check its sha256 and extract it atomically. A valid install is
            skipped. It never touches ~/.maestro or any other Maestro.
  doctor    Report the engine (version, path, pinned or override), Java, Xcode and simulator
            runtimes, and whether they match the tested range. Exit 1 if the engine is missing or
            out of range, or Java is too old.

Options:
  --config <file>  Combined fixture/plugin configuration (default: ./deja.config.json).
  --flow <file>    Candidate YAML for remember; independently runnable without Déjà.
  --json           Emit a redacted machine-readable result instead of human-readable text.
  --junit <file>   Write Maestro's own JUnit report to <file> (config field: "junit"). If Maestro
                   never ran or did not pass, a minimal report with one failed testcase is written,
                   so a missing file is never a pass. Listed as a junit artifact in the result.
  --dry-run        migrate only: show what would be written, and write nothing.
  --verify         With a manifest: if the flow changed, or no build has validated it, re-run your
                   current file in place. A pass with unchanged assertions updates the manifest's sha.
                   Changed assertions fail (exit 6) and write nothing.
  --rerun-failed   run only. If the replay fails, replay the same YAML once more, under the same
                   timeout, with no model call, no repair and no exploration. Failed twice with the
                   same label and step is a steady failure, reported as before. Failed then passed,
                   or failed twice with a different label or step, is "flaky" (exit 7), never a
                   pass. The report lists both attempts. With a manifest, a flow that changed
                   between the attempts gives flow-changed and is not rerun. Off by default.
  --accept-assertion-change
                   With a manifest, remember only: let this reviewed run change the manifest's
                   assertion hash. Review the flow's assertions in the diff first.
  --store <id>     Use a separate store for this run, for example one per device or simulator.
                   Run one deja per device in parallel, each with its own id. A second writer on
                   one store is refused (exit 5). Flows are shared; history is not.
  --summary        Print one line: deja-summary v=1 outcome cache fingerprint engine model_calls
                   failure [attempts attempt1 attempt2 ...] goal (goal last, percent-encoded). The
                   attempt keys appear only when --rerun-failed reran. Last stdout line, or stderr with --json.
  -h, --help       Show help without loading configuration or constructing plugins.

Semantic retrieval:
  Opt in with {"semantic":{"provider":"local-lexical-v1"}} in the config file.
  Exact identity still runs first. A semantic selection must clear app/platform/schema/build,
  adapter/browser/checksum/latest-pass gates, similarity and margin thresholds, then replay YAML.
  Missing providers fall back to the ordinary miss lifecycle. This is a narrow local lexical proof.

Manifest (config "manifest": "deja.walks.json", relative to workspaceRoot):
  The manifest is committed with your flows. It names each goal's flow file, a sha256 of the flow
  and every file it runs, and a hash of its assertions. Replay runs your file in place; .deja/
  keeps history only. A changed flow gives "flow-changed" and nothing is replayed. Déjà writes
  the manifest and never commits it. Cold exploration writes its verified flow as a new file in
  "flowsDir" (default .maestro/flows/deja) plus a manifest entry, and never overwrites a file.
  Repair is not available with a manifest yet.

Engine:
  Runs use the pinned Maestro in $DEJA_HOME. DEJA_MAESTRO or "maestro.executable" overrides it
  and the report names the override. An override outside the tested range is refused (exit 2)
  unless {"maestro":{"allowUntested":true}} is set.

Exit codes:
  0  Successful run/explore/remember, list, inspect, or help.
  1  Product/unknown/runner failure (including executable launch), rejected candidate,
     or rejected/failed bounded repair.
  2  Usage, configuration, engine (missing, checksum, untested version), report-inspection, or
     direct I/O error.
  3  Cold exploration did not produce a verified reusable walk.
  4  Invalid/corrupt local memory; quarantine it and use cold exploration to recover.
  5  Store locked: another run is writing the same store. Use a different --store <id>.
  6  Manifest: flow-changed, not-validated, needs-migration or assertions-changed. Nothing was
     replayed or written. Run with --verify, or review the change and run remember.
  7  Flaky (--rerun-failed): the replay failed, then passed or failed differently on a rerun of
     the same YAML. Not a pass. The first failure is recorded in history. The host decides.

YAML verification and replay always invoke standalone Maestro flows. No YAML depends on
Déjà or an LLM after it has been written.
`

function renderEngine(engine: NonNullable<RunResult['engine']>) {
  const version = engine.version ? `Maestro ${engine.version}` : 'Maestro (version unreadable)'
  const origin = engine.source === 'pinned'
    ? 'pinned'
    : `override via ${engine.overrideVia === 'config' ? 'maestro.executable' : 'DEJA_MAESTRO'}`
  return `Engine: ${version} (${origin}${engine.inRange ? '' : ', outside the tested range'})`
}

export function renderResult(
  input: RunResult,
  receipt: ReportReceipt | null,
  redact: Redactor,
) {
  const result = redactJson(input, redact)
  const lines = [
    `Command: ${result.command}`,
    `Goal: ${result.goalKey}`,
    `Outcome: ${result.outcome}`,
    `Passed: ${result.passed ? 'yes' : 'no'}`,
    `Cache status: ${result.cacheStatus}${result.cacheMissReason ? ` (${result.cacheMissReason})` : ''}`,
    `Model calls: ${result.modelCalls ?? 'unknown'} (${result.modelCallBasis})`,
    ...(result.engine ? [renderEngine(result.engine)] : []),
    `Duration: ${result.durationMs} ms`,
    `Assertions: ${result.assertions ?? 'unknown'}`,
    `Repair count: ${result.repairCount}`,
    `YAML: ${result.yamlPath ?? 'none'}`,
  ]
  if (result.yamlSha256) lines.push(`YAML SHA-256: ${result.yamlSha256}`)
  if (result.replayedOnNewBuild) {
    const replay = result.replayedOnNewBuild
    lines.push(`Replayed on a new build: this build was not verified; the YAML was last verified on build ${replay.previousBuildFingerprint}`)
    lines.push(`Stored for this build: ${replay.recordedForNewBuild ? 'yes' : 'no'}`)
  }
  if (result.semantic) {
    lines.push(`Semantic lookup: ${result.semantic.status}${result.semantic.reason ? ` (${result.semantic.reason})` : ''}`)
    lines.push(`Semantic embedding calls: ${result.semantic.embeddingCalls}`)
    lines.push(`Eligible semantic candidates: ${result.semantic.eligibleCandidates}`)
    if (result.semantic.goalSimilarity !== null) {
      lines.push(`Goal similarity: ${result.semantic.goalSimilarity.toFixed(6)}`)
    }
    if (result.semantic.runnerUpSimilarity !== null) {
      lines.push(`Runner-up similarity: ${result.semantic.runnerUpSimilarity.toFixed(6)}`)
    }
    if (result.semantic.selectedWalk) {
      lines.push(`Selected walk: ${result.semantic.selectedWalk.goalKey}`)
      lines.push(`Selected walk YAML: ${result.semantic.selectedWalk.flowPath}`)
    }
  }
  if (result.failure) {
    const label = result.passed ? 'Resolved diagnosis' : 'Failure'
    lines.push(`${label}: ${result.failure.category} (repair eligible: ${result.failure.repairEligible})`)
    if (result.failure.failingStep) {
      lines.push(`${result.passed ? 'Resolved step' : 'Failing step'}: ${result.failure.failingStep}`)
    }
  }
  if (result.attempts) {
    for (const attempt of result.attempts) {
      const failure = attempt.failure
      lines.push(`Attempt ${attempt.attempt}: ${attempt.passed ? 'passed' : `failed (${failure?.category ?? 'unclassified'})`}`)
      if (failure?.failingStep) lines.push(`Attempt ${attempt.attempt} failing step: ${failure.failingStep}`)
      if (failure?.rawArtifactPath) lines.push(`Attempt ${attempt.attempt} output: ${failure.rawArtifactPath}`)
      if (failure?.crash) {
        const crash = failure.crash
        lines.push(`Attempt ${attempt.attempt} crash signal: ${crash.source}${crash.reportFile ? ` (${crash.reportFile}${crash.process ? `, process ${crash.process}` : ''})` : ''}`)
      }
    }
  }
  if (result.failure?.crash && !result.attempts) {
    const crash = result.failure.crash
    lines.push(`Crash signal: ${crash.source}${crash.reportFile ? ` (${crash.reportFile}${crash.process ? `, process ${crash.process}` : ''})` : ''}`)
  }
  for (const error of result.errors) lines.push(`Error: ${error}`)
  if (receipt) {
    lines.push(`Report: ${receipt.reportPath}`)
    lines.push(`Latest report: ${receipt.latestPath}`)
  }
  if (result.artifacts.length > 0) {
    lines.push('Artifacts:')
    for (const artifact of result.artifacts) lines.push(`- [${artifact.kind}] ${artifact.path}`)
  }
  if (result.diffs.length > 0) {
    lines.push('Source diffs (proposed only; never auto-committed):')
    for (const diff of result.diffs) {
      lines.push(`Diff: ${diff.path ?? 'not persisted'}`)
      for (const file of diff.affectedFiles) lines.push(`Affected file: ${file}`)
      lines.push(diff.contents.trimEnd())
    }
  } else {
    lines.push('Source diffs: none')
  }
  return `${lines.join('\n')}\n`
}

export function renderMigration(report: MigrationReport, redact: Redactor) {
  const lines = [`Migration${report.dryRun ? ' (dry run, nothing written)' : ''}: ${report.migrated.length} moved, ${report.skipped.length} left`]
  for (const walk of report.migrated) {
    lines.push(`- ${redact(walk.goal)} -> ${redact(walk.flow)}`)
    lines.push(`  closure sha256 ${walk.closureSha256}`)
    lines.push(`  assertions sha256 ${walk.assertionsSha256}`)
    for (const file of walk.filesWritten) lines.push(`  ${report.dryRun ? 'would write' : 'wrote'} ${redact(file)}`)
    for (const file of walk.filesUnchanged) lines.push(`  already there ${redact(file)}`)
    lines.push(`  history entries kept: ${walk.historyEntries}${walk.olderWalksDropped ? `, older walks left in the backup: ${walk.olderWalksDropped}` : ''}`)
  }
  for (const walk of report.skipped) {
    lines.push(`- ${redact(walk.goal)} left in the old store (${walk.reason}): ${redact(walk.detail)}`)
  }
  lines.push(`Manifest: ${redact(report.manifestPath)}${report.manifestChanged ? (report.dryRun ? ' (would change)' : ' (changed)') : ' (unchanged)'}`)
  if (report.backupPath) lines.push(`Old store kept at ${redact(report.backupPath)}`)
  if (!report.dryRun && report.migrated.length > 0) {
    lines.push('Review the new files and the manifest, then commit them. Déjà never commits.')
  }
  return `${lines.join('\n')}\n`
}

export function renderWalks(walks: StoredWalk[], redact: Redactor) {
  if (walks.length === 0) return 'No verified walks in local memory.\n'
  const lines = [`Verified walks: ${walks.length}`]
  for (const walk of walks) {
    lines.push([
      `- ${redact(walk.goalKey)}`,
      `app=${redact(walk.appId)}`,
      `platform=${redact(walk.platform)}`,
      `build=${walk.buildFingerprint}`,
      `lastValidation=${walk.lastValidation.passed ? 'passed' : 'failed'}`,
      `yaml=${redact(walk.flowPath)}`,
      `origin=${walk.origin ?? 'unknown'}`,
      `repairs=${walk.repairHistory?.length ?? 0}`,
    ].join(' '))
  }
  return `${lines.join('\n')}\n`
}

/** Percent-encodes UTF-8 bytes outside [A-Za-z0-9._~-], so no space, `=`, quote or separator survives. */
export function encodeSummaryGoal(goal: string) {
  return [...new TextEncoder().encode(goal)].map(byte => {
    const character = String.fromCharCode(byte)
    return /[A-Za-z0-9._~-]/.test(character)
      ? character
      : `%${byte.toString(16).toUpperCase().padStart(2, '0')}`
  }).join('')
}

/**
 * One stable, parseable line per run, for CI host reports. Grammar (single physical line):
 *
 *   line        = "deja-summary v=1 outcome=" outcome " cache=" cache " fingerprint=" fingerprint
 *                 " engine=" engine " model_calls=" calls " failure=" failure [rerun] " goal=" goal
 *   outcome     = 1*( "a-z" / "-" )                    ; a run outcome, or "error"
 *   cache       = "hit" / "miss" / "not-checked" / "unknown"
 *   fingerprint = 12 lower-case hex / "unknown"
 *   engine      = 1*DIGIT "." 1*DIGIT "." 1*DIGIT / "unknown"  ; the Maestro that ran the flows
 *   calls       = 1*DIGIT / "unknown"
 *   failure     = 1*( "a-z" / "_" )                    ; category, "unclassified", "none" or "error"
 *   rerun       = " attempts=2 attempt1=" label " attempt2=" label
 *                 [" attempt1_step=" enc] [" attempt1_artifact=" enc]
 *                 [" attempt2_step=" enc] [" attempt2_artifact=" enc]
 *                                                        ; only after an opt-in rerun, between failure and goal
 *   label       = 1*32( "a-z" / "_" )                  ; the attempt's failure label, "none" if it passed
 *   enc         = *( [A-Za-z0-9._~-] / "%" 2HEXDIG )   ; redacted, percent-encoded, at most 200 bytes
 *   goal        = *( [A-Za-z0-9._~-] / "%" 2HEXDIG )   ; percent-encoded UTF-8, always last
 *
 * Every field before `goal` has a fixed charset, and `goal` cannot contain a space, `=`, quote or
 * any raw control or separator character, so a goal cannot add or change a field or a line.
 * `result` is null when no run result exists (usage, config, or memory errors).
 */
/** Strict x.y.z or "unknown": a hand-edited report cannot put anything else on the line. */
function summaryEngine(result: RunResult) {
  const version = result.engine?.version
  return typeof version === 'string' && /^\d{1,6}\.\d{1,6}\.\d{1,6}$/.test(version) ? version : 'unknown'
}

const LABEL = /^[a-z_]{1,32}$/

/** Redacts, cuts to 200 characters, then percent-encodes, so a step or path cannot add a field. */
function summaryText(value: string, redact: Redactor) {
  return encodeSummaryGoal(redact(value).slice(0, 200))
}

/** Extra keys for a run that reran. Labels are checked here: a hand-edited report cannot add fields. */
function summaryAttempts(result: RunResult, redact: Redactor) {
  const attempts = result.attempts
  if (!Array.isArray(attempts) || attempts.length !== 2) return []
  const parts = ['attempts=2']
  attempts.forEach((attempt, index) => {
    const label = attempt?.passed === true ? 'none' : attempt?.failure?.category
    parts.push(`attempt${index + 1}=${typeof label === 'string' && LABEL.test(label) ? label : 'unclassified'}`)
  })
  attempts.forEach((attempt, index) => {
    const step = attempt?.failure?.failingStep
    const artifact = attempt?.failure?.rawArtifactPath
    if (typeof step === 'string' && step) parts.push(`attempt${index + 1}_step=${summaryText(step, redact)}`)
    if (typeof artifact === 'string' && artifact) parts.push(`attempt${index + 1}_artifact=${summaryText(artifact, redact)}`)
  })
  return parts
}

export function renderSummary(result: RunResult | null, goalText: string, redact: Redactor) {
  const goal = encodeSummaryGoal(redact(result?.goalKey ?? goalText))
  if (!result) {
    return `deja-summary v=1 outcome=error cache=unknown fingerprint=unknown engine=unknown model_calls=unknown failure=error goal=${goal}`
  }
  const fingerprint = /^[0-9a-f]{12,}$/.test(result.identity.buildFingerprint)
    ? result.identity.buildFingerprint.slice(0, 12)
    : 'unknown'
  const category = result.failure?.category
  const failure = category === undefined || category === null
    ? (result.passed ? 'none' : 'unclassified')
    : LABEL.test(category) ? category : 'unclassified'
  return [
    'deja-summary v=1',
    `outcome=${/^[a-z-]{1,32}$/.test(result.outcome) ? result.outcome : 'unknown'}`,
    `cache=${result.cacheStatus}`,
    `fingerprint=${fingerprint}`,
    `engine=${summaryEngine(result)}`,
    `model_calls=${result.modelCalls ?? 'unknown'}`,
    `failure=${failure}`,
    ...summaryAttempts(result, redact),
    `goal=${goal}`,
  ].join(' ')
}
