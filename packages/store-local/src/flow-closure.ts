import { createHash } from 'node:crypto'
import { mkdir, readFile, realpath, stat, writeFile } from 'node:fs/promises'
import { dirname, isAbsolute, relative, resolve, sep } from 'node:path'
import { ClosureRefusedError, findFlowReferences, FlowFileCommandError, FlowReferenceError, type StoredFlowFile } from '@deja-qa/core'

/** A flow's references cannot be stored safely, so the flow is not remembered. */
export class FlowClosureError extends ClosureRefusedError {
  constructor(message: string, options?: ErrorOptions) {
    super(`Cannot store flow: ${message}`, options)
    this.name = 'FlowClosureError'
  }
}

/** A file the flow runs is gone. In manifest mode the consumer deleted or moved it. */
export class MissingFlowFileError extends FlowClosureError {
  constructor(path: string) {
    super(`referenced file does not exist: ${path}`)
    this.name = 'MissingFlowFileError'
  }
}

const MAX_FILES = 64
const MAX_DEPTH = 16
const MAX_FILE_BYTES = 1024 * 1024
/** `addMedia` files are images and short videos. They are hashed and copied as bytes. */
const MAX_MEDIA_BYTES = 8 * 1024 * 1024
/** All media of one closure together. Each replay copies the closure again. */
const MAX_TOTAL_MEDIA_BYTES = 32 * 1024 * 1024

export type ClosureFile = {
  /** Path from the closure root, with `/` separators. */
  path: string
  sha256: string
  bytes: Buffer
  /**
   * `script` is a JavaScript file run by `runScript`, `media` a binary file `addMedia` reads; the
   * rest are Maestro flows.
   */
  kind: 'flow' | 'script' | 'media'
  /** Where the file was read from: its workspace-relative path, with `/` separators. */
  source: string
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

/** Key and environment files are never copied into storage, whatever their folder. */
function secretName(name: string) {
  return /\.(?:pem|key|p12|pfx|jks|keystore|env)$/i.test(name) || /^(?:id_rsa|id_ed25519|id_ecdsa|credentials)/i.test(name)
}

/**
 * A hidden folder or file (`.env`, `.secrets/`, `.git/`, `.aws/`) is not copied unless the main flow
 * itself sits under one of that name (the workspace's own `.maestro` or `.deja`). Both the path as
 * resolved and the path after symlinks are checked, so an innocent name cannot point at a secret.
 */
function secretLooking(root: string, path: string, allowedHidden: Set<string>) {
  const segments = posix(relative(root, path)).split('/')
  return segments.some(segment => segment.startsWith('.') && !allowedHidden.has(segment)) ||
    secretName(segments.at(-1) ?? '')
}

function posix(path: string) {
  return path.split(sep).join('/')
}

/**
 * Spellings that differ only in case or Unicode normalization (`é` as one code point or as `e` plus
 * a combining accent) name one file on some disks (macOS) and two on others (Linux).
 */
function spellingKey(path: string) {
  return path.normalize('NFC').toLowerCase()
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
  const found = new Map<string, { bytes: Buffer; kind: ClosureFile['kind'] }>()
  let mediaBytes = 0
  const allowedHidden = new Set(
    posix(relative(root, main)).split('/').slice(0, -1).filter(segment => segment.startsWith('.')),
  )

  async function read(path: string, kind: ClosureFile['kind']) {
    let canonical: string
    try {
      canonical = await realpath(path)
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === 'ENOENT') {
        throw new MissingFlowFileError(posix(relative(root, path)))
      }
      throw error
    }
    if (!inside(root, canonical)) {
      throw new FlowClosureError(`reference escapes the workspace: ${posix(relative(root, path))}`)
    }
    if (path !== main && (secretLooking(root, path, allowedHidden) || secretLooking(root, canonical, allowedHidden))) {
      throw new FlowClosureError(
        `reference looks like a secret or hidden file and would be copied into .deja/flows: ${posix(relative(root, path))}`,
      )
    }
    const info = await stat(canonical)
    const limit = kind === 'media' ? MAX_MEDIA_BYTES : MAX_FILE_BYTES
    if (!info.isFile() || info.size > limit) {
      throw new FlowClosureError(
        `referenced path is not a regular file under ${limit / 1024 / 1024} MiB: ${posix(relative(root, path))}`,
      )
    }
    return readFile(canonical)
  }

  async function visit(path: string, kind: ClosureFile['kind'], stack: string[]): Promise<void> {
    if (stack.includes(path)) {
      throw new FlowClosureError(`flows run each other in a cycle: ${posix(relative(root, path))}`)
    }
    if (stack.length >= MAX_DEPTH) throw new FlowClosureError(`flows are nested more than ${MAX_DEPTH} deep`)
    const earlier = found.get(path)
    if (earlier) {
      // One file read as a flow and as media or a script would be hashed as one and followed as the
      // other, so its own references could go unchecked.
      if (earlier.kind !== kind) {
        throw new FlowClosureError(`one file is used as a ${earlier.kind} and as a ${kind}: ${posix(relative(root, path))}`)
      }
      return
    }
    // Spellings that differ only in case or Unicode normalization name one file on some disks, but
    // two on others, where the stored copy would break. Ask for one spelling.
    const sameName = [...found.keys()].find(other => spellingKey(other) === spellingKey(path))
    if (sameName) {
      throw new FlowClosureError(
        `one file is spelled two ways, differing only in case or Unicode normalization: ${posix(relative(root, sameName))} and ${posix(relative(root, path))}`,
      )
    }
    const bytes = await read(path, kind)
    found.set(path, { bytes, kind })
    if (kind === 'media') {
      mediaBytes += bytes.length
      if (mediaBytes > MAX_TOTAL_MEDIA_BYTES) {
        throw new FlowClosureError(`media files add up to more than ${MAX_TOTAL_MEDIA_BYTES / 1024 / 1024} MiB`)
      }
    }
    if (found.size > MAX_FILES) throw new FlowClosureError(`flow runs more than ${MAX_FILES} files`)
    // A script is not YAML, and media is binary: neither is read for references.
    if (kind !== 'flow') return
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
    .map(([path, { bytes, kind }]) => ({
      path: posix(relative(base, path)), sha256: sha256(bytes), bytes, kind, source: posix(relative(root, path)),
    }))
    .sort((left, right) => (left.path < right.path ? -1 : left.path > right.path ? 1 : 0))
  return {
    main: posix(relative(base, main)),
    files,
    sha256: closureSha256Of(files),
  }
}

/** The closure checksum: every path (from the closure root) and file checksum, sorted by path. */
export function closureSha256Of(files: { path: string; sha256: string }[]) {
  const sorted = [...files].sort((left, right) => (left.path < right.path ? -1 : left.path > right.path ? 1 : 0))
  return sha256(sorted.map(file => `${file.path}\n${file.sha256}\n`).join(''))
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
  | { status: 'legacy' }
  | { status: 'missing'; path: string }
  | { status: 'invalid'; message: string }

/**
 * Check that every file a stored walk runs is present, under `.deja/flows`, and unchanged, and that
 * the metadata lists everything the stored flows run. The last part re-reads the references from
 * the stored bytes, so metadata that was stripped or cut short cannot hide a file. `subflowFiles`
 * is undefined only for a walk from before memory schema 3, which is not checked. A walk marked
 * `legacyClosure` was migrated from one: it is `legacy` when its flow runs files, since those were
 * never stored.
 */
export async function checkStoredSubflows(
  root: string,
  flowPath: string,
  subflowFiles: StoredFlowFile[] | undefined,
  legacyClosure = false,
): Promise<StoredClosureCheck> {
  if (!subflowFiles || legacyClosure) {
    // Stored before subflows were kept. It replays only if its flow runs no file at all.
    try {
      return findFlowReferences((await readFile(resolve(root, flowPath))).toString('utf8')).length > 0
        ? { status: 'legacy' } : { status: 'ok' }
    } catch (error) {
      if (error instanceof FlowReferenceError || (error as NodeJS.ErrnoException).code === 'ENOENT') {
        return { status: 'legacy' }
      }
      throw error
    }
  }
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
      // A command this build now refuses (assertScreenshot, a malformed addMedia) was never stored by
      // an older one, which ignored it. Treat the walk as stored before subflows were kept.
      if (error instanceof FlowFileCommandError) return { status: 'legacy' }
      if (error instanceof FlowReferenceError) {
        return { status: 'invalid', message: `stored flow ${path} cannot be read for references: ${error.message}` }
      }
      throw error
    }
    for (const reference of references) {
      const target = posix(relative(root, resolve(dirname(absolute), reference.path)))
      if (!listed.has(target)) {
        // `addMedia` and `retry` files were not part of a closure before DEJA-32, so a walk stored
        // then cannot list them. Treat it as stored before subflows were kept: remember it again.
        if (reference.command === 'addMedia' || reference.command === 'retry') return { status: 'legacy' }
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
  /** Every file copied: its source path in the workspace and its checksum. */
  files: { path: string; sha256: string }[]
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
    files: closure.files.map(file => ({ path: file.source, sha256: file.sha256 })),
  }
}
