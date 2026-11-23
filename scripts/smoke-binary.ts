#!/usr/bin/env bun
import { chmod, copyFile, mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises'
import { homedir, tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { spawnSync } from 'node:child_process'

/*
 * Smoke check for a compiled deja binary (DEJA-15). Runs it the way a bash host would: an empty
 * environment with only PATH=/usr/bin:/bin and HOME, no Bun on PATH, a temporary DEJA_HOME (never
 * the real one) and the repository's fake Maestro and fake OpenClaw fixtures.
 *
 * Usage: bun scripts/smoke-binary.ts <binary> [--expect-version <x.y.z>]
 * Only the fixtures need Bun to start, so they are copied with a shebang that names this Bun by
 * absolute path. The binary under test never sees Bun.
 */

const root = resolve(import.meta.dir, '..')
const argv = Bun.argv.slice(2)
const binary = argv[0] ? resolve(argv[0]) : ''
if (!binary) throw new Error('Usage: bun scripts/smoke-binary.ts <binary> [--expect-version <x.y.z>]')
const expectedVersion = argv.includes('--expect-version') ? argv[argv.indexOf('--expect-version') + 1] : undefined

const cleanPath = '/usr/bin:/bin'
const lookup = spawnSync('/bin/sh', ['-c', 'command -v bun'], { env: { PATH: cleanPath }, encoding: 'utf8' })
if (lookup.status === 0) throw new Error(`bun is on ${cleanPath} (${lookup.stdout.trim()}), so this check would prove nothing`)

const temporary = await realpath(await mkdtemp(join(tmpdir(), 'deja-binary-smoke-')))
const workspace = join(temporary, 'workspace')
const dejaHome = join(temporary, 'deja-home')
const failures: string[] = []
function check(label: string, ok: boolean, detail = '') {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${label}${ok ? '' : ` ${detail}`}`)
  if (!ok) failures.push(label)
}

function execute(args: string[], extra: Record<string, string> = {}) {
  const result = spawnSync(binary, args, {
    cwd: workspace,
    encoding: 'utf8',
    env: {
      PATH: cleanPath,
      HOME: homedir(),
      DEJA_HOME: dejaHome,
      ANDROID_HOME: join(workspace, 'fixture-sdk'),
      DEJA_MAESTRO: '',
      ...extra,
    },
    timeout: 60_000,
  })
  return { status: result.status, stdout: result.stdout ?? '', stderr: result.stderr ?? '' }
}

async function fixture(source: string, destination: string) {
  const text = await readFile(resolve(root, source), 'utf8')
  await writeFile(destination, text.replace(/^#!.*\n/, `#!${process.execPath}\n`))
  await chmod(destination, 0o755)
}

try {
  await mkdir(join(workspace, 'app/build'), { recursive: true })
  await mkdir(join(workspace, 'flows'), { recursive: true })
  await mkdir(join(workspace, 'scripts'), { recursive: true })
  await mkdir(join(workspace, 'fixture-sdk/platform-tools'), { recursive: true })
  await writeFile(join(workspace, 'app/build/app.apk'), 'deterministic smoke build')
  await writeFile(join(workspace, 'scripts/maestro-mcp-proxy.ts'), '// fixture: no MCP process is started\n')
  await writeFile(join(workspace, 'fixture-sdk/platform-tools/adb'), '#!/bin/sh\nexit 0\n', { mode: 0o755 })
  await writeFile(join(workspace, 'flows/candidate.yaml'), `appId: dev.deja.checkout
---
- launchApp:
    clearState: true
- assertVisible: Déjà Shop
- assertVisible: Order confirmed
`)
  const fakeMaestro = join(temporary, 'fake-maestro.cjs')
  const fakeOpenClaw = join(temporary, 'fake-openclaw.cjs')
  await fixture('packages/maestro/test/fixtures/fake-maestro.cjs', fakeMaestro)
  await fixture('packages/agent-openclaw/test/fixtures/fake-openclaw.cjs', fakeOpenClaw)
  await copyFile(binary, join(temporary, 'deja'))
  await chmod(join(temporary, 'deja'), 0o755)
  await writeFile(join(workspace, 'deja.config.json'), `${JSON.stringify({
    workspaceRoot: '.',
    appId: 'dev.deja.checkout',
    platform: 'android',
    buildPath: 'app/build/app.apk',
    device: 'fixture-device',
    openClawModel: 'cerebras/moonshotai-kimi-k3.1-pro-trial',
    openClawBaseUrl: 'https://api.cerebras.ai/v1',
    explorationTimeoutSeconds: 30,
    verificationTimeoutMs: 2000,
    fixtureCredentials: [{ label: 'Fixture password', value: 'testing123' }],
    maestro: { executable: fakeMaestro, timeoutMs: 30_000 },
    openClaw: { executable: fakeOpenClaw, mcpProxy: 'scripts/maestro-mcp-proxy.ts' },
  }, null, 2)}\n`)

  const version = execute(['--version'])
  console.log(version.stdout.trim())
  check('--version prints version, commit and contract',
    version.status === 0 && /^deja \S+ \(commit \S+, contract \d+\)\n$/.test(version.stdout), version.stderr)
  if (expectedVersion) check(`--version is ${expectedVersion}`, version.stdout.startsWith(`deja ${expectedVersion} `), version.stdout)

  const help = execute(['--help'])
  check('--help prints usage', help.status === 0 && help.stdout.includes('Usage:') && help.stdout.includes('deja run <goal>'), help.stderr)

  // No pinned engine is installed in the temporary DEJA_HOME, so doctor must report that and exit 1.
  const doctor = execute(['doctor', '--json'], { DEJA_MAESTRO: fakeMaestro })
  let doctorJson: { checks?: { name: string }[] } = {}
  try { doctorJson = JSON.parse(doctor.stdout) } catch { /* reported below */ }
  check('doctor --json prints a JSON report', Array.isArray(doctorJson.checks) && doctorJson.checks.length > 0, `${doctor.stdout}${doctor.stderr}`)

  const cold = execute(['run', 'checkout', '--config', 'deja.config.json', '--summary'])
  check('cold run passes and records a walk', cold.status === 0 && cold.stdout.includes('Cache status: miss'), `${cold.stdout}${cold.stderr}`)
  const warm = execute(['run', 'checkout', '--config', 'deja.config.json', '--summary'])
  console.log(warm.stdout.trim().split('\n').slice(-1)[0])
  check('warm run replays from memory with zero model calls',
    warm.status === 0 && warm.stdout.includes('exact local hit') && warm.stdout.includes('Model calls: 0'), `${warm.stdout}${warm.stderr}`)
  const list = execute(['list', '--json'])
  let walks = 0
  try { walks = JSON.parse(list.stdout).walks.length } catch { /* reported below */ }
  check('list reads the stored walk back', list.status === 0 && walks === 1, `${list.stdout}${list.stderr}`)
} finally {
  await rm(temporary, { recursive: true, force: true })
}

if (failures.length) {
  console.error(`Binary smoke check failed: ${failures.join('; ')}`)
  process.exit(1)
}
console.log('Binary smoke check passed')
