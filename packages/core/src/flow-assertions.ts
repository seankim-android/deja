import { FlowReferenceError } from './flow-references.ts'
import { parseYamlDocuments } from './yaml-documents.ts'

/** Version of the assertion hash form. A change to the form bumps it, and the manifest's `hashVersion`. */
export const ASSERTION_HASH_VERSION = 1

export type FlowAssertion = {
  /** The line that goes into the assertion hash, before the file and call chain are added. */
  entry: string
  /**
   * `assertVisible`, `assertTrue` and so on, or `runFlow`, `repeat`, `retry`, `runScript`,
   * `evalScript` or `env` for what controls or feeds them.
   */
  command: string
  /** The `id` selector of an assert command, when it has one. */
  id: string | null
  /**
   * True when this command always runs when its file runs: it sits at the top level (or in
   * `onFlowStart`/`onFlowComplete`), is not `optional`, and, for a call, has no `when`.
   */
  always: boolean
  /** Set on a `runFlow`, `retry` or `runScript` that names a file: the path as written. */
  call?: { kind: 'flow' | 'script'; path: string }
}

/** JSON with sorted keys, so two spellings of one mapping hash alike. */
function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`
  if (typeof value === 'object' && value !== null) {
    return `{${Object.keys(value).sort().map(key =>
      `${JSON.stringify(key)}:${canonicalJson((value as Record<string, unknown>)[key])}`).join(',')}}`
  }
  return JSON.stringify(value === undefined ? null : value)
}

function isMapping(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

/**
 * What decides whether, how often and with which values a command's children run: every field but
 * the children (`commands`), the file path (it is a path, and its bytes are hashed where it is
 * reached) and the cosmetic `label`.
 */
function parameters(value: unknown): Record<string, unknown> {
  if (!isMapping(value)) return {}
  const { commands: _commands, file: _file, label: _label, ...rest } = value
  return rest
}

/** Anything but an absent or literal `false` counts as optional (`yes`, `on`, a string): fail closed. */
function isOptional(value: unknown) {
  return isMapping(value) && value.optional !== undefined && value.optional !== false
}

const CALLS = new Set(['runFlow', 'repeat', 'retry', 'runScript'])
const ALWAYS_RUNS = new Set(['onFlowStart', 'onFlowComplete'])

/**
 * The lines that make up the assertions of one Maestro YAML file, in document order, hash form
 * version 1. A file's lines are only meaningful with the file's path and the chain of calls that
 * reaches it, which the caller adds (`closureAssertions` in the store).
 *
 * - Every mapping key that starts with `assert` counts, with its whole command mapping, so adding
 *   `optional: true` or `enabled:` changes it.
 * - Each line names the enclosing `runFlow`, `repeat` and `retry` with their parameters (`when`,
 *   `times`, `while`, `maxRetries`, `env`), so a condition, `times: 0` or another env value changes
 *   it.
 * - Each `runFlow`, `retry` and `runScript` is a line of its own, and names the file it reaches.
 * - A bare word item such as `- assertNoDefectsWithAi` counts like the mapping form.
 * - `extendedWaitUntil` counts with its whole mapping (`visible`, `notVisible`, `timeout`): it fails
 *   the flow when its condition is not met. It is hashed, but it is not an assertion for
 *   `minAssertions`.
 * - `evalScript` counts with its source, and `env` mappings count where they sit, because
 *   `assertTrue` and `${VAR}` selectors read what scripts and env set.
 * - Order counts: the lines keep the order the flow runs them in.
 */
export function findFlowAssertions(contents: string): FlowAssertion[] {
  const parsed = parseYamlDocuments(contents)
  if ('error' in parsed) throw new FlowReferenceError(parsed.error.message, { cause: parsed.error.cause })
  const found: FlowAssertion[] = []
  const visit = (node: unknown, ancestors: string[], direct: boolean) => {
    if (Array.isArray(node)) {
      for (const item of node) {
        if (typeof item === 'string' && item.startsWith('assert')) {
          // A command written as a bare word, like `- assertNoDefectsWithAi`.
          found.push({
            entry: `${ancestors.join('>')}|${item}|null`, command: item, id: null,
            always: ancestors.every(element => ALWAYS_RUNS.has(element)),
          })
        } else {
          visit(item, ancestors, false)
        }
      }
      return
    }
    if (!isMapping(node)) return
    const always = ancestors.every(element => ALWAYS_RUNS.has(element))
    for (const [key, value] of Object.entries(node)) {
      const where = ancestors.join('>')
      if (key.startsWith('assert')) {
        const optional = isOptional(value)
        const id = isMapping(value) && typeof value.id === 'string' ? value.id : null
        found.push({
          entry: `${where}|${key}|${canonicalJson(value)}`, command: key, id, always: always && !optional,
        })
        continue
      }
      if (key === '<<') {
        // An unresolved merge key could bring in anything, `optional` included.
        found.push({ entry: `${where}|<<|${canonicalJson(value)}`, command: '<<', id: null, always: false })
        continue
      }
      if (key === 'extendedWaitUntil') {
        // It fails the flow when its condition is not met, so it guards behavior like an assertion.
        found.push({
          entry: `${where}|extendedWaitUntil|${canonicalJson(value)}`, command: 'extendedWaitUntil', id: null,
          always: always && !isOptional(value),
        })
        continue
      }
      if (key === 'evalScript') {
        found.push({ entry: `${where}|evalScript|${canonicalJson(value)}`, command: 'evalScript', id: null, always })
        continue
      }
      if (key === 'env') {
        // A call's own env is part of the call's parameters below.
        if (!direct) found.push({ entry: `${where}|env|${canonicalJson(value)}`, command: 'env', id: null, always })
        continue
      }
      if (CALLS.has(key)) {
        const params = canonicalJson(parameters(value))
        const file = typeof value === 'string' ? value : isMapping(value) ? value.file : undefined
        const conditional = isMapping(value) && (value.when !== undefined || isOptional(value))
        found.push({
          entry: `${where}|${key}|params=${params}`,
          command: key,
          id: null,
          always: always && !conditional,
          ...(typeof file === 'string' && key !== 'repeat'
            ? { call: { kind: key === 'runScript' ? 'script' as const : 'flow' as const, path: file.trim() } }
            : {}),
        })
        if (isMapping(value)) visit(value, [...ancestors, `${key}${params}`], true)
        continue
      }
      visit(value, [...ancestors, key], false)
    }
  }
  for (const document of parsed.documents) visit(document, [], false)
  return found
}
