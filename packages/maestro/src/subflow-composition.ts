import { createHash } from 'node:crypto'
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises'
import { dirname, isAbsolute, relative, resolve, sep } from 'node:path'
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

/** Compose by reference: no login command or fixture value is copied into the composed YAML. */
export function composeMaestroLoginFlow(
  appId: string,
  segmentReference: string,
  continuationYaml: string,
): string {
  if (!/^[A-Za-z0-9][A-Za-z0-9_.-]{0,159}$/.test(appId)) {
    throw new TypeError('A bounded Maestro appId is required')
  }
  if (!segmentReference || isAbsolute(segmentReference) || segmentReference.includes('\\') ||
      segmentReference.split('/').includes('..') === false) {
    throw new TypeError('The segment reference must be a portable relative path within attempt/storage roots')
  }
  const continuation = validateContinuation(continuationYaml)
  return `appId: ${appId}\n---\n- runFlow: ${JSON.stringify(portable(segmentReference))}\n${continuation}`
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

function contractCandidateErrors(contract: SubflowContract, contents: string): string[] {
  const errors: string[] = []
  if (!contents.startsWith(`appId: ${contract.identity.appId}\n---\n`)) {
    errors.push('Candidate appId does not match segment identity')
  }
  if (/^\s*- runFlow:/m.test(contents)) errors.push('Nested segment references are outside this narrow proof')
  if (contract.preconditions.some(condition => condition.kind === 'clean-app-launch') &&
      !/^- launchApp:\s*\n {4}clearState: true\s*$/m.test(contents)) {
    errors.push('Candidate does not enforce its clean-app-launch precondition')
  }
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
    const yaml = composeMaestroLoginFlow(identity.appId, segmentReference, continuation)
    const candidateBytes = Buffer.from(yaml, 'utf8')
    const candidateSha256 = createHash('sha256').update(candidateBytes).digest('hex')
    await mkdir(candidateDirectory, { recursive: true })
    const temporary = `${resolve(candidateDirectory, 'candidate.yaml')}.${process.pid}.tmp`
    await writeFile(temporary, candidateBytes)
    await rename(temporary, resolve(root, candidatePath))

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
