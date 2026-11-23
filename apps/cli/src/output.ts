import type {
  ReportReceipt,
  RunResult,
  StoredWalk,
} from '../../../packages/core/src/index.ts'
import { redactJson, type Redactor } from './redaction.ts'

export const HELP = `Déjà — explore once, replay without a model, repair only bounded staleness.

Usage:
  deja explore <goal> [--config <file>] [--json]
  deja run <goal> [--config <file>] [--json]
  deja remember <goal> --flow <candidate.yaml> [--config <file>] [--json]
  deja list [--config <file>] [--json]
  deja inspect <report> [--config <file>] [--json]
  deja help

Commands:
  explore   Force verified cold exploration and store only a passing candidate.
  run       Check exact local memory first. Hits never construct an explorer/model client.
            Opt-in semantic fallback applies only to not-found misses and replays verified YAML.
            Other misses explore cold; stale-build memory enters only bounded selector repair.
  remember  Independently verify an existing YAML candidate before storing it.
  list      List local walks without constructing an explorer or runner invocation.
  inspect   Summarize a saved run result, artifact paths, failures, and source diffs.

Options:
  --config <file>  Combined fixture/plugin configuration (default: ./deja.config.json).
  --flow <file>    Candidate YAML for remember; independently runnable without Déjà.
  --json           Emit a redacted machine-readable result instead of human-readable text.
  -h, --help       Show help without loading configuration or constructing plugins.

Semantic retrieval:
  Opt in with {"semantic":{"provider":"local-lexical-v1"}} in the config file.
  Exact identity still runs first. A semantic selection must clear app/platform/schema/build,
  adapter/browser/checksum/latest-pass gates, similarity and margin thresholds, then replay YAML.
  Missing providers fall back to the ordinary miss lifecycle. This is a narrow local lexical proof.

Exit codes:
  0  Successful run/explore/remember, list, inspect, or help.
  1  Product/unknown/runner failure (including executable launch), rejected candidate,
     or rejected/failed bounded repair.
  2  Usage, configuration, report-inspection, or direct I/O error.
  3  Cold exploration did not produce a verified reusable walk.
  4  Invalid/corrupt local memory; quarantine it and use cold exploration to recover.

YAML verification and replay always invoke standalone Maestro flows. No YAML depends on
Déjà or an LLM after it has been written.
`

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
    `Duration: ${result.durationMs} ms`,
    `Assertions: ${result.assertions ?? 'unknown'}`,
    `Repair count: ${result.repairCount}`,
    `YAML: ${result.yamlPath ?? 'none'}`,
  ]
  if (result.yamlSha256) lines.push(`YAML SHA-256: ${result.yamlSha256}`)
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
