import { createHash, randomUUID } from 'node:crypto'
import { lstat, mkdir, readFile, realpath, rename, stat, unlink, writeFile } from 'node:fs/promises'
import { dirname, isAbsolute, posix as posixPath, relative, resolve, sep } from 'node:path'
import {
  AssertionHashChangedError,
  ASSERTION_HASH_VERSION,
  checkManifestFlowPath,
  emptyWalkManifest,
  ExpectedClosureMismatchError,
  ExpectedFlowChecksumMismatchError,
  findFlowAssertions,
  FlowChangedError,
  InvalidWalkManifestError,
  manifestWalkKey,
  normalizeGoal,
  OwnerRuleError,
  parseWalkManifest,
  serializeWalkManifest,
  type CandidateSnapshot,
  type FlowAssertion,
  type LookupHit,
  type LookupMiss,
  type ManifestOwnerRules,
  type ManifestWalk,
  type PersistVerifiedOptions,
  type StoredWalk,
  type WalkIdentity,
  type WalkManifest,
} from '@deja-qa/core'
import { collectFlowClosure, FlowClosureError, MissingFlowFileError, type FlowClosure } from './flow-closure.ts'
import { StoreLockedError, WriterLock } from './writer-lock.ts'

/** The verified flow is not the consumer's own file, so manifest mode cannot keep it. */
export class ConsumerFlowError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(`Consumer flow: ${message}`, options)
    this.name = 'ConsumerFlowError'
  }
}

/** The manifest changed on disk between lookup and write, so nothing was written. */
export class ManifestChangedError extends Error {
  constructor() {
    super('The manifest changed while this run was saving; nothing was written. Run again.')
    this.name = 'ManifestChangedError'
  }
}

/** Another run is writing the shared manifest. Unlike a store lock, a different `--store` does not help. */
export class ManifestLockedError extends Error {
  readonly lockPath: string

  constructor(lockPath: string) {
    super(
      `Another run is writing the walk manifest (${lockPath}). All stores of one workspace share it: ` +
      'run again in a moment. If no run is active, delete that lock file.',
    )
    this.name = 'ManifestLockedError'
    this.lockPath = lockPath
  }
}

const MAX_HISTORY = 100
const LOCK_RETRIES = 40
const LOCK_WAIT_MS = 50

function sha256(contents: Buffer | string) {
  return createHash('sha256').update(contents).digest('hex')
}

function posix(path: string) {
  return path.split(sep).join('/')
}

function inside(root: string, candidate: string) {
  return candidate !== root && candidate.startsWith(`${root}${sep}`)
}

function errorCode(error: unknown) {
  return (error as NodeJS.ErrnoException).code
}

/** One line of the assertion hash, with where it sits and how it is reached. */
export type ClosureAssertion = FlowAssertion & {
  /** True when the line runs whenever the flow does: every call on the way is unconditional too. */
  reachedAlways: boolean
}

/**
 * The assertions of a closure in the order the flow runs them (hash form version 1). Every line
 * carries the path of its file and the chain of calls that reaches it, each with its parameters
 * and the blocks around it. So moving an assertion into a subflow that is called under a condition,
 * moving it between files, or making the script that feeds it return something else, changes it.
 */
export function closureAssertions(closure: FlowClosure): ClosureAssertion[] {
  const files = new Map(closure.files.map(file => [file.path, file]))
  const found: ClosureAssertion[] = []
  const walk = (path: string, chain: string[], reachedAlways: boolean) => {
    if (chain.length > 16) throw new FlowClosureError('flows are nested more than 16 deep')
    const file = files.get(path)!
    for (const item of findFlowAssertions(file.bytes.toString('utf8'))) {
      const line = `${path}|${chain.join(' > ')}|${item.entry}`
      found.push({ ...item, entry: line, reachedAlways })
      if (!item.call) continue
      const target = posixPath.normalize(posixPath.join(posixPath.dirname(path), item.call.path))
      const reached = files.get(target)
      if (!reached) throw new FlowClosureError(`${path} runs ${item.call.path}, which the closure does not hold`)
      if (item.call.kind === 'script') {
        // `assertTrue` can read what a script sets, so a script's bytes are part of the assertions.
        found.push({
          entry: `${line}|script=${target}|sha256=${reached.sha256}`, command: 'runScript', id: null,
          always: item.always, reachedAlways,
        })
      } else {
        walk(target, [...chain, line], reachedAlways && item.always)
      }
    }
  }
  walk(closure.main, [], true)
  return found
}

/** One hash over the ordered assertion lines, repeats kept. */
export function assertionsSha256(closure: FlowClosure): string {
  const lines = closureAssertions(closure).map(assertion => assertion.entry)
  return sha256(`deja-assertions-v${ASSERTION_HASH_VERSION}\n${lines.join('\n')}\n`)
}

function checkOwnerRules(rules: ManifestOwnerRules | undefined, assertions: ClosureAssertion[]) {
  if (!rules) return
  const asserts = assertions.filter(assertion => assertion.command.startsWith('assert'))
  if (rules.minAssertions !== undefined && asserts.length < rules.minAssertions) {
    throw new OwnerRuleError(`the flow has ${asserts.length} assertions, the manifest requires at least ${rules.minAssertions}`)
  }
  // A required id counts only when it always runs: not under a condition, a repeat or a retry, not
  // `optional`, and not behind a conditional call.
  const visibleIds = new Set(asserts
    .filter(item => item.command === 'assertVisible' && item.always && item.reachedAlways).map(item => item.id))
  for (const id of rules.requiredAssertionIds ?? []) {
    if (!visibleIds.has(id)) throw new OwnerRuleError(`the flow never runs assertVisible on id "${id}" unconditionally`)
  }
}

type NormalizedIdentity = WalkIdentity & { goalKey: string }

function sameLogical(walk: StoredWalk, identity: NormalizedIdentity) {
  return walk.goalKey === identity.goalKey && walk.appId === identity.appId &&
    walk.platform === identity.platform &&
    (walk.adapter ?? null) === (identity.adapter ?? null) &&
    (walk.browser ?? null) === (identity.browser ?? null)
}

/** Memory schema 4 walk fields: a consumer-owned walk has a closure sha and a workspace path, no copy. */
export function readConsumerWalkFields(value: { flowPath: string; closureSha256: unknown }) {
  if (typeof value.closureSha256 !== 'string' || !/^[a-f0-9]{64}$/.test(value.closureSha256)) {
    return 'closureSha256 must be a SHA-256'
  }
  try {
    checkManifestFlowPath(value.flowPath, 'flowPath')
  } catch (error) {
    return (error as Error).message
  }
  return null
}

export type ConsumerFlowsOptions = {
  workspaceRoot: string
  /** Relative to the workspace root. */
  manifestPath: string
  /** Fixture values that must never be written into the manifest. */
  forbiddenValues?: string[]
}

export type ConsumerPlan = {
  walk: StoredWalk
  /** The walks to keep next to the new one. */
  remaining: StoredWalk[]
  flowPath: string
  flowSha256: string
  /** Write the manifest if its entry changed. Call after memory is written, under `withLock`. */
  commit(): Promise<boolean>
}

/**
 * Manifest mode, shared by every store: the committed manifest names each flow, and the store keeps
 * history only. The consumer's files run in place; nothing is copied into `.deja/flows`.
 */
export class ConsumerFlows {
  readonly manifestPath: string
  private readonly workspaceRoot: string
  private readonly forbiddenValues: string[]

  constructor(options: ConsumerFlowsOptions) {
    this.manifestPath = checkManifestPath(options.manifestPath)
    this.workspaceRoot = resolve(options.workspaceRoot)
    this.forbiddenValues = (options.forbiddenValues ?? []).filter(value => value.length >= 3)
  }

  private canonicalRoot() {
    return realpath(this.workspaceRoot)
  }

  private async manifestFile() {
    const root = await this.canonicalRoot()
    const target = resolve(root, this.manifestPath)
    if (!inside(root, target)) throw new InvalidWalkManifestError('the manifest path escapes the workspace')
    return { root, target }
  }

  /** The manifest on disk. A missing file is an empty manifest; anything unreadable fails closed. */
  async load(): Promise<WalkManifest> {
    const { root, target } = await this.manifestFile()
    let contents: string
    try {
      const canonical = await realpath(target)
      if (!inside(root, canonical) || !(await stat(canonical)).isFile()) {
        throw new InvalidWalkManifestError('the manifest is not a regular file inside the workspace')
      }
      contents = await readFile(canonical, 'utf8')
    } catch (error) {
      if (error instanceof InvalidWalkManifestError) throw error
      if (errorCode(error) === 'ENOENT') return emptyWalkManifest()
      throw new InvalidWalkManifestError('the manifest cannot be read', { cause: error })
    }
    return parseWalkManifest(contents)
  }

  /** The consumer's closure for one manifest entry, or why there is none. */
  private async inspect(entry: ManifestWalk) {
    const root = await this.canonicalRoot()
    const absolute = resolve(root, entry.flow)
    if (!inside(root, absolute)) throw new InvalidWalkManifestError(`flow escapes the workspace: ${entry.flow}`)
    let canonical: string
    try {
      canonical = await realpath(absolute)
      if (!inside(root, canonical) || !(await stat(canonical)).isFile()) return { status: 'missing' as const }
    } catch (error) {
      if (errorCode(error) === 'ENOENT') return { status: 'missing' as const }
      throw error
    }
    try {
      return { status: 'ok' as const, closure: await collectFlowClosure(root, canonical), canonical }
    } catch (error) {
      if (error instanceof MissingFlowFileError) return { status: 'missing' as const }
      throw error
    }
  }

  /** Hash the consumer's closure now. Copies nothing: the flow is run in place, then re-hashed on save. */
  async snapshot(candidatePath: string): Promise<CandidateSnapshot> {
    const { canonical, root } = await this.consumerCandidate(candidatePath)
    const closure = await collectFlowClosure(root, canonical)
    return { candidatePath: posix(relative(root, canonical)), closureSha256: closure.sha256 }
  }

  private async consumerCandidate(candidatePath: string) {
    const root = await this.canonicalRoot()
    const candidate = isAbsolute(candidatePath) ? resolve(candidatePath) : resolve(root, candidatePath)
    let canonical: string
    try {
      canonical = await realpath(candidate)
    } catch (error) {
      if (errorCode(error) === 'ENOENT') throw new Error(`Verified candidate flow does not exist: ${candidatePath}`)
      throw error
    }
    if (!inside(root, canonical) || !(await stat(canonical)).isFile()) {
      throw new ConsumerFlowError(`the flow must be a file inside the workspace: ${candidatePath}`)
    }
    const flow = posix(relative(root, canonical))
    try {
      checkManifestFlowPath(flow)
    } catch (error) {
      throw new ConsumerFlowError(`${(error as Error).message}. With a manifest, the flow is the consumer's own file`, { cause: error })
    }
    return { root, canonical, flow }
  }

  async lookup(
    walks: StoredWalk[],
    identity: NormalizedIdentity,
    previousSuccessful = false,
  ): Promise<LookupHit | LookupMiss> {
    const manifest = await this.load()
    const sameGoal = manifest.walks.filter(entry => normalizeGoal(entry.goal).key === identity.goalKey)
    if (sameGoal.length === 0) {
      // A goal that only has a schema 3 walk must be migrated; manifest mode never reads its copy.
      const legacy = walks.some(walk => walk.closureSha256 === undefined && walk.goalKey === identity.goalKey)
      return { status: 'miss', reason: legacy ? 'needs-migration' : 'not-found' }
    }
    const sameApp = sameGoal.filter(entry => entry.appId === identity.appId)
    if (sameApp.length === 0) return { status: 'miss', reason: 'wrong-app' }
    const samePlatform = sameApp.filter(entry => entry.platform === identity.platform)
    if (samePlatform.length === 0) return { status: 'miss', reason: 'wrong-platform' }
    if (identity.flowSchemaVersion !== 'maestro/v1') return { status: 'miss', reason: 'wrong-flow-schema' }
    const sameAdapter = samePlatform.filter(entry => entry.adapter === (identity.adapter ?? null))
    if (sameAdapter.length === 0) return { status: 'miss', reason: 'wrong-adapter' }
    const entry = sameAdapter.find(candidate => (candidate.browser ?? null) === (identity.browser ?? null))
    if (!entry) return { status: 'miss', reason: 'wrong-browser' }

    const consumer = await this.inspect(entry)
    if (consumer.status === 'missing') return { status: 'miss', reason: 'missing-flow' }
    const flowPath = consumer.canonical
    if (consumer.closure.sha256 !== entry.closureSha256) {
      return {
        status: 'miss', reason: 'flow-changed', flowPath,
        flowChange: {
          manifestClosureSha256: entry.closureSha256,
          currentClosureSha256: consumer.closure.sha256,
        },
      }
    }
    const history = walks.filter(walk => walk.closureSha256 === entry.closureSha256 && sameLogical(walk, identity))
    const mainSha = consumer.closure.files.find(file => file.path === consumer.closure.main)!.sha256
    const asConsumerWalk = (walk: StoredWalk): StoredWalk => ({ ...walk, flowPath: entry.flow, flowSha256: mainSha })
    if (previousSuccessful) {
      const passed = history.filter(walk => walk.lastValidation.passed)
        .sort((left, right) => right.lastValidation.at.localeCompare(left.lastValidation.at))
      if (passed.length === 0) return { status: 'miss', reason: 'not-validated', flowPath }
      return { status: 'hit', walk: asConsumerWalk(passed[0]), flowPath }
    }
    if (history.length === 0) return { status: 'miss', reason: 'not-validated', flowPath }
    const walk = history.find(item => item.buildFingerprint === identity.buildFingerprint)
    if (!walk) return { status: 'miss', reason: 'stale-build', flowPath }
    if (!walk.lastValidation.passed) return { status: 'miss', reason: 'failed-validation', flowPath }
    return { status: 'hit', walk: asConsumerWalk(walk), flowPath }
  }

  /** Throws `FlowChangedError` if the consumer's files no longer match the manifest. */
  async assertUnchanged(walks: StoredWalk[], identity: NormalizedIdentity) {
    const result = await this.lookup(walks, identity)
    if (result.status === 'miss' && result.reason === 'flow-changed' && result.flowChange) {
      throw new FlowChangedError(result.flowChange.manifestClosureSha256, result.flowChange.currentClosureSha256)
    }
    return result
  }

  /** Run `work` holding the manifest lock, which all stores of one workspace share. */
  async withLock<T>(work: () => Promise<T>): Promise<T> {
    const root = await this.canonicalRoot()
    const lock = new WriterLock(resolve(root, '.deja/manifest.lock'))
    for (let attempt = 0; ; attempt += 1) {
      try {
        lock.acquire()
        break
      } catch (error) {
        if (!(error instanceof StoreLockedError)) throw error
        if (attempt >= LOCK_RETRIES) throw new ManifestLockedError(lock.path)
        await new Promise(done => setTimeout(done, LOCK_WAIT_MS))
      }
    }
    try {
      return await work()
    } finally {
      lock.release()
    }
  }

  /**
   * Decide what a verified pass writes, before anything is written. Call under `withLock`. Throws
   * before any write if the candidate is not the consumer's file, differs from what was verified,
   * breaks an owner rule, or changes the assertions without a reviewed `remember`.
   */
  async plan(walks: StoredWalk[], identity: NormalizedIdentity, options: PersistVerifiedOptions): Promise<ConsumerPlan> {
    if (!identity.adapter) throw new TypeError('A manifest walk needs the adapter in its identity')
    const { root, canonical, flow } = await this.consumerCandidate(options.candidatePath)
    const closure = await collectFlowClosure(root, canonical)
    const mainSha = closure.files.find(file => file.path === closure.main)!.sha256
    if (options.expectedFlowSha256 !== undefined && options.expectedFlowSha256 !== mainSha) {
      throw new ExpectedFlowChecksumMismatchError(options.expectedFlowSha256, mainSha)
    }
    if (options.expectedClosureSha256 !== undefined && options.expectedClosureSha256 !== closure.sha256) {
      throw new ExpectedClosureMismatchError(options.expectedClosureSha256, closure.sha256)
    }
    const assertions = closureAssertions(closure)
    const assertionsHash = assertionsSha256(closure)

    const manifest = await this.load()
    const key = manifestWalkKey({
      goal: identity.goal, appId: identity.appId, platform: identity.platform,
      adapter: identity.adapter, ...(identity.browser !== undefined ? { browser: identity.browser } : {}),
    })
    const existing = manifest.walks.find(entry => manifestWalkKey(entry) === key)
    if ((options.origin === 'verify' || options.origin === 'replayed-on-new-build') && !existing) {
      throw new ConsumerFlowError(`the manifest has no entry for "${identity.goal}"; only remember can add one`)
    }
    if (existing && (options.origin === 'verify' || options.origin === 'replayed-on-new-build') &&
        existing.flow !== flow) {
      throw new ConsumerFlowError(`the manifest lists ${existing.flow}, not ${flow}; only remember can move a flow`)
    }
    if (existing && options.origin === 'replayed-on-new-build' && existing.closureSha256 !== closure.sha256) {
      throw new FlowChangedError(existing.closureSha256, closure.sha256)
    }
    checkOwnerRules(existing?.ownerRules, assertions)
    if (existing && existing.assertionsSha256 !== assertionsHash) {
      const mayAccept = (options.origin === 'remember' || options.origin === 'explore') &&
        options.assertionChange === 'accept'
      if (!mayAccept) throw new AssertionHashChangedError(existing.assertionsSha256, assertionsHash)
    }

    const next: ManifestWalk = {
      goal: existing?.goal ?? identity.goal,
      appId: identity.appId,
      platform: identity.platform,
      adapter: identity.adapter,
      ...(identity.browser !== undefined ? { browser: identity.browser } : {}),
      flow,
      closureSha256: closure.sha256,
      assertionsSha256: assertionsHash,
      ...(existing?.ownerRules ? { ownerRules: existing.ownerRules } : {}),
      ...(existing?.profile ? { profile: existing.profile } : {}),
    }
    const changed = !existing || serializeWalkManifest({ ...manifest, walks: [existing] }) !==
      serializeWalkManifest({ ...manifest, walks: [next] })
    // Refuse before memory or the manifest is written, not after.
    if (changed) this.checkManifest({ ...manifest, walks: [...manifest.walks.filter(item => manifestWalkKey(item) !== key), next] })

    const previous = walks.find(walk => sameLogical(walk, identity) && walk.closureSha256 === closure.sha256)
    const entry = { passed: true as const, at: options.validation.at, flowSha256: mainSha }
    const exploration = previous?.exploration ?? options.exploration
    const walk: StoredWalk = {
      ...identity,
      flowPath: flow,
      flowSha256: mainSha,
      closureSha256: closure.sha256,
      lastValidation: { passed: true, at: options.validation.at },
      validationHistory: [...(previous?.validationHistory ?? []), entry].slice(-MAX_HISTORY),
      origin: options.origin,
      ...(exploration ? { exploration } : {}),
    }
    const remaining = walks.filter(item => !sameLogical(item, identity))
    return {
      walk, remaining, flowPath: flow, flowSha256: mainSha,
      commit: async () => {
        if (!changed) return false
        // Re-read under the lock: another store may have changed this entry since the plan was made.
        const current = await this.load()
        const now = current.walks.find(item => manifestWalkKey(item) === key)
        if (JSON.stringify(now ?? null) !== JSON.stringify(existing ?? null)) throw new ManifestChangedError()
        const walksNext = [...current.walks.filter(item => manifestWalkKey(item) !== key), next]
        await this.writeManifest({ ...current, walks: walksNext })
        return true
      },
    }
  }

  /** Throws if the manifest's bytes would hold a fixture value. Never names the value. */
  checkManifest(manifest: WalkManifest) {
    const text = serializeWalkManifest(manifest)
    for (const value of this.forbiddenValues) {
      if (text.includes(value)) {
        throw new InvalidWalkManifestError('the manifest would hold a fixture credential value; nothing was written')
      }
    }
    // Whatever is written must read back. A goal or app id that the reader refuses (a path, say)
    // would leave a manifest that fails every later run, so it is refused here, before any write.
    try {
      parseWalkManifest(text)
    } catch (error) {
      throw new InvalidWalkManifestError(
        `the manifest would not read back (${(error as Error).message.replace(/^Invalid walk manifest: /, '')}); nothing was written. ` +
        'Use a goal without a path in it.',
        { cause: error },
      )
    }
    return text
  }

  /** Write the manifest atomically, as Déjà's fixed bytes. Call under `withLock`. */
  async writeManifest(manifest: WalkManifest) {
    const text = this.checkManifest(manifest)
    const { root, target } = await this.manifestFile()
    await mkdir(dirname(target), { recursive: true })
    const parent = await realpath(dirname(target))
    if (parent !== root && !inside(root, parent)) {
      throw new InvalidWalkManifestError('the manifest path escapes the workspace')
    }
    // A symlinked manifest would be replaced by a plain file, so it is refused.
    try {
      if ((await lstat(target)).isSymbolicLink()) {
        throw new InvalidWalkManifestError('the manifest is a symbolic link; replace it with a regular file')
      }
    } catch (error) {
      if (error instanceof InvalidWalkManifestError) throw error
      if (errorCode(error) !== 'ENOENT') throw error
    }
    const temporary = `${target}.${process.pid}-${randomUUID()}.tmp`
    try {
      await writeFile(temporary, text)
      await rename(temporary, target)
    } catch (error) {
      // A failed write leaves nothing behind in the consumer's tree.
      await unlink(temporary).catch(() => {})
      throw error
    }
  }
}

/** A manifest path inside the workspace, not under `.deja/`, as a JSON file. */
export function checkManifestPath(path: string) {
  if (typeof path !== 'string' || isAbsolute(path) || path.includes('\\') || path.includes('\0') ||
      path.split('/').some(segment => segment === '' || segment === '.' || segment === '..') ||
      path.split('/')[0].toLowerCase() === '.deja' || !path.endsWith('.json')) {
    throw new InvalidWalkManifestError(`the manifest path must be a relative .json path outside .deja/: ${path}`)
  }
  return path
}
