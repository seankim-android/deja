import { ConsumerFlows, type ManifestCheck, type ManifestCheckEntry } from '@deja-run/store-local'
import { CliConfigurationError, type ProductionConfiguration } from './config.ts'
import type { CliIo } from './main.ts'
import { createExecutionAdapter } from './orchestration.ts'
import { redactJson, type Redactor } from './redaction.ts'

/** Exit code when any manifest entry no longer matches its files. Not used by any other command. */
export const MANIFEST_MISMATCH_EXIT = 8

// Control characters, DEL, C1 controls, bidi controls (U+061C, U+200E/F, U+202A-E, U+2066-9), line and
// paragraph separators, and the zero-width characters: never printed raw, and never in a fix line.
// eslint-disable-next-line no-control-regex
const UNSAFE = /[\u0000-\u001f\u007f-\u009f\u061c\u200b-\u200f\u2028-\u202e\u2060\u2066-\u2069\ufeff]/u

const NAMED: Record<string, string> = { '\n': 'n', '\r': 'r', '\t': 't' }

/** Text for the screen: unsafe characters shown as escapes. */
function display(text: string) {
  return [...text].map(character => {
    if (!UNSAFE.test(character)) return character
    if (NAMED[character]) return `\\${NAMED[character]}`
    const code = character.codePointAt(0)!
    return code < 0x100 ? `\\x${code.toString(16).padStart(2, '0')}` : `\\u${code.toString(16).padStart(4, '0')}`
  }).join('')
}

/** POSIX single-quote quoting only, valid in sh, dash, bash and zsh. The word must be safe. */
function shell(word: string) {
  if (/^[A-Za-z0-9_./:@%+=-]+$/.test(word) && !word.startsWith('-')) return word
  return `'${word.replace(/'/g, `'\\''`)}'`
}

/**
 * The command that fixes one failing entry, or null when the fix is to restore or repair a file or
 * change the manifest or config. `config` is the `--config` value as typed; the default is omitted.
 * A goal that starts with `-` goes after `--`, so the CLI does not read it as an option. Plain
 * single-quote quoting is the only quoting used. A goal, path or config word with an unsafe
 * character gets no runnable line (`unsafe`): no quoting of it is the same in every shell.
 */
export function fixCommand(entry: ManifestCheckEntry, config?: string): string | { unsafe: string } | null {
  if (entry.status !== 'flow-changed' && entry.status !== 'assertions-changed') return null
  const words = [entry.goal, ...(entry.status === 'assertions-changed' ? [entry.flow] : []), ...(config ? [config] : [])]
  if (words.some(word => UNSAFE.test(word))) {
    return {
      unsafe: entry.status === 'flow-changed' ? 'deja run --verify' : 'deja remember --flow <flow> --accept-assertion-change',
    }
  }
  const withGoal = (command: string, flags: string[]) => {
    const all = [...flags, ...(config ? ['--config', shell(config)] : [])]
    return entry.goal.startsWith('-')
      ? `deja ${command} ${[...all, '--', shell(entry.goal)].join(' ')}`
      : `deja ${command} ${[shell(entry.goal), ...all].join(' ')}`
  }
  return entry.status === 'flow-changed'
    ? withGoal('run', ['--verify'])
    : withGoal('remember', ['--flow', shell(entry.flow), '--accept-assertion-change'])
}

/** The `fix:` text for a person. */
function fixText(fix: string | { unsafe: string }) {
  return typeof fix === 'string'
    ? fix
    : `run \`${fix.unsafe}\` for this goal by hand (its name contains characters that can't be shown safely)`
}

function render(check: ManifestCheck, config?: string) {
  const failing = check.entries.filter(entry => entry.status !== 'ok')
  const lines = [failing.length === 0
    ? `deja manifest check: ${check.entries.length} of ${check.entries.length} entries match (${display(check.manifest)})`
    : `deja manifest check: ${failing.length} of ${check.entries.length} entries need attention (${display(check.manifest)})`]
  for (const entry of check.entries) {
    lines.push(`${entry.status.padEnd(18)}  ${display(entry.goal)}  ${display(entry.flow)}`)
    if (entry.status === 'ok') continue
    if (entry.status === 'missing-file') lines.push(`  missing file: ${display(entry.file ?? '')}`)
    if (entry.status === 'closure-error') lines.push(`  ${display(entry.reason ?? '')}`)
    if (entry.identity) {
      lines.push(`  ${entry.identity.field} differs: manifest ${display(String(entry.identity.manifest))}, config ${display(String(entry.identity.config))}`,
        '  run would not use this entry. Fix the manifest entry or the config.')
    }
    if (entry.currentClosureSha256 !== undefined && entry.currentClosureSha256 !== entry.manifestClosureSha256) {
      lines.push(`  closure:    manifest ${entry.manifestClosureSha256}`, `              current  ${entry.currentClosureSha256}`)
    }
    if (entry.currentAssertionsSha256 !== undefined && entry.currentAssertionsSha256 !== entry.manifestAssertionsSha256) {
      lines.push(`  assertions: manifest ${entry.manifestAssertionsSha256}`, `              current  ${entry.currentAssertionsSha256}`)
    }
    const fix = fixCommand(entry, config)
    if (fix) lines.push(`  fix: ${fixText(fix)}`)
    if (entry.assertionsAlsoChanged) {
      const remember = fixCommand({ ...entry, status: 'assertions-changed' }, config)!
      lines.push('  The assertions changed too. If --verify reports assertions-changed, review them, then:',
        `    ${fixText(remember)}`)
    }
  }
  if (check.untracked) {
    lines.push(check.untracked.length === 0
      ? 'Untracked flows: none'
      : `Untracked flows (no manifest entry uses them; not a failure): ${check.untracked.length}`)
    for (const path of check.untracked) lines.push(`  ${display(path)}`)
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
  /** The `--config` value as typed, for the printed fix commands. */
  configGiven?: string
}): Promise<number> {
  const { configuration, redact, io } = options
  if (!configuration.manifest) throw new CliConfigurationError('manifest check needs a "manifest" in the config')
  const flows = new ConsumerFlows({
    workspaceRoot: configuration.workspaceRoot,
    manifestPath: configuration.manifest,
    ...(configuration.flowsDir ? { flowsDir: configuration.flowsDir } : {}),
  })
  // The identity a run builds from the config, without a build or a device.
  const adapter = createExecutionAdapter(configuration)
  const check = await flows.check({
    untracked: options.untracked,
    identity: {
      appId: configuration.explore.appId, platform: adapter.platform, adapter: adapter.id, browser: adapter.browser,
    },
  })
  const clean = check.entries.every(entry => entry.status === 'ok')
  if (options.json) {
    const entries = check.entries.map(entry => {
      const fix = fixCommand(entry, options.configGiven)
      if (fix && typeof fix !== 'string') return { ...entry, fix: null, fixUnsafe: true }
      return { ...entry, ...(fix ? { fix } : {}) }
    })
    io.stdout(`${JSON.stringify(redactJson({
      manifest: check.manifest, ok: clean, entries, ...(check.untracked ? { untracked: check.untracked } : {}),
    }, redact), null, 2)}\n`)
  } else {
    io.stdout(redact(render(check, options.configGiven)))
  }
  return clean ? 0 : MANIFEST_MISMATCH_EXIT
}
