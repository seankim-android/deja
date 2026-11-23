import type { EngineReport } from '@deja-qa/core'
import {
  MAESTRO_ENGINE_PIN,
  MAESTRO_TESTED_RANGE,
  MaestroEngineError,
  dejaHome,
  engineDirectory,
  installMaestroEngine,
  resolveMaestroEngine,
  type InstallEngineOptions,
  type ResolveEngineOptions,
  type ResolvedEngine,
} from '@deja-qa/maestro'
import { CliConfigurationError, type ProductionConfiguration } from './config.ts'

export type SelectedEngine = { resolved: ResolvedEngine; report: EngineReport; warning?: string }

export const TESTED_RANGE_TEXT = `>=${MAESTRO_TESTED_RANGE.min} <${MAESTRO_TESTED_RANGE.maxExclusive}`

/**
 * Picks the Maestro for this run and applies the version-range rule. Outside the tested range (or
 * an unreadable version) the run is refused, unless `maestro.allowUntested` is true; then it runs
 * with a warning and the report says `inRange: false`.
 */
export async function selectEngine(
  configuration: ProductionConfiguration,
  options: Omit<ResolveEngineOptions, 'configExecutable'> = {},
): Promise<SelectedEngine> {
  let resolved: ResolvedEngine
  try {
    resolved = await resolveMaestroEngine({
      ...(configuration.maestro.executable ? { configExecutable: configuration.maestro.executable } : {}),
      ...options,
    })
  } catch (error) {
    if (error instanceof MaestroEngineError) throw new CliConfigurationError(error.message, { cause: error })
    throw error
  }
  const inRange = resolved.range === 'in-range'
  const report: EngineReport = {
    version: resolved.version,
    source: resolved.source,
    ...(resolved.overrideVia ? { overrideVia: resolved.overrideVia } : {}),
    inRange,
  }
  // An override that cannot start fails the run as a launch failure (exit 1), as before.
  if (inRange || resolved.unlaunchable) return { resolved, report }
  const via = resolved.overrideVia === 'config' ? 'maestro.executable' : 'DEJA_MAESTRO'
  const found = resolved.version ? `Maestro ${resolved.version}` : 'a Maestro whose version could not be read'
  const message = `${via} points to ${found}, outside the tested range ${TESTED_RANGE_TEXT}`
  if (!configuration.maestro.allowUntested) {
    throw new CliConfigurationError(
      `${message}. Use the pinned engine (deja engine install) or set maestro.allowUntested to true.`,
    )
  }
  return { resolved, report, warning: `Warning: ${message}; running because maestro.allowUntested is true.` }
}

export async function installEngineCommand(options: InstallEngineOptions = {}) {
  try {
    const result = await installMaestroEngine(options)
    return result
  } catch (error) {
    if (error instanceof MaestroEngineError) throw new CliConfigurationError(error.message, { cause: error })
    throw error
  }
}

export function engineHome(env: NodeJS.ProcessEnv = process.env) {
  return { home: dejaHome(env), directory: engineDirectory(dejaHome(env), MAESTRO_ENGINE_PIN.version) }
}
