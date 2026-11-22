import { readFile } from 'node:fs/promises'
import { relative, resolve } from 'node:path'
import type {
  ExecutionResult,
  RunnerRequest,
  WalkRunner,
} from '../../core/src/index.ts'
import {
  classifyMaestroFailure,
  runMaestro,
  type MaestroRunResult,
} from './index.ts'

export type MaestroWalkRunnerOptions = {
  workspaceRoot: string
  executable: string
  device: string
  timeoutMs: number
  androidSdkRoot?: string
  javaHome?: string
}

function portable(root: string, path: string) {
  return relative(root, path).split('\\').join('/')
}

export async function countFlowAssertions(flowPath: string): Promise<number | null> {
  try {
    const contents = await readFile(flowPath, 'utf8')
    return (contents.match(/^\s*-\s+assertVisible:/gm) ?? []).length
  } catch {
    return null
  }
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
  ): Promise<MaestroRunResult> {
    return runMaestro({
      executable: this.options.executable,
      device: this.options.device,
      platform: 'android',
      workspaceRoot: this.options.workspaceRoot,
      flowPath,
      timeoutMs,
      ...(this.options.androidSdkRoot ? { androidSdkRoot: this.options.androidSdkRoot } : {}),
      ...(this.options.javaHome ? { javaHome: this.options.javaHome } : {}),
      artifacts: { directory: artifactDirectory },
    })
  }

  async adapt(result: MaestroRunResult): Promise<ExecutionResult> {
    const outputPath = result.artifacts
      ? portable(this.options.workspaceRoot, result.artifacts.outputPath)
      : null
    const classified = classifyMaestroFailure({
      passed: result.passed,
      timedOut: result.timedOut,
      output: result.output,
      errors: result.errors,
      rawArtifactPath: outputPath,
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
    ))
  }
}
