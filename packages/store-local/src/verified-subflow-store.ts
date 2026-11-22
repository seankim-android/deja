import { createHash } from 'node:crypto'
import { mkdir, readFile, realpath, rename, stat, writeFile } from 'node:fs/promises'
import { dirname, isAbsolute, resolve, sep } from 'node:path'
import {
  validateSubflowContract,
  VERIFIED_SUBFLOW_SCHEMA_VERSION,
  sameSubflowIdentity,
  subflowMissReason,
  type PersistVerifiedSubflowOptions,
  type SubflowContract,
  type SubflowIdentity,
  type VerifiedSubflow,
  type VerifiedSubflowLookup,
  type VerifiedSubflowStore,
} from '../../core/src/subflow.ts'

export const VERIFIED_SUBFLOW_INDEX_PATH = '.walkcache/segments/index.json'

const CONTRACT_KEYS = [
  'schemaVersion', 'segmentId', 'identity', 'inputs', 'outputs', 'preconditions',
  'postconditions', 'stepCount', 'assertionCount', 'provenance',
] as const

type SubflowIndex = {
  schemaVersion: typeof VERIFIED_SUBFLOW_SCHEMA_VERSION
  segments: VerifiedSubflow[]
}

export class InvalidSubflowMemoryError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(`Invalid subflow memory: ${message}`, options)
    this.name = 'InvalidSubflowMemoryError'
  }
}

export type LocalVerifiedSubflowStoreOptions = {
  workspaceRoot: string
  /** Test-only injection; production uses fs.rename. */
  renameFile?: (temporaryPath: string, destinationPath: string) => Promise<void>
}

function errorCode(error: unknown) {
  return (error as NodeJS.ErrnoException).code
}

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function timestamp(value: unknown): value is string {
  return typeof value === 'string' && value.length === 24 &&
    Number.isFinite(Date.parse(value)) && new Date(value).toISOString() === value
}

function fingerprint(value: unknown): value is string {
  return typeof value === 'string' && /^[a-f0-9]{64}$/.test(value)
}

function inside(root: string, candidate: string) {
  return candidate !== root && candidate.startsWith(`${root}${sep}`)
}

function slug(value: string) {
  const result = value.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '')
  if (!result) throw new TypeError('A path component must contain a letter or number')
  return result
}

function readSegment(value: unknown): VerifiedSubflow {
  if (!object(value)) throw new InvalidSubflowMemoryError('segment entry must be an object')
  const allowed = new Set([...CONTRACT_KEYS, 'flowPath', 'flowSha256', 'lastValidation'])
  if (Object.keys(value).some(key => !allowed.has(key))) {
    throw new InvalidSubflowMemoryError('segment entry has unsupported fields')
  }
  const contractValue: Record<string, unknown> = {}
  for (const key of CONTRACT_KEYS) contractValue[key] = value[key]
  let contract: SubflowContract
  try {
    contract = validateSubflowContract(contractValue)
  } catch (error) {
    throw new InvalidSubflowMemoryError('segment contract is invalid', { cause: error })
  }
  if (typeof value.flowPath !== 'string' || isAbsolute(value.flowPath) ||
      !value.flowPath.startsWith('.walkcache/segments/') ||
      value.flowPath.split('/').includes('..') || !fingerprint(value.flowSha256) ||
      !object(value.lastValidation) || value.lastValidation.passed !== true ||
      !timestamp(value.lastValidation.at)) {
    throw new InvalidSubflowMemoryError('verified flow identity or validation is invalid')
  }
  return {
    ...contract,
    flowPath: value.flowPath,
    flowSha256: value.flowSha256,
    lastValidation: { passed: true, at: value.lastValidation.at },
  }
}

export class LocalVerifiedSubflowStore implements VerifiedSubflowStore {
  readonly workspaceRoot: string
  readonly indexPath: string
  private readonly renameFile: (temporaryPath: string, destinationPath: string) => Promise<void>

  constructor(options: LocalVerifiedSubflowStoreOptions) {
    this.workspaceRoot = resolve(options.workspaceRoot)
    this.indexPath = resolve(this.workspaceRoot, VERIFIED_SUBFLOW_INDEX_PATH)
    this.renameFile = options.renameFile ?? rename
  }

  private async canonicalRoot() {
    return realpath(this.workspaceRoot)
  }

  private async existingFile(root: string, path: string, allowAbsolute = false) {
    if (!allowAbsolute && isAbsolute(path)) {
      throw new InvalidSubflowMemoryError(`flow path must be relative: ${path}`)
    }
    const candidate = allowAbsolute && isAbsolute(path) ? resolve(path) : resolve(root, path)
    if (!inside(root, candidate)) {
      throw new InvalidSubflowMemoryError(`flow path escapes workspace: ${path}`)
    }
    try {
      const canonical = await realpath(candidate)
      if (!inside(root, canonical) || !(await stat(canonical)).isFile()) {
        throw new InvalidSubflowMemoryError(`flow path is not a workspace file: ${path}`)
      }
      return canonical
    } catch (error) {
      if (error instanceof InvalidSubflowMemoryError) throw error
      if (errorCode(error) === 'ENOENT') return null
      throw new InvalidSubflowMemoryError(`flow path cannot be resolved: ${path}`, { cause: error })
    }
  }

  private async loadIndex(): Promise<SubflowIndex> {
    let contents: string
    try {
      contents = await readFile(this.indexPath, 'utf8')
    } catch (error) {
      if (errorCode(error) === 'ENOENT') {
        return { schemaVersion: VERIFIED_SUBFLOW_SCHEMA_VERSION, segments: [] }
      }
      throw new InvalidSubflowMemoryError('segment index cannot be read', { cause: error })
    }
    let parsed: unknown
    try {
      parsed = JSON.parse(contents)
    } catch (error) {
      throw new InvalidSubflowMemoryError('segment index is not valid JSON', { cause: error })
    }
    if (!object(parsed) || parsed.schemaVersion !== VERIFIED_SUBFLOW_SCHEMA_VERSION ||
        !Array.isArray(parsed.segments) || Object.keys(parsed).some(key =>
          key !== 'schemaVersion' && key !== 'segments')) {
      throw new InvalidSubflowMemoryError('segment index schema is unsupported')
    }
    const segments = parsed.segments.map(readSegment)
    if (new Set(segments.map(segment => segment.segmentId)).size !== segments.length) {
      throw new InvalidSubflowMemoryError('duplicate segment IDs are ambiguous')
    }
    return { schemaVersion: VERIFIED_SUBFLOW_SCHEMA_VERSION, segments }
  }

  private async atomicWrite(path: string, contents: string | Buffer) {
    const root = await this.canonicalRoot()
    await mkdir(dirname(path), { recursive: true })
    const canonicalParent = await realpath(dirname(path))
    if (!inside(root, canonicalParent)) {
      throw new InvalidSubflowMemoryError(`atomic write destination escapes workspace: ${path}`)
    }
    const temporary = `${path}.${process.pid}.tmp`
    await writeFile(temporary, contents)
    await this.renameFile(temporary, path)
  }

  private async writeIndex(index: SubflowIndex) {
    await this.atomicWrite(this.indexPath, `${JSON.stringify(index, null, 2)}\n`)
  }

  async persistVerified(options: PersistVerifiedSubflowOptions): Promise<VerifiedSubflow> {
    if (!options.validation.passed || !timestamp(options.validation.at)) {
      throw new Error('A reusable segment requires an independently passing validation')
    }
    const contract = validateSubflowContract(options.contract)
    const root = await this.canonicalRoot()
    const candidate = await this.existingFile(root, options.candidatePath, true)
    if (!candidate) throw new Error(`Verified segment candidate does not exist: ${options.candidatePath}`)
    const index = await this.loadIndex()
    const bytes = await readFile(candidate)
    const flowSha256 = createHash('sha256').update(bytes).digest('hex')
    const flowPath = `.walkcache/segments/${slug(contract.segmentId)}/${
      slug(contract.identity.platform)}-${flowSha256}.yaml`
    await this.atomicWrite(resolve(root, flowPath), bytes)
    const segment: VerifiedSubflow = {
      ...contract,
      flowPath,
      flowSha256,
      lastValidation: { passed: true, at: options.validation.at },
    }
    const remaining = index.segments.filter(item => item.segmentId !== contract.segmentId)
    await this.writeIndex({
      schemaVersion: VERIFIED_SUBFLOW_SCHEMA_VERSION,
      segments: [...remaining, segment],
    })
    return segment
  }

  private async storedFlowPath(segment: VerifiedSubflow): Promise<string | null> {
    const root = await this.canonicalRoot()
    const segmentRoot = resolve(root, '.walkcache/segments')
    const candidate = resolve(root, segment.flowPath)
    if (!inside(segmentRoot, candidate)) {
      throw new InvalidSubflowMemoryError('stored segment escapes its flow directory')
    }
    const canonical = await this.existingFile(root, segment.flowPath)
    if (!canonical) return null
    const canonicalRoot = await realpath(segmentRoot)
    if (!inside(root, canonicalRoot) || !inside(canonicalRoot, canonical)) {
      throw new InvalidSubflowMemoryError('canonical segment path escapes its flow directory')
    }
    return canonical
  }

  async lookup(segmentId: string, identity: SubflowIdentity): Promise<VerifiedSubflowLookup> {
    if (typeof segmentId !== 'string' || !segmentId.trim()) {
      throw new TypeError('A segment ID is required')
    }
    const index = await this.loadIndex()
    const matching = index.segments.filter(segment => segment.segmentId === segmentId)
    if (matching.length === 0) return { status: 'miss', reason: 'not-found' }
    const segment = matching.find(item => sameSubflowIdentity(item.identity, identity))
    if (!segment) return { status: 'miss', reason: subflowMissReason(identity, matching) }
    const flowPath = await this.storedFlowPath(segment)
    if (!flowPath) return { status: 'miss', reason: 'missing-flow' }
    const flowSha256 = createHash('sha256').update(await readFile(flowPath)).digest('hex')
    if (flowSha256 !== segment.flowSha256) {
      throw new InvalidSubflowMemoryError(`stored segment checksum does not match ${segment.flowPath}`)
    }
    return { status: 'hit', segment, flowPath }
  }

  async list(): Promise<VerifiedSubflow[]> {
    return [...(await this.loadIndex()).segments]
  }
}
