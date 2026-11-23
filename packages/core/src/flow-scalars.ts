import { parseYamlDocuments } from './yaml-documents.ts'

/**
 * Every string a YAML file holds once it is decoded: scalar values and mapping keys, in every
 * document. Quotes, escapes, block scalars and flow style are resolved, so `"p\"ss"`, `'p"ss'` and
 * a `|` block all give the same string. Returns null when the text is not YAML (a script, say).
 */
export function findFlowStrings(contents: string): string[] | null {
  const parsed = parseYamlDocuments(contents)
  if ('error' in parsed) return null
  const found: string[] = []
  const visit = (node: unknown) => {
    if (typeof node === 'string') found.push(node)
    else if (Array.isArray(node)) node.forEach(visit)
    else if (typeof node === 'object' && node !== null) {
      for (const [key, value] of Object.entries(node)) {
        found.push(key)
        visit(value)
      }
    }
  }
  parsed.documents.forEach(visit)
  return found
}
