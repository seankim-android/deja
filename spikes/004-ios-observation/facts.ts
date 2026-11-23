/*
 * DEJA-18 extra iOS facts (design review of PR #7). One short script, process transport.
 *   DEJA_MAESTRO=… DEJA_SIM_UDID=… bun spikes/004-ios-observation/facts.ts
 * Writes evidence/facts.json. It only touches the simulator named by DEJA_SIM_UDID.
 */
import { loadavg } from 'node:os'
import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { spawnSync } from 'node:child_process'
import { parseMaestroHierarchy, ProcessMaestroTransport } from '@deja-qa/maestro'

const executable = process.env.DEJA_MAESTRO
const device = process.env.DEJA_SIM_UDID
if (!executable || !device) { console.error('set DEJA_MAESTRO and DEJA_SIM_UDID'); process.exit(2) }
const APP = 'com.blankaday.dev'
const transport = new ProcessMaestroTransport({ executable, device, timeoutMs: 300_000 })
const facts: Record<string, unknown> = {}
const log = (key: string, value: unknown) => { facts[key] = value; console.log(key, JSON.stringify(value)) }

const header = `appId: ${APP}\n---\n`
const launch = (extra = '') => `${header}- launchApp:\n    clearState: true\n    arguments:\n      uiTesting: true\n${extra}`
const flow = async (body: string) => {
  const r = await transport.runFlow(`${header}${body}`)
  return { passed: r.passed, ms: r.durationMs, load1: loadavg()[0], note: r.passed ? '' : r.output.split('\n').filter(line => /FAILED|not found|not visible|Assertion|Error/i.test(line)).slice(-2).join(' | ').replace(/\/Users\/[^/\s]+\//g, '~/').slice(0, 300) || 'failed (no detail kept)' }
}
const has = async (id: string) => parseMaestroHierarchy((await transport.hierarchy()).raw).some(e => e.rawId === id)

await transport.runFlow(launch())
await transport.runFlow(`${header}- extendedWaitUntil:\n    visible:\n      id: "screen.today"\n    timeout: 15000\n`)

// 1. Animation settle. Baseline: a trivial assertVisible costs one process start.
const baseline: unknown[] = []; const settle: unknown[] = []
for (let i = 0; i < 3; i++) {
  baseline.push(await flow('- assertVisible:\n    id: "screen.today"\n'))
  settle.push(await flow('- waitForAnimationToEnd:\n    timeout: 10000\n'))
}
log('1_waitForAnimationToEnd_on_today', { baseline, settle })

// 3. Duplicate IDs and selector semantics.
const raw = parseMaestroHierarchy((await transport.hierarchy()).raw)
const counts = new Map<string, number>()
for (const element of raw) if (element.rawId) counts.set(element.rawId, (counts.get(element.rawId) ?? 0) + 1)
log('3_duplicate_ids_on_today', {
  idsWithAnId: [...counts.values()].reduce((a, b) => a + b, 0), distinctIds: counts.size,
  duplicated: Object.fromEntries([...counts].filter(([, n]) => n > 1)),
})
log('3_selector_probes', {
  literal_id_matches: await flow('- assertVisible:\n    id: "screen.today"\n'),
  dot_matches_any_char: await flow('- assertVisible:\n    id: "screen.tod.y"\n'),
  wildcard_pattern: await flow('- assertVisible:\n    id: "scr.*"\n'),
  partial_is_not_a_match: await flow('- assertVisible:\n    id: "screen"\n'),
  escaped_dot_matches_literal: await flow('- assertVisible:\n    id: "screen\\\\.today"\n'),
})

// 6. Keyboard dismissal.
const dismissals: Record<string, string> = {
  pressKey_Enter: '- pressKey: Enter\n',
  hideKeyboard: '- hideKeyboard\n',
  tap_outside: '- tapOn:\n    point: "50%, 12%"\n',
}
const keyboard: Record<string, unknown> = {}
for (const [name, body] of Object.entries(dismissals)) {
  await transport.runFlow(launch())
  await transport.runFlow(`${header}- tapOn:\n    id: "today.answer"\n- extendedWaitUntil:\n    visible:\n      id: "space"\n    timeout: 10000\n`)
  const shown = await has('space')
  const result = await flow(body)
  keyboard[name] = { keyboardShownBefore: shown, step: result, keyboardGoneAfter: !(await has('space')) }
}
log('6_keyboard_dismissal', keyboard)

// 4. Alerts. The in-app alert (empty export) is measured; a SpringBoard permission sheet is not.
await transport.runFlow(launch())
for (const id of ['today.you', 'you.settings', 'settings.category.privacyAndData']) {
  await transport.runFlow(`${header}- tapOn:\n    id: ${JSON.stringify(id)}\n`)
}
await transport.runFlow(`${header}- scrollUntilVisible:\n    element:\n      id: "settings.export"\n`)
await transport.runFlow(`${header}- tapOn:\n    id: "settings.export"\n`)
const alertElements = parseMaestroHierarchy((await transport.hierarchy()).raw)
log('4_in_app_alert', {
  emptyOKInHierarchy: alertElements.some(e => e.rawId === 'export.emptyOK'),
  systemPermissionSheet: 'not measured: Blankaday in uiTesting mode shows none, and no dedicated trigger was built',
})
await transport.runFlow(`${header}- tapOn:\n    id: "export.emptyOK"\n`)

// 5. Crash. Send SIGSEGV to the app process inside this simulator only.
await transport.runFlow(launch())
await transport.runFlow(`${header}- extendedWaitUntil:\n    visible:\n      id: "screen.today"\n    timeout: 15000\n`)
const listing = spawnSync('xcrun', ['simctl', 'spawn', device, 'launchctl', 'list'], { encoding: 'utf8' }).stdout
const pid = listing.split('\n').map(line => line.split(/\s+/)).find(parts => parts[2]?.includes(`UIKitApplication:${APP}`))?.[0]
if (pid && /^\d+$/.test(pid)) {
  spawnSync('xcrun', ['simctl', 'spawn', device, 'kill', '-SEGV', pid])
  await new Promise(resolve => setTimeout(resolve, 3000))
  let afterHierarchy: unknown
  try {
    const elements = parseMaestroHierarchy((await transport.hierarchy()).raw)
    afterHierarchy = { ok: true, ids: elements.filter(e => e.rawId).length, hasToday: elements.some(e => e.rawId === 'screen.today') }
  } catch (error) { afterHierarchy = { ok: false, error: String(error).slice(0, 300) } }
  log('5_crash', {
    signalSent: 'SIGSEGV to the Blankaday Dev process in the dedicated simulator',
    hierarchyAfter: afterHierarchy,
    assertAfter: await flow('- assertVisible:\n    id: "screen.today"\n'),
    tapAfter: await flow('- tapOn:\n    id: "tab.feed"\n'),
  })
} else log('5_crash', 'not measured: could not find the app process')
await transport.close()
const dir = join(import.meta.dir, 'evidence')
mkdirSync(dir, { recursive: true })
writeFileSync(join(dir, 'facts.json'), `${JSON.stringify({ schema: 'deja-18-ios-facts/1', maestro: process.env.DEJA_MAESTRO_LABEL ?? '', device, loadAverageAtEnd: loadavg(), facts }, null, 2)}\n`)
