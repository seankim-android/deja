import { findFlowStrings } from '@deja-run/core'

/** Shortest fixture value that can be scanned for without matching half of every file. */
export const MIN_FORBIDDEN_VALUE_LENGTH = 3

/** A fixture value that is too short to scan for. Refused up front, never skipped. */
export class ForbiddenValueTooShortError extends Error {
  constructor() {
    super(
      `A fixture credential value is shorter than ${MIN_FORBIDDEN_VALUE_LENGTH} characters, so a written file ` +
      'could not be checked for it. Use a longer fixture value.',
    )
    this.name = 'ForbiddenValueTooShortError'
  }
}

/** The ways a value can be written into a file without matching its plain bytes. */
function spellings(value: string) {
  const json = JSON.stringify(value).slice(1, -1)
  return new Set([
    value,
    json,
    // YAML double quotes: JSON escapes, plus `\/` and `\x..` spellings are only ever the same bytes.
    json.replaceAll('/', '\\/'),
    value.replaceAll("'", "''"),
    encodeURIComponent(value),
    Buffer.from(value).toString('base64'),
  ])
}

/**
 * True if `text` holds a fixture value. It looks at the raw bytes in each spelling above, and, for
 * YAML, at every decoded string, so quoting and block scalars cannot hide a value from it.
 */
export function holdsForbiddenValue(text: string, values: string[]) {
  if (values.length === 0) return false
  const decoded = findFlowStrings(text) ?? []
  return values.some(value =>
    [...spellings(value)].some(form => text.includes(form)) ||
    decoded.some(item => item.includes(value)))
}

/**
 * The same check for a binary file (`addMedia`): the raw bytes in each spelling, and no YAML decode,
 * which a binary file can only fail or misread.
 */
export function bytesHoldForbiddenValue(bytes: Buffer, values: string[]) {
  return values.some(value => [...spellings(value)].some(form => bytes.includes(form)))
}

/** The same check for a JSON document, such as the manifest. */
export function jsonHoldsForbiddenValue(text: string, values: string[]) {
  if (values.length === 0) return false
  const strings: string[] = []
  const visit = (node: unknown) => {
    if (typeof node === 'string') strings.push(node)
    else if (Array.isArray(node)) node.forEach(visit)
    else if (typeof node === 'object' && node !== null) {
      for (const [key, value] of Object.entries(node)) { strings.push(key); visit(value) }
    }
  }
  try { visit(JSON.parse(text)) } catch { /* the raw check below still runs */ }
  return values.some(value =>
    [...spellings(value)].some(form => text.includes(form)) || strings.some(item => item.includes(value)))
}
