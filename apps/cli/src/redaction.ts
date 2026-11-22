import { homedir } from 'node:os'

export type Redactor = (text: string) => string

export function createRedactor(workspaceRoot: string, fixtureValues: string[]): Redactor {
  const replacements: Array<[string, string]> = [
    [workspaceRoot, '<workspace>'],
    [homedir(), '<home>'],
    ...fixtureValues.filter(value => value.length > 0)
      .map(value => [value, '<redacted-fixture-value>'] as [string, string]),
  ]
  return text => replacements.reduce(
    (redacted, [search, replacement]) => redacted.split(search).join(replacement),
    text,
  )
}

export function redactJson<T>(value: T, redact: Redactor): T {
  if (typeof value === 'string') return redact(value) as T
  if (Array.isArray(value)) return value.map(entry => redactJson(entry, redact)) as T
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value as Record<string, unknown>).map(([key, entry]) => {
      if (/(?:password|secret|token|credential)/i.test(key) && typeof entry === 'string') {
        return [key, '<redacted>']
      }
      return [key, redactJson(entry, redact)]
    })) as T
  }
  return value
}
