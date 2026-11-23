/*
 * DEJA-18 evidence walk. Drives the Blankaday Dev build step by step through one Maestro
 * transport, observes after each step, and records IDs, fingerprint and timing.
 *
 *   (DEJA_WALK_PARTS=bench runs only the repeated observe/tap benchmark.)
 *   DEJA_MAESTRO=<maestro binary> DEJA_SIM_UDID=<simulator> \
 *     bun spikes/004-ios-observation/walk.ts <process|mcp> [repeats]
 *
 * Writes evidence/walk-<transport>.json. Labels never reach the file (snapshots keep IDs, roles
 * and flags). Run through a dedicated simulator only.
 */
import { loadavg } from 'node:os'
import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import {
  findChromeIds, fingerprintScreenV2, type FingerprintConfig, type Observation, type Segment,
} from '@deja-qa/core'
import {
  iosExplorationDriver, McpMaestroTransport, ProcessMaestroTransport, type MaestroTransport,
} from '@deja-qa/maestro'

const kind = process.argv[2]
const repeats = Number(process.argv[3] ?? 6)
const executable = process.env.DEJA_MAESTRO
const device = process.env.DEJA_SIM_UDID
if ((kind !== 'process' && kind !== 'mcp') || !executable || !device) {
  console.error('usage: DEJA_MAESTRO=… DEJA_SIM_UDID=… bun walk.ts <process|mcp> [repeats]')
  process.exit(2)
}
const APP = 'com.blankaday.dev'
const config: FingerprintConfig = { screenMarkerPattern: '^screen\\.' }

const transport: MaestroTransport = kind === 'mcp'
  ? new McpMaestroTransport({ executable, device, timeoutMs: 300_000 })
  : new ProcessMaestroTransport({ executable, device, timeoutMs: 300_000 })
const driver = iosExplorationDriver({ transport })

type Sample = {
  step: string
  op: 'reset' | 'act' | 'observe'
  ms: number
  load1: number
  passed?: boolean
  failure?: string | null
}
const samples: Sample[] = []
const screens: {
  step: string; marker: string | null; markers: string[]; anchors: string[]; fingerprint: string
  stable: boolean; stableScoped: boolean; fingerprintScoped: string; unstableReason: string | null; markersInDocumentOrder: string[]; keyboardVisible: boolean; systemUi: string | null; truncated: boolean
  elementCount: number; observation: Observation
}[] = []

const tap = (id: string): Segment => ({ label: `tap:${id}`, yaml: `- tapOn:\n    id: ${JSON.stringify(id)}\n` })
const wait = (id: string, timeout = 10000): Segment => ({
  label: `wait:${id}`,
  yaml: `- extendedWaitUntil:\n    visible:\n      id: ${JSON.stringify(id)}\n    timeout: ${timeout}\n`,
})

async function act(step: string, segment: Segment) {
  const result = await driver.act(segment)
  samples.push({
    step, op: 'act', ms: result.durationMs, load1: loadavg()[0], passed: result.passed,
    failure: result.failure?.failingStep ?? null,
  })
  console.log(`${step} act ${result.durationMs}ms ${result.passed ? 'ok' : `FAIL ${result.failure?.failingStep}`}`)
  return result
}

async function observe(step: string, chromeIds: string[] = []) {
  const started = performance.now()
  let observation: Observation
  try {
    observation = await driver.observe()
  } catch (error) {
    samples.push({ step, op: 'observe', ms: Math.round(performance.now() - started), load1: loadavg()[0], passed: false, failure: String(error).slice(0, 200) })
    console.log(`${step} observe FAIL ${String(error).slice(0, 120)}`)
    return null
  }
  samples.push({ step, op: 'observe', ms: observation.durationMs, load1: loadavg()[0], passed: true })
  const fp = fingerprintScreenV2(observation.elements, { ...config, chromeIds })
  screens.push({
    step, marker: fp.marker, markers: fp.markers, anchors: fp.anchors, fingerprint: fp.fingerprint,
    stable: fp.stable, unstableReason: fp.unstableReason,
    ...(() => { const scoped = fingerprintScreenV2(observation.elements, { ...config, markerScope: 'last-marker' }); return { stableScoped: scoped.stable, fingerprintScoped: scoped.fingerprint } })(),
    markersInDocumentOrder: [...new Set(observation.elements.filter(e => !e.overlay && e.id && fp.markers.includes(e.id)).map(e => e.id!))],
    keyboardVisible: observation.keyboardVisible,
    systemUi: observation.systemUi?.kind ?? null, truncated: observation.truncated,
    elementCount: observation.elements.length, observation,
  })
  console.log(`${step} observe ${observation.durationMs}ms markers=${fp.markers.join(',')} anchors=${fp.anchors.length} kb=${observation.keyboardVisible} ui=${observation.systemUi?.kind ?? '-'} ${fp.fingerprint.slice(0, 10)}`)
  return observation
}

const profile = (extra: Record<string, string | boolean>) => ({
  appId: APP, clearState: true, arguments: { uiTesting: true, ...extra },
})

async function resetStep(step: string, extra: Record<string, string | boolean>) {
  const result = await driver.reset(profile(extra), [])
  samples.push({ step, op: 'reset', ms: result.durationMs, load1: loadavg()[0], passed: result.passed, failure: result.failure?.failingStep ?? null })
  console.log(`${step} reset ${result.durationMs}ms ${result.passed ? 'ok' : 'FAIL'}`)
}

const benchOnly = process.env.DEJA_WALK_PARTS === 'bench'
try {
  if (!benchOnly) {
  // Part 1: smoke.yaml's route, observing each screen twice.
  await resetStep('reset:smoke', {})
  await act('wait:today', wait('screen.today'))
  await observe('today#1'); await observe('today#2')
  await act('tap:tab.feed', tap('tab.feed')); await act('wait:feed', wait('screen.feed'))
  await observe('feed#1'); await observe('feed#2')
  await act('tap:tab.journal', tap('tab.journal')); await act('wait:journal', wait('screen.journal'))
  await observe('journal#1'); await observe('journal#2')
  await act('tap:tab.today', tap('tab.today')); await act('wait:today-back', wait('screen.today'))
  await observe('today-back#1'); await observe('today-back#2')

  // Part 2: keyboard on today.answer. The keyboard must not change the fingerprint.
  await act('tap:today.answer', tap('today.answer'))
  await act('wait:space', wait('space', 8000))
  await observe('today-keyboard#1'); await observe('today-keyboard#2')
  await act('key:enter', { label: 'key:enter', yaml: '- pressKey: Enter\n' })
  await observe('today-after-keyboard')

  // Part 3: share sheet (export.yaml's route) on the sample journal.
  await resetStep('reset:history', { '-screenshotState': 'history' })
  for (const id of ['today.you', 'you.settings', 'settings.category.privacyAndData']) {
    await act(`tap:${id}`, tap(id))
  }
  await act('scroll:settings.export', {
    label: 'scroll', yaml: '- scrollUntilVisible:\n    element:\n      id: "settings.export"\n',
  })
  await observe('settings-before-share')
  await act('tap:settings.export', tap('settings.export'))
  await act('tap:export.csv', tap('export.csv'))
  await act('wait:ActivityListView', wait('ActivityListView', 15000))
  await observe('share-sheet#1')
  await act('swipe:close-sheet', { label: 'swipe', yaml: '- swipe:\n    start: 50%, 40%\n    end: 50%, 95%\n' })
  await observe('settings-after-share')

  }
  // Part 4: cost microbenchmark. Repeated observe and repeated tab taps on the live app.
  await resetStep('reset:bench', {})
  await act('bench:wait', wait('screen.today'))
  for (let i = 0; i < repeats; i++) await observe(`bench-observe#${i + 1}`)
  for (let i = 0; i < repeats; i++) {
    await act(`bench-tap#${i + 1}`, tap(i % 2 === 0 ? 'tab.feed' : 'tab.today'))
  }
} finally {
  await transport.close()
}

const chromeIds = findChromeIds(screens.filter(screen => !screen.step.startsWith('bench')))
const withChrome = screens.map(screen => ({
  step: screen.step,
  fingerprintWithChrome: fingerprintScreenV2(screen.observation.elements, { ...config, chromeIds }).fingerprint,
}))
const out = {
  schema: 'deja-18-ios-walk/1',
  transport: benchOnly ? `${kind}-bench` : kind,
  maestro: process.env.DEJA_MAESTRO_LABEL ?? '(unlabelled)',
  device,
  repeats,
  startupMs: kind === 'mcp' ? (transport as McpMaestroTransport).startupMs : null,
  loadAverageAtEnd: loadavg(),
  chromeIds,
  samples,
  screens: screens.map(({ observation, ...rest }) => ({
    ...rest,
    snapshotSha256: observation.snapshot.sha256,
    fingerprintWithChrome: withChrome.find(entry => entry.step === rest.step)!.fingerprintWithChrome,
  })),
}
const dir = join(import.meta.dir, 'evidence')
mkdirSync(dir, { recursive: true })
writeFileSync(join(dir, `walk-${kind}${benchOnly ? '-bench' : ''}.json`), `${JSON.stringify(out, null, 2)}\n`)
console.log(`wrote walk-${kind}${benchOnly ? '-bench' : ''}.json`)
