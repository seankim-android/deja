export type FlowReference = {
  /** `flow` is another Maestro YAML file; `script` is a JavaScript file run by `runScript`. */
  kind: 'flow' | 'script'
  /** The path exactly as written in the YAML. It is relative to the file that holds it. */
  path: string
}

export class FlowReferenceError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'FlowReferenceError'
  }
}

/** Returns null when the text is not valid YAML. */
function parseDocuments(contents: string): unknown {
  const yaml = (globalThis as { Bun?: { YAML?: { parse(text: string): unknown } } }).Bun?.YAML
  if (!yaml) throw new FlowReferenceError('Reading flow references needs a runtime with a YAML parser (Bun 1.4+)')
  try {
    return yaml.parse(contents)
  } catch {
    return null
  }
}

function referencePath(kind: string, value: unknown): string {
  if (typeof value !== 'string' || value.trim().length === 0) {
    throw new FlowReferenceError(`${kind} needs a file path`)
  }
  return value.trim()
}

/**
 * Every `runFlow` and `runScript` file a Maestro flow names, in any document of the file and at
 * any depth (`repeat`, `retry`, `when`, `onFlowStart`, `onFlowComplete`, inline `commands`).
 * Accepts `runFlow: path.yaml` and `runFlow: { file: path.yaml, ... }`. A `runFlow` with inline
 * `commands` and no `file` names nothing itself; its commands are still searched.
 */
export function findFlowReferences(contents: string): FlowReference[] {
  const found: FlowReference[] = []
  const visit = (node: unknown) => {
    if (Array.isArray(node)) {
      for (const item of node) visit(item)
      return
    }
    if (typeof node !== 'object' || node === null) return
    for (const [key, value] of Object.entries(node)) {
      if (key === 'runFlow' || key === 'runScript') {
        const kind = key === 'runFlow' ? 'flow' : 'script'
        if (typeof value === 'object' && value !== null && !Array.isArray(value)) {
          const file = (value as Record<string, unknown>).file
          if (file !== undefined || key === 'runScript') {
            found.push({ kind, path: referencePath(key, file) })
          }
        } else {
          found.push({ kind, path: referencePath(key, value) })
        }
      }
      visit(value)
    }
  }
  const parsed = parseDocuments(contents)
  if (parsed === null && /\b(?:runFlow|runScript)\s*:/.test(contents)) {
    // Not YAML this parser can read, but it seems to run files. Never guess which ones.
    throw new FlowReferenceError('Flow runs other files but is not valid YAML')
  }
  visit(parsed)
  return found
}
