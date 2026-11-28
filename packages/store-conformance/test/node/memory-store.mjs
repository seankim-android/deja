// A third-party WalkStore: it knows nothing of store-local or store-sqlite. Walk metadata lives in
// memory (shared per workspace, so two stores for one workspace agree). Only the verified flow is
// written to disk, because the runner executes a file. Written against @deja-run/core alone.
import { createHash } from 'node:crypto'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import { ExpectedFlowChecksumMismatchError, normalizeGoal } from '@deja-run/core'

const memory = new Map()

const sha256 = bytes => createHash('sha256').update(bytes).digest('hex')
const same = (walk, identity, goalKey) =>
  walk.goalKey === goalKey && walk.appId === identity.appId && walk.platform === identity.platform &&
  walk.flowSchemaVersion === identity.flowSchemaVersion && (walk.adapter ?? null) === (identity.adapter ?? null) &&
  (walk.browser ?? null) === (identity.browser ?? null) && walk.buildFingerprint === identity.buildFingerprint

export class MemoryWalkStore {
  constructor(workspaceRoot) {
    this.workspaceRoot = resolve(workspaceRoot)
    if (!memory.has(this.workspaceRoot)) memory.set(this.workspaceRoot, [])
    this.walks = memory.get(this.workspaceRoot)
  }

  async lookup(identity) {
    const { key } = normalizeGoal(identity.goal)
    const miss = reason => ({ status: 'miss', reason })
    let rest = this.walks.filter(walk => walk.goalKey === key)
    if (!rest.length) return miss('not-found')
    rest = rest.filter(walk => walk.appId === identity.appId)
    if (!rest.length) return miss('wrong-app')
    rest = rest.filter(walk => walk.platform === identity.platform)
    if (!rest.length) return miss('wrong-platform')
    rest = rest.filter(walk => walk.flowSchemaVersion === identity.flowSchemaVersion)
    if (!rest.length) return miss('wrong-flow-schema')
    const walk = rest.find(item => item.buildFingerprint === identity.buildFingerprint &&
      (item.adapter ?? null) === (identity.adapter ?? null) && (item.browser ?? null) === (identity.browser ?? null))
    if (!walk) return miss('stale-build')
    if (!walk.lastValidation.passed) return miss('failed-validation')
    const flowPath = resolve(this.workspaceRoot, walk.flowPath)
    let bytes
    try { bytes = await readFile(flowPath) } catch { return miss('missing-flow') }
    if (sha256(bytes) !== walk.flowSha256) throw new Error(`stored flow checksum does not match ${walk.flowPath}`)
    return { status: 'hit', walk: structuredClone(walk), flowPath }
  }

  async lookupPreviousSuccessful(identity) {
    const { key } = normalizeGoal(identity.goal)
    const found = this.walks.filter(walk => walk.goalKey === key && walk.appId === identity.appId &&
      walk.platform === identity.platform && walk.flowSchemaVersion === identity.flowSchemaVersion && walk.lastValidation.passed)
    if (!found.length) return { status: 'miss', reason: 'not-found' }
    found.sort((a, b) => b.lastValidation.at.localeCompare(a.lastValidation.at))
    return { status: 'hit', walk: structuredClone(found[0]), flowPath: resolve(this.workspaceRoot, found[0].flowPath) }
  }

  async persistVerifiedCandidate(options) {
    const bytes = await readFile(resolve(this.workspaceRoot, options.candidatePath))
    const flowSha256 = sha256(bytes)
    if (options.expectedFlowSha256 !== undefined && options.expectedFlowSha256 !== flowSha256) {
      throw new ExpectedFlowChecksumMismatchError(options.expectedFlowSha256, flowSha256)
    }
    const { text, key } = normalizeGoal(options.identity.goal)
    const flowPath = `.deja/memory-store/${flowSha256}.yaml`
    await mkdir(join(this.workspaceRoot, '.deja/memory-store'), { recursive: true })
    await writeFile(join(this.workspaceRoot, flowPath), bytes)
    const walk = {
      ...options.identity, goal: text, goalKey: key, flowPath, flowSha256, subflowFiles: [],
      lastValidation: { ...options.validation },
      validationHistory: [{ ...options.validation, flowSha256 }],
      origin: options.origin,
    }
    const index = this.walks.findIndex(item => same(item, options.identity, key))
    if (index < 0) this.walks.push(walk)
    else this.walks[index] = walk
    return { walk: structuredClone(walk), flowPath, flowSha256, storedMetadataPath: 'memory', atomicWrite: true }
  }

  async persistVerifiedRepair() {
    throw new Error('MemoryWalkStore does not store repairs')
  }

  async recordValidation(identity, validation) {
    const hit = await this.lookup(identity)
    if (hit.status !== 'hit') throw new Error(`cannot validate a flow that is not a usable hit: ${hit.reason}`)
    const walk = this.walks.find(item => same(item, identity, hit.walk.goalKey))
    walk.lastValidation = { passed: validation.passed, at: validation.at }
    walk.validationHistory.push({ ...validation, flowSha256: walk.flowSha256 })
    return structuredClone(walk)
  }

  async list() {
    return structuredClone(this.walks)
  }
}

export const memoryStoreContract = {
  name: 'MemoryWalkStore (third-party fixture)',
  createStore: workspaceRoot => new MemoryWalkStore(workspaceRoot),
}
