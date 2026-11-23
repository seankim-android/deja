import { ConsumerFlows, type ManifestCheck, type ManifestCheckEntry } from '@deja-qa/store-local'
import { CliConfigurationError, type ProductionConfiguration } from './config.ts'
import type { CliIo } from './main.ts'
import { redactJson, type Redactor } from './redaction.ts'

/** Exit code when any manifest entry no longer matches its files. Not used by any other command. */
export const MANIFEST_MISMATCH_EXIT = 8

/** Quote a word for a POSIX shell, only when it needs it. */
function shell(word: string) {
  return /^[A-Za-z0-9_./:@%+=-]+$/.test(word) ? word : `'${word.replace(/'/g, `'\\''`)}'`
}

/** The command that fixes one failing entry, or null when the fix is to restore or repair a file. */
export function fixCommand(entry: ManifestCheckEntry): string | null {
  if (entry.status === 'flow-changed') return `deja run ${shell(entry.goal)} --verify`
  if (entry.status === 'assertions-changed') {
    return `deja remember ${shell(entry.goal)} --flow ${shell(entry.flow)} --accept-assertion-change`
  }
  return null
}

function render(check: ManifestCheck) {
  const failing = check.entries.filter(entry => entry.status !== 'ok')
  const lines = [failing.length === 0
    ? `deja manifest check: ${check.entries.length} of ${check.entries.length} entries match (${check.manifest})`
    : `deja manifest check: ${failing.length} of ${check.entries.length} entries need attention (${check.manifest})`]
  for (const entry of check.entries) {
    lines.push(`${entry.status.padEnd(18)}  ${entry.goal}  ${entry.flow}`)
    if (entry.status === 'ok') continue
    if (entry.status === 'missing-file') lines.push(`  missing file: ${entry.file}`)
    if (entry.status === 'closure-error') lines.push(`  ${entry.reason}`)
    if (entry.currentClosureSha256 !== undefined && entry.currentClosureSha256 !== entry.manifestClosureSha256) {
      lines.push(`  closure:    manifest ${entry.manifestClosureSha256}`, `              current  ${entry.currentClosureSha256}`)
    }
    if (entry.currentAssertionsSha256 !== undefined && entry.currentAssertionsSha256 !== entry.manifestAssertionsSha256) {
      lines.push(`  assertions: manifest ${entry.manifestAssertionsSha256}`, `              current  ${entry.currentAssertionsSha256}`)
    }
    const fix = fixCommand(entry)
    if (fix) lines.push(`  fix: ${fix}`)
    if (entry.assertionsAlsoChanged) {
      lines.push('  The assertions changed too. If --verify reports assertions-changed, review them, then run:',
        `    deja remember ${shell(entry.goal)} --flow ${shell(entry.flow)} --accept-assertion-change`)
    }
  }
  if (check.untracked) {
    lines.push(check.untracked.length === 0
      ? 'Untracked flows: none'
      : `Untracked flows (no manifest entry uses them; not a failure): ${check.untracked.length}`)
    for (const path of check.untracked) lines.push(`  ${path}`)
  }
  return `${lines.join('\n')}\n`
}

/**
 * `deja manifest check`: compare each manifest entry's closure and assertion hashes with the files
 * on disk. It uses the store's own hashing (`ConsumerFlows.check`), opens no store, takes no lock
 * and writes nothing. It never runs a fix. Exit 0 all match, 8 any entry differs or is missing,
 * 2 (thrown) a manifest that cannot be read or trusted.
 */
export async function runManifestCheck(options: {
  configuration: ProductionConfiguration
  redact: Redactor
  json: boolean
  untracked: boolean
  io: CliIo
}): Promise<number> {
  const { configuration, redact, io } = options
  if (!configuration.manifest) throw new CliConfigurationError('manifest check needs a "manifest" in the config')
  const flows = new ConsumerFlows({
    workspaceRoot: configuration.workspaceRoot,
    manifestPath: configuration.manifest,
    ...(configuration.flowsDir ? { flowsDir: configuration.flowsDir } : {}),
  })
  const check = await flows.check({ untracked: options.untracked })
  const clean = check.entries.every(entry => entry.status === 'ok')
  if (options.json) {
    const entries = check.entries.map(entry => {
      const fix = fixCommand(entry)
      return { ...entry, ...(fix ? { fix } : {}) }
    })
    io.stdout(`${JSON.stringify(redactJson({
      manifest: check.manifest, ok: clean, entries, ...(check.untracked ? { untracked: check.untracked } : {}),
    }, redact), null, 2)}\n`)
  } else {
    io.stdout(redact(render(check)))
  }
  return clean ? 0 : MANIFEST_MISMATCH_EXIT
}
