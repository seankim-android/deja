import { createHash } from 'node:crypto'
import { lstat, mkdir, readFile, realpath, rename, writeFile } from 'node:fs/promises'
import { dirname, isAbsolute, normalize, relative, resolve, sep } from 'node:path'
import {
  normalizeGoal,
  validateSubflowContract,
  type CompositionModelMetrics,
  type CompositionStepMetrics,
  type ExecutionResult,
  type PersistedWalk,
  type SubflowContract,
  type SubflowIdentity,
  type VerifiedSubflow,
  type VerifiedSubflowStore,
  type WalkIdentity,
  type WalkRunner,
  type WalkStore,
} from '../../core/src/index.ts'
import {
  readCompositionRunnerTrace,
  type CompositionRunnerTrace,
} from './execution-observations.ts'

export const MAESTRO_COMPOSITE_FLOW_SCHEMA = 'maestro/v1'
export const COMPOSITION_MODEL_METRICS: CompositionModelMetrics = {
  composition: 0,
  newExploration: 0,
  segmentVerification: 0,
  fullJourneyVerification: 0,
  basis: 'Curated continuation and deterministic process runner; no explorer or model client is constructed',
}

export type SegmentVerificationResult = {
  status: 'verified' | 'rejected'
  passed: boolean
  segment: VerifiedSubflow | null
  execution: ExecutionResult
  errors: string[]
}

export type LoginCompositionResult = {
  status: 'composed' | 'rejected' | 'persist-failed'
  passed: boolean
  verificationPassed: boolean
  identity: WalkIdentity
  candidatePath: string | null
  candidateSha256: string | null
  segment: VerifiedSubflow | null
  persisted: PersistedWalk | null
  execution: ExecutionResult | null
  steps: CompositionStepMetrics | null
  modelCalls: CompositionModelMetrics
  errors: string[]
}

export type MaestroLoginComposerOptions = {
  workspaceRoot: string
  segments: VerifiedSubflowStore
  walks: WalkStore
  runner: WalkRunner
  artifactId?: (prefix: 'login-verification' | 'composition-verification') => string
}

export type MaestroFlowContainment = {
  workspaceRoot: string
  flowDirectory: string
}

function portable(path: string) {
  return path.split(sep).join('/')
}

function flowSlug(goal: string) {
  const key = normalizeGoal(goal).key
  const slug = key.replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '')
  if (!slug) throw new TypeError('The goal must contain a letter or number')
  return slug
}

function subflowIdentity(identity: WalkIdentity): SubflowIdentity {
  return {
    appId: identity.appId,
    platform: identity.platform,
    flowSchemaVersion: identity.flowSchemaVersion,
    buildFingerprint: identity.buildFingerprint,
    ...(identity.adapter !== undefined ? { adapter: identity.adapter } : {}),
    ...(identity.browser !== undefined ? { browser: identity.browser } : {}),
  }
}

function validateContinuation(contents: string): string {
  if (contents.includes('\0') || contents.includes('\t')) {
    throw new TypeError('Continuation YAML must be bounded text without NULs or tabs')
  }
  if (contents.length > 32_768 || /^\s*appId:/m.test(contents) || /^\s*---\s*$/m.test(contents)) {
    throw new TypeError('Continuation YAML contains a flow header or exceeds its size bound')
  }
  if (/^\s*- (?:launchApp|runFlow):/m.test(contents)) {
    throw new TypeError('The verified segment owns launch and nested subflows are not supported')
  }
  // This deliberately narrow login composer cannot silently duplicate login controls in its suffix.
  if (/\b(?:screen_login|username_input|password_input|login_button)\b/.test(contents)) {
    throw new TypeError('Continuation YAML duplicates part of the login segment')
  }
  const steps = contents.match(/^- /gm) ?? []
  if (steps.length < 1 || steps.length > 128) {
    throw new TypeError('Continuation YAML must contain 1 to 128 top-level steps')
  }
  return contents.endsWith('\n') ? contents : `${contents}\n`
}

function inside(root: string, candidate: string) {
  return candidate !== root && candidate.startsWith(`${root}${sep}`)
}

function errorCode(error: unknown) {
  return (error as NodeJS.ErrnoException).code
}

/**
 * Validate output ancestry before mkdir and again before writing. Unlike the lexical runFlow check,
 * this inspects the filesystem, including dangling links and non-directory traversal. New nested
 * directories are supported by validating their nearest existing ancestor.
 */
async function assertCanonicalOutputDirectory(workspaceRoot: string, candidateDirectory: string) {
  const lexicalRoot = resolve(workspaceRoot)
  const candidate = resolve(candidateDirectory)
  if (!inside(lexicalRoot, candidate)) {
    throw new Error('Composition output directory escapes the lexical workspace')
  }
  const canonicalRoot = await realpath(lexicalRoot)
  if (!(await lstat(canonicalRoot)).isDirectory()) {
    throw new Error('Composition workspace must be a directory')
  }
  let ancestor = candidate
  while (true) {
    try {
      const metadata = await lstat(ancestor)
      if (metadata.isSymbolicLink()) {
        throw new Error('Composition output path contains a symbolic link')
      }
      if (!metadata.isDirectory()) {
        throw new Error('Composition output path must traverse directories')
      }
      const canonicalAncestor = await realpath(ancestor)
      if (canonicalAncestor !== canonicalRoot && !inside(canonicalRoot, canonicalAncestor)) {
        throw new Error('Composition output directory escapes the canonical workspace')
      }
      return
    } catch (error) {
      if (errorCode(error) === 'ENOTDIR') {
        throw new Error('Composition output path must traverse directories', { cause: error })
      }
      if (errorCode(error) !== 'ENOENT') throw error
      const parent = dirname(ancestor)
      if (parent === ancestor) throw error
      ancestor = parent
    }
  }
}

async function assertCanonicalCandidateFile(candidatePath: string) {
  try {
    const metadata = await lstat(candidatePath)
    if (metadata.isSymbolicLink() || !metadata.isFile()) {
      throw new Error('Composition candidate must be a regular file when it already exists')
    }
  } catch (error) {
    if (errorCode(error) === 'ENOENT') return
    throw error
  }
}

export function validateMaestroFlowReference(
  reference: string,
  containment?: MaestroFlowContainment,
): string {
  if (!reference || reference.includes('\0') || reference.includes('\\') || isAbsolute(reference)) {
    throw new TypeError('Flow reference must be a non-empty portable relative path')
  }
  if (containment) {
    const root = resolve(containment.workspaceRoot)
    const flowDirectory = resolve(containment.flowDirectory)
    if (flowDirectory !== root && !inside(root, flowDirectory)) {
      throw new TypeError('Flow directory escapes containment root')
    }
    const target = resolve(flowDirectory, reference)
    if (!inside(root, target)) throw new TypeError('Flow reference escapes containment root')
    return portable(relative(flowDirectory, target))
  }
  const normalized = normalize(reference)
  if (normalized === '..' || normalized.startsWith(`..${sep}`)) {
    throw new TypeError('Flow reference escapes its flow directory')
  }
  return portable(normalized)
}

/** Compose by reference: no login command or fixture value is copied into the composed YAML. */
export function composeMaestroLoginFlow(
  appId: string,
  segmentReference: string,
  continuationYaml: string,
  containment?: MaestroFlowContainment,
): string {
  if (!/^[A-Za-z0-9][A-Za-z0-9_.-]{0,159}$/.test(appId)) {
    throw new TypeError('A bounded Maestro appId is required')
  }
  const validatedReference = validateMaestroFlowReference(segmentReference, containment)
  const continuation = validateContinuation(continuationYaml)
  return `appId: ${appId}\n---\n- runFlow: ${JSON.stringify(validatedReference)}\n${continuation}`
}

export function countMaestroSteps(contents: string): number {
  return (contents.match(/^- /gm) ?? []).length
}

export function countMaestroAssertions(contents: string): number {
  return (contents.match(/^\s*-\s+assertVisible:/gm) ?? []).length
}

function expression(value: string): RegExp {
  return new RegExp(value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'))
}

function hasResourceAssertion(contents: string, resourceId: string): boolean {
  const id = expression(resourceId)
  return new RegExp(`^- assertVisible:\\s*\\n(?: {4}[^\\n]*\\n)*? {4}id: ${id.source}\\s*$`, 'm')
    .test(contents)
}

function cleanLaunchErrors(trace: CompositionRunnerTrace): string[] {
  const launches = trace.observations.filter(observation => observation.kind === 'launch')
  if (launches.length !== 1 || trace.observations[0]?.kind !== 'launch' ||
      trace.observations[0].clearState !== true) {
    return ['Runner observations require exactly one clean launch as the first product step']
  }
  return []
}

function preconditionErrors(
  trace: CompositionRunnerTrace,
  contract: SubflowContract,
): string[] {
  const errors = cleanLaunchErrors(trace)
  const firstAction = trace.observations.findIndex(observation => observation.kind === 'action')
  const prefix = trace.observations.slice(0, firstAction < 0 ? undefined : firstAction)
  for (const condition of contract.preconditions) {
    if (condition.kind === 'screen-visible' && !prefix.some(observation =>
      observation.kind === 'assertVisible' && observation.resourceId === condition.resourceId)) {
      errors.push(`Precondition ${condition.resourceId} was not observed before the first action`)
    }
  }
  return errors
}

function trailingAssertions(observations: CompositionRunnerTrace['observations']) {
  const trailing = []
  for (let index = observations.length - 1; index >= 0; index -= 1) {
    const observation = observations[index]
    if (observation.kind !== 'assertVisible') break
    trailing.push(observation)
  }
  return trailing
}

function postconditionErrors(
  contract: SubflowContract,
  observations: CompositionRunnerTrace['observations'],
  finalVisibleResourceIds: string[] | null,
): string[] {
  const trailing = trailingAssertions(observations)
  const errors: string[] = []
  for (const condition of contract.postconditions) {
    if (!trailing.some(observation => observation.resourceId === condition.resourceId)) {
      errors.push(`Postcondition ${condition.resourceId} was not the completed assertion boundary`)
    }
    if (finalVisibleResourceIds && !finalVisibleResourceIds.includes(condition.resourceId)) {
      errors.push(`Postcondition ${condition.resourceId} was not visible at verification completion`)
    }
  }
  return errors
}

function segmentObservationErrors(
  trace: CompositionRunnerTrace,
  contract: SubflowContract,
): string[] {
  return [
    ...preconditionErrors(trace, contract),
    ...postconditionErrors(contract, trace.observations, trace.finalVisibleResourceIds),
  ]
}

function reusedBoundaryObservationErrors(
  trace: CompositionRunnerTrace,
  contract: SubflowContract,
): string[] {
  return [
    ...preconditionErrors(trace, contract),
    ...postconditionErrors(
      contract, trace.observations.slice(0, contract.stepCount), null),
  ]
}

function contractCandidateErrors(contract: SubflowContract, contents: string): string[] {
  const errors: string[] = []
  if (!contents.startsWith(`appId: ${contract.identity.appId}\n---\n`)) {
    errors.push('Candidate appId does not match segment identity')
  }
  if (/^\s*- runFlow:/m.test(contents)) errors.push('Nested segment references are outside this narrow proof')
  for (const condition of contract.postconditions) {
    if (!hasResourceAssertion(contents, condition.resourceId)) {
      errors.push(`Candidate lacks postcondition assertion for ${condition.resourceId}`)
    }
  }
  if (countMaestroSteps(contents) !== contract.stepCount ||
      countMaestroAssertions(contents) !== contract.assertionCount) {
    errors.push('Candidate step/assertion counts differ from the typed contract')
  }
  return errors
}

/** A one-level, clean-launch login segment plus explicitly supplied new checkout steps. */
export class MaestroLoginComposer {
  private readonly options: MaestroLoginComposerOptions

  constructor(options: MaestroLoginComposerOptions) {
    this.options = options
  }

  private artifactId(prefix: 'login-verification' | 'composition-verification') {
    return this.options.artifactId?.(prefix) ??
      `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`
  }

  async verifyAndSaveSegment(
    contractInput: SubflowContract,
    candidatePath: string,
  ): Promise<SegmentVerificationResult> {
    const contract = validateSubflowContract(contractInput)
    if (contract.identity.flowSchemaVersion !== MAESTRO_COMPOSITE_FLOW_SCHEMA) {
      throw new TypeError('The Maestro login composer requires maestro/v1 segment identity')
    }
    const execution = await this.options.runner.run({
      flowPath: candidatePath,
      artifactDirectory: `.walkcache/reports/${this.artifactId('login-verification')}`,
    })
    if (!execution.passed) {
      return {
        status: 'rejected', passed: false, segment: null, execution,
        errors: ['Independent segment verification failed; reusable storage was not written'],
      }
    }
    const candidateContents = await readFile(resolve(this.options.workspaceRoot, candidatePath), 'utf8')
    const candidateErrors = contractCandidateErrors(contract, candidateContents)
    if (execution.assertions !== contract.assertionCount) {
      candidateErrors.push('Runner assertion count differs from the typed contract')
    }
    const trace = await readCompositionRunnerTrace(this.options.workspaceRoot, execution)
    if (!trace) {
      candidateErrors.push('Runner did not expose bounded observations required for segment verification')
    } else {
      candidateErrors.push(...segmentObservationErrors(trace, contract))
    }
    if (candidateErrors.length > 0) {
      return {
        status: 'rejected', passed: false, segment: null, execution,
        errors: [...candidateErrors, 'Reusable storage was not written'],
      }
    }
    const segment = await this.options.segments.persistVerified({
      contract,
      candidatePath,
      validation: { passed: true, at: execution.endedAt },
    })
    return { status: 'verified', passed: true, segment, execution, errors: [] }
  }

  async compose(
    identity: WalkIdentity,
    segmentId: string,
    continuationYaml: string,
  ): Promise<LoginCompositionResult> {
    if (identity.flowSchemaVersion !== MAESTRO_COMPOSITE_FLOW_SCHEMA) {
      throw new TypeError('The Maestro login composer requires maestro/v1 walk identity')
    }
    const base = {
      passed: false,
      identity,
      candidatePath: null,
      candidateSha256: null,
      segment: null,
      persisted: null,
      execution: null,
      steps: null,
      modelCalls: COMPOSITION_MODEL_METRICS,
    }
    const lookup = await this.options.segments.lookup(segmentId, subflowIdentity(identity))
    if (lookup.status === 'miss') {
      return {
        ...base, status: 'rejected', verificationPassed: false,
        errors: [`Verified segment is unavailable for this exact app/build identity: ${lookup.reason}`],
      }
    }
    const continuation = validateContinuation(continuationYaml)
    const root = resolve(this.options.workspaceRoot)
    const goalSlug = flowSlug(identity.goal)
    const candidatePath = `.walkcache/composed/${goalSlug}/candidate.yaml`
    const candidateDirectory = resolve(root, dirname(candidatePath))
    const finalDirectory = resolve(root, `.walkcache/flows/${goalSlug}`)
    const segmentPath = resolve(root, lookup.segment.flowPath)
    const segmentReference = portable(relative(candidateDirectory, segmentPath))
    // LocalWalkStore moves bytes one peer directory sideways. Prove that move cannot change runFlow
    // resolution before the candidate is executed and persisted.
    if (portable(relative(finalDirectory, segmentPath)) !== segmentReference) {
      throw new Error('Candidate and stored flow would resolve different segment paths')
    }
    const yaml = composeMaestroLoginFlow(identity.appId, segmentReference, continuation, {
      workspaceRoot: root,
      flowDirectory: candidateDirectory,
    })
    const candidateBytes = Buffer.from(yaml, 'utf8')
    const candidateSha256 = createHash('sha256').update(candidateBytes).digest('hex')
    const candidateAbsolutePath = resolve(root, candidatePath)
    // No filesystem mutation is permitted until the nearest existing output ancestor is proven
    // canonical. Recheck after mkdir and before either write or rename.
    await assertCanonicalOutputDirectory(root, candidateDirectory)
    await assertCanonicalCandidateFile(candidateAbsolutePath)
    await mkdir(candidateDirectory, { recursive: true })
    await assertCanonicalOutputDirectory(root, candidateDirectory)
    await assertCanonicalCandidateFile(candidateAbsolutePath)
    const temporary = `${candidateAbsolutePath}.${process.pid}.tmp`
    await writeFile(temporary, candidateBytes, { flag: 'wx' })
    await rename(temporary, candidateAbsolutePath)

    const execution = await this.options.runner.run({
      flowPath: candidatePath,
      artifactDirectory: `.walkcache/reports/${this.artifactId('composition-verification')}`,
    })
    const newSteps = countMaestroSteps(continuation)
    const newAssertions = countMaestroAssertions(continuation)
    const steps: CompositionStepMetrics = {
      reusedSteps: lookup.segment.stepCount,
      newlyExploredSteps: newSteps,
      totalSteps: lookup.segment.stepCount + newSteps,
      reusedAssertions: lookup.segment.assertionCount,
      newAssertions,
      totalAssertions: lookup.segment.assertionCount + newAssertions,
    }
    if (!execution.passed) {
      return {
        ...base, status: 'rejected', verificationPassed: false, candidatePath, candidateSha256,
        segment: lookup.segment, execution, steps,
        errors: ['Clean-state full-journey verification failed; goal memory was not replaced'],
      }
    }
    if (execution.assertions !== steps.totalAssertions) {
      return {
        ...base, status: 'rejected', verificationPassed: false, candidatePath, candidateSha256,
        segment: lookup.segment, execution, steps,
        errors: ['Runner assertion accounting did not include every reused and new assertion'],
      }
    }
    const trace = await readCompositionRunnerTrace(this.options.workspaceRoot, execution)
    const observationErrors = trace
      ? reusedBoundaryObservationErrors(trace, lookup.segment)
      : ['Runner did not expose bounded observations required for composed verification']
    if (observationErrors.length > 0) {
      return {
        ...base, status: 'rejected', verificationPassed: false, candidatePath, candidateSha256,
        segment: lookup.segment, execution, steps,
        errors: [...observationErrors, 'Goal memory was not replaced'],
      }
    }
    try {
      const persisted = await this.options.walks.persistVerifiedCandidate({
        identity,
        candidatePath,
        validation: { passed: true, at: execution.endedAt },
        origin: 'remember',
        exploration: {
          explorationDurationMs: 0,
          verificationDurationMs: execution.durationMs,
          mcpCalls: 0,
          modelCalls: 0,
          modelCallBasis: COMPOSITION_MODEL_METRICS.basis,
        },
      })
      return {
        ...base, status: 'composed', passed: true, verificationPassed: true,
        candidatePath, candidateSha256, segment: lookup.segment, persisted, execution, steps,
        errors: [],
      }
    } catch (error) {
      return {
        ...base, status: 'persist-failed', verificationPassed: true, candidatePath,
        candidateSha256, segment: lookup.segment, execution, steps,
        errors: [`Verified composition persistence failed; last-known-good memory is retained: ${
          error instanceof Error ? error.message : String(error)}`],
      }
    }
  }
}

/** Read a continuation without treating it as a new runner or route-planning interface. */
export async function readMaestroContinuation(path: string): Promise<string> {
  return validateContinuation(await readFile(path, 'utf8'))
}
