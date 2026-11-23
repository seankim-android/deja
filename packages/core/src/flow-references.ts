import { parseYamlDocuments } from './yaml-documents.ts'

export type FlowReference = {
  /**
   * `flow` is another Maestro YAML file; `script` is a JavaScript file run by `runScript`; `media`
   * is a file `addMedia` reads (an image or video, so binary).
   */
  kind: 'flow' | 'script' | 'media'
  /** The command that names the file. */
  command: 'runFlow' | 'retry' | 'runScript' | 'addMedia'
  /** The path exactly as written in the YAML. It is relative to the file that holds it. */
  path: string
}

export class FlowReferenceError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'FlowReferenceError'
  }
}

function parseDocuments(contents: string): unknown[] {
  const parsed = parseYamlDocuments(contents)
  if ('error' in parsed) {
    throw new FlowReferenceError(parsed.error.message, { cause: parsed.error.cause })
  }
  return parsed.documents
}

function referencePath(kind: string, value: unknown): string {
  if (typeof value !== 'string' || value.trim().length === 0) {
    throw new FlowReferenceError(`${kind} needs a file path`)
  }
  return value.trim()
}

/**
 * Maestro commands that read a file Déjà cannot store or hash yet. Refused, so a flow never runs
 * against a file the closure does not hold. `assertScreenshot` compares the screen with a
 * reference image.
 */
const UNSUPPORTED_FILE_COMMANDS = new Set(['assertScreenshot'])

function mediaPaths(value: unknown): string[] {
  if (typeof value === 'string') return [referencePath('addMedia', value)]
  if (Array.isArray(value) && value.length > 0) return value.map(item => referencePath('addMedia', item))
  if (typeof value === 'object' && value !== null && !Array.isArray(value)) {
    const { file, files } = value as Record<string, unknown>
    if (file !== undefined) return mediaPaths(file)
    if (files !== undefined) return mediaPaths(files)
  }
  throw new FlowReferenceError('addMedia needs file paths')
}

/**
 * Every file a Maestro flow reads, in any document of the file and at any depth (`repeat`, `retry`,
 * `when`, `onFlowStart`, `onFlowComplete`, inline `commands`): `runFlow` and `retry` files,
 * `runScript` files and `addMedia` files. Accepts `runFlow: path.yaml` and
 * `runFlow: { file: path.yaml, ... }`. A `runFlow` or `retry` with inline `commands` and no `file`
 * names nothing itself; its commands are still searched. `addMedia` takes a path, a list of paths,
 * or a mapping with `file` or `files`. A command that reads a file in a way this cannot follow
 * (`assertScreenshot`) is a `FlowReferenceError`.
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
      if (UNSUPPORTED_FILE_COMMANDS.has(key)) {
        throw new FlowReferenceError(
          `${key} reads a reference image that Déjà does not store or hash, so a stored or manifest flow cannot use it`,
        )
      }
      if (key === 'addMedia') {
        for (const path of mediaPaths(value)) found.push({ kind: 'media', command: 'addMedia', path })
        continue
      }
      if (key === 'runFlow' || key === 'runScript' || key === 'retry') {
        const kind = key === 'runScript' ? 'script' : 'flow'
        if (typeof value === 'object' && value !== null && !Array.isArray(value)) {
          const file = (value as Record<string, unknown>).file
          if (file !== undefined || key === 'runScript') {
            found.push({ kind, command: key, path: referencePath(key, file) })
          }
        } else {
          found.push({ kind, command: key, path: referencePath(key, value) })
        }
      }
      visit(value)
    }
  }
  visit(parseDocuments(contents))
  return found
}
