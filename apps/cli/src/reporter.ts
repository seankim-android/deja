import { mkdir, readFile, realpath, rename, stat, writeFile } from 'node:fs/promises'
import { dirname, resolve, sep } from 'node:path'
import type { ReportReceipt, RunReporter, RunResult } from '@deja-run/core'
import { CliConfigurationError } from './config.ts'
import { redactJson, type Redactor } from './redaction.ts'

async function atomicWrite(path: string, contents: string) {
  await mkdir(dirname(path), { recursive: true })
  const temporary = `${path}.${process.pid}.tmp`
  await writeFile(temporary, contents)
  await rename(temporary, path)
}

export class JsonRunReporter implements RunReporter {
  private readonly workspaceRoot: string
  private readonly redact: Redactor
  private lastReceipt: ReportReceipt | null = null

  constructor(workspaceRoot: string, redact: Redactor) {
    this.workspaceRoot = workspaceRoot
    this.redact = redact
  }

  get receipt() {
    return this.lastReceipt
  }

  async report(result: RunResult): Promise<ReportReceipt> {
    const directory = resolve(this.workspaceRoot, '.deja/reports')
    const reportPath = resolve(
      directory,
      `${result.command}-${Date.now()}-${process.pid}.json`,
    )
    const latestPath = resolve(directory, 'latest.json')
    const receipt = {
      reportPath: `.deja/reports/${reportPath.split(sep).at(-1)}`,
      latestPath: '.deja/reports/latest.json',
    }
    const serialized = `${JSON.stringify(redactJson(result, this.redact), null, 2)}\n`
    await atomicWrite(reportPath, serialized)
    await atomicWrite(latestPath, serialized)
    this.lastReceipt = receipt
    return receipt
  }
}

export async function readRunReport(
  workspaceRoot: string,
  reportPath: string,
  redact: Redactor,
): Promise<RunResult> {
  const root = await realpath(resolve(workspaceRoot))
  const candidate = resolve(root, reportPath)
  if (candidate === root || !candidate.startsWith(`${root}${sep}`)) {
    throw new CliConfigurationError('Report must be inside the configured workspace')
  }
  const canonical = await realpath(candidate)
  if (!canonical.startsWith(`${root}${sep}`) || !(await stat(canonical)).isFile()) {
    throw new CliConfigurationError('Report must resolve to a file inside the configured workspace')
  }
  const contents = await readFile(canonical, 'utf8')
  if (Buffer.byteLength(contents) > 2_000_000) {
    throw new CliConfigurationError('Report exceeds the 2 MB inspection limit')
  }
  const value = JSON.parse(contents) as RunResult
  if (value.schemaVersion !== 1 || !['run', 'explore', 'remember'].includes(value.command) ||
      typeof value.cacheStatus !== 'string' || typeof value.outcome !== 'string' ||
      typeof value.passed !== 'boolean' || !Array.isArray(value.artifacts) ||
      !Array.isArray(value.diffs)) {
    throw new CliConfigurationError('Report is not a supported Déjà run result')
  }
  return redactJson(value, redact)
}
