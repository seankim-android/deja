import type {
  CacheMissReason,
  LookupMiss,
  WalkIdentity,
  WalkValidation,
} from './contracts.ts'

export const VERIFIED_SUBFLOW_SCHEMA_VERSION = 1

/** The app/build half of exact walk identity. A segment has no goal and is never a semantic hit. */
export type SubflowIdentity = Omit<WalkIdentity, 'goal'>

export type SubflowInput = {
  name: string
  kind: 'text' | 'credential'
  required: boolean
}

export type SubflowOutput = {
  name: string
  kind: 'screen'
  resourceId: string
}

export type SubflowPrecondition = {
  kind: 'clean-app-launch'
} | {
  kind: 'screen-visible'
  resourceId: string
}

export type SubflowPostcondition = {
  kind: 'screen-visible'
  resourceId: string
}

export type SubflowEvidenceArtifact = {
  path: string
  sha256: string
}

export type SubflowProvenance = {
  kind: 'curated-evidence'
  ticket: string
  runId: string
  newDeviceRunRecorded: false
  artifacts: SubflowEvidenceArtifact[]
} | {
  kind: 'deterministic-fixture'
  fixtureId: string
  newDeviceRunRecorded: false
}

export type SubflowContract = {
  schemaVersion: typeof VERIFIED_SUBFLOW_SCHEMA_VERSION
  segmentId: string
  identity: SubflowIdentity
  inputs: SubflowInput[]
  outputs: SubflowOutput[]
  preconditions: SubflowPrecondition[]
  postconditions: SubflowPostcondition[]
  stepCount: number
  assertionCount: number
  provenance: SubflowProvenance
}

export type VerifiedSubflow = SubflowContract & {
  flowPath: string
  flowSha256: string
  lastValidation: WalkValidation & { passed: true }
}

export type VerifiedSubflowHit = {
  status: 'hit'
  segment: VerifiedSubflow
  flowPath: string
}

export type VerifiedSubflowLookup = VerifiedSubflowHit | LookupMiss

export type PersistVerifiedSubflowOptions = {
  contract: SubflowContract
  candidatePath: string
  validation: WalkValidation
}

export interface VerifiedSubflowStore {
  lookup(segmentId: string, identity: SubflowIdentity): Promise<VerifiedSubflowLookup>
  persistVerified(options: PersistVerifiedSubflowOptions): Promise<VerifiedSubflow>
  list(): Promise<VerifiedSubflow[]>
}

export type CompositionStepMetrics = {
  reusedSteps: number
  newlyExploredSteps: number
  totalSteps: number
  reusedAssertions: number
  newAssertions: number
  totalAssertions: number
}

export type CompositionModelMetrics = {
  composition: 0
  newExploration: 0
  segmentVerification: 0
  fullJourneyVerification: 0
  basis: string
}

export class InvalidSubflowError extends Error {
  constructor(message: string) {
    super(`Invalid subflow: ${message}`)
    this.name = 'InvalidSubflowError'
  }
}

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function exactObject(value: unknown, keys: string[], path: string): Record<string, unknown> {
  if (!object(value)) throw new InvalidSubflowError(`${path} must be an object`)
  const allowed = new Set(keys)
  if (Object.keys(value).some(key => !allowed.has(key)) ||
      keys.some(key => !Object.hasOwn(value, key))) {
    throw new InvalidSubflowError(`${path} has missing or unsupported fields`)
  }
  return value
}

function boundedArray(value: unknown, minimum: number, maximum: number, path: string): unknown[] {
  if (!Array.isArray(value) || value.length < minimum || value.length > maximum) {
    throw new InvalidSubflowError(`${path} must contain ${minimum} to ${maximum} entries`)
  }
  return value
}

function token(value: unknown, path: string, maximum = 160): string {
  if (typeof value !== 'string' || value.length > maximum ||
      !new RegExp(`^[A-Za-z0-9][A-Za-z0-9_.:/-]{0,${maximum - 1}}$`).test(value)) {
    throw new InvalidSubflowError(`${path} must be a bounded identifier`)
  }
  return value
}

function sha256(value: unknown, path: string): string {
  if (typeof value !== 'string' || !/^[a-f0-9]{64}$/.test(value)) {
    throw new InvalidSubflowError(`${path} must be a SHA-256 fingerprint`)
  }
  return value
}

function identity(value: unknown): SubflowIdentity {
  if (!object(value)) throw new InvalidSubflowError('identity must be an object')
  const source = value
  const entry = source
  for (const key of ['appId', 'platform', 'flowSchemaVersion', 'buildFingerprint']) {
    if (!Object.hasOwn(source, key)) throw new InvalidSubflowError(`identity.${key} is required`)
  }
  if (Object.keys(source).some(key => ![
    'appId', 'platform', 'flowSchemaVersion', 'buildFingerprint', 'adapter', 'browser',
  ].includes(key))) throw new InvalidSubflowError('identity has unsupported fields')
  const result: SubflowIdentity = {
    appId: token(entry.appId, 'identity.appId'),
    platform: token(entry.platform, 'identity.platform'),
    flowSchemaVersion: token(entry.flowSchemaVersion, 'identity.flowSchemaVersion'),
    buildFingerprint: sha256(entry.buildFingerprint, 'identity.buildFingerprint'),
  }
  if (source.adapter !== undefined) result.adapter = token(source.adapter, 'identity.adapter')
  if (source.browser !== undefined) result.browser = token(source.browser, 'identity.browser')
  return result
}

function provenance(value: unknown): SubflowProvenance {
  if (!object(value) || value.newDeviceRunRecorded !== false) {
    throw new InvalidSubflowError('provenance must explicitly exclude a new device run')
  }
  if (value.kind === 'deterministic-fixture') {
    const entry = exactObject(value,
      ['kind', 'fixtureId', 'newDeviceRunRecorded'], 'provenance')
    return {
      kind: 'deterministic-fixture',
      fixtureId: token(entry.fixtureId, 'provenance.fixtureId'),
      newDeviceRunRecorded: false,
    }
  }
  if (value.kind !== 'curated-evidence') {
    throw new InvalidSubflowError('provenance kind is unsupported')
  }
  const entry = exactObject(value,
    ['kind', 'ticket', 'runId', 'newDeviceRunRecorded', 'artifacts'], 'provenance')
  const artifacts = boundedArray(entry.artifacts, 1, 16, 'provenance.artifacts')
    .map((item, index) => {
      const path = `provenance.artifacts[${index}]`
      const artifact = exactObject(item, ['path', 'sha256'], path)
      if (typeof artifact.path !== 'string' || artifact.path.startsWith('/') ||
          artifact.path.split('/').includes('..') || !/\.(json|yaml|md)$/.test(artifact.path)) {
        throw new InvalidSubflowError(`${path}.path must be curated relative text evidence`)
      }
      return {
        path: token(artifact.path, `${path}.path`, 240),
        sha256: sha256(artifact.sha256, `${path}.sha256`),
      }
    })
  if (new Set(artifacts.map(artifact => artifact.path)).size !== artifacts.length) {
    throw new InvalidSubflowError('provenance contains duplicate artifacts')
  }
  return {
    kind: 'curated-evidence',
    ticket: token(entry.ticket, 'provenance.ticket'),
    runId: token(entry.runId, 'provenance.runId'),
    newDeviceRunRecorded: false,
    artifacts,
  }
}

/** Validate a closed, bounded segment contract; checksums and execution remain store/runner duties. */
export function validateSubflowContract(value: unknown): SubflowContract {
  const entry = exactObject(value, [
    'schemaVersion', 'segmentId', 'identity', 'inputs', 'outputs', 'preconditions',
    'postconditions', 'stepCount', 'assertionCount', 'provenance',
  ], 'root')
  if (entry.schemaVersion !== VERIFIED_SUBFLOW_SCHEMA_VERSION) {
    throw new InvalidSubflowError('schemaVersion is unsupported')
  }
  const inputs = boundedArray(entry.inputs, 0, 8, 'inputs').map((item, index) => {
    const path = `inputs[${index}]`
    const input = exactObject(item, ['name', 'kind', 'required'], path)
    if (input.kind !== 'text' && input.kind !== 'credential') {
      throw new InvalidSubflowError(`${path}.kind is unsupported`)
    }
    if (typeof input.required !== 'boolean') {
      throw new InvalidSubflowError(`${path}.required must be boolean`)
    }
    return {
      name: token(input.name, `${path}.name`, 64),
      kind: input.kind as 'text' | 'credential',
      required: input.required,
    }
  })
  if (new Set(inputs.map(input => input.name)).size !== inputs.length) {
    throw new InvalidSubflowError('input names must be unique')
  }
  const outputs = boundedArray(entry.outputs, 1, 8, 'outputs').map((item, index) => {
    const path = `outputs[${index}]`
    const output = exactObject(item, ['name', 'kind', 'resourceId'], path)
    if (output.kind !== 'screen') throw new InvalidSubflowError(`${path}.kind must be screen`)
    return {
      name: token(output.name, `${path}.name`, 64),
      kind: 'screen' as const,
      resourceId: token(output.resourceId, `${path}.resourceId`),
    }
  })
  if (new Set(outputs.map(output => output.name)).size !== outputs.length) {
    throw new InvalidSubflowError('output names must be unique')
  }
  const preconditions = boundedArray(entry.preconditions, 1, 8, 'preconditions')
    .map((item, index) => {
      const path = `preconditions[${index}]`
      if (!object(item)) throw new InvalidSubflowError(`${path} must be an object`)
      if (item.kind === 'clean-app-launch') {
        exactObject(item, ['kind'], path)
        return { kind: 'clean-app-launch' as const }
      }
      const condition = exactObject(item, ['kind', 'resourceId'], path)
      if (condition.kind !== 'screen-visible') {
        throw new InvalidSubflowError(`${path}.kind is unsupported`)
      }
      return {
        kind: 'screen-visible' as const,
        resourceId: token(condition.resourceId, `${path}.resourceId`),
      }
    })
  const postconditions = boundedArray(entry.postconditions, 1, 8, 'postconditions')
    .map((item, index) => {
      const path = `postconditions[${index}]`
      const condition = exactObject(item, ['kind', 'resourceId'], path)
      if (condition.kind !== 'screen-visible') {
        throw new InvalidSubflowError(`${path}.kind must be screen-visible`)
      }
      return {
        kind: 'screen-visible' as const,
        resourceId: token(condition.resourceId, `${path}.resourceId`),
      }
    })
  for (const output of outputs) {
    if (!postconditions.some(condition => condition.resourceId === output.resourceId)) {
      throw new InvalidSubflowError(`output ${output.name} has no matching postcondition`)
    }
  }
  if (!Number.isInteger(entry.stepCount) || (entry.stepCount as number) < 1 ||
      (entry.stepCount as number) > 256) {
    throw new InvalidSubflowError('stepCount must be an integer from 1 to 256')
  }
  if (!Number.isInteger(entry.assertionCount) || (entry.assertionCount as number) < 0 ||
      (entry.assertionCount as number) > (entry.stepCount as number)) {
    throw new InvalidSubflowError('assertionCount must fit within stepCount')
  }
  return {
    schemaVersion: VERIFIED_SUBFLOW_SCHEMA_VERSION,
    segmentId: token(entry.segmentId, 'segmentId', 64),
    identity: identity(entry.identity),
    inputs,
    outputs,
    preconditions,
    postconditions,
    stepCount: entry.stepCount as number,
    assertionCount: entry.assertionCount as number,
    provenance: provenance(entry.provenance),
  }
}

export function sameSubflowIdentity(left: SubflowIdentity, right: SubflowIdentity): boolean {
  return left.appId === right.appId && left.platform === right.platform &&
    left.flowSchemaVersion === right.flowSchemaVersion &&
    left.buildFingerprint === right.buildFingerprint &&
    (left.adapter ?? null) === (right.adapter ?? null) &&
    (left.browser ?? null) === (right.browser ?? null)
}

export function subflowMissReason(
  requested: SubflowIdentity,
  stored: readonly VerifiedSubflow[],
): CacheMissReason {
  if (stored.length === 0) return 'not-found'
  const sameApp = stored.filter(item => item.identity.appId === requested.appId)
  if (sameApp.length === 0) return 'wrong-app'
  const samePlatform = sameApp.filter(item => item.identity.platform === requested.platform)
  if (samePlatform.length === 0) return 'wrong-platform'
  const sameSchema = samePlatform.filter(item =>
    item.identity.flowSchemaVersion === requested.flowSchemaVersion)
  if (sameSchema.length === 0) return 'wrong-flow-schema'
  const sameAdapter = sameSchema.filter(item =>
    (item.identity.adapter ?? null) === (requested.adapter ?? null))
  if (sameAdapter.length === 0) return 'wrong-adapter'
  const sameBrowser = sameAdapter.filter(item =>
    (item.identity.browser ?? null) === (requested.browser ?? null))
  if (sameBrowser.length === 0) return 'wrong-browser'
  if (!sameBrowser.some(item => item.identity.buildFingerprint === requested.buildFingerprint)) {
    return 'stale-build'
  }
  return 'failed-validation'
}
