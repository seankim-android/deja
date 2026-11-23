import { afterEach, describe, expect, test } from 'bun:test'
import { link, mkdir, mkdtemp, readFile, realpath, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { Client } from '@modelcontextprotocol/sdk/client/index.js'
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js'
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js'
import { LocalWalkStore } from '@deja-qa/store-local'
import { loadProductionConfiguration } from '../src/config.ts'
import { confineFlow, createMcpServer, checkFlowApp, isKeyLikeName, parseMcpArguments, providerKeyRedactor, relativizeResult } from '../src/mcp.ts'
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

async function connect(root: string, options: { storeId?: string; crashReportDirectory?: string; env?: NodeJS.ProcessEnv } = {}) {
  const configuration = await loadProductionConfiguration(join(root, 'deja.config.json'))
  const { env: extraEnv, ...serverOptions } = options
  const server = createMcpServer({
    configuration,
    ...serverOptions,
    env: { ...process.env, DEJA_HOME: join(root, '..', 'deja-home'), DEJA_MAESTRO: '', ...extraEnv },
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
    await writeFile(join(root, '.env'), `appId: dev.deja.fixture\nSECRET=1\n`)
    await symlink(join(root, '.env'), join(root, '.maestro/flows/dotenv.yaml'))
    await writeFile(join(root, '.maestro/flows/shared.yaml'), TODAY)
    await link(join(root, '.maestro/flows/shared.yaml'), join(root, '.maestro/flows/hard.yaml'))
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
      ['.maestro/flows/dotenv.yaml', /resolve to a \.yaml or \.yml/],
      ['.maestro/flows/hard.yaml', /hard link/],
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
    // The canonical path is what gets verified and saved.
    expect(await confineFlow(root, '.maestro/flows/alias.yaml')).toBe('.maestro/flows/today.yaml')
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

  test('the flow header must name the configured app, and the check fails closed', async () => {
    const { root } = await workspace()
    const client = await connect(root)
    const body = '- launchApp\n- assertVisible: x\n'
    const refused: Record<string, string> = {
      'other app': `appId: com.apple.Preferences\n---\n${body}`,
      'leading document marker': `---\nappId: com.evil\n---\n${body}`,
      'duplicate key, good first': `appId: dev.deja.fixture\nappId: com.evil\n---\n${body}`,
      'duplicate key, good last': `appId: com.evil\nappId: dev.deja.fixture\n---\n${body}`,
      'space before colon': `appId : com.evil\n---\n${body}`,
      'quoted key': `"appId": com.evil\n---\n${body}`,
      'flow mapping': `{appId: com.evil}\n---\n${body}`,
      'BOM': `\uFEFFappId: com.evil\n---\n${body}`,
      'tagged value': `appId: !!str com.evil\n---\n${body}`,
      'tagged good value': `appId: !!str dev.deja.fixture\n---\n${body}`,
      'no header': body,
      'empty first document': `---\n---\n${body}`,
      'header without appId': `name: x\n---\n${body}`,
      'appId not a string': `appId: 123\n---\n${body}`,
      'appId an alias': `base: &a dev.deja.fixture\nappId: *a\n---\n${body}`,
      'does not parse': `appId: [unclosed\n---\n${body}`,
    }
    for (const [name, text] of Object.entries(refused)) {
      await writeFile(join(root, '.maestro/flows/case.yaml'), text)
      const result = await call(client, 'deja_remember', { goal: 'case', flow: '.maestro/flows/case.yaml' })
      expect({ name, code: result.payload.error?.code, message: result.payload.error?.message ?? '' })
        .toEqual({ name, code: 'INVALID_PATH', message: expect.stringMatching(/flow header refused|does not match/) })
    }
    await expect(readFile(join(root, 'deja.walks.json'), 'utf8')).rejects.toBeDefined()
    const accepted: Record<string, string> = {
      plain: `appId: dev.deja.fixture\n---\n${body}`,
      'quoted value and comment': `appId: "dev.deja.fixture" # ours\n---\n${body}`,
      'quoted key': `"appId": dev.deja.fixture\n---\n${body}`,
      'flow mapping': `{appId: dev.deja.fixture}\n---\n${body}`,
      'leading document marker': `---\nappId: dev.deja.fixture\n---\n${body}`,
      BOM: `\uFEFFappId: dev.deja.fixture\n---\n${body}`,
    }
    for (const [name, text] of Object.entries(accepted)) {
      await writeFile(join(root, '.maestro/flows/case.yaml'), text)
      const result = await call(client, 'deja_remember', { goal: 'case', flow: '.maestro/flows/case.yaml' })
      expect({ name, outcome: result.payload.outcome ?? result.payload.error }).toEqual({ name, outcome: 'remembered' })
    }
  })

  test('a web flow must stay on the configured page origin', () => {
    const web = { appId: 'http://127.0.0.1:4173', platform: 'web' }
    expect(() => checkFlowApp('url: http://127.0.0.1:4173/shop\n---\n- tapOn: x\n', web)).not.toThrow()
    expect(() => checkFlowApp('url: https://evil.example/\n---\n- tapOn: x\n', web)).toThrow(/not on the configured page/)
    expect(() => checkFlowApp('appId: http://127.0.0.1:4173\n---\n- tapOn: x\n', web)).toThrow(/url must be a plain string/)
  })

  test('inherited property names are unknown arguments, and rerunFailed is not an argument', async () => {
    const { root } = await workspace()
    const client = await connect(root)
    for (const name of ['constructor', 'toString', 'hasOwnProperty', 'rerunFailed']) {
      const outcome = await client.callTool({ name: 'deja_run', arguments: JSON.parse(`{"goal":"today","${name}":true}`) })
        .then(() => 'resolved', (error: Error) => error.message)
      expect({ name, outcome }).toEqual({ name, outcome: expect.stringMatching(/Unknown argument/) })
    }
    const { tools } = await client.listTools()
    expect(JSON.stringify(tools)).not.toContain('rerun')
  })

  test('deja mcp still classifies app_crash from the output and from a crash report', async () => {
    const { temporary, root } = await workspace()
    const reports = join(temporary, 'DiagnosticReports')
    await mkdir(reports)
    const client = await connect(root, { crashReportDirectory: reports })
    expect((await call(client, 'deja_remember', { goal: 'today', flow: '.maestro/flows/today.yaml' })).payload.outcome).toBe('remembered')
    process.env.DEJA_FAKE_MAESTRO_SCRIPT = 'crash'
    process.env.DEJA_FAKE_MAESTRO_COUNTER = join(temporary, 'counter-output')
    const output = await call(client, 'deja_run', { goal: 'today' })
    expect(output.payload).toMatchObject({ outcome: 'failed', passed: false, failure: { category: 'app_crash', repairEligible: false } })
    expect(output.payload.attempts).toBeUndefined()

    // A failed replay is not reusable: save the walk again before the next run.
    delete process.env.DEJA_FAKE_MAESTRO_SCRIPT
    expect((await call(client, 'deja_remember', { goal: 'today', flow: '.maestro/flows/today.yaml' })).payload.outcome).toBe('remembered')
    // No crash line in Maestro's output: only the crash report for the configured app ID says so, which
    // proves the runner got the app ID and the report directory.
    process.env.DEJA_FAKE_MAESTRO_SCRIPT = 'quiet-crash'
    process.env.DEJA_FAKE_MAESTRO_COUNTER = join(temporary, 'counter-report')
    process.env.DEJA_FAKE_CRASH_REPORT = join(reports, 'DejaFixture-2026-11-23-010203.ips')
    process.env.DEJA_FAKE_CRASH_REPORT_BODY =
      '{"app_name":"DejaFixture"}\n{"procName":"DejaFixture","bundleInfo":{"CFBundleIdentifier":"dev.deja.fixture"},"private":"patient"}'
    try {
      const report = await call(client, 'deja_run', { goal: 'today' })
      expect(report.payload.failure).toMatchObject({ category: 'app_crash', crash: { source: 'crash-report', process: 'DejaFixture' } })
      expect(report.text).not.toContain('patient')
    } finally {
      for (const name of ['DEJA_FAKE_MAESTRO_SCRIPT', 'DEJA_FAKE_MAESTRO_COUNTER', 'DEJA_FAKE_CRASH_REPORT', 'DEJA_FAKE_CRASH_REPORT_BODY']) delete process.env[name]
    }
  })

  test('relativizeResult rewrites paths in attempts, execution and failures', () => {
    const root = '/work/space'
    const failure = { category: 'app_crash', advisory: true, repairEligible: false, failingStep: null, rawArtifactPath: `${root}/.deja/reports/a/output.log` } as const
    const result = {
      yamlPath: `${root}/flow.yaml`, failure, artifacts: [{ kind: 'output', path: `${root}/.deja/x` }],
      attempts: [
        { attempt: 1, passed: false, failure, artifacts: [{ kind: 'output', path: `${root}/.deja/reports/a/output.log` }] },
        { attempt: 2, passed: true, failure: null, artifacts: [{ kind: 'output', path: `/elsewhere/${root}/y` }] },
      ],
      execution: { failure, artifacts: [{ kind: 'output', path: `${root}/.deja/z` }] },
    } as never
    const text = JSON.stringify(relativizeResult(result, [root]))
    expect(text).not.toContain(`"${root}`)
    expect(text).toContain('".deja/reports/a/output.log"')
    expect(text).toContain('"flow.yaml"')
  })

  test('provider keys are redacted from error responses and results', async () => {
    // Built at runtime so no literal key sits in the repository.
    const providerKey = `sk-${'a1b2c3d4'.repeat(4)}`
    const { root } = await workspace({ buildPath: `${providerKey}.app` })
    await mkdir(join(root, `${providerKey}.app`), { recursive: true })
    await writeFile(join(root, `${providerKey}.app/Fixture`), 'binary')
    const envKey = ['tok', 'en', '-', 'z9y8x7w6v5u4'].join('')
    const client = await connect(root, { env: { OPENCLAW_API_KEY: envKey } })
    await rm(join(root, `${providerKey}.app`), { recursive: true })
    const failed = await call(client, 'deja_run', { goal: `look at ${envKey}` })
    expect(failed.isError).toBe(true)
    expect(failed.payload.error.code).toBe('INTERNAL')
    expect(failed.text).not.toContain(providerKey)
    expect(failed.text).toContain('<redacted-secret>')
    expect(failed.text).not.toContain(root)
    const listed = await call(client, 'deja_inspect', { report: '.deja/reports/none.json' })
    expect(listed.text).not.toContain(envKey)
  })

  test('redaction leaves ordinary words alone and still catches real keys', () => {
    const redact = providerKeyRedactor({ KEYBOARD: 'dvorak-layout', API_TOKEN: 'tok3n-Zq81xv07', SESSION_SECRET: 'x'.repeat(30), SHORT_KEY: 'abc' })
    const key = `sk-${'a1b2c3d4'.repeat(4)}`
    const bearer = `Bearer ${'Ab1.'.repeat(8)}`
    expect(redact('Bearer authentication scheme with dvorak-layout and flow-' + key + ' and disk-' + 'x'.repeat(20) + ' task_' + 'x'.repeat(20) + ' risk_' + 'y'.repeat(20)))
      .toBe('Bearer authentication scheme with dvorak-layout and flow-' + key + ' and disk-' + 'x'.repeat(20) + ' task_' + 'x'.repeat(20) + ' risk_' + 'y'.repeat(20))
    expect(redact(`key ${key}, header ${bearer}, env tok3n-Zq81xv07 and ${'x'.repeat(30)}`))
      .toBe('key <redacted-secret>, header Bearer <redacted-secret>, env <redacted-secret> and <redacted-secret>')
  })

  test('key-like names: distinctive words anywhere, short words by whole part, and no ordinary words', () => {
    for (const name of ['API_KEY', 'SSH_KEY', 'MY_TOKEN', 'APIKEY', 'apiKey', 'githubToken', 'DB_PASS', 'DB_PWD', 'AUTH', 'SSH_AUTH_SOCK', 'SESSION',
      'COOKIE', 'PASSWD', 'AWS_CREDENTIALS', 'GPG_PRIVATE', 'client.secret', 'x-api-key', 'XDG_SESSION_TYPE',
      // No separator, or a digit after the word.
      'PGPASSWORD', 'APITOKEN', 'ACCESSTOKEN', 'OPENAIAPIKEY', 'ANTHROPICAPIKEY', 'SECRETKEY', 'AWSSECRETKEY', 'DBPASS', 'SSHKEY',
      'TOKEN2', 'NPM_TOKEN2', 'KEY2', 'GITHUB_PAT', 'JWT', 'AWS_ACCESSKEY', 'MYPRIVATEKEY']) {
      expect({ name, keyLike: isKeyLikeName(name) }).toEqual({ name, keyLike: true })
    }
    for (const name of ['KEYBOARD', 'MONKEY', 'KEYS_DIR', 'SESSIONNAME', 'PATH', 'HOME', 'PWD', 'OLDPWD', 'PASSTHROUGH', 'AUTHOR', 'PASSAGE',
      'COMPASS', 'PATCH', 'PATTERN', 'SSH_AGENT_PID', 'USER', 'SHELL']) {
      expect({ name, keyLike: isKeyLikeName(name) }).toEqual({ name, keyLike: false })
    }
    const env: Record<string, string> = {
      API_TOKEN: 'abcdefghijklmnopqrstuv', API_KEY: 'correct-horse-battery', SSH_KEY: 'abcdefghij', MY_TOKEN: 'abcDEFghiJKLmnoPQRst',
      DB_PASSWORD: 'correct horse battery', AUTH: 'hunter2hunter2', SESSION: 'sessionvalue', COOKIE: 'cookie=abcdefgh', DB_PASS: 'letmeinletmein',
      DB_PWD: 'passw0rdpassw0rd', PGPASSWORD: 'pgsecretvalue', APITOKEN: 'apitokenvalue', OPENAIAPIKEY: 'openaikeyvalue', SECRETKEY: 'secretkeyvalue',
      DBPASS: 'dbpassvalue', TOKEN2: 'tokentwovalue', KEY2: 'keytwovalue', GITHUB_PAT: 'patvaluepat', PWD: '/work/project', OLDPWD: '/work/previous',
      KEYBOARD: 'dvorak-layout', COMPASS: 'north-east-heading', API_KEY_SHORT_KEY: 'short',
    }
    const redact = providerKeyRedactor(env)
    for (const [name, value] of Object.entries(env)) {
      const expected = ['PWD', 'OLDPWD', 'KEYBOARD', 'COMPASS'].includes(name) || value.length < 8 ? value : '<redacted-secret>'
      expect({ name, text: redact(`value ${value}!`) }).toEqual({ name, text: `value ${expected}!` })
    }
  })

  test('password-type names hide values from 4 characters, other names from 8', () => {
    const redact = providerKeyRedactor({ DB_PASSWORD: 'hunter2', PGPASSWORD: 'qwer', DB_PASS: 'xyz', MY_PWD: 'pw12', API_TOKEN: 'abcdefg', API_KEY: 'abcdefgh' })
    expect(redact('hunter2 qwer xyz pw12 abcdefg abcdefgh')).toBe('<redacted-secret> <redacted-secret> xyz <redacted-secret> abcdefg <redacted-secret>')
  })

  test('provider value shapes are redacted without a key-like name', () => {
    const redact = providerKeyRedactor({})
    // Built at runtime, so no literal credential sits in the repository.
    const samples = [
      `ghp_${'a1B2c3D4e5'.repeat(4)}`, `gho_${'a1B2c3D4e5'.repeat(4)}`, `ghs_${'a1B2c3D4e5'.repeat(4)}`, `ghu_${'a1B2c3D4e5'.repeat(4)}`,
      `github_pat_${'A1b2C3d4E5'.repeat(6)}_${'z9'.repeat(10)}`,
      `AKIA${'Q7W8E9R2T3Y4U5I6'}`, `ASIA${'Q7W8E9R2T3Y4U5I6'}`,
      `xoxb-${'1234567890'}-${'abcdefghij'}`, `xoxp-${'1234567890'}-${'abcdefghij'}`, `xoxa-${'1234567890'}-x`, `xoxr-${'1234567890'}-x`,
    ]
    for (const sample of samples) {
      expect({ sample, out: redact(`before ${sample} after`) }).toEqual({ sample, out: 'before <redacted-secret> after' })
    }
    const ordinary = ['ghost_story_title_of_the_week', 'ghp_short', 'AKIAshort', 'MAKIA' + 'Q7W8E9R2T3Y4U5I6', 'AKIA' + 'Q7W8E9R2T3Y4U5I6X', 'xoxz-1234567890-abcdefghij']
    for (const text of ordinary) expect(redact(text)).toBe(text)
  })

  test('only the password of a URL is redacted', () => {
    const redact = providerKeyRedactor({})
    expect(redact('postgres://app:s3cr3t-pass@db.internal:5432/app')).toBe('postgres://app:<redacted-secret>@db.internal:5432/app')
    expect(redact('REDIS_URL=redis://:x@cache:6379 and rediss://default:abc123@cache:6380/0'))
      .toBe('REDIS_URL=redis://:x@cache:6379 and rediss://default:<redacted-secret>@cache:6380/0')
    expect(redact('https://example.com/path?x=a:b@c and http://user@host/ and https://host:8080/')).toBe('https://example.com/path?x=a:b@c and http://user@host/ and https://host:8080/')
  })

  test('underscore provider keys are redacted by shape, ordinary words are not', () => {
    const redact = providerKeyRedactor({})
    const live = `sk_live_${'a1b2c3d4'.repeat(3)}`
    const lower = `sk_test_${'abcdefgh'.repeat(3)}`
    expect(redact(`${live} ${lower} (pk_live_${'q'.repeat(20)})`)).toBe('<redacted-secret> <redacted-secret> (<redacted-secret>)')
    expect(redact(`task_${'x'.repeat(20)} ask_${'x'.repeat(20)}`)).toBe(`task_${'x'.repeat(20)} ask_${'x'.repeat(20)}`)
  })

  test('a bad configured URL is a flow path error, not a crash', () => {
    const header = 'url: http://127.0.0.1:4173/\n---\n- tapOn: x\n'
    expect(() => checkFlowApp(header, { appId: 'not a url', platform: 'web' })).toThrow(/not on the configured page/)
    // And a bad declared url, with a good configured page.
    expect(() => checkFlowApp('url: not a url\n---\n- tapOn: x\n', { appId: 'http://127.0.0.1:4173', platform: 'web' }))
      .toThrow(/not on the configured page/)
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
