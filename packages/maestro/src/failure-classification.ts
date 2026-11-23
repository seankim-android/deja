import type { MaestroRunnerError } from './index.ts'
import { detectCrashInOutput, type MaestroCrashSignal } from './crash-signals.ts'

export type MaestroFailureCategory =
  | 'selector'
  | 'navigation'
  | 'product_assertion'
  | 'runner'
  | 'timeout'
  | 'unknown'
  | 'app_crash'

export type MaestroFailureClassification = {
  category: MaestroFailureCategory
  failingStep: string | null
  rawArtifactPath: string | null
  advisory: true
  repairEligible: boolean
  /** Set only with `app_crash`: the explicit signal that justified the label. */
  crash?: MaestroCrashSignal
}

export type MaestroFailureInput = {
  passed: boolean
  timedOut: boolean
  output: string
  errors: Pick<MaestroRunnerError, 'kind'>[]
  rawArtifactPath?: string | null
  /** An explicit crash signal found outside the output, such as a new crash report for the app ID. */
  crashSignal?: MaestroCrashSignal | null
}

function failedStep(output: string): string | null {
  const failures = output
    .split(/\r?\n/)
    .map(line => line.trim())
    .filter(line => line.endsWith('... FAILED'))
  if (failures.length !== 1) return null
  return failures[0].slice(0, -'... FAILED'.length).trimEnd()
}

function classification(
  category: MaestroFailureCategory,
  step: string | null,
  rawArtifactPath: string | null,
): MaestroFailureClassification {
  return {
    category,
    failingStep: step,
    rawArtifactPath,
    advisory: true,
    repairEligible: category === 'selector' || category === 'navigation',
  }
}

/**
 * Classify only explicit runner signals and narrow Maestro output shapes. These are advisory
 * heuristics, not a root-cause diagnosis. Conflicting or unrecognized output stays unknown.
 */
export function classifyMaestroFailure(
  input: MaestroFailureInput,
): MaestroFailureClassification | null {
  if (input.passed) return null
  const rawArtifactPath = input.rawArtifactPath ?? null
  const step = failedStep(input.output)

  if (input.timedOut || input.errors.some(error => error.kind === 'timeout')) {
    return classification('timeout', step, rawArtifactPath)
  }
  if (input.errors.some(error => error.kind !== 'execution')) {
    return classification('runner', step, rawArtifactPath)
  }

  // An explicit crash signal beats the element and launch shapes below: the app died, so a missing
  // element or a failed launch afterwards is a symptom, and a crash is never sent to repair.
  const crash = input.crashSignal ?? detectCrashInOutput(input.output)
  if (crash) return { ...classification('app_crash', step, rawArtifactPath), crash }

  const hasSelectorDiagnostic = /^Element not found:/m.test(input.output)
  const hasNavigationDiagnostic = /^(?:Unable to launch app|App launch failed):/m.test(input.output)
  const hasAssertionDiagnostic = /^Assertion is false:/m.test(input.output)
  const diagnosticCount = [hasSelectorDiagnostic, hasNavigationDiagnostic, hasAssertionDiagnostic]
    .filter(Boolean).length
  if (step === null || diagnosticCount !== 1) return classification('unknown', step, rawArtifactPath)

  if (hasSelectorDiagnostic && /^(?:Tap on|Long press on|Input text) /.test(step)) {
    return classification('selector', step, rawArtifactPath)
  }
  if (hasNavigationDiagnostic && /^(?:Launch app|Open link) /.test(step)) {
    return classification('navigation', step, rawArtifactPath)
  }
  // A quoted text assertion is a product expectation. An id assertion or mixed diagnostics are
  // deliberately not inferred to be product regressions because a stale selector could look alike.
  if (hasAssertionDiagnostic && /^Assert that "/.test(step)) {
    return classification('product_assertion', step, rawArtifactPath)
  }
  return classification('unknown', step, rawArtifactPath)
}
