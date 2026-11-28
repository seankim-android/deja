#!/usr/bin/env bun
import { createHash } from 'node:crypto'
import { readFile, writeFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import {
  makeScreenNode,
  validateRouteGraph,
  type RouteGraph,
} from '@deja-run/core'

const coldYamlPath = 'demo/live-evidence/android/cold-saved-checkout.yaml'
const warmYamlPath = 'demo/live-evidence/android/warm-saved-checkout.yaml'
const coldMemoryPath = 'demo/live-evidence/android/cold-stored-memory.json'
const coldReportPath = 'demo/live-evidence/android/cold-run-report.json'
const warmReportPath = 'demo/live-evidence/android/warm-run-report.json'
const graphPath = 'spikes/001-android-deja/evidence/walk017-route-graph.json'

function sha256(contents: string | Buffer) {
  return createHash('sha256').update(contents).digest('hex')
}

function object(value: unknown, path: string): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new Error(`${path} is not an object`)
  }
  return value as Record<string, unknown>
}

function required(value: unknown, expected: unknown, path: string) {
  if (value !== expected) throw new Error(`${path} does not match the accepted WALK-020 evidence`)
}

async function json(root: string, path: string) {
  return JSON.parse(await readFile(resolve(root, path), 'utf8')) as unknown
}

/** Derive only the four root screens and three taps directly attested by the accepted full flow. */
export async function deriveWalk017AndroidGraph(root = process.cwd()): Promise<RouteGraph> {
  const coldYaml = await readFile(resolve(root, coldYamlPath), 'utf8')
  const warmYaml = await readFile(resolve(root, warmYamlPath), 'utf8')
  if (coldYaml !== warmYaml) throw new Error('Cold and warm curated YAML are not byte-identical')
  const flowSha256 = sha256(coldYaml)
  const resourceIds = [...coldYaml.matchAll(/^    id: ([a-z0-9_]+)$/gm)].map(match => match[1])
  const attestedIds = [
    'screen_login', 'username_input', 'password_input', 'login_button',
    'username_input', 'password_input', 'login_button',
    'screen_catalog', 'add_to_cart', 'add_to_cart',
    'screen_cart', 'checkout_button', 'checkout_button',
    'order_confirmation', 'screen_confirmation',
  ]
  if (JSON.stringify(resourceIds) !== JSON.stringify(attestedIds)) {
    throw new Error('Curated YAML no longer has the exact accepted WALK-020 observation sequence')
  }

  const coldMemory = object(await json(root, coldMemoryPath), 'cold memory')
  const coldReport = object(await json(root, coldReportPath), 'cold report')
  const warmReport = object(await json(root, warmReportPath), 'warm report')
  const coldIdentity = object(coldReport.identity, 'cold identity')
  const warmIdentity = object(warmReport.identity, 'warm identity')
  const warmExecution = object(warmReport.execution, 'warm execution')
  const lastValidation = object(coldMemory.lastValidation, 'cold lastValidation')
  required(coldReport.command, 'explore', 'cold command')
  required(coldReport.outcome, 'cold-recorded', 'cold outcome')
  required(coldReport.passed, true, 'cold passed')
  required(warmReport.command, 'run', 'warm command')
  required(warmReport.outcome, 'passed', 'warm outcome')
  required(warmReport.cacheStatus, 'hit', 'warm exact cache status')
  required(warmReport.passed, true, 'warm passed')
  required(warmReport.modelCalls, 0, 'warm model calls')
  required(warmExecution.passed, true, 'warm execution passed')
  required(lastValidation.passed, true, 'independent cold validation')
  required(coldMemory.flowSha256, flowSha256, 'stored flow SHA-256')
  required(coldReport.yamlSha256, flowSha256, 'cold report flow SHA-256')
  required(warmReport.yamlSha256, flowSha256, 'warm report flow SHA-256')
  required(coldIdentity.appId, 'dev.walkcache.checkout', 'app identity')
  required(coldIdentity.platform, 'android', 'platform identity')
  required(warmIdentity.appId, coldIdentity.appId, 'warm app identity')
  required(warmIdentity.platform, coldIdentity.platform, 'warm platform identity')
  required(warmIdentity.buildFingerprint, coldIdentity.buildFingerprint, 'warm build identity')
  required(coldMemory.buildFingerprint, coldIdentity.buildFingerprint, 'stored build identity')
  if (typeof coldIdentity.buildFingerprint !== 'string' ||
      typeof lastValidation.at !== 'string' || typeof warmExecution.endedAt !== 'string') {
    throw new Error('Accepted evidence is missing build or validation identity')
  }

  const validatedAt = lastValidation.at
  const graph: RouteGraph = {
    schemaVersion: 1,
    appId: 'dev.walkcache.checkout',
    platform: 'android',
    buildFingerprint: coldIdentity.buildFingerprint,
    screenFingerprintVersion: 1,
    validatedAt,
    replayValidatedAt: warmExecution.endedAt,
    nodes: [
      makeScreenNode('login', [
        { kind: 'android-resource-id', value: 'screen_login' },
      ], validatedAt),
      makeScreenNode('catalog', [
        { kind: 'android-resource-id', value: 'screen_catalog' },
      ], validatedAt),
      makeScreenNode('cart', [
        { kind: 'android-resource-id', value: 'screen_cart' },
      ], validatedAt),
      makeScreenNode('confirmation', [
        { kind: 'android-resource-id', value: 'screen_confirmation' },
      ], validatedAt),
    ],
    transitions: [
      {
        transitionId: 'login-to-catalog', fromNodeId: 'login',
        action: { kind: 'tap', targetResourceId: 'login_button' },
        toNodeId: 'catalog', validatedAt,
      },
      {
        transitionId: 'catalog-to-cart', fromNodeId: 'catalog',
        action: { kind: 'tap', targetResourceId: 'add_to_cart' },
        toNodeId: 'cart', validatedAt,
      },
      {
        transitionId: 'cart-to-confirmation', fromNodeId: 'cart',
        action: { kind: 'tap', targetResourceId: 'checkout_button' },
        toNodeId: 'confirmation', validatedAt,
      },
    ],
    failedBranches: [],
    provenance: {
      kind: 'curated-android-evidence',
      ticket: 'WALK-020',
      runId: 'walk020-android-checkout-06',
      newDeviceRunRecorded: false,
      artifacts: await Promise.all([
        coldYamlPath, warmYamlPath, coldMemoryPath, coldReportPath, warmReportPath,
      ].map(async path => ({ path, sha256: sha256(await readFile(resolve(root, path), 'utf8')) }))),
    },
  }
  return validateRouteGraph(graph)
}

if (import.meta.main) {
  const mode = process.argv[2]
  if (mode !== '--check' && mode !== '--write') {
    throw new Error('Use --check to compare curated evidence or --write to regenerate it')
  }
  const graph = await deriveWalk017AndroidGraph()
  const serialized = `${JSON.stringify(graph, null, 2)}\n`
  if (mode === '--write') {
    await writeFile(resolve(process.cwd(), graphPath), serialized)
  } else {
    const existing = await readFile(resolve(process.cwd(), graphPath), 'utf8')
    if (existing !== serialized) {
      throw new Error(`${graphPath} is stale or differs from the accepted WALK-020 source evidence`)
    }
  }
  console.log(JSON.stringify({
    result: 'pass',
    graphPath,
    schemaValidation: 'pass',
    sourceProvenance: 'existing curated WALK-020 cold/warm evidence',
    newDeviceRunRecorded: false,
    nodes: graph.nodes.length,
    verifiedTransitions: graph.transitions.length,
    failedBranches: graph.failedBranches.length,
    validatedAt: graph.validatedAt,
    replayValidatedAt: graph.replayValidatedAt,
  }, null, 2))
}
