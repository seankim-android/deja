import { FlowReferenceError } from './flow-references.ts'
import { parseYamlDocuments } from './yaml-documents.ts'

/** Version of the assertion hash form. A change to the form bumps it, and the manifest's `hashVersion`. */
export const ASSERTION_HASH_VERSION = 1

export type FlowAssertion = {
  /** The line that goes into the assertion hash. */
  entry: string
  /** `assertVisible`, `assertTrue` and so on, or `runFlow`, `repeat`, `retry` or `env` for what controls them. */
  command: string
  /** The `id` selector of an assert command, when it has one. */
  id: string | null
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

/** Parameters that decide whether, how often or with what values a command's children run. */
function parameters(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return {}
  const { commands: _commands, file: _file, env: _env, label: _label, ...rest } = value as Record<string, unknown>
  return rest
}

/**
 * The product assertions of one Maestro YAML file, in hash form version 1.
 *
 * - Every mapping key that starts with `assert` counts, with its whole command mapping, so adding
 *   `optional: true` or `enabled:` changes the entry. Each entry also names the keys it sits
 *   under (`repeat>commands`, `when`, ...), so moving an assertion under a condition changes it.
 * - A `runFlow`, `repeat` or `retry` counts with its parameters (`when`, `times`, `while`,
 *   `maxRetries`), so a condition added around a call, or `times: 0`, changes the entry. The
 *   subflow's own assertions are found in the subflow's file.
 * - Every `env` mapping counts, where it sits: an assertion that reads `${VAR}` is only as strong
 *   as the value that is passed in.
 */
export function findFlowAssertions(contents: string): FlowAssertion[] {
  const parsed = parseYamlDocuments(contents)
  if ('error' in parsed) throw new FlowReferenceError(parsed.error.message, { cause: parsed.error.cause })
  const found: FlowAssertion[] = []
  const visit = (node: unknown, ancestors: string[]) => {
    if (Array.isArray(node)) {
      for (const item of node) visit(item, ancestors)
      return
    }
    if (typeof node !== 'object' || node === null) return
    for (const [key, value] of Object.entries(node)) {
      const where = ancestors.join('>')
      if (key.startsWith('assert')) {
        const id = typeof value === 'object' && value !== null && !Array.isArray(value) &&
          typeof (value as Record<string, unknown>).id === 'string'
          ? (value as Record<string, string>).id : null
        found.push({ entry: `${where}|${key}|${canonicalJson(value)}`, command: key, id })
        continue
      }
      if (key === 'env') {
        found.push({ entry: `${where}|env|${canonicalJson(value)}`, command: 'env', id: null })
        continue
      }
      if (key === 'runFlow' || key === 'repeat' || key === 'retry') {
        found.push({ entry: `${where}|${key}|params=${canonicalJson(parameters(value))}`, command: key, id: null })
      }
      visit(value, [...ancestors, key])
    }
  }
  for (const document of parsed.documents) visit(document, [])
  return found
}
