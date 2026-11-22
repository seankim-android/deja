import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import {
  matchScreen,
  validateRouteGraph,
  type RouteGraph,
} from '../src/index.ts'
import { deriveWalk017AndroidGraph } from '../../../scripts/derive-walk017-route-graph.ts'

const graphPath = 'spikes/001-android-walkcache/evidence/walk017-route-graph.json'

async function curatedGraph() {
  return validateRouteGraph(JSON.parse(await readFile(resolve(process.cwd(), graphPath), 'utf8')))
}

describe('WALK-017 curated Android evidence (not a new device run)', () => {
  test('re-derives the four-node graph only from accepted WALK-020 cold/warm evidence', async () => {
    const [stored, derived] = await Promise.all([curatedGraph(), deriveWalk017AndroidGraph()])
    expect(stored).toEqual(derived)
    expect(stored.nodes.map(node => node.nodeId)).toEqual([
      'login', 'catalog', 'cart', 'confirmation',
    ])
    expect(stored.transitions.map(transition => transition.transitionId)).toEqual([
      'login-to-catalog', 'catalog-to-cart', 'cart-to-confirmation',
    ])
    expect(stored.validatedAt).toBe('2026-11-22T20:27:33.668Z')
    expect(stored.replayValidatedAt).toBe('2026-11-22T20:28:37.591Z')
    expect(stored.provenance).toMatchObject({
      kind: 'curated-android-evidence', ticket: 'WALK-020',
      runId: 'walk020-android-checkout-06', newDeviceRunRecorded: false,
    })
  })

  test('contains only bounded resource IDs, hashed text provenance, and no raw flow inputs/screenshots', async () => {
    const graph = await curatedGraph()
    const serialized = JSON.stringify(graph)
    expect(graph.nodes.every(node => node.observations.every(observation =>
      observation.kind === 'android-resource-id' && observation.value.startsWith('screen_'),
    ))).toBe(true)
    expect(serialized).not.toContain('inputText')
    expect(serialized).not.toContain('qa@walkcache.dev')
    expect(serialized).not.toContain('testing123')
    expect(serialized).not.toMatch(/\.(png|jpg|jpeg|mp4)/)
  })

  test('changed and unknown observations of the evidence graph never authorize a cache hit', async () => {
    const graph: RouteGraph = await curatedGraph()
    const changed = matchScreen(graph, {
      observations: [{ kind: 'android-resource-id', value: 'screen_cart_changed' }],
      expectedNodeId: 'cart',
    })
    const unknown = matchScreen(graph, {
      observations: [{ kind: 'android-resource-id', value: 'screen_unrecorded' }],
    })
    expect(changed).toMatchObject({
      status: 'changed', cacheHit: false, fallback: 'existing-exact-replay-or-exploration',
    })
    expect(unknown).toMatchObject({
      status: 'unknown', cacheHit: false, fallback: 'existing-exact-replay-or-exploration',
    })
  })
})
