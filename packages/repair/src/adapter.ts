import { resolve } from 'node:path'
import type {
  RepairRequest,
  RepairResult,
  RunArtifact,
  SelectorRepairer,
  WalkStore,
} from '@deja-qa/core'
import {
  BoundedCheckoutRepairer,
  runCheckoutBuildAndInstall,
  type BoundedCheckoutRepairReport,
  type CheckoutRepairPolicy,
} from './index.ts'
import {
  countFlowAssertions,
  MaestroWalkRunner,
} from '@deja-qa/maestro'

export type BoundedSelectorRepairerOptions = {
  workspaceRoot: string
  policy: CheckoutRepairPolicy
  timeoutMs: number
  apkPath: string
  device: string
  store: WalkStore
  /** Workspace-relative path of the store's durable memory file; see BoundedCheckoutRepairOptions. */
  memoryPath: string
  runner: MaestroWalkRunner
  fingerprint: () => Promise<string>
  androidSdkRoot?: string
  javaHome?: string
  displayPatch?: (diff: string, affectedFiles: string[]) => void
}

function failure(report: BoundedCheckoutRepairReport) {
  return report.diagnosis?.failureClassification ??
    report.verification?.failureClassification ?? null
}

export class BoundedSelectorRepairer implements SelectorRepairer {
  private readonly options: BoundedSelectorRepairerOptions

  constructor(options: BoundedSelectorRepairerOptions) {
    this.options = options
  }

  async repair(request: RepairRequest): Promise<RepairResult> {
    const root = resolve(this.options.workspaceRoot)
    const report = await new BoundedCheckoutRepairer({
      workspaceRoot: root,
      goal: request.identity.goal,
      appId: request.identity.appId,
      platform: 'android',
      timeoutMs: this.options.timeoutMs,
      policy: this.options.policy,
      store: this.options.store,
      memoryPath: this.options.memoryPath,
      fingerprint: this.options.fingerprint,
      runFlow: (flowPath, artifactDirectory, timeoutMs) =>
        this.options.runner.runFlow(flowPath, artifactDirectory, timeoutMs),
      build: timeoutMs => runCheckoutBuildAndInstall({
        workspaceRoot: root,
        timeoutMs,
        device: this.options.device,
        apkPath: this.options.apkPath,
        ...(this.options.androidSdkRoot ? { androidSdkRoot: this.options.androidSdkRoot } : {}),
        ...(this.options.javaHome ? { javaHome: this.options.javaHome } : {}),
      }),
      ...(this.options.displayPatch ? { displayPatch: this.options.displayPatch } : {}),
    }).repair()

    let diff = ''
    if (report.proposedPatchPath) {
      diff = await Bun.file(resolve(root, report.proposedPatchPath)).text()
    }
    const artifacts: RunArtifact[] = [
      { kind: 'attempt', path: report.attemptDirectory },
      {
        kind: 'repair-report',
        path: `${report.attemptDirectory}/repair-report.json`,
      },
      { kind: 'memory', path: report.previous.memoryPath },
    ]
    if (report.diagnosis?.outputPath) artifacts.push({ kind: 'output', path: report.diagnosis.outputPath })
    if (report.build && typeof report.build.outputPath === 'string') {
      artifacts.push({ kind: 'output', path: report.build.outputPath })
    }
    if (report.verification?.outputPath) artifacts.push({ kind: 'output', path: report.verification.outputPath })
    if (report.proposedPatchPath) artifacts.push({ kind: 'diff', path: report.proposedPatchPath })
    const persisted = report.persistence
    const flowPath = typeof persisted?.flowPath === 'string'
      ? persisted.flowPath
      : report.previous.flowPath
    const flowSha256 = typeof persisted?.flowSha256 === 'string'
      ? persisted.flowSha256
      : report.previous.flowSha256
    return {
      status: report.status,
      passed: report.passed,
      durationMs: report.durationMs,
      modelCalls: report.modelCalls,
      repairCount: report.repairAttempts,
      assertions: await countFlowAssertions(resolve(root, flowPath)),
      flowPath,
      flowSha256,
      failure: failure(report),
      artifacts,
      diffs: diff ? [{
        path: report.proposedPatchPath,
        affectedFiles: report.affectedFiles,
        contents: diff,
        autoCommit: false,
      }] : [],
      errors: [report.rejectionReason, report.error].filter((value): value is string => Boolean(value)),
      details: {
        attemptId: report.attemptId,
        timeBudgetMs: report.timeBudgetMs,
        oneAttemptOnly: report.oneAttemptOnly,
        autoCommit: report.autoCommit,
        pullRequest: report.pullRequest,
        previous: report.previous,
        diagnosisBuildFingerprint: report.diagnosisBuildFingerprint,
        buildFingerprintAfter: report.buildFingerprintAfter,
        buildIdentityRecomputed: report.buildIdentityRecomputed,
        diagnosis: report.diagnosis ? {
          passed: report.diagnosis.passed,
          durationMs: report.diagnosis.durationMs,
          failureClassification: report.diagnosis.failureClassification,
        } : null,
        build: report.build ? {
          passed: report.build.passed,
          durationMs: report.build.durationMs,
        } : null,
        verification: report.verification ? {
          passed: report.verification.passed,
          durationMs: report.verification.durationMs,
          failureClassification: report.verification.failureClassification,
        } : null,
        persistence: report.persistence,
        preservation: report.preservation,
      },
    }
  }
}
