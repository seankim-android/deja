import { homedir } from 'node:os'

const SENSITIVE_KEY = /(?:password|passwd|secret|token|credential|api[_-]?key|authorization|cookie)/i
const SECRET_PATTERNS: Array<RegExp> = [
  /(bearer\s+)[a-z0-9._~+/-]+=*/gi,
  /((?:password|passwd|secret|token|api[_-]?key|authorization|cookie)\s*[:=]\s*)([^\s,;]+)/gi,
  /((?:--(?:password|secret|token|api-key)\s+))([^\s]+)/gi,
  /(https?:\/\/[^/\s:]+:)[^@\s]+@/gi,
]

/** Defense-in-depth redaction for report and artifact text. The CLI remains the primary redactor. */
export function redactViewerText(input: string, workspaceRoot: string): string {
  let output = input
  for (const literal of [workspaceRoot, homedir()]) {
    if (literal && literal !== '/') output = output.split(literal).join('<local-path>')
  }
  for (const pattern of SECRET_PATTERNS) output = output.replace(pattern, '$1<redacted>')
  // YAML commonly puts a credential on the line following a sensitive key.
  output = output.replace(
    /^(\s*(?:password|passwd|secret|token|credential|api[_-]?key|authorization|cookie)\s*:\s*)(?!["']?<redacted)(.*)$/gim,
    '$1<redacted>',
  )
  return output
}

export function redactViewerJson<T>(value: T, workspaceRoot: string): T {
  if (typeof value === 'string') return redactViewerText(value, workspaceRoot) as T
  if (Array.isArray(value)) {
    return value.map(entry => redactViewerJson(entry, workspaceRoot)) as T
  }
  if (value && typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value as Record<string, unknown>).map(([key, entry]) => [
        key,
        SENSITIVE_KEY.test(key) ? '<redacted>' : redactViewerJson(entry, workspaceRoot),
      ]),
    ) as T
  }
  return value
}
