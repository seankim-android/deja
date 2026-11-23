import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import type { ExecutionResult } from '@deja-qa/core'

export type CompositionRunnerObservation = {
  kind: 'launch'
  clearState: boolean
} | {
  kind: 'assertVisible'
  resourceId: string | null
  text: string | null
} | {
  kind: 'action'
  command: 'tapOn' | 'inputText'
}

export type CompositionRunnerTrace = {
  observations: CompositionRunnerObservation[]
  finalVisibleResourceIds: string[]
  finalScreen: string
}

const OBSERVATION_PREFIX = 'FIXTURE_OBSERVATION '
const SUMMARY_PREFIX = 'FIXTURE_SUMMARY '
const MAX_OBSERVATIONS = 512

function token(value: unknown): value is string {
  return typeof value === 'string' && /^[A-Za-z0-9][A-Za-z0-9_.:-]{0,159}$/.test(value)
}

function observation(value: unknown): CompositionRunnerObservation | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null
  const entry = value as Record<string, unknown>
  if (entry.kind === 'launch') {
    return typeof entry.clearState === 'boolean'
      ? { kind: 'launch', clearState: entry.clearState } : null
  }
  if (entry.kind === 'assertVisible') {
    if ((entry.resourceId !== null && !token(entry.resourceId)) ||
        (entry.text !== null && typeof entry.text !== 'string')) return null
    return {
      kind: 'assertVisible',
      resourceId: entry.resourceId as string | null,
      text: entry.text as string | null,
    }
  }
  if (entry.kind === 'action' && (entry.command === 'tapOn' || entry.command === 'inputText')) {
    return { kind: 'action', command: entry.command }
  }
  return null
}

/**
 * Read the bounded observation protocol emitted by the executable composition fixture within its
 * actual process output. Ordinary Maestro output without observations cannot authorize a segment.
 */
export async function readCompositionRunnerTrace(
  workspaceRoot: string,
  execution: ExecutionResult,
): Promise<CompositionRunnerTrace | null> {
  const output = execution.artifacts.find(artifact =>
    artifact.kind === 'output' && artifact.path.endsWith('output.log'))
  if (!output) return null
  let contents: string
  try {
    contents = await readFile(resolve(workspaceRoot, output.path), 'utf8')
  } catch {
    return null
  }
  const observations: CompositionRunnerObservation[] = []
  let summary: unknown
  for (const line of contents.split(/\r?\n/)) {
    if (line.startsWith(OBSERVATION_PREFIX)) {
      if (observations.length >= MAX_OBSERVATIONS) return null
      try {
        const observed = observation(JSON.parse(line.slice(OBSERVATION_PREFIX.length)))
        if (!observed) return null
        observations.push(observed)
      } catch {
        return null
      }
    } else if (line.startsWith(SUMMARY_PREFIX)) {
      try {
        summary = JSON.parse(line.slice(SUMMARY_PREFIX.length))
      } catch {
        return null
      }
    }
  }
  if (typeof summary !== 'object' || summary === null || Array.isArray(summary)) return null
  const result = summary as Record<string, unknown>
  if (!Array.isArray(result.visibleResourceIds) ||
      result.visibleResourceIds.some(value => !token(value)) || !token(result.screen)) return null
  return {
    observations,
    finalVisibleResourceIds: result.visibleResourceIds as string[],
    finalScreen: result.screen,
  }
}
