import { describe, expect, test } from 'bun:test'
import { existsSync } from 'node:fs'
import { chmod, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fingerprintScreenV2 } from '@deja-qa/core'
import {
  McpMaestroTransport,
  ProcessMaestroTransport,
  iosExplorationDriver,
  iosFingerprintRules,
  escapeRegex,
  idSelector,
  tapSegment,
  parseMaestroHierarchy,
  type MaestroFlowResult,
  type MaestroTransport,
} from '../src/index.ts'

const attributes = (id: string, label = '', bounds = '[0,0][10,10]') => ({
  attributes: {
    accessibilityText: label, title: '', value: '', text: '', hintText: '', 'resource-id': id,
    bounds, enabled: 'true', focused: 'false', selected: 'false', checked: 'false',
  },
  children: [],
})
const tree = (...children: unknown[]) => ({ attributes: { bounds: '[0,0][402,874]' }, children })

function transportFor(raw: () => unknown, flows: string[] = []): MaestroTransport {
  const ok: MaestroFlowResult = { passed: true, exitCode: 0, output: '', durationMs: 2, timedOut: false }
  return {
    kind: 'process',
    hierarchy: async () => ({ raw: raw(), durationMs: 3 }),
    runFlow: async yaml => { flows.push(yaml); return ok },
    close: async () => {},
  }
}

describe('parseMaestroHierarchy', () => {
  test('reads the full `maestro hierarchy` shape', () => {
    const elements = parseMaestroHierarchy(tree(attributes('screen.today', 'Today'), attributes('', 'Just text')))
    expect(elements.map(e => [e.rawId, e.label])).toEqual([['screen.today', 'Today'], [null, 'Just text']])
    expect(elements[0].bounds).toEqual([0, 0, 10, 10])
  })

  test('reads the MCP `inspect_screen` shape with schema defaults', () => {
    const elements = parseMaestroHierarchy({
      ui_schema: { defaults: { enabled: true, focused: false, selected: false, checked: false } },
      elements: [{ b: '[0,0][402,874]', c: [{ b: '[1,2][3,4]', rid: 'tab.feed', a11y: 'Feed', enabled: false }] }],
    })
    expect(elements).toHaveLength(1)
    expect(elements[0]).toMatchObject({ rawId: 'tab.feed', label: 'Feed', enabled: false, role: 'tab', bounds: [1, 2, 3, 4] })
  })

  test('both shapes give the same fingerprint for the same screen', () => {
    const full = parseMaestroHierarchy(tree(attributes('screen.today'), attributes('today.answer')))
    const abbreviated = parseMaestroHierarchy({
      ui_schema: { defaults: {} },
      elements: [{ c: [{ rid: 'screen.today' }, { rid: 'today.answer' }] }],
    })
    expect(full.map(e => e.rawId)).toEqual(abbreviated.map(e => e.rawId))
  })
})

describe('iOS fingerprint rules', () => {
  test('keep code-like IDs; drop labels in the ID slot and system containers', () => {
    expect(iosFingerprintRules.normalizeId('screen.today')).toBe('screen.today')
    expect(iosFingerprintRules.normalizeId('Toolbar')).toBeNull()
    expect(iosFingerprintRules.normalizeId('Fill in today’s blank')).toBeNull()
    expect(iosFingerprintRules.normalizeId('  ')).toBeNull()
    expect(iosFingerprintRules.normalizeId('BackButton')).toBe('nav.back')
  })
})

describe('selectors', () => {
  test('idSelector escapes regex metacharacters, so IDs that differ at a dot do not collide', () => {
    expect(escapeRegex('a.b*c(d)[e]|f\\g')).toBe('a\\.b\\*c\\(d\\)\\[e\\]\\|f\\\\g')
    const selector = JSON.parse(idSelector('today.save')) as string
    const fullMatch = new RegExp(`^(?:${selector})$`)
    expect(fullMatch.test('today.save')).toBe(true)
    expect(fullMatch.test('today_save')).toBe(false)
    expect(fullMatch.test('todayXsave')).toBe(false)
    expect(tapSegment('settings.export').yaml).toBe('- tapOn:\n    id: "settings\\\\.export"\n')
  })
})

describe('iosExplorationDriver', () => {
  const today = tree(attributes('screen.today'), attributes('today.answer'), attributes('Toolbar'))

  test('observe: keyboard keys are overlays, the share sheet is system UI, the fingerprint is unchanged', async () => {
    let screen: unknown = today
    const driver = iosExplorationDriver({ transport: transportFor(() => screen) })
    const config = { screenMarkerPattern: '^screen\\.' }
    const plain = await driver.observe()
    expect(plain.keyboardVisible).toBe(false)
    expect(driver.systemUi(plain)).toBeNull()

    screen = tree(attributes('screen.today'), attributes('today.answer'), attributes('space'), attributes('a'), attributes('return'))
    const typing = await driver.observe()
    expect(typing.keyboardVisible).toBe(true)
    expect(driver.systemUi(typing)?.kind).toBe('keyboard')
    expect(fingerprintScreenV2(typing.elements, config).fingerprint)
      .toBe(fingerprintScreenV2(plain.elements, config).fingerprint)

    screen = tree(attributes('screen.settings'), attributes('settings.export'), attributes('ActivityListView'))
    const sharing = await driver.observe()
    expect(driver.systemUi(sharing)).toEqual({ kind: 'share-sheet', evidence: ['ActivityListView'] })
    expect(fingerprintScreenV2(sharing.elements, config).anchors).toEqual(['settings.export'])
  })

  test('observe stores no labels in its snapshot hash input', async () => {
    const a = await iosExplorationDriver({ transport: transportFor(() => tree(attributes('screen.today', 'Today'))) }).observe()
    const b = await iosExplorationDriver({ transport: transportFor(() => tree(attributes('screen.today', 'Hoy'))) }).observe()
    expect(a.snapshot.sha256).toBe(b.snapshot.sha256)
  })

  test('keyboard accessory views, system IDs and the keyboard Done button never reach the fingerprint', async () => {
    const config = { screenMarkerPattern: '^screen\\.' }
    const plain = await iosExplorationDriver({ transport: transportFor(() => tree(attributes('screen.today'), attributes('wifi'), attributes('today.answer'))) }).observe()
    const typing = await iosExplorationDriver({
      transport: transportFor(() => tree(
        attributes('screen.today'), attributes('today.answer'), attributes('space'),
        attributes('SystemInputAssistantView'), attributes('inputView'), attributes('CenterPageView'),
        attributes('Done'), attributes('com.apple.text.grabber.leading'),
      )),
    }).observe()
    expect(fingerprintScreenV2(typing.elements, config).fingerprint)
      .toBe(fingerprintScreenV2(plain.elements, config).fingerprint)
  })

  test('two screen markers make the screen a gap, never a node', async () => {
    const observation = await iosExplorationDriver({
      transport: transportFor(() => tree(attributes('screen.journal'), attributes('screen.today'), attributes('today.answer'))),
    }).observe()
    expect(fingerprintScreenV2(observation.elements, { screenMarkerPattern: '^screen\\.' }))
      .toMatchObject({ stable: false, unstableReason: 'marker-conflict' })
  })

  test('permission buttons are reported as system UI', async () => {
    const driver = iosExplorationDriver({ transport: transportFor(() => tree(attributes('', 'Allow'), attributes('', 'Don’t Allow'))) })
    expect(driver.systemUi(await driver.observe())?.kind).toBe('permission')
  })

  test('reset writes a launchApp flow with the profile; act wraps segments and never relaunches', async () => {
    const flows: string[] = []
    const driver = iosExplorationDriver({ transport: transportFor(() => today, flows) })
    await expect(driver.act({ label: 'x', yaml: '- tapOn: x\n' })).rejects.toThrow('reset')
    const result = await driver.reset(
      { appId: 'com.blankaday.dev', clearState: true, arguments: { uiTesting: true, '-screenshotState': 'history' } },
      [[{ label: 'tap', yaml: '- tapOn:\n    id: "tab.feed"\n' }]],
    )
    expect(result.passed).toBe(true)
    expect(flows[0]).toContain('launchApp')
    expect(flows[0]).toContain('clearState: true')
    expect(flows[0]).toContain('"uiTesting": true')
    expect(flows[0]).toContain('"-screenshotState": "history"')
    expect(flows[1]).toBe('appId: "com.blankaday.dev"\n---\n- tapOn:\n    id: "tab.feed"\n')
    await driver.act({ label: 'again', yaml: '- tapOn: y\n' })
    expect(flows.filter(flow => flow.includes('launchApp'))).toHaveLength(1)
  })

  test('act parses the segment: every bypass of the relaunch guard is refused, data that only looks like one passes', async () => {
    const flows: string[] = []
    const driver = iosExplorationDriver({ transport: transportFor(() => today, flows) })
    await driver.reset({ appId: 'com.blankaday.dev', clearState: false, arguments: {} }, [])
    const refused = [
      '- {launchApp: {}}\n', '- "launchApp"\n', "- 'stopApp'\n", '- {killApp: x}\n',
      '- runFlow: other.yaml\n', '- runFlow:\n    file: other.yaml\n', '- {runFlow: {file: other.yaml}}\n',
      '- runFlow:\n    file: other.yaml\n    commands:\n      - tapOn: x\n',
      '- runScript: a.js\n', '- evalScript: ${1}\n',
      '- repeat:\n    times: 2\n    commands:\n      - launchApp\n',
      '- retry:\n    commands:\n      - clearState\n',
      '- runFlow:\n    when:\n      visible: x\n    commands:\n      - openLink: a://b\n',
      '- tapOn: x\n- {clearKeychain: {}}\n', 'appId: other.app\n---\n- tapOn: x\n', '- tapOn: [unclosed\n',
    ]
    const before = flows.length
    for (const yaml of refused) await expect(driver.act({ label: 'bad', yaml })).rejects.toThrow(TypeError)
    expect(flows.length).toBe(before)
    for (const yaml of [
      '- inputText: |\n    appId is here\n    launchApp too\n',
      '- inputText: launchApp\n',
      '- runFlow:\n    commands:\n      - tapOn: x\n',
      '- repeat:\n    times: 2\n    commands:\n      - swipe:\n          direction: DOWN\n',
    ]) expect((await driver.act({ label: 'ok', yaml })).passed).toBe(true)
  })

  test('act refuses segments that relaunch or rebind the app, and reset validates the profile', async () => {
    const flows: string[] = []
    const driver = iosExplorationDriver({ transport: transportFor(() => today, flows) })
    await driver.reset({ appId: 'com.blankaday.dev', clearState: false, arguments: {} }, [])
    const before = flows.length
    for (const yaml of [
      '- launchApp\n', '- launchApp:\n    clearState: true\n', 'appId: other.app\n---\n- tapOn: x\n',
      '- openLink: x://y\n', '- tapOn: x\n- clearState\n', '- stopApp\n',
    ]) await expect(driver.act({ label: 'bad', yaml })).rejects.toThrow(TypeError)
    expect(flows.length).toBe(before)
    for (const profile of [
      { appId: 'a.b', clearState: 'true\n  evil: 1', arguments: {} },
      { appId: 'a b\n- launchApp', clearState: true, arguments: {} },
      { appId: 'a.b', clearState: true, arguments: { k: 3 } },
      { appId: 'a.b', clearState: true, arguments: {}, permissions: { notifications: 'sure' } },
    ]) await expect(driver.reset(profile as never, [])).rejects.toThrow(TypeError)
  })

  test('a failing step is a result, not a throw', async () => {
    const failing: MaestroTransport = {
      ...transportFor(() => today),
      runFlow: async () => ({ passed: false, exitCode: 1, output: 'a\nElement not found: id tab.x', durationMs: 5, timedOut: true }),
    }
    const driver = iosExplorationDriver({ transport: failing })
    const result = await driver.reset({ appId: 'a.b', clearState: false, arguments: {} }, [])
    expect(result).toMatchObject({ passed: false, crashed: false, durationMs: 5 })
    expect(result.failure).toMatchObject({ category: 'timeout', failingStep: 'Element not found: id tab.x' })
  })

  test('back actions: nav bar and edge swipe with a back button; swipe down for a sheet; none at a root screen', async () => {
    const pushed = iosExplorationDriver({ transport: transportFor(() => tree(attributes('screen.settings'), attributes('BackButton', 'Today'))) })
    const pushedActions = pushed.backActions(await pushed.observe())
    expect(pushedActions.map(a => a.via)).toEqual(['nav-bar', 'edge-swipe'])
    expect(pushedActions[0].segment.yaml).toContain('BackButton')

    const sheet = iosExplorationDriver({ transport: transportFor(() => tree(attributes('screen.journal'), attributes('journal.daySheet'))) })
    const sheetActions = sheet.backActions(await sheet.observe())
    expect(sheetActions.map(a => a.via)).toEqual(['sheet-swipe-down'])
    expect(sheetActions[0].kind).toBe('dismiss')

    const root = iosExplorationDriver({ transport: transportFor(() => today) })
    expect(root.backActions(await root.observe())).toEqual([])
    for (const action of [...pushedActions, ...sheetActions]) expect(action.segment.yaml).not.toContain('launchApp')
  })
})

async function fakeExecutable(directory: string, source: string) {
  const path = join(directory, 'fake-maestro')
  await writeFile(path, `#!/usr/bin/env bun\n${source}\n`)
  await chmod(path, 0o755)
  return path
}

/** Request timeouts that fire only when the test says so, never on real time. */
function manualSchedule() {
  const live = new Set<() => void>()
  return {
    schedule: (run: () => void) => {
      live.add(run)
      return () => { live.delete(run) }
    },
    /** Fires the one armed timeout. More or fewer than one armed means the test lost track. */
    fireOnly() {
      expect(live.size).toBe(1)
      const [run] = [...live]
      live.delete(run)
      run()
    },
  }
}

/** Waits for a condition a child process signals (a file it writes), with no fixed delay. */
async function until(condition: () => boolean | Promise<boolean>, what: string) {
  for (let i = 0; i < 3000; i++) {
    if (await condition()) return
    await Bun.sleep(10)
  }
  throw new Error(`never happened: ${what}`)
}

/** The error a promise rejects with, as a value. Fails if it resolves. */
const settled = (promise: Promise<unknown>) =>
  promise.then(() => { throw new Error('expected a rejection') }, (error: Error) => error)

const isAlive = (pid: number) => { try { process.kill(pid, 0); return true } catch { return false } }

describe('ProcessMaestroTransport', () => {
  test('hierarchy parses JSON after log lines; runFlow passes env as -e and cleans up', async () => {
    const directory = await mkdtemp(join(tmpdir(), 'deja-transport-'))
    try {
      const argsFile = join(directory, 'args.json')
      const executable = await fakeExecutable(directory, `
const args = process.argv.slice(2)
if (args.includes('hierarchy')) { console.log('log line'); console.log(JSON.stringify({ attributes: {}, children: [] })) }
else { await Bun.write(${JSON.stringify(argsFile)}, JSON.stringify(args)); console.log('Tap on id: x... COMPLETED') }
`)
      const transport = new ProcessMaestroTransport({ executable, device: 'UDID-1', timeoutMs: 20_000 })
      const read = await transport.hierarchy()
      expect(read.raw).toEqual({ attributes: {}, children: [] })
      const flow = await transport.runFlow('appId: a\n---\n- tapOn: x\n', { NAME: 'value' })
      expect(flow.passed).toBe(true)
      const args = JSON.parse(await readFile(argsFile, 'utf8')) as string[]
      expect(args.slice(0, 6)).toEqual(['--no-ansi', '--platform', 'ios', '--device', 'UDID-1', 'test'])
      expect(args.slice(6, 8)).toEqual(['-e', 'NAME=value'])
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test('a failing exit code is a failed flow with output; a timeout is flagged', async () => {
    const directory = await mkdtemp(join(tmpdir(), 'deja-transport-'))
    try {
      const failing = new ProcessMaestroTransport({
        executable: await fakeExecutable(directory, 'console.log("Element not found"); process.exit(1)'),
        device: 'u', timeoutMs: 20_000,
      })
      const result = await failing.runFlow('appId: a\n---\n- tapOn: x\n')
      expect(result).toMatchObject({ passed: false, exitCode: 1, timedOut: false })
      expect(result.output).toContain('Element not found')
      const slow = new ProcessMaestroTransport({
        executable: await fakeExecutable(directory, 'await Bun.sleep(30000)'), device: 'u', timeoutMs: 300,
      })
      expect(await slow.runFlow('appId: a\n---\n- tapOn: x\n')).toMatchObject({ passed: false, timedOut: true })
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })
})

describe('McpMaestroTransport', () => {
  test('speaks JSON-RPC over stdio: initialize once, inspect_screen, run, and stops its process', async () => {
    const directory = await mkdtemp(join(tmpdir(), 'deja-mcp-'))
    try {
      const executable = await fakeExecutable(directory, `
const reply = (id, result) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\\n')
let buffer = ''
for await (const chunk of process.stdin) {
  buffer += new TextDecoder().decode(chunk)
  for (let end = buffer.indexOf('\\n'); end >= 0; end = buffer.indexOf('\\n')) {
    const message = JSON.parse(buffer.slice(0, end)); buffer = buffer.slice(end + 1)
    if (message.method === 'initialize') reply(message.id, { protocolVersion: '2024-11-05' })
    if (message.method === 'tools/call') {
      const { name, arguments: args } = message.params
      if (name === 'inspect_screen') reply(message.id, { content: [
        { type: 'text', text: 'Important: viewer link' },
        { type: 'text', text: JSON.stringify({ ui_schema: { defaults: {} }, elements: [{ rid: 'screen.today', a11y: args.device_id }] }) },
      ] })
      if (name === 'run') reply(message.id, { content: [{ type: 'text', text: args.yaml.includes('boom') ? 'Flow FAILED' : 'Flow COMPLETED' }] })
    }
  }
}
`)
      const transport = new McpMaestroTransport({ executable, device: 'UDID-2', timeoutMs: 20_000 })
      const read = await transport.hierarchy()
      expect(parseMaestroHierarchy(read.raw).map(e => e.rawId)).toEqual(['screen.today'])
      expect(transport.startupMs).toBeGreaterThanOrEqual(0)
      expect((await transport.runFlow('appId: a\n---\n- tapOn: ok\n')).passed).toBe(true)
      expect((await transport.runFlow('appId: a\n---\n- tapOn: boom\n')).passed).toBe(false)
      await transport.close()
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })
})

describe('McpMaestroTransport failure handling', () => {
  test('a nonexistent executable rejects at once, not after the timeout, and does not crash the host', async () => {
    const transport = new McpMaestroTransport({ executable: '/nonexistent/maestro', device: 'x', timeoutMs: 20_000 })
    const started = performance.now()
    await expect(transport.hierarchy()).rejects.toThrow()
    expect(performance.now() - started).toBeLessThan(5_000)
    // A second call starts again and fails the same way, still fast.
    const again = performance.now()
    await expect(transport.runFlow('appId: a\n---\n- tapOn: x\n')).rejects.toThrow()
    expect(performance.now() - again).toBeLessThan(5_000)
    await transport.close()
  })

  test('a child that dies mid-run rejects the pending call at once, and the next call starts a new one', async () => {
    const directory = await mkdtemp(join(tmpdir(), 'deja-mcp-die-'))
    try {
      const counter = join(directory, 'spawns')
      const executable = await fakeExecutable(directory, `
import { appendFileSync } from 'node:fs'
appendFileSync(${JSON.stringify(counter)}, 'x')
const spawns = (await Bun.file(${JSON.stringify(counter)}).text()).length
const reply = (id, result) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\\n')
let buffer = ''
for await (const chunk of process.stdin) {
  buffer += new TextDecoder().decode(chunk)
  for (let end = buffer.indexOf('\\n'); end >= 0; end = buffer.indexOf('\\n')) {
    const message = JSON.parse(buffer.slice(0, end)); buffer = buffer.slice(end + 1)
    if (message.method === 'initialize') reply(message.id, { protocolVersion: '2024-11-05' })
    if (message.method === 'tools/call') {
      if (spawns === 1) process.exit(3)
      reply(message.id, { content: [{ type: 'text', text: 'Flow COMPLETED' }] })
    }
  }
}
`)
      const transport = new McpMaestroTransport({ executable, device: 'u', timeoutMs: 20_000 })
      const started = performance.now()
      await expect(transport.runFlow('appId: a\n---\n- tapOn: x\n')).rejects.toThrow('exited')
      expect(performance.now() - started).toBeLessThan(5_000)
      expect((await transport.runFlow('appId: a\n---\n- tapOn: x\n')).passed).toBe(true)
      expect((await readFile(counter, 'utf8')).length).toBe(2)
      await transport.close()
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test('an initialize timeout kills the child and the next call starts a new one', async () => {
    const directory = await mkdtemp(join(tmpdir(), 'deja-mcp-hang-'))
    try {
      const pidFile = join(directory, 'pids')
      const executable = await fakeExecutable(directory, `
import { appendFileSync } from 'node:fs'
appendFileSync(${JSON.stringify(pidFile)}, process.pid + '\\n')
await Bun.sleep(60000)
`)
      const clock = manualSchedule()
      const transport = new McpMaestroTransport({ executable, device: 'u', timeoutMs: 1500, schedule: clock.schedule })
      const pids: number[] = []
      for (const round of [1, 2]) {
        // Not `expect(...).rejects`: Bun waits on that at once, and this call settles only after the timer fires.
        const call = settled(transport.hierarchy())
        // The child is up and never answers `initialize`. Only then does the timeout fire.
        await until(async () => existsSync(pidFile) && (await readFile(pidFile, 'utf8')).trim().split('\n').length === round, `child ${round} started`)
        pids.push(Number((await readFile(pidFile, 'utf8')).trim().split('\n')[round - 1]))
        clock.fireOnly()
        expect((await call).message).toContain('timed out')
        // The timed-out child is killed, and the second round proves a new one was started.
        await until(() => !isAlive(pids[round - 1]), `child ${round} killed`)
      }
      expect(pids[1]).not.toBe(pids[0])
      await transport.close()
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test('a timed-out call does not poison the next one: the old child exiting late must not reject the new child', async () => {
    const directory = await mkdtemp(join(tmpdir(), 'deja-mcp-stale-'))
    try {
      const counter = join(directory, 'spawns')
      const received = join(directory, 'received')
      const pidFile = join(directory, 'pid')
      const release = join(directory, 'release')
      const executable = await fakeExecutable(directory, `
import { appendFileSync, existsSync, writeFileSync } from 'node:fs'
appendFileSync(${JSON.stringify(counter)}, 'x')
const first = (await Bun.file(${JSON.stringify(counter)}).text()).length === 1
if (first) writeFileSync(${JSON.stringify(pidFile)}, String(process.pid))
const reply = (id, result) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\\n')
let buffer = ''
for await (const chunk of process.stdin) {
  buffer += new TextDecoder().decode(chunk)
  for (let end = buffer.indexOf('\\n'); end >= 0; end = buffer.indexOf('\\n')) {
    const message = JSON.parse(buffer.slice(0, end)); buffer = buffer.slice(end + 1)
    if (message.method === 'initialize') reply(message.id, { protocolVersion: '2024-11-05' })
    if (message.method === 'tools/call') {
      // The first child never answers a tool call, so the first call times out.
      if (first) { writeFileSync(${JSON.stringify(received)}, '1'); continue }
      // The second child answers only after the test has seen the first child die.
      while (!existsSync(${JSON.stringify(release)})) await Bun.sleep(10)
      reply(message.id, { content: [{ type: 'text', text: 'Flow COMPLETED' }] })
    }
  }
}
`)
      const clock = manualSchedule()
      const transport = new McpMaestroTransport({ executable, device: 'u', timeoutMs: 700, schedule: clock.schedule })
      const firstCall = settled(transport.runFlow('appId: a\n---\n- tapOn: x\n'))
      // The first child holds the tool call and stays silent. Only then does the timeout fire.
      await until(() => existsSync(received), 'first child received the tool call')
      clock.fireOnly()
      expect((await firstCall).message).toContain('timed out')
      // The second call starts at once, so the killed child's close event lands while it is pending.
      const second = transport.runFlow('appId: a\n---\n- tapOn: x\n')
      const oldPid = Number(await readFile(pidFile, 'utf8'))
      await until(() => !isAlive(oldPid), 'first child died')
      // Let the old child's close event reach the transport before the new child may answer. This
      // only widens the window in which a stale event could hurt, so it cannot hide a failure.
      await Bun.sleep(50)
      await writeFile(release, '1')
      expect((await second).passed).toBe(true)
      expect((await readFile(counter, 'utf8')).length).toBe(2)
      await transport.close()
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test('flow variable names are validated', async () => {
    const transport = new McpMaestroTransport({ executable: '/nonexistent/maestro', device: 'x', timeoutMs: 1000 })
    await expect(transport.runFlow('appId: a\n---\n- tapOn: x\n', { 'BAD NAME': '1' })).rejects.toThrow('Invalid flow variable')
    const process_ = new ProcessMaestroTransport({ executable: '/nonexistent/maestro', device: 'x' })
    await expect(process_.runFlow('appId: a\n---\n- tapOn: x\n', { '1BAD': '1' })).rejects.toThrow('Invalid flow variable')
  })
})
