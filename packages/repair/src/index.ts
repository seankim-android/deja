export {
  BoundedSelectorRepairer,
  type BoundedSelectorRepairerOptions,
} from './adapter.ts'

import type { WalkStore } from '@deja-qa/core'
import { createHash } from 'node:crypto'
import { mkdir, readFile, realpath, rename, writeFile } from 'node:fs/promises'
import { dirname, relative, resolve, sep } from 'node:path'
import {
  classifyMaestroFailure,
  type MaestroFailureClassification,
  type MaestroRunResult,
} from '@deja-qa/maestro'
import {
  LOCAL_MEMORY_PATH,
  LocalWalkStore,
  MAESTRO_FLOW_SCHEMA_VERSION,
  type LookupHit,
  type WalkIdentity,
} from '@deja-qa/store-local'
import { writeRepairProcessArtifacts, type RepairBuildResult } from './build.ts'
import {
  applyCheckoutSelectorRepair,
  CHECKOUT_AFFECTED_FILES,
  type CheckoutRepairPlan,
  type CheckoutRepairPolicy,
  planCheckoutSelectorRepair,
  rollbackCheckoutSelectorRepair,
  sourceTreeDigest,
} from './policy.ts'

export {
  runCheckoutBuildAndInstall,
  runCheckoutGradleBuild,
  type RepairBuildInstallOptions,
  type RepairBuildOptions,
  type RepairBuildResult,
} from './build.ts'
export {
  CHECKOUT_AFFECTED_FILES,
  CHECKOUT_COMPONENT_PATH,
  CHECKOUT_IDS_PATH,
  createCheckoutRepairDiff,
  planCheckoutSelectorRepair,
  type CheckoutRepairPlan,
  type CheckoutRepairPolicy,
  type PlannedRepairFile,
} from './policy.ts'

const EXPECTED_FAILING_STEP = 'Tap on id: dev.deja.checkout:id/checkout_button'

export type BoundedCheckoutRepairOptions = {
  workspaceRoot: string
  goal: string
  appId: string
  platform: 'android'
  timeoutMs: number
  policy: CheckoutRepairPolicy
  attemptId?: string
  store?: WalkStore
  memoryPath?: string
  fingerprint: () => Promise<string>
  runFlow: (flowPath: string, artifactDirectory: string, timeoutMs: number) => Promise<MaestroRunResult>
  build: (timeoutMs: number) => Promise<RepairBuildResult>
  displayPatch?: (diff: string, affectedFiles: string[]) => void
}

export type RepairExecutionReport = {
  startedAt: string
  endedAt: string
  durationMs: number
  monotonicDurationMs: number
  exitCode: number | null
  timedOut: boolean
  passed: boolean
  errors: unknown[]
  stdoutPath: string
  stderrPath: string
  outputPath: string
  failureClassification: MaestroFailureClassification | null
}

export type BoundedCheckoutRepairReport = {
  schemaVersion: 1
  command: 'repair'
  attemptId: string
  attemptDirectory: string
  goalKey: string
  appId: string
  platform: 'android'
  startedAt: string
  endedAt: string
  durationMs: number
  monotonicDurationMs: number
  timeBudgetMs: number
  status: 'repaired' | 'rejected' | 'failed'
  passed: boolean
  modelCalls: 0
  repairAttempts: 0 | 1
  oneAttemptOnly: true
  autoCommit: false
  pullRequest: false
  affectedFiles: string[]
  proposedPatchPath: string | null
  previous: {
    buildFingerprint: string
    flowPath: string
    flowSha256: string
    memoryPath: typeof LOCAL_MEMORY_PATH
    memorySha256: string
  }
  diagnosisBuildFingerprint: string
  buildFingerprintAfter: string | null
  buildIdentityRecomputed: boolean
  diagnosis: RepairExecutionReport | null
  build: Record<string, unknown> | null
  verification: RepairExecutionReport | null
  persistence: Record<string, unknown> | null
  preservation: Record<string, unknown>
  rejectionReason: string | null
  error: string | null
}

class BoundedRepairTimeoutError extends Error {
  constructor(phase: string) {
    super(`Bounded repair time budget expired during ${phase}`)
    this.name = 'BoundedRepairTimeoutError'
  }
}

class RepairDeadline {
  private readonly started = performance.now()
  private readonly budgetMs: number

  constructor(budgetMs: number) {
    if (!Number.isInteger(budgetMs) || budgetMs <= 0) {
      throw new TypeError('A single positive integer repair time budget is required')
    }
    this.budgetMs = budgetMs
  }

  elapsed() {
    return Math.round(performance.now() - this.started)
  }

  remaining() {
    return Math.max(0, this.budgetMs - this.elapsed())
  }

  async within<T>(phase: string, operation: (remainingMs: number) => Promise<T>): Promise<T> {
    const remainingMs = this.remaining()
    if (remainingMs <= 0) throw new BoundedRepairTimeoutError(phase)
    let timer: NodeJS.Timeout | undefined
    const timeout = new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new BoundedRepairTimeoutError(phase)), remainingMs)
    })
    try {
      const result = await Promise.race([
        Promise.resolve().then(() => operation(remainingMs)),
        timeout,
      ])
      if (this.remaining() <= 0) throw new BoundedRepairTimeoutError(phase)
      return result
    } finally {
      if (timer) clearTimeout(timer)
    }
  }
}

function sha256(contents: Buffer | string) {
  return createHash('sha256').update(contents).digest('hex')
}

function inside(root: string, candidate: string) {
  return candidate !== root && candidate.startsWith(`${root}${sep}`)
}

function portablePath(root: string, path: string) {
  return relative(root, path)
}

async function atomicWrite(path: string, contents: string | Buffer) {
  await mkdir(dirname(path), { recursive: true })
  const temporary = `${path}.${process.pid}.tmp`
  await writeFile(temporary, contents)
  await rename(temporary, path)
}

function validateAttemptId(attemptId: string) {
  if (!/^repair-[a-zA-Z0-9-]+$/.test(attemptId)) {
    throw new Error('Repair attempt ID must contain only letters, numbers, and hyphens')
  }
}

function executionReport(
  root: string,
  result: MaestroRunResult,
  artifactDirectory: string,
): RepairExecutionReport {
  const outputPath = portablePath(root, resolve(artifactDirectory, 'output.log'))
  return {
    startedAt: result.startedAt,
    endedAt: result.endedAt,
    durationMs: result.durationMs,
    monotonicDurationMs: result.monotonicDurationMs,
    exitCode: result.exitCode,
    timedOut: result.timedOut,
    passed: result.passed,
    errors: result.errors,
    stdoutPath: portablePath(root, resolve(artifactDirectory, 'stdout.log')),
    stderrPath: portablePath(root, resolve(artifactDirectory, 'stderr.log')),
    outputPath,
    failureClassification: classifyMaestroFailure({
      passed: result.passed,
      timedOut: result.timedOut,
      output: result.output,
      errors: result.errors,
      rawArtifactPath: outputPath,
    }),
  }
}

function buildReport(
  root: string,
  result: RepairBuildResult,
  artifactDirectory: string,
): Record<string, unknown> {
  return {
    command: result.command,
    startedAt: result.startedAt,
    endedAt: result.endedAt,
    durationMs: result.durationMs,
    monotonicDurationMs: result.monotonicDurationMs,
    exitCode: result.exitCode,
    signal: result.signal,
    timedOut: result.timedOut,
    passed: result.passed,
    errors: result.errors,
    stdoutPath: portablePath(root, resolve(artifactDirectory, 'stdout.log')),
    stderrPath: portablePath(root, resolve(artifactDirectory, 'stderr.log')),
    outputPath: portablePath(root, resolve(artifactDirectory, 'output.log')),
    ...(result.installCommand ? { installCommand: result.installCommand } : {}),
    ...(result.installExitCode !== undefined ? { installExitCode: result.installExitCode } : {}),
  }
}

function isStrictlyEligible(classification: MaestroFailureClassification | null) {
  return classification?.category === 'selector' && classification.repairEligible &&
    classification.failingStep === EXPECTED_FAILING_STEP
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error)
}

export class BoundedCheckoutRepairer {
  private readonly options: BoundedCheckoutRepairOptions
  private consumed = false

  constructor(options: BoundedCheckoutRepairOptions) {
    this.options = options
  }

  async repair(): Promise<BoundedCheckoutRepairReport> {
    if (this.consumed) throw new Error('This repair lifecycle has already consumed its one attempt')
    this.consumed = true
    return executeBoundedCheckoutRepair(this.options)
  }
}

async function executeBoundedCheckoutRepair(
  options: BoundedCheckoutRepairOptions,
): Promise<BoundedCheckoutRepairReport> {
  if (options.goal.trim().toLowerCase() !== 'checkout' || options.appId !== 'dev.deja.checkout' ||
      options.platform !== 'android') {
    throw new Error('Only the validated Android checkout selector repair is supported')
  }
  const root = await realpath(resolve(options.workspaceRoot))
  const attemptId = options.attemptId ?? `repair-${Date.now()}-${process.pid}`
  validateAttemptId(attemptId)
  const attemptDirectory = resolve(root, '.deja/patches', attemptId)
  if (!inside(root, attemptDirectory)) throw new Error('Repair attempt directory escapes workspace')
  await mkdir(attemptDirectory, { recursive: true })
  const deadline = new RepairDeadline(options.timeoutMs)
  const started = new Date()
  const monotonicStart = performance.now()
  const store = options.store ?? new LocalWalkStore({ workspaceRoot: root })
  const diagnosisDirectory = resolve(attemptDirectory, 'diagnosis')
  const buildDirectory = resolve(attemptDirectory, 'build')
  const verificationDirectory = resolve(attemptDirectory, 'verification')
  const previousDirectory = resolve(attemptDirectory, 'previous')
  const snapshotFlowPath = resolve(previousDirectory, 'flow.yaml')
  const patchPath = resolve(attemptDirectory, 'proposed.patch')
  const relativePatchPath = portablePath(root, patchPath)

  let previousHit: LookupHit | null = null
  let beforeMemory: Buffer
  let beforeFlow: Buffer
  let diagnosisFingerprint = ''
  let afterFingerprint: string | null = null
  let diagnosis: RepairExecutionReport | null = null
  let build: Record<string, unknown> | null = null
  let verification: RepairExecutionReport | null = null
  let persistence: Record<string, unknown> | null = null
  let plan: CheckoutRepairPlan | null = null
  let repairAttempts: 0 | 1 = 0
  let applied = false
  let rejectionReason: string | null = null

  const sourceFiles = [...CHECKOUT_AFFECTED_FILES]
  const beforeSource = await sourceTreeDigest(root, sourceFiles)
  const beforeMemoryPath = options.memoryPath ?? resolve(root, LOCAL_MEMORY_PATH)
  beforeMemory = await readFile(beforeMemoryPath)
  const identityBase = {
    goal: options.goal,
    appId: options.appId,
    platform: options.platform,
    flowSchemaVersion: MAESTRO_FLOW_SCHEMA_VERSION,
  }

  async function preservation(previousFlowSha?: string) {
    const afterMemory = await readFile(beforeMemoryPath)
    const afterSource = await sourceTreeDigest(root, sourceFiles)
    const afterFlow = await readFile(snapshotFlowPath)
    return {
      memoryBeforeSha256: sha256(beforeMemory),
      memoryAfterSha256: sha256(afterMemory),
      memoryUnchangedByteForByte: beforeMemory.equals(afterMemory),
      flowBeforeSha256: previousFlowSha ?? sha256(beforeFlow),
      flowAfterSha256: sha256(afterFlow),
      assertionsUnchangedByteForByte: beforeFlow.equals(afterFlow),
      sourceBeforeSha256: beforeSource,
      sourceAfterSha256: afterSource,
      sourceUnchangedByteForByte: sourceFiles.every(path => beforeSource[path] === afterSource[path]),
    }
  }

  async function finish(
    status: BoundedCheckoutRepairReport['status'],
    previousDetails: BoundedCheckoutRepairReport['previous'],
    error: unknown = null,
  ) {
    const ended = new Date()
    const report: BoundedCheckoutRepairReport = {
      schemaVersion: 1,
      command: 'repair',
      attemptId,
      attemptDirectory: portablePath(root, attemptDirectory),
      goalKey: options.goal.trim().toLowerCase().replace(/\s+/g, ' '),
      appId: options.appId,
      platform: options.platform,
      startedAt: started.toISOString(),
      endedAt: ended.toISOString(),
      durationMs: Math.max(0, ended.getTime() - started.getTime()),
      monotonicDurationMs: Math.round(performance.now() - monotonicStart),
      timeBudgetMs: options.timeoutMs,
      status,
      passed: status === 'repaired',
      modelCalls: 0,
      repairAttempts,
      oneAttemptOnly: true,
      autoCommit: false,
      pullRequest: false,
      affectedFiles: plan?.files.map(file => file.path) ?? [],
      proposedPatchPath: plan ? relativePatchPath : null,
      previous: previousDetails,
      diagnosisBuildFingerprint: diagnosisFingerprint,
      buildFingerprintAfter: afterFingerprint,
      buildIdentityRecomputed: afterFingerprint !== null,
      diagnosis,
      build,
      verification,
      persistence,
      preservation: await preservation(previousDetails.flowSha256),
      rejectionReason,
      error: error ? errorMessage(error) : null,
    }
    await atomicWrite(
      resolve(attemptDirectory, 'repair-report.json'),
      `${JSON.stringify(report, null, 2)}\n`,
    )
    return report
  }

  try {
    diagnosisFingerprint = await deadline.within('initial build identity', () => options.fingerprint())
    const identity: WalkIdentity = { ...identityBase, buildFingerprint: diagnosisFingerprint }
    const previous = await deadline.within('previous successful memory lookup', () =>
      store.lookupPreviousSuccessful(identity),
    )
    if (previous.status !== 'hit') {
      throw new Error(`A previous successful checkout walk is required: ${previous.reason}`)
    }
    previousHit = previous
    beforeFlow = await readFile(previous.flowPath)
    await atomicWrite(resolve(previousDirectory, 'walks.json'), beforeMemory)
    await atomicWrite(snapshotFlowPath, beforeFlow)
    const previousDetails: BoundedCheckoutRepairReport['previous'] = {
      buildFingerprint: previous.walk.buildFingerprint,
      flowPath: portablePath(root, previous.flowPath),
      flowSha256: previous.walk.flowSha256,
      memoryPath: LOCAL_MEMORY_PATH,
      memorySha256: sha256(beforeMemory),
    }

    const diagnosisResult = await deadline.within('independent stale-flow diagnosis', remainingMs =>
      options.runFlow(snapshotFlowPath, diagnosisDirectory, remainingMs),
    )
    await writeRepairProcessArtifacts(diagnosisDirectory, diagnosisResult)
    diagnosis = executionReport(root, diagnosisResult, diagnosisDirectory)
    if (diagnosis.passed) {
      rejectionReason = 'The previous successful journey passed; no repair is needed'
      return await finish('rejected', previousDetails)
    }
    if (!isStrictlyEligible(diagnosis.failureClassification)) {
      const category = diagnosis.failureClassification?.category ?? 'unknown'
      rejectionReason = `Repair refused for ${category}; only the validated checkout selector failure is eligible`
      return await finish('rejected', previousDetails)
    }

    plan = await deadline.within('validated two-file patch planning', () =>
      planCheckoutSelectorRepair(options.policy),
    )
    await atomicWrite(patchPath, plan.diff)
    await atomicWrite(resolve(attemptDirectory, 'plan.json'), `${JSON.stringify({
      affectedFiles: plan.files.map(file => ({
        path: file.path,
        beforeSha256: file.beforeSha256,
        afterSha256: file.afterSha256,
      })),
      diffSha256: sha256(plan.diff),
      sourceRoots: options.policy.sourceRoots,
      permittedFileTypes: options.policy.permittedFileTypes,
      modelCalls: 0,
      generalizedAutonomousRepair: false,
    }, null, 2)}\n`)
    options.displayPatch?.(plan.diff, plan.files.map(file => file.path))

    repairAttempts = 1
    applied = true
    await deadline.within('single two-file source patch', () => applyCheckoutSelectorRepair(plan!))
    const buildResult = await deadline.within('bounded rebuild', remainingMs => options.build(remainingMs))
    await writeRepairProcessArtifacts(buildDirectory, buildResult)
    build = buildReport(root, buildResult, buildDirectory)
    await atomicWrite(resolve(buildDirectory, 'build-report.json'), `${JSON.stringify(build, null, 2)}\n`)
    if (!buildResult.passed) throw new Error('The bounded rebuild failed; cached memory was not replaced')

    afterFingerprint = await deadline.within('post-repair build identity', () => options.fingerprint())
    if (!/^[a-f0-9]{64}$/.test(afterFingerprint)) {
      throw new Error('The rebuilt APK did not produce a valid SHA-256 build fingerprint')
    }
    if (afterFingerprint === diagnosisFingerprint) {
      throw new Error('The rebuilt APK fingerprint did not change; refusing unverified cache replacement')
    }

    const verificationResult = await deadline.within('complete independent journey verification', remainingMs =>
      options.runFlow(snapshotFlowPath, verificationDirectory, remainingMs),
    )
    await writeRepairProcessArtifacts(verificationDirectory, verificationResult)
    verification = executionReport(root, verificationResult, verificationDirectory)
    await atomicWrite(
      resolve(verificationDirectory, 'verification-report.json'),
      `${JSON.stringify(verification, null, 2)}\n`,
    )
    if (!verification.passed || !verification.endedAt) {
      throw new Error('The complete repaired journey failed; cached memory was not replaced')
    }

    const persisted = await deadline.within('atomic verified cache replacement', () =>
      store.persistVerifiedRepair({
        identity: { ...identityBase, buildFingerprint: afterFingerprint! },
        candidatePath: snapshotFlowPath,
        validation: { passed: true, at: verificationResult.endedAt },
        previous: {
          buildFingerprint: previous.walk.buildFingerprint,
          flowSha256: previous.walk.flowSha256,
        },
        repair: {
          attemptId,
          patchPath: relativePatchPath,
          previousBuildFingerprint: previous.walk.buildFingerprint,
          previousFlowSha256: previous.walk.flowSha256,
        },
      }),
    )
    persistence = {
      flowPath: persisted.flowPath,
      flowSha256: persisted.flowSha256,
      storedMetadataPath: persisted.storedMetadataPath,
      atomicWrite: persisted.atomicWrite,
      origin: persisted.walk.origin,
      repairHistory: persisted.walk.repairHistory,
      preservedValidationHistoryEntries: persisted.walk.validationHistory.length,
    }
    applied = false
    return await finish('repaired', previousDetails)
  } catch (error) {
    let rollbackError: unknown = null
    if (plan && applied) {
      try {
        await rollbackCheckoutSelectorRepair(plan)
      } catch (caught) {
        rollbackError = caught
      }
    }
    if (!previousHit) throw error
    const previousDetails: BoundedCheckoutRepairReport['previous'] = {
      buildFingerprint: previousHit.walk.buildFingerprint,
      flowPath: portablePath(root, previousHit.flowPath),
      flowSha256: previousHit.walk.flowSha256,
      memoryPath: LOCAL_MEMORY_PATH,
      memorySha256: sha256(beforeMemory),
    }
    if (rollbackError) {
      return finish('failed', previousDetails, new Error(`${errorMessage(error)}; rollback failed: ${errorMessage(rollbackError)}`))
    }
    return finish('failed', previousDetails, error)
  }
}
