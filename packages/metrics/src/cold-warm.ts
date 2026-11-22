export type SampleMode = 'cold' | 'warm'

export type MetricSample = {
  schemaVersion: 1
  sampleId: string
  pairId: string
  mode: SampleMode
  startedAt: string
  endedAt: string
  commandWallDurationMs: number
  commandExitCode: number | null
  command: string[]
  conditions: {
    goal: string
    appId: string
    platform: string
    device: string
    deviceState: string
    fixtureMode: 'stable'
    buildPath: string
    buildFingerprint: string
    buildInstalledBeforeSeries: boolean
    emulatorBootIncluded: false
    buildOrInstallIncluded: false
  }
  cacheBefore: {
    status: string
    reason?: string
  }
  metrics: {
    runnerDurationMs: number | null
    lifecycleDurationMs: number | null
    explorationDurationMs: number | null
    verificationWrapperDurationMs: number | null
    mcpCalls: number | null
    mcpCallBasis: string
    modelCalls: number | null
    modelCallBasis: string
    cacheStatus: string
    passed: boolean
  }
  modelInstrumentation: {
    source: 'openclaw-result' | 'direct-cache-hit-contract'
    sourcePath?: string
    sourceSha256?: string
    assistantTurns?: number
    model?: string
    provider?: string | null
  }
  flow: {
    path: string | null
    sha256: string | null
    assertVisibleCount: number
    finalProductOutcomeAssertion: boolean
  }
  artifacts: {
    commandStdoutPath: string
    commandStderrPath: string
    lifecycleReportPath: string
  }
  runReport: Record<string, unknown>
}

export type NumericSummary = {
  values: Array<number | null>
  count: number | null
  min: number | null
  max: number | null
  range: number | null
  mean: number | null
  median: number | null
}

export const METRIC_BOUNDARIES = {
  sharedPreconditions: [
    'The same already-built and already-installed APK, emulator, stable selector fixture, app, and normalized goal are established before the measured series.',
    'Emulator boot, Gradle build, APK installation, and fixture-mode selection are outside both measured commands.',
    'Every executed Maestro flow launches the fixture with clearState: true and asserts the exact Order #WC-1001 product outcome.',
  ],
  cold: {
    start: 'Immediately before spawning bun scripts/walkcache.ts explore checkout --config explore.config.json.',
    end: 'After that CLI process exits, including its final report write.',
    included: [
      'Bun/CLI startup, configuration loading, APK fingerprinting, and attempt setup.',
      'The exploration-phase adb pm clear device setup.',
      'The complete OpenClaw/Kimi exploration, Maestro MCP activity, and candidate authoring.',
      'The verification-phase adb pm clear and independent WALK-002 clearState Maestro execution.',
      'Atomic verified-flow persistence and report writing.',
    ],
    internalTimers: 'runReport.durationMs is the internal lifecycle; explorationDurationMs and verificationDurationMs are nested components, not alternatives to command wall time.',
  },
  warm: {
    start: 'Immediately before spawning bun scripts/walkcache.ts run checkout.',
    end: 'After that CLI process exits, including validation-history persistence and its final report write.',
    included: [
      'Bun/CLI startup, APK fingerprinting, exact cache lookup, and stored-flow loading.',
      'The WALK-002 Maestro process and its flow-level clearState app launch.',
      'Validation-history persistence and report writing.',
    ],
    excluded: ['OpenClaw/model startup and Maestro MCP transport: the direct cache-hit path never invokes either.'],
    internalTimers: 'runReport.durationMs is the nested Maestro runner time only; commandWallDurationMs is the complete warm boundary.',
  },
} as const

function summary(values: Array<number | null>): NumericSummary {
  const present = values.filter((value): value is number => typeof value === 'number' && Number.isFinite(value))
  if (present.length !== values.length || present.length === 0) {
    return { values, count: null, min: null, max: null, range: null, mean: null, median: null }
  }
  const sorted = [...present].sort((a, b) => a - b)
  const middle = Math.floor(sorted.length / 2)
  const median = sorted.length % 2 === 1
    ? sorted[middle]
    : (sorted[middle - 1] + sorted[middle]) / 2
  return {
    values,
    count: present.length,
    min: sorted[0],
    max: sorted[sorted.length - 1],
    range: sorted[sorted.length - 1] - sorted[0],
    mean: Math.round((present.reduce((total, value) => total + value, 0) / present.length) * 1000) / 1000,
    median,
  }
}

function requireFinite(value: unknown, label: string): number {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    throw new Error(`${label} must be a finite number`)
  }
  return value
}

function commonCondition(samples: MetricSample[], key: keyof MetricSample['conditions']): unknown {
  const values = [...new Set(samples.map(sample => sample.conditions[key]))]
  if (values.length !== 1) throw new Error(`Samples are not comparable: ${String(key)} differs`)
  return values[0]
}

function aggregate(samples: MetricSample[]) {
  return {
    sampleIds: samples.map(sample => sample.sampleId),
    pairIds: samples.map(sample => sample.pairId),
    samples: samples.length,
    passing: samples.filter(sample => sample.metrics.passed && sample.commandExitCode === 0).length,
    cacheStatuses: samples.map(sample => sample.metrics.cacheStatus),
    verificationResults: samples.map(sample => sample.metrics.passed),
    commandWallMs: summary(samples.map(sample => requireFinite(sample.commandWallDurationMs, 'commandWallDurationMs'))),
    internalLifecycleMs: summary(samples.map(sample => sample.metrics.lifecycleDurationMs)),
    runnerMs: summary(samples.map(sample => sample.metrics.runnerDurationMs)),
    explorationMs: summary(samples.map(sample => sample.metrics.explorationDurationMs)),
    verificationWrapperMs: summary(samples.map(sample => sample.metrics.verificationWrapperDurationMs)),
    mcpCalls: summary(samples.map(sample => sample.metrics.mcpCalls)),
    modelCalls: summary(samples.map(sample => sample.metrics.modelCalls)),
  }
}

export type ColdWarmComparison = ReturnType<typeof compareColdWarm>

export function compareColdWarm(samples: MetricSample[]) {
  if (samples.length < 4) throw new Error('At least two cold and two warm samples are required')
  const cold = samples.filter(sample => sample.mode === 'cold')
  const warm = samples.filter(sample => sample.mode === 'warm')
  if (cold.length < 2 || warm.length < 2) {
    throw new Error('At least two cold and two warm samples are required')
  }
  const ids = new Set(samples.map(sample => sample.sampleId))
  if (ids.size !== samples.length) throw new Error('Sample IDs must be unique')
  for (const sample of samples) {
    if (!sample.flow.finalProductOutcomeAssertion) {
      throw new Error(`Sample ${sample.sampleId} lacks the exact final product-outcome assertion`)
    }
  }
  const conditions = {
    goal: commonCondition(samples, 'goal'),
    appId: commonCondition(samples, 'appId'),
    platform: commonCondition(samples, 'platform'),
    device: commonCondition(samples, 'device'),
    deviceState: commonCondition(samples, 'deviceState'),
    fixtureMode: commonCondition(samples, 'fixtureMode'),
    buildPath: commonCondition(samples, 'buildPath'),
    buildFingerprint: commonCondition(samples, 'buildFingerprint'),
    buildInstalledBeforeSeries: commonCondition(samples, 'buildInstalledBeforeSeries'),
    emulatorBootIncluded: commonCondition(samples, 'emulatorBootIncluded'),
    buildOrInstallIncluded: commonCondition(samples, 'buildOrInstallIncluded'),
  }
  const coldAggregate = aggregate(cold)
  const warmAggregate = aggregate(warm)
  const allPassed = coldAggregate.passing === cold.length && warmAggregate.passing === warm.length
  const coldWall = coldAggregate.commandWallMs.median
  const warmWall = warmAggregate.commandWallMs.median
  const medianWallDeltaMs = coldWall !== null && warmWall !== null ? coldWall - warmWall : null
  const medianWallRatio = coldWall !== null && warmWall !== null && warmWall > 0
    ? Math.round((coldWall / warmWall) * 1000) / 1000
    : null
  const coldRunner = coldAggregate.runnerMs.median
  const warmRunner = warmAggregate.runnerMs.median
  const medianRunnerDeltaMs = coldRunner !== null && warmRunner !== null ? coldRunner - warmRunner : null

  return {
    schemaVersion: 1,
    metricBoundaries: METRIC_BOUNDARIES,
    comparableConditions: conditions,
    allSamplesPassed: allPassed,
    cold: coldAggregate,
    warm: warmAggregate,
    comparison: {
      medianCommandWallDeltaMs: medianWallDeltaMs,
      medianCommandWallRatioColdOverWarm: medianWallRatio,
      medianRunnerDeltaMs: medianRunnerDeltaMs,
      wallClockReductionSupported: allPassed && medianWallDeltaMs !== null && medianWallDeltaMs > 0,
      modelAvoidanceObserved: cold.every(sample => typeof sample.metrics.modelCalls === 'number' && sample.metrics.modelCalls > 0) &&
        warm.every(sample => sample.metrics.modelCalls === 0),
      interpretation: 'Model avoidance and wall-clock duration are separate outcomes. The observed median delta describes only these controlled samples; ranges retain setup and provider variability. No universal speedup is claimed.',
      setupNoise: {
        coldWallRangeMs: coldAggregate.commandWallMs.range,
        coldExplorationRangeMs: coldAggregate.explorationMs.range,
        coldVerificationRangeMs: coldAggregate.verificationWrapperMs.range,
        coldRunnerRangeMs: coldAggregate.runnerMs.range,
        warmWallRangeMs: warmAggregate.commandWallMs.range,
        warmRunnerRangeMs: warmAggregate.runnerMs.range,
      },
    },
    historicalOnly: {
      source: 'spikes/001-android-walkcache/evidence/run-summary.json',
      samplesMs: [24_824, 24_510],
      excludedFromComparableAggregates: true,
      reason: 'Historical WALK-001 warm runs used earlier/non-series conditions and have no matching full cold command-wall measurement.',
    },
    rawSamples: samples,
  }
}

function milliseconds(value: number | null) {
  return value === null ? 'unknown' : `${value.toLocaleString('en-US')} ms`
}

function tableRow(label: string, cold: NumericSummary, warm: NumericSummary, unit: 'ms' | 'count' = 'ms') {
  const format = (value: number | null) => unit === 'ms'
    ? milliseconds(value)
    : value === null ? 'unknown' : String(value)
  const cell = (summaryValue: NumericSummary) => {
    if (summaryValue.median === null) return 'unknown'
    return `${format(summaryValue.median)} (range ${format(summaryValue.min)}–${format(summaryValue.max)})`
  }
  return `| ${label} | ${cell(cold)} | ${cell(warm)} |`
}

export function renderColdWarmMarkdown(comparison: ColdWarmComparison) {
  const cold = comparison.cold
  const warm = comparison.warm
  const sampleRows = comparison.rawSamples.map(sample => [
    sample.sampleId,
    sample.pairId,
    sample.mode,
    milliseconds(sample.commandWallDurationMs),
    milliseconds(sample.metrics.runnerDurationMs),
    sample.metrics.mcpCalls === null ? 'unknown' : String(sample.metrics.mcpCalls),
    sample.metrics.modelCalls === null ? 'unknown' : String(sample.metrics.modelCalls),
    sample.metrics.cacheStatus,
    sample.metrics.passed && sample.commandExitCode === 0 ? 'PASS' : 'FAIL',
  ].join(' | '))
  const wallDelta = comparison.comparison.medianCommandWallDeltaMs
  const runnerDelta = comparison.comparison.medianRunnerDeltaMs
  return `# WALK-005 — Comparable cold/warm Android metrics

## Verdict

${comparison.allSamplesPassed ? 'All controlled samples passed the unchanged meaningful product assertions.' : 'Not all controlled samples passed; these measurements do not support a performance claim.'}
Warm replay avoided model calls in every warm sample (${warm.modelCalls.values.map(value => value === null ? 'unknown' : value).join(', ')}), while cold
exploration used actual OpenClaw instrumentation (${cold.modelCalls.values.map(value => value === null ? 'unknown' : value).join(', ')} assistant turns).
**Model avoidance is not the same as Maestro execution speed.** The median complete-command wall-time
delta is ${milliseconds(wallDelta)} cold minus warm; the median nested runner-time delta is
${milliseconds(runnerDelta)}. These are observed sample differences, not a universal speedup claim.

## Exact boundaries and equivalent conditions

Both modes used the same APK SHA-256 \`${comparison.comparableConditions.buildFingerprint}\`, goal
\`${String(comparison.comparableConditions.goal)}\`, app \`${String(comparison.comparableConditions.appId)}\`,
device \`${String(comparison.comparableConditions.device)}\`, stable selector fixture, clearState app launch,
and exact \`Order #WC-1001\` outcome assertion. Emulator boot, build, installation, and fixture selection were
completed before the series and are excluded from both command-wall timers.

- **Cold start:** ${comparison.metricBoundaries.cold.start}
- **Cold end:** ${comparison.metricBoundaries.cold.end}
- **Cold includes:** ${comparison.metricBoundaries.cold.included.join(' ')}
- **Warm start:** ${comparison.metricBoundaries.warm.start}
- **Warm end:** ${comparison.metricBoundaries.warm.end}
- **Warm includes:** ${comparison.metricBoundaries.warm.included.join(' ')}

The outer command-wall timer is directly comparable. Internal timers answer different questions: cold
lifecycle includes exploration, verification, and persistence; warm's existing report timer is runner-only.

## Side-by-side aggregates

Median with full observed minimum–maximum range; no best-case selection.

| Metric | Cold (${cold.samples} samples) | Warm (${warm.samples} samples) |
| --- | --- | --- |
${tableRow('Complete command wall', cold.commandWallMs, warm.commandWallMs)}
${tableRow('Nested runner', cold.runnerMs, warm.runnerMs)}
${tableRow('Exploration', cold.explorationMs, warm.explorationMs)}
${tableRow('Independent verification wrapper', cold.verificationWrapperMs, warm.verificationWrapperMs)}
${tableRow('MCP calls', cold.mcpCalls, warm.mcpCalls, 'count')}
${tableRow('Model calls / assistant turns', cold.modelCalls, warm.modelCalls, 'count')}

Mean command wall: cold ${milliseconds(cold.commandWallMs.mean)}, warm ${milliseconds(warm.commandWallMs.mean)}.
Cold wall variability: ${milliseconds(cold.commandWallMs.range)}; warm wall variability:
${milliseconds(warm.commandWallMs.range)}. Cold exploration variability:
${milliseconds(cold.explorationMs.range)}; cold verification-wrapper variability:
${milliseconds(cold.verificationWrapperMs.range)}; nested runner variability: cold
${milliseconds(cold.runnerMs.range)}, warm ${milliseconds(warm.runnerMs.range)}.

## Raw samples

| Sample | Pair | Mode | Command wall | Nested runner | MCP calls | Model calls | Cache | Verification |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
${sampleRows.map(row => `| ${row} |`).join('\n')}

Cold MCP counts are observed \`tools/call\` requests. Cold model counts are OpenClaw
\`assistantTurns\` (completed provider round trips), never inferred from MCP. Warm MCP/model counts are zero
because the direct \`run\` path spawns neither Maestro MCP nor OpenClaw; this is separate from runner duration.

## Historical evidence (not comparable)

The 24.824 s and 24.510 s WALK-001 runs remain historical-only in
\`spikes/001-android-walkcache/evidence/run-summary.json\`. They are excluded from every aggregate above.
`
}
