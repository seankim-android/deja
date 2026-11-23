import { readFile, realpath } from 'node:fs/promises'
import { dirname, isAbsolute, relative, resolve } from 'node:path'
import type {
  ExecutionResult,
  RunnerRequest,
  WalkRunner,
} from '@deja-qa/core'
import {
  classifyMaestroFailure,
  defaultCrashReportDirectory,
  detectCrashInOutput,
  findCrashReport,
  runMaestro,
  type MaestroExecutionAdapter,
  type MaestroRunResult,
} from './index.ts'

export type MaestroWalkRunnerOptions = {
  workspaceRoot: string
  executable: string
  device: string
  timeoutMs: number
  adapter?: MaestroExecutionAdapter
  androidSdkRoot?: string
  javaHome?: string
  /** Absolute path; every Maestro run overwrites it, so the last run's report is kept. */
  junitPath?: string
  /**
   * The app under test. With an iOS adapter it turns on the crash-report signal: a new `.ips` for
   * this app ID written during a failed run labels it `app_crash`. Absent means no report is read.
   */
  appId?: string
  /** Seam: where crash reports are written. Defaults to ~/Library/Logs/DiagnosticReports. */
  crashReportDirectory?: string
  /**
   * How long after a failed run a crash report may still land. The system often writes it a few
   * seconds after the process dies. Default 10 s. The next attempt starts only after this wait ends
   * or the report is found, so a report is attributed to the attempt it was written for.
   */
  crashReportGraceMs?: number
  /** Poll interval while waiting inside the grace window. Default 500 ms. */
  crashReportPollMs?: number
}

function portable(root: string, path: string) {
  return relative(root, path).split('\\').join('/')
}

export async function countFlowAssertions(flowPath: string): Promise<number | null> {
  const active = new Set<string>()
  async function visit(path: string): Promise<number | null> {
    try {
      const canonical = await realpath(path)
      if (active.has(canonical)) return null
      active.add(canonical)
      try {
        const contents = await readFile(canonical, 'utf8')
        let count = (contents.match(/^\s*-\s+assertVisible:/gm) ?? []).length
        for (const match of contents.matchAll(/^\s*-\s+runFlow:\s*(.+)$/gm)) {
          const rawReference = match[1].trim()
          let reference: string
          try {
            reference = rawReference.startsWith('"') ? JSON.parse(rawReference) : rawReference
          } catch {
            return null
          }
          if (!reference || isAbsolute(reference)) return null
          const nested = await visit(resolve(dirname(canonical), reference))
          if (nested === null) return null
          count += nested
        }
        return count
      } finally {
        active.delete(canonical)
      }
    } catch {
      return null
    }
  }
  return visit(flowPath)
}

export class MaestroWalkRunner implements WalkRunner {
  readonly executable: string
  private readonly options: MaestroWalkRunnerOptions

  constructor(options: MaestroWalkRunnerOptions) {
    this.options = options
    this.executable = options.executable
  }

  async runFlow(
    flowPath: string,
    artifactDirectory: string,
    timeoutMs = this.options.timeoutMs,
    env?: Record<string, string>,
  ): Promise<MaestroRunResult> {
    return runMaestro({
      executable: this.options.executable,
      device: this.options.device,
      platform: this.options.adapter?.platform ?? 'android',
      ...(this.options.adapter ? { adapter: this.options.adapter } : {}),
      workspaceRoot: this.options.workspaceRoot,
      flowPath,
      timeoutMs,
      ...(env ? { env } : {}),
      ...(this.options.androidSdkRoot ? { androidSdkRoot: this.options.androidSdkRoot } : {}),
      ...(this.options.javaHome ? { javaHome: this.options.javaHome } : {}),
      ...(this.options.junitPath ? { junitPath: this.options.junitPath } : {}),
      artifacts: { directory: artifactDirectory },
    })
  }

  /** A crash report written during this run for the app ID. Only a failed iOS run looks. */
  private async crashReportSignal(result: MaestroRunResult) {
    if (result.passed || this.options.adapter?.platform !== 'ios' || !this.options.appId) return null
    if (detectCrashInOutput(result.output)) return null
    // A failed assertion is a product answer, not a symptom a crash could hide behind, so it never waits.
    const grace = /^Assertion is false:/m.test(result.output) ? 0 : this.options.crashReportGraceMs ?? 10_000
    const poll = this.options.crashReportPollMs ?? 500
    const search = {
      directory: this.options.crashReportDirectory ?? defaultCrashReportDirectory(),
      appId: this.options.appId,
      sinceMs: Date.parse(result.startedAt),
      untilMs: Date.parse(result.endedAt) + grace,
    }
    // Poll until the report lands or the grace window closes. A report written later than that
    // belongs to no attempt we can name, so it is not used.
    const deadline = Date.now() + grace
    for (;;) {
      const found = await findCrashReport(search)
      if (found || Date.now() >= deadline) return found
      await new Promise(resolve => setTimeout(resolve, Math.min(poll, Math.max(1, deadline - Date.now()))))
    }
  }

  async adapt(result: MaestroRunResult): Promise<ExecutionResult> {
    const outputPath = result.artifacts
      ? portable(this.options.workspaceRoot, result.artifacts.outputPath)
      : null
    const crashSignal = await this.crashReportSignal(result)
    const classified = classifyMaestroFailure({
      passed: result.passed,
      timedOut: result.timedOut,
      output: result.output,
      errors: result.errors,
      rawArtifactPath: outputPath,
      crashSignal,
    })
    return {
      passed: result.passed,
      startedAt: result.startedAt,
      endedAt: result.endedAt,
      durationMs: result.durationMs,
      exitCode: result.exitCode,
      assertions: await countFlowAssertions(result.flowPath),
      failure: classified,
      artifacts: [
        { kind: 'yaml', path: portable(this.options.workspaceRoot, result.flowPath) },
        ...(result.artifacts ? [
          { kind: 'output' as const, path: outputPath! },
          { kind: 'output' as const, path: portable(this.options.workspaceRoot, result.artifacts.stdoutPath) },
          { kind: 'output' as const, path: portable(this.options.workspaceRoot, result.artifacts.stderrPath) },
        ] : []),
      ],
    }
  }

  async run(request: RunnerRequest): Promise<ExecutionResult> {
    return this.adapt(await this.runFlow(
      resolve(this.options.workspaceRoot, request.flowPath),
      request.artifactDirectory,
      request.timeoutMs,
      request.env,
    ))
  }
}
