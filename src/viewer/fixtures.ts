import type { RunResult } from '../../packages/core/src/contracts.ts'
import type { ViewerHistory, ViewerReportEntry } from '../../viewer/contracts.ts'

export type FixtureName = 'cold' | 'warm' | 'repair' | 'regression'

export type ViewerFixture = {
  name: FixtureName
  label: string
  entry: ViewerReportEntry
  artifacts: Record<string, string>
  history: ViewerHistory
}

const identity = {
  goal: 'checkout',
  appId: 'dev.walkcache.checkout',
  platform: 'android',
  flowSchemaVersion: 'maestro/v1',
  buildFingerprint: '1de8864ea046b64d124ece93ca97b0ae69d11d1fc3d39a89ff405f0e2dd2262d',
}
const coldSha = 'a04eb69edc7081f24b5e8a11c5ef573d398724ba9b5619d37519e41f1a0b6c72'
const repairedSha = 'be212808db3b372a6178a8f6ef03ca8894342b0edb021ab8cb9283c7319da07c'
const coldYamlPath = '.walkcache/flows/checkout/fixture-checkout.yaml'
const repairedYamlPath = '.walkcache/flows/checkout/fixture-repaired-checkout.yaml'
const repairDiffPath = '.walkcache/patches/fixture-repair/proposed.diff'
const coldYaml = `appId: dev.walkcache.checkout
---
- launchApp:
    clearState: true
- assertVisible: WalkCache Shop
- tapOn: Add to cart
- tapOn: Checkout
- assertVisible: Order confirmed
`
const repairedYaml = `appId: dev.walkcache.checkout
---
- launchApp:
    clearState: true
- assertVisible: WalkCache Shop
- tapOn:
    id: add_to_cart
- tapOn:
    id: checkout_button
- assertVisible: Order confirmed
`
const repairDiff = `diff --git a/fixture-app/src/main/CheckoutActivity.kt b/fixture-app/src/main/CheckoutActivity.kt
--- a/fixture-app/src/main/CheckoutActivity.kt
+++ b/fixture-app/src/main/CheckoutActivity.kt
@@ -18,7 +18,7 @@
-        contentDescription = "Add"
+        contentDescription = "Add to cart"
@@ -31,7 +31,7 @@
-        testTag = "continue"
+        testTag = "checkout_button"
`
const base = {
  schemaVersion: 1 as const,
  command: 'run' as const,
  identity,
  goalKey: 'checkout',
  startedAt: '2026-11-22T07:30:00.000Z',
  modelCallBasis: '',
  yamlSha256: coldSha,
  failure: null,
  errors: [],
}

function entry(name: FixtureName, report: RunResult): ViewerReportEntry {
  return {
    path: `fixture://${name}`,
    state: 'ready',
    report,
    message: null,
    modifiedAt: report.endedAt,
  }
}

const coldReport: RunResult = {
  ...base,
  cacheStatus: 'miss',
  cacheMissReason: 'not-found',
  outcome: 'cold-recorded',
  passed: true,
  endedAt: '2026-11-22T07:31:35.499Z',
  durationMs: 95_499,
  modelCalls: 13,
  modelCallBasis: 'fixture: completed exploration round trips',
  assertions: 11,
  repairCount: 0,
  yamlPath: coldYamlPath,
  artifacts: [
    { kind: 'yaml', path: coldYamlPath, sha256: coldSha },
    { kind: 'model-metrics', path: '.walkcache/reports/fixture-cold/model-metrics.json' },
    { kind: 'output', path: '.walkcache/reports/fixture-cold/output.log' },
  ],
  diffs: [],
}

const warmReport: RunResult = {
  ...base,
  cacheStatus: 'hit',
  outcome: 'passed',
  passed: true,
  endedAt: '2026-11-22T07:30:24.662Z',
  durationMs: 24_662,
  modelCalls: 0,
  modelCallBasis: 'exact local hit: explorer and model factories were not invoked',
  assertions: 12,
  repairCount: 0,
  yamlPath: coldYamlPath,
  artifacts: [
    { kind: 'yaml', path: coldYamlPath, sha256: coldSha },
    { kind: 'output', path: '.walkcache/reports/fixture-warm/output.log' },
  ],
  diffs: [],
}

const repairReport: RunResult = {
  ...base,
  identity: { ...identity, buildFingerprint: '2ef9975fb157c75e135fd04e51ad7cc6f95ef02ce94f16bc4ff05c9d2261ca' },
  cacheStatus: 'miss',
  cacheMissReason: 'stale-build',
  outcome: 'repaired',
  passed: true,
  startedAt: '2026-11-22T07:40:00.000Z',
  endedAt: '2026-11-22T07:41:05.865Z',
  durationMs: 65_865,
  modelCalls: 0,
  modelCallBasis: 'bounded deterministic selector policy; no model client invoked',
  assertions: 12,
  repairCount: 1,
  yamlPath: repairedYamlPath,
  yamlSha256: repairedSha,
  failure: {
    category: 'selector', advisory: true, repairEligible: true,
    failingStep: 'tapOn: Add to cart', rawArtifactPath: null,
  },
  artifacts: [
    { kind: 'yaml', path: repairedYamlPath, sha256: repairedSha },
    { kind: 'diff', path: repairDiffPath },
    { kind: 'repair-report', path: '.walkcache/patches/fixture-repair/repair-report.json' },
  ],
  diffs: [{
    path: repairDiffPath,
    affectedFiles: ['fixture-app/src/main/CheckoutActivity.kt'],
    contents: repairDiff,
    autoCommit: false,
  }],
}

const regressionReport: RunResult = {
  ...base,
  cacheStatus: 'hit',
  outcome: 'product-regression',
  passed: false,
  endedAt: '2026-11-22T07:30:23.012Z',
  durationMs: 23_012,
  modelCalls: 0,
  modelCallBasis: 'exact local hit: no repair or model invocation after product failure',
  assertions: 11,
  repairCount: 0,
  yamlPath: coldYamlPath,
  failure: {
    category: 'product_assertion', advisory: true, repairEligible: false,
    failingStep: 'assertVisible: Order confirmed',
    rawArtifactPath: '.walkcache/reports/fixture-regression/output.log',
  },
  artifacts: [
    { kind: 'yaml', path: coldYamlPath, sha256: coldSha },
    { kind: 'output', path: '.walkcache/reports/fixture-regression/output.log' },
  ],
  diffs: [],
  errors: ['Meaningful product assertion failed; repair was not attempted'],
}

function history(entries: ViewerHistory['entries']): ViewerHistory {
  return { state: 'ready', entries, message: null }
}

export const fixtures: Record<FixtureName, ViewerFixture> = {
  cold: {
    name: 'cold', label: 'Cold discovery', entry: entry('cold', coldReport),
    artifacts: {
      [coldYamlPath]: coldYaml,
      '.walkcache/reports/fixture-cold/model-metrics.json': JSON.stringify({
        explorationMs: 70_220, verificationMs: 25_131, modelCalls: 13, source: 'fixture-only',
      }, null, 2),
    },
    history: history([
      { passed: true, at: coldReport.endedAt, flowSha256: coldSha },
    ]),
  },
  warm: {
    name: 'warm', label: 'Warm replay', entry: entry('warm', warmReport),
    artifacts: { [coldYamlPath]: coldYaml },
    history: history([
      { passed: true, at: coldReport.endedAt, flowSha256: coldSha },
      { passed: true, at: warmReport.endedAt, flowSha256: coldSha },
    ]),
  },
  repair: {
    name: 'repair', label: 'Bounded repair', entry: entry('repair', repairReport),
    artifacts: {
      [repairedYamlPath]: repairedYaml,
      [repairDiffPath]: repairDiff,
      '.walkcache/patches/fixture-repair/repair-report.json': JSON.stringify({
        status: 'repaired', attempts: 1, independentlyVerified: true, autoCommit: false,
      }, null, 2),
    },
    history: history([
      { passed: true, at: coldReport.endedAt, flowSha256: coldSha },
      { passed: false, at: '2026-11-22T07:39:00.000Z', flowSha256: coldSha },
      { passed: true, at: repairReport.endedAt, flowSha256: repairedSha },
    ]),
  },
  regression: {
    name: 'regression', label: 'Product regression', entry: entry('regression', regressionReport),
    artifacts: { [coldYamlPath]: coldYaml },
    history: history([
      { passed: true, at: warmReport.endedAt, flowSha256: coldSha },
      { passed: false, at: regressionReport.endedAt, flowSha256: coldSha },
    ]),
  },
}

export function fixtureFromSearch(search: string): FixtureName | null {
  const value = new URLSearchParams(search).get('fixture')
  return value === 'cold' || value === 'warm' || value === 'repair' || value === 'regression'
    ? value : null
}
