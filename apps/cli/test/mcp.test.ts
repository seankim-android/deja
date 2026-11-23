import { afterEach, describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, readFile, realpath, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { Client } from '@modelcontextprotocol/sdk/client/index.js'
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js'
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js'
import { LocalWalkStore } from '@deja-qa/store-local'
import { loadProductionConfiguration } from '../src/config.ts'
import { confineFlow, createMcpServer, parseMcpArguments } from '../src/mcp.ts'
import { runCli } from '../src/main.ts'

const fakeMaestro = resolve(import.meta.dir, '../../../packages/maestro/test/fixtures/fake-maestro.cjs')
const FIXTURE_VALUE = 'hunter2-fixture'

const TODAY = `appId: dev.deja.fixture
---
- launchApp:
    clearState: true
- assertVisible:
    id: "screen.today"
- assertVisible:
    id: "today.saved"
`

type Tool = { name: string; inputSchema: Record<string, unknown> }
type Payload = Record<string, any>

const cleanups: Array<() => Promise<void>> = []
afterEach(async () => {
  delete process.env.DEJA_FAKE_MAESTRO_MODE
  while (cleanups.length > 0) await cleanups.pop()!()
})

async function workspace(config: Record<string, unknown> = {}, executable = fakeMaestro) {
  const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-mcp-')))
  const root = join(temporary, 'workspace')
  await mkdir(join(root, 'Fixture.app'), { recursive: true })
  await mkdir(join(root, '.maestro/flows'), { recursive: true })
  await writeFile(join(root, 'Fixture.app/Fixture'), 'binary-one')
  await writeFile(join(root, '.maestro/flows/today.yaml'), TODAY)
  await writeFile(join(root, 'deja.config.json'), JSON.stringify({
    workspaceRoot: '.', appId: 'dev.deja.fixture', platform: 'ios', adapter: 'maestro-ios/v1',
    device: 'DFC4D233-498B-4B33-B713-E59B239FA86A', buildPath: 'Fixture.app',
    fixtureCredentials: [{ label: 'Fixture password', value: FIXTURE_VALUE }],
    maestro: { executable, timeoutMs: 30_000 },
    manifest: 'deja.walks.json',
    ...config,
  }))
  cleanups.push(() => rm(temporary, { recursive: true, force: true }))
  return { temporary, root }
}

async function connect(root: string, options: { storeId?: string } = {}) {
  const configuration = await loadProductionConfiguration(join(root, 'deja.config.json'))
  const server = createMcpServer({
    configuration,
    ...options,
    env: { ...process.env, DEJA_HOME: join(root, '..', 'deja-home'), DEJA_MAESTRO: '' },
    compiledBinary: false,
    stderr: () => {},
  })
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair()
  await server.connect(serverTransport)
  const client = new Client({ name: 'test', version: '1' })
  await client.connect(clientTransport)
  cleanups.push(async () => { await client.close(); await server.close() })
  return client
}

async function call(client: Client, name: string, args: Record<string, unknown> = {}) {
  const result = await client.callTool({ name, arguments: args })
  return { isError: result.isError === true, payload: result.structuredContent as Payload, text: JSON.stringify(result.content) }
}

/** A Maestro that waits, so a second call can arrive while the first holds the store. */
async function slowMaestro(directory: string, versionDelaySeconds = 0) {
  const path = join(directory, 'slow-maestro.sh')
  await writeFile(path, `#!/bin/sh
if [ "$1" = "--version" ]; then sleep ${versionDelaySeconds}; echo 2.12.0; exit 0; fi
echo run >> "${join(directory, 'maestro-runs.log')}"
sleep 1
exit 0
`, { mode: 0o755 })
  return path
}

describe('deja mcp', () => {
  test('lists the replay tools with strict schemas, and no explore by default', async () => {
    const { root } = await workspace()
    const client = await connect(root)
    const { tools } = await client.listTools()
    expect(tools.map(tool => tool.name).sort()).toEqual(['deja_inspect', 'deja_list', 'deja_remember', 'deja_run'])
    for (const tool of tools as Tool[]) {
      expect(tool.name).toMatch(/^[a-zA-Z0-9_-]{1,64}$/)
      expect(tool.inputSchema).toMatchObject({ type: 'object', additionalProperties: false })
    }
    const run = tools.find(tool => tool.name === 'deja_run')!
    expect(run.inputSchema).toMatchObject({ required: ['goal'], properties: { goal: { type: 'string' } } })
    // Nothing a caller can pass names a device, app, workspace, store, verify or an assertion override.
    expect(Object.keys((run.inputSchema as { properties: object }).properties)).toEqual(['goal'])
    const remember = tools.find(tool => tool.name === 'deja_remember')!
    expect(Object.keys((remember.inputSchema as { properties: object }).properties)).toEqual(['goal', 'flow'])
    await expect(call(client, 'deja_explore', { goal: 'today' })).rejects.toThrow(/Unknown tool/)
  })

  test('deja_explore is registered only when the host config enables it', async () => {
    const { root } = await workspace({ mcp: { explore: true } })
    const client = await connect(root)
    expect((await client.listTools()).tools.map(tool => tool.name)).toContain('deja_explore')
    // iOS has no cold explorer: the call goes through the configured explorer and spends no model call.
    const explored = await call(client, 'deja_explore', { goal: 'something new' })
    expect(explored.payload).toMatchObject({ schemaVersion: 1, command: 'explore', outcome: 'exploration-rejected', modelCalls: 0 })
    expect(explored.payload.errors.join(' ')).toContain('Cold iOS exploration is not supported')
  })

  test('mcp config is validated', async () => {
    const { root } = await workspace({ mcp: { explore: 'yes' } })
    await expect(loadProductionConfiguration(join(root, 'deja.config.json'))).rejects.toThrow(/mcp.explore must be a boolean/)
    const other = await workspace({ mcp: { repair: true } })
    await expect(loadProductionConfiguration(join(other.root, 'deja.config.json'))).rejects.toThrow(/Unknown mcp setting/)
  })

  test('remember, list, run and inspect give schema-1 results with workspace-relative paths', async () => {
    const { root } = await workspace()
    const client = await connect(root)
    expect((await call(client, 'deja_list')).payload).toEqual({ manifest: 'deja.walks.json', walks: [] })

    const remembered = await call(client, 'deja_remember', { goal: 'today', flow: '.maestro/flows/today.yaml' })
    expect(remembered.isError).toBe(false)
    expect(remembered.payload).toMatchObject({
      schemaVersion: 1, command: 'remember', outcome: 'remembered', passed: true, modelCalls: 0,
      yamlPath: '.maestro/flows/today.yaml',
    })
    expect(JSON.parse(await readFile(join(root, 'deja.walks.json'), 'utf8')).walks).toHaveLength(1)

    const listed = (await call(client, 'deja_list')).payload
    expect(listed.walks).toEqual([expect.objectContaining({
      goal: 'today', goalKey: 'today', platform: 'ios', appId: 'dev.deja.fixture',
      flowPath: '.maestro/flows/today.yaml', lastValidation: expect.objectContaining({ passed: true }),
    })])

    const ran = await call(client, 'deja_run', { goal: 'today' })
    expect(ran.isError).toBe(false)
    expect(ran.payload).toMatchObject({
      schemaVersion: 1, command: 'run', cacheStatus: 'hit', outcome: 'passed', passed: true,
      modelCalls: 0, failure: null,
      engine: expect.objectContaining({ inRange: true }),
      report: { latestPath: '.deja/reports/latest.json' },
    })
    expect(Array.isArray(ran.payload.artifacts)).toBe(true)
    // No absolute workspace path in anything the agent receives.
    expect(ran.text).not.toContain(root)
    expect(ran.text).not.toContain(FIXTURE_VALUE)

    const inspected = await call(client, 'deja_inspect', { report: ran.payload.report.reportPath })
    expect(inspected.isError).toBe(false)
    expect(inspected.payload).toMatchObject({ schemaVersion: 1, command: 'run', outcome: 'passed' })
    expect((await call(client, 'deja_inspect')).payload).toMatchObject({ schemaVersion: 1, command: 'run' })
  })

  test('a failing replay returns its failure label and failing step', async () => {
    const { root } = await workspace()
    const client = await connect(root)
    await call(client, 'deja_remember', { goal: 'today', flow: '.maestro/flows/today.yaml' })
    process.env.DEJA_FAKE_MAESTRO_MODE = 'product'
    const failed = await call(client, 'deja_run', { goal: 'today' })
    expect(failed.isError).toBe(false)
    expect(failed.payload).toMatchObject({
      schemaVersion: 1, outcome: 'product-regression', passed: false, cacheStatus: 'hit', modelCalls: 0,
      failure: { category: 'product_assertion' },
    })
    expect(failed.payload.failure.failingStep).toContain('Order #WC-1001')
    expect(failed.payload.failure.rawArtifactPath).not.toMatch(/^\//)
    expect(failed.text).not.toContain(root)
    // A failing run is still a result, not a tool error, and it needs no repair or explore to be reported.
    expect(failed.payload.repairCount).toBe(0)
  })

  test('a missing walk is a not-found miss with zero model calls, with and without a manifest', async () => {
    for (const config of [{}, { manifest: undefined }]) {
      const { root } = await workspace(config)
      const client = await connect(root)
      const miss = await call(client, 'deja_run', { goal: 'something nobody saved' })
      expect(miss.isError).toBe(false)
      expect(miss.payload).toMatchObject({
        schemaVersion: 1, command: 'run', cacheStatus: 'miss', cacheMissReason: 'not-found',
        outcome: 'exploration-rejected', passed: false, modelCalls: 0, artifacts: [],
      })
      expect((await call(client, 'deja_list')).payload.walks).toEqual([])
    }
  })

  test('remember rejects a failing candidate and saves nothing', async () => {
    const { root } = await workspace()
    const client = await connect(root)
    process.env.DEJA_FAKE_MAESTRO_MODE = 'product'
    const rejected = await call(client, 'deja_remember', { goal: 'today', flow: '.maestro/flows/today.yaml' })
    expect(rejected.payload).toMatchObject({ outcome: 'rejected', passed: false, yamlPath: null, modelCalls: 0 })
    await expect(readFile(join(root, 'deja.walks.json'), 'utf8')).rejects.toBeDefined()
    expect((await call(client, 'deja_list')).payload.walks).toEqual([])
  })

  test('a weakened flow is refused by remember, and no argument can accept the change', async () => {
    const { root } = await workspace()
    const client = await connect(root)
    await call(client, 'deja_remember', { goal: 'today', flow: '.maestro/flows/today.yaml' })
    const before = await readFile(join(root, 'deja.walks.json'), 'utf8')
    await writeFile(join(root, '.maestro/flows/today.yaml'), TODAY.replace('- assertVisible:\n    id: "today.saved"\n', ''))
    const refused = await call(client, 'deja_remember', { goal: 'today', flow: '.maestro/flows/today.yaml' })
    expect(refused.payload).toMatchObject({ outcome: 'assertions-changed', passed: false })
    await expect(call(client, 'deja_remember', {
      goal: 'today', flow: '.maestro/flows/today.yaml', acceptAssertionChange: true,
    })).rejects.toThrow(/Unknown argument: acceptAssertionChange/)
    expect(await readFile(join(root, 'deja.walks.json'), 'utf8')).toBe(before)
  })

  test('manifest outcomes come back as results: flow-changed is reported, never repaired or re-verified', async () => {
    const { root } = await workspace()
    const client = await connect(root)
    await call(client, 'deja_remember', { goal: 'today', flow: '.maestro/flows/today.yaml' })
    await writeFile(join(root, '.maestro/flows/today.yaml'), `${TODAY}# edited\n`)
    const changed = await call(client, 'deja_run', { goal: 'today' })
    expect(changed.payload).toMatchObject({ outcome: 'flow-changed', passed: false, cacheMissReason: 'flow-changed', modelCalls: 0 })
  })

  test('inputs are strict: unknown fields, wrong types and missing fields are rejected', async () => {
    const { root } = await workspace()
    const client = await connect(root)
    await expect(call(client, 'deja_run', { goal: 'today', verify: true })).rejects.toThrow(/Unknown argument: verify/)
    await expect(call(client, 'deja_run', { goal: 'today', device: 'other' })).rejects.toThrow(/Unknown argument: device/)
    await expect(call(client, 'deja_run', { goal: 'today', workspaceRoot: '/' })).rejects.toThrow(/Unknown argument/)
    await expect(call(client, 'deja_run', { goal: 'today', repair: true })).rejects.toThrow(/Unknown argument/)
    await expect(call(client, 'deja_run', {})).rejects.toThrow(/goal is required/)
    await expect(call(client, 'deja_run', { goal: 7 })).rejects.toThrow(/goal must be a string/)
    await expect(call(client, 'deja_run', { goal: '  ' })).rejects.toThrow(/must not be empty/)
    await expect(call(client, 'deja_list', { all: true })).rejects.toThrow(/Unknown argument/)
    await expect(call(client, 'deja_inspect', { report: 5 })).rejects.toThrow(/report must be a string/)
  })

  test('remember confines the flow path to the workspace', async () => {
    const { temporary, root } = await workspace()
    const outside = join(temporary, 'outside.yaml')
    await writeFile(outside, TODAY)
    await mkdir(join(root, '.deja/flows'), { recursive: true })
    await writeFile(join(root, '.deja/flows/stored.yaml'), TODAY)
    await symlink(outside, join(root, '.maestro/flows/escape.yaml'))
    await symlink(join(root, '.deja/flows/stored.yaml'), join(root, '.maestro/flows/into-deja.yaml'))
    await symlink(temporary, join(root, 'linked-dir'))
    await writeFile(join(root, 'notes.txt'), TODAY)
    const client = await connect(root)
    const refused: Array<[string, RegExp]> = [
      ['../outside.yaml', /inside the workspace/],
      ['.maestro/flows/../../../outside.yaml', /inside the workspace/],
      [outside, /relative/],
      ['.maestro/flows/escape.yaml', /symlinks included/],
      ['linked-dir/outside.yaml', /symlinks included/],
      ['.deja/flows/stored.yaml', /under \.deja/],
      ['.DEJA/flows/stored.yaml', /\.deja|does not exist/],
      ['.deja/../.deja/flows/stored.yaml', /under \.deja/],
      ['.maestro/flows/into-deja.yaml', /into \.deja/],
      ['.maestro/flows/missing.yaml', /does not exist/],
      ['notes.txt', /\.yaml or \.yml/],
      ['.maestro/flows', /\.yaml or \.yml/],
    ]
    for (const [flow, message] of refused) {
      const result = await call(client, 'deja_remember', { goal: 'today', flow })
      expect({ flow, isError: result.isError, code: result.payload.error?.code }).toEqual({ flow, isError: true, code: 'INVALID_PATH' })
      expect(result.payload.error.message).toMatch(message)
      expect(result.text).not.toContain(temporary)
    }
    await expect(readFile(join(root, 'deja.walks.json'), 'utf8')).rejects.toBeDefined()
    // A symlink that stays inside the workspace is fine.
    await symlink(join(root, '.maestro/flows/today.yaml'), join(root, '.maestro/flows/alias.yaml'))
    expect(await confineFlow(root, '.maestro/flows/alias.yaml')).toBe('.maestro/flows/alias.yaml')
  })

  test('inspect reads only reports under .deja/reports', async () => {
    const { temporary, root } = await workspace()
    const client = await connect(root)
    await call(client, 'deja_run', { goal: 'nothing' })
    await writeFile(join(temporary, 'outside.json'), '{}')
    await mkdir(join(root, '.deja/reports'), { recursive: true })
    await symlink(join(temporary, 'outside.json'), join(root, '.deja/reports/link.json'))
    for (const report of ['deja.config.json', '../outside.json', '.deja/reports/link.json', '.deja/reports/../memory/x.json', join(root, '.deja/reports/latest.json'), '.deja/reports/none.json']) {
      const result = await call(client, 'deja_inspect', { report })
      expect({ report, isError: result.isError }).toEqual({ report, isError: true })
      expect(result.text).not.toContain(root)
    }
  })

  test('responses are redacted like CLI reports', async () => {
    const { root } = await workspace()
    const client = await connect(root)
    const goal = `login with ${FIXTURE_VALUE}`
    const ran = await call(client, 'deja_run', { goal })
    expect(ran.text).not.toContain(FIXTURE_VALUE)
    expect(ran.payload.identity.goal).toContain('<redacted-fixture-value>')
    const saved = await readFile(join(root, '.deja/reports/latest.json'), 'utf8')
    expect(saved).not.toContain(FIXTURE_VALUE)
  })

  test('a second concurrent run on the same store is refused, and the lock is released afterwards', async () => {
    const { temporary, root } = await workspace()
    const slow = await slowMaestro(temporary)
    await writeFile(join(root, 'deja.config.json'), JSON.stringify({
      ...JSON.parse(await readFile(join(root, 'deja.config.json'), 'utf8')), maestro: { executable: slow, timeoutMs: 30_000 },
    }))
    const client = await connect(root)
    expect((await call(client, 'deja_remember', { goal: 'today', flow: '.maestro/flows/today.yaml' })).payload.outcome).toBe('remembered')

    const first = call(client, 'deja_run', { goal: 'today' })
    // Wait until the first run has reached Maestro, so it holds the lock.
    for (let wait = 0; wait < 100; wait += 1) {
      const log = await readFile(join(temporary, 'maestro-runs.log'), 'utf8').catch(() => '')
      if (log.trim().split('\n').length >= 2) break
      await Bun.sleep(50)
    }
    const second = await call(client, 'deja_run', { goal: 'today' })
    expect(second.isError).toBe(true)
    expect(second.payload.error.code).toBe('STORE_LOCKED')
    // A second writer of any kind is refused too.
    expect((await call(client, 'deja_remember', { goal: 'other', flow: '.maestro/flows/today.yaml' })).payload.error.code).toBe('STORE_LOCKED')
    // Reads still work while a run is in flight.
    expect((await call(client, 'deja_list')).isError).toBe(false)
    expect((await first).payload).toMatchObject({ outcome: 'passed', passed: true })
    expect((await call(client, 'deja_run', { goal: 'today' })).payload.passed).toBe(true)
    // Only the three real replays ran Maestro; the refused calls never reached it.
    expect((await readFile(join(temporary, 'maestro-runs.log'), 'utf8')).trim().split('\n')).toHaveLength(3)
  })

  test('another process holding the store refuses deja_run; a different --store id works', async () => {
    const { root } = await workspace()
    const holder = new LocalWalkStore({ workspaceRoot: root })
    holder.acquireWriter()
    try {
      const client = await connect(root)
      expect((await call(client, 'deja_run', { goal: 'today' })).payload.error.code).toBe('STORE_LOCKED')
      const other = await connect(root, { storeId: 'device-b' })
      expect((await call(other, 'deja_run', { goal: 'today' })).isError).toBe(false)
    } finally {
      holder.close()
    }
  })

  test('the request abort signal reaches the call: an aborted run never starts Maestro', async () => {
    const { temporary, root } = await workspace()
    const slow = await slowMaestro(temporary, 1)
    await writeFile(join(root, 'deja.config.json'), JSON.stringify({
      ...JSON.parse(await readFile(join(root, 'deja.config.json'), 'utf8')), maestro: { executable: slow, timeoutMs: 30_000 },
    }))
    const client = await connect(root)
    // A saved walk, so an unaborted run would reach Maestro. That is one logged run.
    expect((await call(client, 'deja_remember', { goal: 'today', flow: '.maestro/flows/today.yaml' })).payload.outcome).toBe('remembered')
    const runs = async () => (await readFile(join(temporary, 'maestro-runs.log'), 'utf8')).trim().split('\n').length
    expect(await runs()).toBe(1)
    const controller = new AbortController()
    // The version probe takes a second, so the abort lands before the runner is reached.
    const pending = client.callTool({ name: 'deja_run', arguments: { goal: 'today' } }, undefined, { signal: controller.signal })
    await Bun.sleep(300)
    controller.abort()
    await expect(pending).rejects.toBeDefined()
    await Bun.sleep(2000)
    expect(await runs()).toBe(1)
    // The lock was released, so the next call is not refused, and it does run Maestro.
    expect((await call(client, 'deja_run', { goal: 'today' })).payload).toMatchObject({ outcome: 'passed' })
    expect(await runs()).toBe(2)
  })
})

describe('deja mcp command line', () => {
  test('serves over stdio from the deja binary, and exits when the host closes it', async () => {
    const { root } = await workspace()
    const transport = new StdioClientTransport({
      command: process.execPath,
      args: [resolve(import.meta.dir, '../deja.ts'), 'mcp', '--config', join(root, 'deja.config.json')],
      cwd: root,
      env: { ...process.env as Record<string, string>, DEJA_HOME: join(root, '..', 'deja-home'), DEJA_MAESTRO: '', DEJA_FAKE_MAESTRO_MODE: '' },
      stderr: 'pipe',
    })
    const client = new Client({ name: 'test', version: '1' })
    await client.connect(transport)
    try {
      expect((await client.listTools()).tools).toHaveLength(4)
      const result = await client.callTool({ name: 'deja_remember', arguments: { goal: 'today', flow: '.maestro/flows/today.yaml' } })
      expect(result.structuredContent).toMatchObject({ outcome: 'remembered', passed: true })
    } finally {
      const started = performance.now()
      await client.close()
      // The server exits on its own when stdin closes; the SDK waits 2 seconds before it sends SIGTERM.
      expect(performance.now() - started).toBeLessThan(1500)
    }
  })

  test('accepts only --config and --store', () => {
    expect(parseMcpArguments(['--config', 'a.json', '--store', 'b'])).toEqual({ configPath: 'a.json', storeId: 'b' })
    expect(() => parseMcpArguments(['--json'])).toThrow(/only --config and --store/)
    expect(() => parseMcpArguments(['--store'])).toThrow(/requires a store id/)
  })

  test('refuses options that do not apply, before reading any config', async () => {
    const errors: string[] = []
    const code = await runCli(['mcp', '--verify'], { stdout: () => {}, stderr: text => errors.push(text) })
    expect(code).toBe(2)
    expect(errors.join('')).toContain('mcp accepts only --config and --store')
  })
})
