import { describe, expect, test } from 'bun:test'
import {
  compareColdWarm,
  renderColdWarmMarkdown,
  type MetricSample,
  type SampleMode,
} from '../src/index.ts'

function sample(mode: SampleMode, sampleId: string, overrides: {
  wall?: number
  runner?: number
  exploration?: number | null
  verification?: number | null
  modelCalls?: number | null
  buildFingerprint?: string
  finalAssertion?: boolean
  passed?: boolean
} = {}): MetricSample {
  const cold = mode === 'cold'
  const modelCalls = overrides.modelCalls === undefined ? (cold ? 12 : 0) : overrides.modelCalls
  return {
    schemaVersion: 1,
    sampleId,
    pairId: sampleId.endsWith('01') || sampleId.endsWith('02') ? 'pair-1' : 'pair-2',
    mode,
    startedAt: '2026-11-22T00:00:00.000Z',
    endedAt: '2026-11-22T00:01:00.000Z',
    commandWallDurationMs: overrides.wall ?? (cold ? 100_000 : 30_000),
    commandExitCode: overrides.passed === false ? 1 : 0,
    command: cold ? ['bun', 'walkcache.ts', 'explore', 'checkout'] : ['bun', 'walkcache.ts', 'run', 'checkout'],
    conditions: {
      goal: 'checkout',
      appId: 'dev.walkcache.checkout',
      platform: 'android',
      device: 'emulator-5554',
      deviceState: 'device',
      fixtureMode: 'stable',
      buildPath: 'app/build/outputs/apk/debug/app-debug.apk',
      buildFingerprint: overrides.buildFingerprint ?? 'same-build',
      buildInstalledBeforeSeries: true,
      emulatorBootIncluded: false,
      buildOrInstallIncluded: false,
    },
    cacheBefore: { status: cold ? 'miss' : 'hit', ...(cold ? { reason: 'not-found' } : {}) },
    metrics: {
      runnerDurationMs: overrides.runner ?? (cold ? 25_000 : 26_000),
      lifecycleDurationMs: cold ? (overrides.wall ?? 100_000) - 500 : null,
      explorationDurationMs: overrides.exploration === undefined ? (cold ? 70_000 : null) : overrides.exploration,
      verificationWrapperDurationMs: overrides.verification === undefined ? (cold ? 25_100 : null) : overrides.verification,
      mcpCalls: cold ? 13 : 0,
      mcpCallBasis: cold ? 'proxy tools/call' : 'no MCP process',
      modelCalls,
      modelCallBasis: modelCalls === null ? 'unknown' : cold ? 'OpenClaw assistantTurns' : 'no model process',
      cacheStatus: cold ? 'recorded' : 'hit',
      passed: overrides.passed ?? true,
    },
    modelInstrumentation: cold
      ? { source: 'openclaw-result', sourceSha256: 'instrumentation-hash', ...(modelCalls !== null ? { assistantTurns: modelCalls } : {}) }
      : { source: 'direct-cache-hit-contract' },
    flow: {
      path: '.walkcache/flows/checkout/android.yaml',
      sha256: 'flow-hash',
      assertVisibleCount: 9,
      finalProductOutcomeAssertion: overrides.finalAssertion ?? true,
    },
    artifacts: {
      commandStdoutPath: `${sampleId}/stdout.log`,
      commandStderrPath: `${sampleId}/stderr.log`,
      lifecycleReportPath: `${sampleId}/report.json`,
    },
    runReport: {},
  }
}

function validSamples() {
  return [
    sample('cold', 'cold-01', { wall: 100_000, runner: 25_000, exploration: 70_000 }),
    sample('warm', 'warm-01', { wall: 29_000, runner: 26_000 }),
    sample('warm', 'warm-02', { wall: 31_000, runner: 28_000 }),
    sample('cold', 'cold-03', { wall: 120_000, runner: 27_000, exploration: 90_000 }),
    sample('warm', 'warm-04', { wall: 33_000, runner: 30_000 }),
    sample('warm', 'warm-05', { wall: 35_000, runner: 32_000 }),
  ]
}

describe('cold/warm comparison aggregation', () => {
  test('retains every raw sample and reports deterministic variability and paired medians', () => {
    const comparison = compareColdWarm(validSamples())
    expect(comparison.cold.samples).toBe(2)
    expect(comparison.warm.samples).toBe(4)
    expect(comparison.cold.commandWallMs).toEqual({
      values: [100_000, 120_000], count: 2, min: 100_000, max: 120_000,
      range: 20_000, mean: 110_000, median: 110_000,
    })
    expect(comparison.warm.commandWallMs).toMatchObject({
      values: [29_000, 31_000, 33_000, 35_000], min: 29_000, max: 35_000,
      range: 6_000, mean: 32_000, median: 32_000,
    })
    expect(comparison.cold.runnerMs).toMatchObject({ min: 25_000, max: 27_000, median: 26_000 })
    expect(comparison.warm.runnerMs).toMatchObject({ min: 26_000, max: 32_000, median: 29_000 })
    expect(comparison.comparison.medianCommandWallDeltaMs).toBe(78_000)
    expect(comparison.comparison.medianRunnerDeltaMs).toBe(-3_000)
    expect(comparison.comparison.wallClockReductionSupported).toBe(true)
    expect(comparison.comparison.modelAvoidanceObserved).toBe(true)
    expect(comparison.comparison.setupNoise.coldExplorationRangeMs).toBe(20_000)
    expect(comparison.historicalOnly.samplesMs).toEqual([24_824, 24_510])
    expect(comparison.historicalOnly.excludedFromComparableAggregates).toBe(true)
    expect(comparison.rawSamples.map(item => item.sampleId)).toEqual([
      'cold-01', 'warm-01', 'warm-02', 'cold-03', 'warm-04', 'warm-05',
    ])
  })

  test('does not turn missing model instrumentation into zero or infer it from MCP calls', () => {
    const samples = validSamples()
    samples[0] = sample('cold', 'cold-01', { modelCalls: null })
    const comparison = compareColdWarm(samples)
    expect(comparison.cold.modelCalls).toEqual({
      values: [null, 12], count: null, min: null, max: null, range: null, mean: null, median: null,
    })
    expect(comparison.cold.mcpCalls.median).toBe(13)
    expect(comparison.comparison.modelAvoidanceObserved).toBe(false)
    expect(renderColdWarmMarkdown(comparison)).toContain('unknown, 12 assistant turns')
  })

  test('rejects incomparable builds and missing meaningful outcome assertions', () => {
    const differentBuild = validSamples()
    differentBuild[1] = sample('warm', 'warm-01', { buildFingerprint: 'different-build' })
    expect(() => compareColdWarm(differentBuild)).toThrow('buildFingerprint differs')

    const weakened = validSamples()
    weakened[0] = sample('cold', 'cold-01', { finalAssertion: false })
    expect(() => compareColdWarm(weakened)).toThrow('lacks the exact final product-outcome assertion')
  })

  test('failed samples remain visible and block a wall-clock reduction claim', () => {
    const samples = validSamples()
    samples[0] = sample('cold', 'cold-01', { passed: false })
    const comparison = compareColdWarm(samples)
    expect(comparison.allSamplesPassed).toBe(false)
    expect(comparison.cold.verificationResults).toEqual([false, true])
    expect(comparison.comparison.wallClockReductionSupported).toBe(false)
    const markdown = renderColdWarmMarkdown(comparison)
    expect(markdown).toContain('Not all controlled samples passed')
    expect(markdown).toContain('| cold-01 | pair-1 | cold |')
    expect(markdown).toContain('FAIL')
    expect(markdown).toContain('Historical evidence (not comparable)')
  })
})
