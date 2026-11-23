import { createHash } from 'node:crypto'
import { mkdir, readFile, realpath, stat, writeFile } from 'node:fs/promises'
import { basename, dirname, isAbsolute, relative, resolve, sep } from 'node:path'
import { findFlowReferences, FlowReferenceError, type StoredFlowFile } from '@deja-qa/core'

/** A flow's references cannot be stored safely, so the flow is not remembered. */
export class FlowClosureError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(`Cannot store flow: ${message}`, options)
    this.name = 'FlowClosureError'
  }
}

const MAX_FILES = 64
const MAX_DEPTH = 16
const MAX_FILE_BYTES = 1024 * 1024

export type ClosureFile = {
  /** Path from the closure root, with `/` separators. */
  path: string
  sha256: string
  bytes: Buffer
}

export type FlowClosure = {
  /** The main flow's path from the closure root. */
  main: string
  /** Every file, the main flow included, sorted by path. */
  files: ClosureFile[]
  /** One checksum over every path and file checksum. */
  sha256: string
}

function sha256(contents: Buffer | string) {
  return createHash('sha256').update(contents).digest('hex')
}

function inside(root: string, candidate: string) {
  return candidate !== root && candidate.startsWith(`${root}${sep}`)
}

/** Dotfiles (`.env*`) and common key files are never copied into storage. */
function secretLooking(path: string) {
  const name = basename(path)
  return name.startsWith('.') || /\.(?:pem|key|p12|pfx|jks|keystore)$/i.test(name) ||
    /^(?:id_rsa|id_ed25519|id_ecdsa|credentials)/i.test(name)
}

function posix(path: string) {
  return path.split(sep).join('/')
}

function commonDirectory(directories: string[]) {
  let common = directories[0]
  for (const directory of directories.slice(1)) {
    while (directory !== common && !directory.startsWith(`${common}${sep}`)) {
      const parent = dirname(common)
      if (parent === common) break
      common = parent
    }
  }
  return common
}

/**
 * Read a flow and every file it runs, at any depth. `root` is the canonical workspace root and
 * `main` the canonical path of the flow. References resolve against the file that holds them, as
 * Maestro does, and must stay inside the workspace by path and after symlinks resolve.
 */
export async function collectFlowClosure(root: string, main: string): Promise<FlowClosure> {
  const found = new Map<string, Buffer>()

  async function read(path: string) {
    let canonical: string
    try {
      canonical = await realpath(path)
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === 'ENOENT') {
        throw new FlowClosureError(`referenced file does not exist: ${posix(relative(root, path))}`)
      }
      throw error
    }
    if (!inside(root, canonical)) {
      throw new FlowClosureError(`reference escapes the workspace: ${posix(relative(root, path))}`)
    }
    const info = await stat(canonical)
    if (!info.isFile() || info.size > MAX_FILE_BYTES) {
      throw new FlowClosureError(`referenced path is not a regular file under 1 MiB: ${posix(relative(root, path))}`)
    }
    return readFile(canonical)
  }

  async function visit(path: string, kind: 'flow' | 'script', stack: string[]): Promise<void> {
    if (stack.includes(path)) {
      throw new FlowClosureError(`flows run each other in a cycle: ${posix(relative(root, path))}`)
    }
    if (stack.length >= MAX_DEPTH) throw new FlowClosureError(`flows are nested more than ${MAX_DEPTH} deep`)
    if (found.has(path)) return
    // Spellings that differ only in case name one file on a case-insensitive disk, but two on a
    // case-sensitive one, where the stored copy would break. Ask for one spelling.
    const sameName = [...found.keys()].find(other => other.toLowerCase() === path.toLowerCase())
    if (sameName) {
      throw new FlowClosureError(
        `one file is spelled two ways, differing only in case: ${posix(relative(root, sameName))} and ${posix(relative(root, path))}`,
      )
    }
    const bytes = await read(path)
    found.set(path, bytes)
    if (found.size > MAX_FILES) throw new FlowClosureError(`flow runs more than ${MAX_FILES} files`)
    if (kind === 'script') return
    let references
    try {
      references = findFlowReferences(bytes.toString('utf8'))
    } catch (error) {
      if (error instanceof FlowReferenceError) {
        throw new FlowClosureError(`${posix(relative(root, path))}: ${error.message}`, { cause: error })
      }
      throw error
    }
    for (const reference of references) {
      if (reference.path.includes('\0')) {
        throw new FlowClosureError('reference contains a NUL byte')
      }
      if (secretLooking(reference.path)) {
        throw new FlowClosureError(
          `reference looks like a secret or hidden file and would be copied into .deja/flows: ${reference.path}`,
        )
      }
      if (reference.path.includes('$') || reference.path.includes('{')) {
        throw new FlowClosureError(`reference uses a variable, so its file cannot be known: ${reference.path}`)
      }
      if (isAbsolute(reference.path) || /^[A-Za-z]:[\\/]/.test(reference.path) ||
          reference.path.includes('\\')) {
        throw new FlowClosureError(`reference must be a relative path: ${reference.path}`)
      }
      const target = resolve(dirname(path), reference.path)
      if (!inside(root, target)) {
        throw new FlowClosureError(`reference escapes the workspace: ${reference.path}`)
      }
      await visit(target, reference.kind, [...stack, path])
    }
  }

  await visit(main, 'flow', [])
  const base = commonDirectory([...found.keys()].map(path => dirname(path)))
  const files = [...found.entries()]
    .map(([path, bytes]) => ({ path: posix(relative(base, path)), sha256: sha256(bytes), bytes }))
    .sort((left, right) => (left.path < right.path ? -1 : left.path > right.path ? 1 : 0))
  return {
    main: posix(relative(base, main)),
    files,
    sha256: sha256(files.map(file => `${file.path}\n${file.sha256}\n`).join('')),
  }
}

export type StoredClosurePlan = {
  /** Workspace-relative path of the main flow. */
  flowPath: string
  flowSha256: string
  /** Every file to write, workspace-relative, the main flow included. */
  writes: { path: string; bytes: Buffer }[]
  /** Metadata for the files other than the main flow. Empty when the flow runs nothing. */
  subflowFiles: StoredFlowFile[]
}

/**
 * Where a closure is stored. A flow that runs nothing keeps the original single-file layout.
 * Otherwise the closure gets its own folder, named by the checksum of all its files, so a changed
 * subflow is a new folder and never overwrites the bytes an older walk was verified with.
 */
export function planStoredClosure(
  closure: FlowClosure,
  goalSlug: string,
  platformSlug: string,
): StoredClosurePlan {
  const main = closure.files.find(file => file.path === closure.main)!
  if (closure.files.length === 1) {
    const flowPath = `.deja/flows/${goalSlug}/${platformSlug}-${main.sha256}.yaml`
    return {
      flowPath, flowSha256: main.sha256, writes: [{ path: flowPath, bytes: main.bytes }], subflowFiles: [],
    }
  }
  const folder = `.deja/flows/${goalSlug}/${platformSlug}-${closure.sha256}`
  const stored = closure.files.map(file => ({
    path: `${folder}/${file.path}`, sha256: file.sha256, bytes: file.bytes, main: file === main,
  }))
  return {
    flowPath: `${folder}/${closure.main}`,
    flowSha256: main.sha256,
    writes: stored.map(({ path, bytes }) => ({ path, bytes })),
    subflowFiles: stored.filter(file => !file.main).map(({ path, sha256: checksum }) => ({
      path, sha256: checksum,
    })),
  }
}

/** Read `subflowFiles` metadata. Returns null when it is malformed. */
export function readStoredFlowFiles(value: unknown): StoredFlowFile[] | null {
  if (!Array.isArray(value)) return null
  const files: StoredFlowFile[] = []
  for (const entry of value) {
    if (typeof entry !== 'object' || entry === null) return null
    const { path, sha256: checksum } = entry as Record<string, unknown>
    if (typeof path !== 'string' || !path.startsWith('.deja/flows/') || isAbsolute(path) ||
        path.split('/').includes('..') || typeof checksum !== 'string' ||
        !/^[a-f0-9]{64}$/.test(checksum)) {
      return null
    }
    files.push({ path, sha256: checksum })
  }
  return files
}

export type StoredClosureCheck =
  | { status: 'ok' }
  | { status: 'missing'; path: string }
  | { status: 'invalid'; message: string }

/**
 * Check that every file a stored walk runs is present, under `.deja/flows`, and unchanged, and that
 * the metadata lists everything the stored flows run. The last part re-reads the references from
 * the stored bytes, so metadata that was stripped or cut short cannot hide a file. `subflowFiles`
 * is undefined only for a walk from before memory schema 3, which is not checked.
 */
export async function checkStoredSubflows(
  root: string,
  flowPath: string,
  subflowFiles: StoredFlowFile[] | undefined,
): Promise<StoredClosureCheck> {
  if (!subflowFiles) return { status: 'ok' }
  const flowRoot = resolve(root, '.deja/flows')
  for (const file of subflowFiles) {
    const candidate = resolve(root, file.path)
    if (!inside(flowRoot, candidate)) {
      return { status: 'invalid', message: `stored subflow must stay under .deja/flows: ${file.path}` }
    }
    let canonical: string
    try {
      canonical = await realpath(candidate)
      const canonicalFlowRoot = await realpath(flowRoot)
      if (!inside(canonicalFlowRoot, canonical) || !(await stat(canonical)).isFile()) {
        return { status: 'invalid', message: `canonical stored subflow escapes .deja/flows: ${file.path}` }
      }
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === 'ENOENT') return { status: 'missing', path: file.path }
      return { status: 'invalid', message: `stored subflow cannot be resolved: ${file.path}` }
    }
    if (sha256(await readFile(canonical)) !== file.sha256) {
      return { status: 'invalid', message: `stored subflow checksum does not match ${file.path}` }
    }
  }
  const listed = new Set(subflowFiles.map(file => file.path))
  for (const path of [flowPath, ...listed].filter(item => /\.ya?ml$/i.test(item))) {
    const absolute = resolve(root, path)
    let references
    try {
      references = findFlowReferences((await readFile(absolute)).toString('utf8'))
    } catch (error) {
      if (error instanceof FlowReferenceError) {
        return { status: 'invalid', message: `stored flow ${path} cannot be read for references: ${error.message}` }
      }
      throw error
    }
    for (const reference of references) {
      const target = posix(relative(root, resolve(dirname(absolute), reference.path)))
      if (!listed.has(target)) {
        return {
          status: 'invalid',
          message: `stored flow ${path} runs ${reference.path}, which the walk metadata does not list`,
        }
      }
    }
  }
  return { status: 'ok' }
}

export type CandidateClosureSnapshot = {
  /** Path of the copied main flow, relative to `root`. */
  candidatePath: string
  closureSha256: string
}

/** Copy a candidate's whole closure under `directory` (inside `root`), keeping its layout. */
export async function snapshotFlowClosure(
  root: string,
  candidate: string,
  directory: string,
): Promise<CandidateClosureSnapshot> {
  const closure = await collectFlowClosure(root, candidate)
  const target = resolve(root, directory)
  if (!inside(root, target)) throw new FlowClosureError(`snapshot folder escapes the workspace: ${directory}`)
  await mkdir(target, { recursive: true })
  if (!inside(root, await realpath(target))) {
    throw new FlowClosureError(`snapshot folder escapes the workspace: ${directory}`)
  }
  for (const file of closure.files) {
    const destination = resolve(target, file.path)
    await mkdir(dirname(destination), { recursive: true })
    await writeFile(destination, file.bytes)
  }
  return {
    candidatePath: posix(relative(root, resolve(target, closure.main))),
    closureSha256: closure.sha256,
  }
}
