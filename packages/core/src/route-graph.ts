import { createHash } from 'node:crypto'

export const ROUTE_GRAPH_SCHEMA_VERSION = 1
export const SCREEN_FINGERPRINT_VERSION = 1
export const ROUTE_GRAPH_FALLBACK = 'existing-exact-replay-or-exploration' as const

const MAX_NODES = 64
const MAX_TRANSITIONS = 256
const MAX_FAILED_BRANCHES = 256
const MAX_OBSERVATIONS = 16
const MAX_ARTIFACTS = 16

export type ScreenObservation = {
  kind: 'android-resource-id'
  value: string
}

export type ScreenNode = {
  nodeId: string
  observations: ScreenObservation[]
  fingerprint: string
  validatedAt: string
}

export type RouteAction = {
  kind: 'tap'
  targetResourceId: string
}

export type VerifiedTransition = {
  transitionId: string
  fromNodeId: string
  action: RouteAction
  toNodeId: string
  validatedAt: string
}

export type FailedBranch = {
  branchId: string
  fromNodeId: string
  action: RouteAction
  category: 'selector' | 'navigation' | 'product_assertion' | 'runner' | 'timeout' | 'unknown'
  failedAt: string
}

export type EvidenceArtifact = {
  path: string
  sha256: string
}

export type RouteGraphProvenance = {
  kind: 'curated-android-evidence'
  ticket: string
  runId: string
  newDeviceRunRecorded: false
  artifacts: EvidenceArtifact[]
} | {
  kind: 'deterministic-fixture'
  fixtureId: string
  newDeviceRunRecorded: false
}

export type RouteGraph = {
  schemaVersion: typeof ROUTE_GRAPH_SCHEMA_VERSION
  appId: string
  platform: 'android'
  buildFingerprint: string
  screenFingerprintVersion: typeof SCREEN_FINGERPRINT_VERSION
  validatedAt: string
  replayValidatedAt: string | null
  nodes: ScreenNode[]
  transitions: VerifiedTransition[]
  failedBranches: FailedBranch[]
  provenance: RouteGraphProvenance
}

export type ScreenMatchOptions = {
  observations: ScreenObservation[]
  /** The logical screen expected at this point, when one is known. */
  expectedNodeId?: string
  /** The last matched node; a subsequent match to it is a repeated observation. */
  previousNodeId?: string
}

export type ScreenMatch = {
  status: 'matched' | 'repeated' | 'ambiguous' | 'changed' | 'unknown'
  fingerprint: string
  candidateNodeIds: string[]
  /** Route-graph observations never authorize a walk-cache hit. */
  cacheHit: false
  fallback: typeof ROUTE_GRAPH_FALLBACK | null
}

export class InvalidRouteGraphError extends Error {
  constructor(message: string) {
    super(`Invalid route graph: ${message}`)
    this.name = 'InvalidRouteGraphError'
  }
}

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function exactObject(value: unknown, keys: string[], path: string): Record<string, unknown> {
  if (!object(value)) throw new InvalidRouteGraphError(`${path} must be an object`)
  const allowed = new Set(keys)
  const actual = Object.keys(value)
  if (actual.some(key => !allowed.has(key)) || keys.some(key => !Object.hasOwn(value, key))) {
    throw new InvalidRouteGraphError(`${path} has missing or unsupported fields`)
  }
  return value
}

function boundedArray(value: unknown, minimum: number, maximum: number, path: string): unknown[] {
  if (!Array.isArray(value) || value.length < minimum || value.length > maximum) {
    throw new InvalidRouteGraphError(`${path} must contain ${minimum} to ${maximum} entries`)
  }
  return value
}

function token(value: unknown, path: string, maximum = 160): string {
  if (typeof value !== 'string' || value.length > maximum ||
      !new RegExp(`^[A-Za-z0-9][A-Za-z0-9_.:/-]{0,${maximum - 1}}$`).test(value)) {
    throw new InvalidRouteGraphError(`${path} must be a bounded identifier`)
  }
  return value
}

function sha256(value: unknown, path: string): string {
  if (typeof value !== 'string' || !/^[a-f0-9]{64}$/.test(value)) {
    throw new InvalidRouteGraphError(`${path} must be a SHA-256 fingerprint`)
  }
  return value
}

function timestamp(value: unknown, path: string): string {
  if (typeof value !== 'string' || value.length !== 24 || !Number.isFinite(Date.parse(value)) ||
      new Date(value).toISOString() !== value) {
    throw new InvalidRouteGraphError(`${path} must be an exact ISO UTC timestamp`)
  }
  return value
}

function observation(value: unknown, path: string): ScreenObservation {
  const entry = exactObject(value, ['kind', 'value'], path)
  if (entry.kind !== 'android-resource-id') {
    throw new InvalidRouteGraphError(`${path} accepts only curated Android resource IDs`)
  }
  return { kind: 'android-resource-id', value: token(entry.value, `${path}.value`) }
}

function observations(value: unknown, path: string): ScreenObservation[] {
  const entries = boundedArray(value, 1, MAX_OBSERVATIONS, path)
  const result = entries.map((entry, index) => observation(entry, `${path}[${index}]`))
  if (new Set(result.map(entry => entry.value)).size !== result.length) {
    throw new InvalidRouteGraphError(`${path} contains duplicate observations`)
  }
  return result
}

/** SHA-256 over a canonical, bounded set; observation input order never changes the fingerprint. */
export function fingerprintScreen(input: ScreenObservation[]): string {
  const curated = observations(input, 'observations')
  const canonical = [...curated].sort((left, right) => left.value.localeCompare(right.value))
  return createHash('sha256').update(JSON.stringify({
    version: SCREEN_FINGERPRINT_VERSION,
    observations: canonical,
  })).digest('hex')
}

export function makeScreenNode(
  nodeId: string,
  input: ScreenObservation[],
  validatedAt: string,
): ScreenNode {
  return {
    nodeId: token(nodeId, 'nodeId'),
    observations: observations(input, 'observations'),
    fingerprint: fingerprintScreen(input),
    validatedAt: timestamp(validatedAt, 'validatedAt'),
  }
}

function action(value: unknown, path: string): RouteAction {
  const entry = exactObject(value, ['kind', 'targetResourceId'], path)
  if (entry.kind !== 'tap') throw new InvalidRouteGraphError(`${path} supports only tap actions`)
  return {
    kind: 'tap',
    targetResourceId: token(entry.targetResourceId, `${path}.targetResourceId`),
  }
}

function artifact(value: unknown, path: string): EvidenceArtifact {
  const entry = exactObject(value, ['path', 'sha256'], path)
  if (typeof entry.path !== 'string' || entry.path.startsWith('/') ||
      entry.path.split('/').includes('..') || !/\.(json|yaml|md)$/.test(entry.path)) {
    throw new InvalidRouteGraphError(`${path}.path must be curated relative text evidence`)
  }
  const artifactPath = token(entry.path, `${path}.path`, 240)
  return { path: artifactPath, sha256: sha256(entry.sha256, `${path}.sha256`) }
}

function provenance(value: unknown): RouteGraphProvenance {
  if (!object(value) || typeof value.kind !== 'string') {
    throw new InvalidRouteGraphError('provenance must declare its evidence kind')
  }
  if (value.newDeviceRunRecorded !== false) {
    throw new InvalidRouteGraphError('provenance must explicitly record that no new device run was captured')
  }
  if (value.kind === 'curated-android-evidence') {
    const entry = exactObject(value,
      ['kind', 'ticket', 'runId', 'newDeviceRunRecorded', 'artifacts'], 'provenance')
    const artifacts = boundedArray(entry.artifacts, 1, MAX_ARTIFACTS, 'provenance.artifacts')
      .map((item, index) => artifact(item, `provenance.artifacts[${index}]`))
    if (new Set(artifacts.map(item => item.path)).size !== artifacts.length) {
      throw new InvalidRouteGraphError('provenance contains duplicate artifact paths')
    }
    return {
      kind: 'curated-android-evidence',
      ticket: token(entry.ticket, 'provenance.ticket'),
      runId: token(entry.runId, 'provenance.runId'),
      newDeviceRunRecorded: false,
      artifacts,
    }
  }
  if (value.kind === 'deterministic-fixture') {
    const entry = exactObject(value, ['kind', 'fixtureId', 'newDeviceRunRecorded'], 'provenance')
    return {
      kind: 'deterministic-fixture',
      fixtureId: token(entry.fixtureId, 'provenance.fixtureId'),
      newDeviceRunRecorded: false,
    }
  }
  throw new InvalidRouteGraphError('provenance kind is unsupported')
}

/** Validate the complete closed schema, including recomputed fingerprints and node references. */
export function validateRouteGraph(value: unknown): RouteGraph {
  const graph = exactObject(value, [
    'schemaVersion', 'appId', 'platform', 'buildFingerprint', 'screenFingerprintVersion',
    'validatedAt', 'replayValidatedAt', 'nodes', 'transitions', 'failedBranches', 'provenance',
  ], 'root')
  if (graph.schemaVersion !== ROUTE_GRAPH_SCHEMA_VERSION) {
    throw new InvalidRouteGraphError('schemaVersion is unsupported')
  }
  if (graph.platform !== 'android') throw new InvalidRouteGraphError('platform must be android')
  if (graph.screenFingerprintVersion !== SCREEN_FINGERPRINT_VERSION) {
    throw new InvalidRouteGraphError('screenFingerprintVersion is unsupported')
  }
  const validatedAt = timestamp(graph.validatedAt, 'validatedAt')
  const replayValidatedAt = graph.replayValidatedAt === null
    ? null
    : timestamp(graph.replayValidatedAt, 'replayValidatedAt')
  if (replayValidatedAt !== null && replayValidatedAt < validatedAt) {
    throw new InvalidRouteGraphError('replayValidatedAt cannot precede validatedAt')
  }

  const nodeValues = boundedArray(graph.nodes, 1, MAX_NODES, 'nodes')
  const nodes = nodeValues.map((value, index) => {
    const path = `nodes[${index}]`
    const entry = exactObject(value, ['nodeId', 'observations', 'fingerprint', 'validatedAt'], path)
    const curated = observations(entry.observations, `${path}.observations`)
    const fingerprint = sha256(entry.fingerprint, `${path}.fingerprint`)
    if (fingerprint !== fingerprintScreen(curated)) {
      throw new InvalidRouteGraphError(`${path}.fingerprint does not match its observations`)
    }
    return {
      nodeId: token(entry.nodeId, `${path}.nodeId`),
      observations: curated,
      fingerprint,
      validatedAt: timestamp(entry.validatedAt, `${path}.validatedAt`),
    }
  })
  const nodeIds = new Set(nodes.map(node => node.nodeId))
  if (nodeIds.size !== nodes.length) throw new InvalidRouteGraphError('nodeId values must be unique')

  const transitions = boundedArray(graph.transitions, 0, MAX_TRANSITIONS, 'transitions')
    .map((value, index) => {
      const path = `transitions[${index}]`
      const entry = exactObject(value,
        ['transitionId', 'fromNodeId', 'action', 'toNodeId', 'validatedAt'], path)
      const fromNodeId = token(entry.fromNodeId, `${path}.fromNodeId`)
      const toNodeId = token(entry.toNodeId, `${path}.toNodeId`)
      if (!nodeIds.has(fromNodeId) || !nodeIds.has(toNodeId)) {
        throw new InvalidRouteGraphError(`${path} references an unknown node`)
      }
      return {
        transitionId: token(entry.transitionId, `${path}.transitionId`),
        fromNodeId,
        action: action(entry.action, `${path}.action`),
        toNodeId,
        validatedAt: timestamp(entry.validatedAt, `${path}.validatedAt`),
      }
    })
  const transitionIds = new Set(transitions.map(transition => transition.transitionId))
  if (transitionIds.size !== transitions.length) {
    throw new InvalidRouteGraphError('transitionId values must be unique')
  }
  const uniqueTransitions = new Set(transitions.map(transition => JSON.stringify([
    transition.fromNodeId, transition.action.targetResourceId, transition.toNodeId,
  ])))
  if (uniqueTransitions.size !== transitions.length) {
    throw new InvalidRouteGraphError('duplicate verified transitions are ambiguous')
  }

  const failedBranches = boundedArray(graph.failedBranches, 0, MAX_FAILED_BRANCHES, 'failedBranches')
    .map((value, index) => {
      const path = `failedBranches[${index}]`
      const entry = exactObject(value,
        ['branchId', 'fromNodeId', 'action', 'category', 'failedAt'], path)
      const fromNodeId = token(entry.fromNodeId, `${path}.fromNodeId`)
      if (!nodeIds.has(fromNodeId)) throw new InvalidRouteGraphError(`${path} references an unknown node`)
      if (!['selector', 'navigation', 'product_assertion', 'runner', 'timeout', 'unknown']
        .includes(entry.category as string)) {
        throw new InvalidRouteGraphError(`${path}.category is unsupported`)
      }
      return {
        branchId: token(entry.branchId, `${path}.branchId`),
        fromNodeId,
        action: action(entry.action, `${path}.action`),
        category: entry.category as FailedBranch['category'],
        failedAt: timestamp(entry.failedAt, `${path}.failedAt`),
      }
    })
  const branchIds = new Set(failedBranches.map(branch => branch.branchId))
  if (branchIds.size !== failedBranches.length) {
    throw new InvalidRouteGraphError('branchId values must be unique')
  }

  return {
    schemaVersion: ROUTE_GRAPH_SCHEMA_VERSION,
    appId: token(graph.appId, 'appId'),
    platform: 'android',
    buildFingerprint: sha256(graph.buildFingerprint, 'buildFingerprint'),
    screenFingerprintVersion: SCREEN_FINGERPRINT_VERSION,
    validatedAt,
    replayValidatedAt,
    nodes,
    transitions,
    failedBranches,
    provenance: provenance(graph.provenance),
  }
}

/**
 * Diagnostic-only matching. Duplicate fingerprints are ambiguous; missing expected screens are
 * changed; unmatched screens without an expectation are unknown. None of these calls WalkStore.
 */
export function matchScreen(graphInput: RouteGraph, options: ScreenMatchOptions): ScreenMatch {
  const graph = validateRouteGraph(graphInput)
  const fingerprint = fingerprintScreen(options.observations)
  if (options.expectedNodeId !== undefined) token(options.expectedNodeId, 'expectedNodeId')
  if (options.previousNodeId !== undefined) token(options.previousNodeId, 'previousNodeId')
  const candidates = graph.nodes.filter(node => node.fingerprint === fingerprint)
    .map(node => node.nodeId).sort()
  let status: ScreenMatch['status']
  if (candidates.length > 1) {
    status = 'ambiguous'
  } else if (candidates.length === 0) {
    status = options.expectedNodeId === undefined ? 'unknown' : 'changed'
  } else if (options.expectedNodeId !== undefined && candidates[0] !== options.expectedNodeId) {
    status = 'changed'
  } else if (options.previousNodeId === candidates[0]) {
    status = 'repeated'
  } else {
    status = 'matched'
  }
  return {
    status,
    fingerprint,
    candidateNodeIds: candidates,
    cacheHit: false,
    fallback: status === 'ambiguous' || status === 'changed' || status === 'unknown'
      ? ROUTE_GRAPH_FALLBACK
      : null,
  }
}
