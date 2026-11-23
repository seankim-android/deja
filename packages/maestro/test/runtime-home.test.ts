import { describe, expect, test } from 'bun:test'
import { chmod, lstat, mkdir, mkdtemp, readdir, readFile, readlink, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  MAESTRO_ENGINE_PIN,
  ProcessMaestroTransport,
  maestroChildEnv,
  maestroRuntimeHome,
  pinnedExecutablePath,
  probeMaestroVersion,
  runMaestro,
  withoutJvmNotices,
} from '../src/index.ts'

async function temporary() {
  return mkdtemp(join(tmpdir(), 'deja-runtime-'))
}

describe('maestroChildEnv', () => {
  test('the pinned engine gets $DEJA_HOME/runtime/<version>; HOME stays real', async () => {
    const root = await temporary()
    try {
      const home = join(root, 'deja')
      const env = maestroChildEnv(pinnedExecutablePath(home), {
        env: { HOME: '/real/home', PATH: '/bin' }, home, realHome: join(root, 'none'),
      })
      const runtime = join(home, 'runtime', MAESTRO_ENGINE_PIN.version)
      expect(env.JAVA_TOOL_OPTIONS).toBe(`-Duser.home=${runtime}`)
      expect(env.HOME).toBe('/real/home')
      expect(env.PATH).toBe('/bin')
      expect(env.MAESTRO_CLI_NO_ANALYTICS).toBe('1')
      expect(env.MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED).toBe('true')
      expect((await lstat(runtime)).isDirectory()).toBe(true)
    } finally {
      await rm(root, { recursive: true, force: true })
    }
  })

  test('an override engine is isolated too, in runtime/override', () => {
    const home = '/deja'
    expect(maestroRuntimeHome('/usr/local/bin/maestro', { home })).toBe('/deja/runtime/override')
  })

  test('keeps an existing JAVA_TOOL_OPTIONS and appends ours last', () => {
    const env = maestroChildEnv('/x/maestro', {
      env: { JAVA_TOOL_OPTIONS: '-Xmx512m -Duser.home=/elsewhere' }, home: '/deja', skipCreate: true,
    })
    expect(env.JAVA_TOOL_OPTIONS).toBe('-Xmx512m -Duser.home=/elsewhere -Duser.home=/deja/runtime/override')
  })

  test('quotes a runtime path that has spaces', () => {
    const env = maestroChildEnv('/x/maestro', { env: {}, home: '/my deja', skipCreate: true })
    expect(env.JAVA_TOOL_OPTIONS).toBe('"-Duser.home=/my deja/runtime/override"')
  })

  test('links the real crash reports folder into the runtime home, read-only use', async () => {
    const root = await temporary()
    try {
      const realHome = join(root, 'real')
      const reports = join(realHome, 'Library', 'Logs', 'DiagnosticReports')
      await mkdir(reports, { recursive: true })
      maestroChildEnv('/x/maestro', { env: {}, home: join(root, 'deja'), realHome })
      const link = join(root, 'deja', 'runtime', 'override', 'Library', 'Logs', 'DiagnosticReports')
      expect(await readlink(link)).toBe(reports)
      // A second call is harmless.
      maestroChildEnv('/x/maestro', { env: {}, home: join(root, 'deja'), realHome })
    } finally {
      await rm(root, { recursive: true, force: true })
    }
  })

  test('withoutJvmNotices drops the JVM banner so its path is never read as a version', () => {
    const banner = 'Picked up JAVA_TOOL_OPTIONS: -Duser.home=/d/runtime/2.12.0\n2.11.0\n'
    expect(withoutJvmNotices(banner)).toBe('2.11.0\n')
  })
})

/**
 * A fake Maestro that behaves like the JVM: it writes its state under `user.home`, which it reads
 * from JAVA_TOOL_OPTIONS, and falls back to $HOME the way an unisolated run would.
 */
async function fakeJvmMaestro(directory: string) {
  const path = join(directory, 'fake-jvm-maestro.cjs')
  await writeFile(path, `#!/usr/bin/env bun
const { mkdirSync, writeFileSync } = require('node:fs')
const { join } = require('node:path')
const match = /-Duser\\.home=(?:"([^"]*)"|(\\S+))/g
let userHome = process.env.HOME
for (const m of (process.env.JAVA_TOOL_OPTIONS || '').matchAll(match)) userHome = m[1] ?? m[2]
mkdirSync(join(userHome, '.maestro'), { recursive: true })
writeFileSync(join(userHome, '.maestro', 'sessions'), 'heartbeat')
if (process.argv.includes('--version')) console.log('Picked up JAVA_TOOL_OPTIONS: ' + process.env.JAVA_TOOL_OPTIONS + '\\n2.11.5')
else if (process.argv.includes('hierarchy')) console.log(JSON.stringify({ attributes: {}, children: [] }))
else console.log('ok')
`)
  await chmod(path, 0o755)
  return path
}

describe('a Maestro run never touches the real HOME/.maestro', () => {
  test('runMaestro, ProcessMaestroTransport and the version probe write only under the runtime home', async () => {
    const root = await temporary()
    const savedHome = process.env.HOME
    const savedDeja = process.env.DEJA_HOME
    try {
      const fakeRealHome = join(root, 'real-home')
      await mkdir(join(fakeRealHome, '.maestro'), { recursive: true })
      const dejaHome = join(root, 'deja')
      process.env.HOME = fakeRealHome
      process.env.DEJA_HOME = dejaHome
      const executable = await fakeJvmMaestro(root)
      const workspaceRoot = join(root, 'ws')
      await mkdir(join(workspaceRoot, 'flows'), { recursive: true })
      await writeFile(join(workspaceRoot, 'flows', 'a.yaml'), '# fixture\n')

      const run = await runMaestro({
        executable, device: 'd', platform: 'ios', workspaceRoot, flowPath: 'flows/a.yaml', timeoutMs: 30_000,
      })
      expect(run.passed).toBe(true)
      const transport = new ProcessMaestroTransport({ executable, device: 'd', timeoutMs: 30_000 })
      await transport.hierarchy()
      await transport.runFlow('appId: a\n---\n- tapOn: x\n')
      expect(normalizeVersion(await probeMaestroVersion(executable))).toBe('2.11.5')

      expect(await readdir(join(fakeRealHome, '.maestro'))).toEqual([])
      expect(await readdir(fakeRealHome)).toEqual(['.maestro'])
      const written = join(dejaHome, 'runtime', 'override', '.maestro', 'sessions')
      expect(await readFile(written, 'utf8')).toBe('heartbeat')
    } finally {
      if (savedHome === undefined) delete process.env.HOME
      else process.env.HOME = savedHome
      if (savedDeja === undefined) delete process.env.DEJA_HOME
      else process.env.DEJA_HOME = savedDeja
      await rm(root, { recursive: true, force: true })
    }
  })
})

function normalizeVersion(text: string) {
  return /\d+\.\d+\.\d+/.exec(text)?.[0]
}
