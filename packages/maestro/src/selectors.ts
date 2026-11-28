import type { Segment } from '@deja-run/core'

/** Escape every regular expression metacharacter. Maestro's `id:` is a full-match regular expression. */
export function escapeRegex(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}

/**
 * The only way to write an `id:` value from a literal ID: regex-escaped, then quoted for YAML.
 * Without it `today.save` would also match `today_save`.
 */
export function idSelector(id: string): string {
  return JSON.stringify(escapeRegex(id))
}

export function tapSegment(id: string): Segment {
  return { label: `tap:${id}`, yaml: `- tapOn:\n    id: ${idSelector(id)}\n` }
}

export function waitVisibleSegment(id: string, timeoutMs = 10_000): Segment {
  return {
    label: `wait:${id}`,
    yaml: `- extendedWaitUntil:\n    visible:\n      id: ${idSelector(id)}\n    timeout: ${Math.trunc(timeoutMs)}\n`,
  }
}
