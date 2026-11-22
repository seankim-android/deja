import { describe, expect, test } from 'bun:test'
import {
  fingerprintScreen,
  InvalidRouteGraphError,
  makeScreenNode,
  matchScreen,
  ROUTE_GRAPH_FALLBACK,
  validateRouteGraph,
  type FailedBranch,
  type RouteGraph,
  type ScreenObservation,
} from '../src/index.ts'

const validatedAt = '2026-11-22T20:27:33.668Z'
const replayValidatedAt = '2026-11-22T20:28:37.591Z'
const buildFingerprint = '6aec5fea876938db14007ba3dbcb91f698832fe855886abd9dea1e7b28861759'
const login: ScreenObservation[] = [{ kind: 'android-resource-id', value: 'screen_login' }]
const catalog: ScreenObservation[] = [{ kind: 'android-resource-id', value: 'screen_catalog' }]
const cart: ScreenObservation[] = [{ kind: 'android-resource-id', value: 'screen_cart' }]

function fixture(): RouteGraph {
  return {
    schemaVersion: 1,
    appId: 'dev.walkcache.checkout',
    platform: 'android',
    buildFingerprint,
    screenFingerprintVersion: 1,
    validatedAt,
    replayValidatedAt,
    nodes: [
      makeScreenNode('login', login, validatedAt),
      makeScreenNode('catalog', catalog, validatedAt),
      makeScreenNode('cart', cart, validatedAt),
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
    ],
    failedBranches: [],
    provenance: {
      kind: 'deterministic-fixture', fixtureId: 'walk017-deterministic-v1',
      newDeviceRunRecorded: false,
    },
  }
}

function invalid(graph: RouteGraph, mutate: (value: RouteGraph) => void) {
  const copy = structuredClone(graph)
  mutate(copy)
  return () => validateRouteGraph(copy)
}

describe('bounded screen fingerprints', () => {
  test('canonicalizes observation order but not screen content', () => {
    const first: ScreenObservation[] = [
      { kind: 'android-resource-id', value: 'screen_cart' },
      { kind: 'android-resource-id', value: 'checkout_button' },
    ]
    expect(fingerprintScreen(first)).toBe(fingerprintScreen([...first].reverse()))
    expect(fingerprintScreen(first)).toMatch(/^[a-f0-9]{64}$/)
    expect(fingerprintScreen(first)).not.toBe(fingerprintScreen(cart))
  })

  test('accepts only bounded resource IDs, never raw visible text or UI dumps', () => {
    expect(() => fingerprintScreen([
      { kind: 'visible-text', value: 'private user content' } as unknown as ScreenObservation,
    ])).toThrow('accepts only curated Android resource IDs')
    expect(() => fingerprintScreen([
      { kind: 'android-resource-id', value: 'screen_login', rawTree: 'secret' } as ScreenObservation,
    ])).toThrow('missing or unsupported fields')
  })

  test('rejects missing, duplicate, malformed, and excessive observations', () => {
    expect(() => fingerprintScreen([])).toThrow('1 to 16 entries')
    expect(() => fingerprintScreen([
      { kind: 'android-resource-id', value: 'screen_login' },
      { kind: 'android-resource-id', value: 'screen_login' },
    ])).toThrow('duplicate observations')
    expect(() => fingerprintScreen([
      { kind: 'android-resource-id', value: 'user@example.com' },
    ])).toThrow('bounded identifier')
    expect(() => fingerprintScreen(Array.from({ length: 17 }, (_, index) => ({
      kind: 'android-resource-id', value: `id_${index}`,
    })))).toThrow('1 to 16 entries')
  })
})

describe('closed route graph schema', () => {
  test('validates a deterministic graph and all verified references', () => {
    const graph = fixture()
    expect(validateRouteGraph(graph)).toEqual(graph)
  })

  test('rejects extra root fields that could carry an unrestricted environment dump', () => {
    expect(invalid(fixture(), graph => {
      Object.assign(graph, { environment: { HOME: '/private' } })
    })).toThrow(InvalidRouteGraphError)
  })

  test('rejects unsupported schema, platform, fingerprint version, and build identity', () => {
    expect(invalid(fixture(), graph => { graph.schemaVersion = 2 as 1 })).toThrow('schemaVersion')
    expect(invalid(fixture(), graph => { graph.platform = 'ios' as 'android' })).toThrow('platform')
    expect(invalid(fixture(), graph => {
      graph.screenFingerprintVersion = 2 as 1
    })).toThrow('screenFingerprintVersion')
    expect(invalid(fixture(), graph => { graph.buildFingerprint = 'abc' })).toThrow('SHA-256')
  })

  test('recomputes node fingerprints instead of trusting supplied values', () => {
    expect(invalid(fixture(), graph => {
      graph.nodes[0].fingerprint = '0'.repeat(64)
    })).toThrow('does not match its observations')
  })

  test('rejects duplicate logical nodes but permits duplicate fingerprints for ambiguity detection', () => {
    expect(invalid(fixture(), graph => {
      graph.nodes.push(structuredClone(graph.nodes[0]))
    })).toThrow('nodeId values must be unique')
    const graph = fixture()
    graph.nodes[1].observations = structuredClone(graph.nodes[0].observations)
    graph.nodes[1].fingerprint = graph.nodes[0].fingerprint
    expect(validateRouteGraph(graph).nodes).toHaveLength(3)
  })

  test('rejects unknown transition endpoints and duplicate successful transitions', () => {
    expect(invalid(fixture(), graph => {
      graph.transitions[0].toNodeId = 'missing'
    })).toThrow('unknown node')
    expect(invalid(fixture(), graph => {
      graph.transitions.push({ ...structuredClone(graph.transitions[0]), transitionId: 'duplicate' })
    })).toThrow('duplicate verified transitions')
  })

  test('rejects unsupported actions and duplicate transition IDs', () => {
    expect(invalid(fixture(), graph => {
      graph.transitions[0].action.kind = 'swipe' as 'tap'
    })).toThrow('only tap actions')
    expect(invalid(fixture(), graph => {
      graph.transitions.push(structuredClone(graph.transitions[0]))
    })).toThrow('transitionId values must be unique')
  })

  test('keeps failed branches separate from verified transitions', () => {
    const graph = fixture()
    graph.failedBranches.push({
      branchId: 'catalog-failed-checkout', fromNodeId: 'catalog',
      action: { kind: 'tap', targetResourceId: 'checkout_button' },
      category: 'selector', failedAt: validatedAt,
    })
    const validated = validateRouteGraph(graph)
    expect(validated.failedBranches).toHaveLength(1)
    expect(validated.transitions.map(transition => transition.transitionId)).toEqual([
      'login-to-catalog', 'catalog-to-cart',
    ])
    expect(validated.transitions.some(transition => transition.action.targetResourceId === 'checkout_button'))
      .toBe(false)
  })

  test('validates failed branch categories, references, timestamps, and unique IDs', () => {
    const failedBranch: FailedBranch = {
      branchId: 'failed', fromNodeId: 'catalog',
      action: { kind: 'tap', targetResourceId: 'add_to_cart' },
      category: 'unknown', failedAt: validatedAt,
    }
    expect(invalid(fixture(), graph => {
      graph.failedBranches.push({ ...failedBranch, category: 'ignored' as 'unknown' })
    })).toThrow('category is unsupported')
    expect(invalid(fixture(), graph => {
      graph.failedBranches.push({ ...failedBranch, fromNodeId: 'missing' })
    })).toThrow('unknown node')
    expect(invalid(fixture(), graph => {
      graph.failedBranches.push({ ...failedBranch, failedAt: 'yesterday' })
    })).toThrow('exact ISO UTC timestamp')
    expect(invalid(fixture(), graph => {
      graph.failedBranches.push(failedBranch, failedBranch)
    })).toThrow('branchId values must be unique')
  })

  test('requires canonical UTC validation timestamps and chronological replay validation', () => {
    expect(invalid(fixture(), graph => { graph.validatedAt = '2026-11-22' })).toThrow('exact ISO UTC')
    expect(invalid(fixture(), graph => {
      graph.replayValidatedAt = '2026-11-22T20:00:00.000Z'
    })).toThrow('cannot precede')
    expect(invalid(fixture(), graph => {
      graph.nodes[0].validatedAt = 'unknown'
    })).toThrow('exact ISO UTC')
  })

  test('bounds curated provenance to declared hashed text artifacts', () => {
    const graph = fixture()
    graph.provenance = {
      kind: 'curated-android-evidence', ticket: 'WALK-020', runId: 'walk020-android-checkout-06',
      newDeviceRunRecorded: false,
      artifacts: [{ path: 'demo/live-evidence/android/cold-run-report.json', sha256: 'a'.repeat(64) }],
    }
    expect(validateRouteGraph(graph).provenance).toEqual(graph.provenance)
    graph.provenance.artifacts[0].path = 'demo/live-evidence/android/contact-sheet.png'
    expect(() => validateRouteGraph(graph)).toThrow('curated relative text evidence')
  })

  test('rejects absolute, escaping, duplicate, or unhashed evidence artifacts', () => {
    const curated = () => {
      const graph = fixture()
      graph.provenance = {
        kind: 'curated-android-evidence', ticket: 'WALK-020', runId: 'walk020-android-checkout-06',
        newDeviceRunRecorded: false,
        artifacts: [{ path: 'evidence/report.json', sha256: 'a'.repeat(64) }],
      }
      return graph
    }
    expect(invalid(curated(), graph => {
      if (graph.provenance.kind === 'curated-android-evidence') {
        graph.provenance.artifacts[0].path = '/private/report.json'
      }
    })).toThrow('curated relative text evidence')
    expect(invalid(curated(), graph => {
      if (graph.provenance.kind === 'curated-android-evidence') {
        graph.provenance.artifacts[0].path = '../report.json'
      }
    })).toThrow('curated relative text evidence')
    expect(invalid(curated(), graph => {
      if (graph.provenance.kind === 'curated-android-evidence') {
        graph.provenance.artifacts[0].sha256 = 'not-a-hash'
      }
    })).toThrow('SHA-256')
    expect(invalid(curated(), graph => {
      if (graph.provenance.kind === 'curated-android-evidence') {
        graph.provenance.artifacts.push(structuredClone(graph.provenance.artifacts[0]))
      }
    })).toThrow('duplicate artifact paths')
  })

  test('requires explicit no-new-device-run provenance', () => {
    expect(invalid(fixture(), graph => {
      graph.provenance.newDeviceRunRecorded = true as false
    })).toThrow('no new device run')
  })
})

describe('conservative diagnostic-only screen matching', () => {
  test('matches a known screen without creating a cache hit or fallback', () => {
    const before = structuredClone(fixture())
    const result = matchScreen(fixture(), { observations: login, expectedNodeId: 'login' })
    expect(result).toEqual({
      status: 'matched', fingerprint: fixture().nodes[0].fingerprint,
      candidateNodeIds: ['login'], cacheHit: false, fallback: null,
    })
    expect(fixture()).toEqual(before)
  })

  test('detects a repeated known screen without treating repetition as a cache hit', () => {
    const result = matchScreen(fixture(), { observations: cart, previousNodeId: 'cart' })
    expect(result.status).toBe('repeated')
    expect(result.cacheHit).toBe(false)
    expect(result.fallback).toBeNull()
  })

  test('detects ambiguous screens even when an expected node is among the candidates', () => {
    const graph = fixture()
    graph.nodes[1].observations = structuredClone(graph.nodes[0].observations)
    graph.nodes[1].fingerprint = graph.nodes[0].fingerprint
    const result = matchScreen(graph, { observations: login, expectedNodeId: 'login' })
    expect(result.status).toBe('ambiguous')
    expect(result.candidateNodeIds).toEqual(['catalog', 'login'])
    expect(result.cacheHit).toBe(false)
    expect(result.fallback).toBe(ROUTE_GRAPH_FALLBACK)
  })

  test('fails closed to existing replay/exploration for a changed expected screen', () => {
    const result = matchScreen(fixture(), {
      observations: [{ kind: 'android-resource-id', value: 'screen_cart_v2' }],
      expectedNodeId: 'cart',
    })
    expect(result.status).toBe('changed')
    expect(result.candidateNodeIds).toEqual([])
    expect(result.cacheHit).toBe(false)
    expect(result.fallback).toBe('existing-exact-replay-or-exploration')
  })

  test('treats a recognized but unexpected screen as changed, not a successful transition', () => {
    const result = matchScreen(fixture(), { observations: cart, expectedNodeId: 'catalog' })
    expect(result.status).toBe('changed')
    expect(result.candidateNodeIds).toEqual(['cart'])
    expect(result.cacheHit).toBe(false)
    expect(result.fallback).toBe(ROUTE_GRAPH_FALLBACK)
  })

  test('fails closed to existing replay/exploration for an unknown screen', () => {
    const result = matchScreen(fixture(), {
      observations: [{ kind: 'android-resource-id', value: 'screen_never_seen' }],
    })
    expect(result.status).toBe('unknown')
    expect(result.candidateNodeIds).toEqual([])
    expect(result.cacheHit).toBe(false)
    expect(result.fallback).toBe(ROUTE_GRAPH_FALLBACK)
  })
})
