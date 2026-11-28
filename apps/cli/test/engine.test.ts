import { afterEach, describe, expect, test } from 'bun:test'
import { createHash } from 'node:crypto'
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { spawnSync } from 'node:child_process'
import { readFile } from 'node:fs/promises'
import { type MaestroEnginePin } from '@deja-run/maestro'
import { runCli } from '../src/main.ts'
import { renderDoctor, runDoctor, type DoctorProbe } from '../src/doctor.ts'

const temporaries: string[] = []
afterEach(async () => {
  while (temporaries.length) await rm(temporaries.pop()!, { recursive: true, force: true })
})
async function temp(prefix: string) {
  const directory = await mkdtemp(join(tmpdir(), prefix))
  temporaries.push(directory)
  return directory
}

async function fakeRelease() {
  const source = await temp('deja-cli-engine-src-')
  await mkdir(join(source, 'maestro', 'bin'), { recursive: true })
  await writeFile(join(source, 'maestro', 'bin', 'maestro'), '#!/bin/sh\necho 2.12.0\n', { mode: 0o755 })
  const zip = `${source}.zip`
  temporaries.push(zip)
  expect(spawnSync('zip', ['-qr', zip, 'maestro'], { cwd: source }).status).toBe(0)
  const bytes = new Uint8Array(await readFile(zip))
  const pin: MaestroEnginePin = {
    version: '2.12.0',
    sha256: createHash('sha256').update(bytes).digest('hex'),
    url: 'https://example.invalid/maestro.zip',
  }
  let calls = 0
  return { pin, fetch: async () => { calls += 1; return new Response(bytes) }, calls: () => calls }
}

function capture() {
  const out: string[] = []
  const err: string[] = []
  return { io: { stdout: (t: string) => { out.push(t) }, stderr: (t: string) => { err.push(t) } }, out, err }
}

describe('deja engine install', () => {
  test('installs from a fake download, then skips, reporting each in text and JSON', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-cli-home-')
    const first = capture()
    expect(await runCli(['engine', 'install'], first.io, {
      env: { DEJA_HOME: home }, engineInstall: { pin: release.pin, fetch: release.fetch },
    })).toBe(0)
    expect(first.out.join('')).toContain('Maestro 2.12.0 installed')
    const second = capture()
    expect(await runCli(['engine', 'install', '--json'], second.io, {
      env: { DEJA_HOME: home }, engineInstall: { pin: release.pin, fetch: release.fetch },
    })).toBe(0)
    expect(JSON.parse(second.out.join('')).engine).toMatchObject({ version: '2.12.0', status: 'skipped' })
    expect(release.calls()).toBe(1)
  })

  test('a checksum mismatch is exit 2 and leaves nothing installed', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-cli-home-')
    const run = capture()
    expect(await runCli(['engine', 'install'], run.io, {
      env: { DEJA_HOME: home },
      engineInstall: { pin: { ...release.pin, sha256: '0'.repeat(64) }, fetch: release.fetch },
    })).toBe(2)
    expect(run.err.join('')).toContain('Checksum mismatch')
    const retry = capture()
    expect(await runCli(['engine', 'install'], retry.io, {
      env: { DEJA_HOME: home }, engineInstall: { pin: release.pin, fetch: release.fetch },
    })).toBe(0)
  })
})

describe('deja doctor', () => {
  const healthy: DoctorProbe = async (command, args) => {
    if (command === 'java') return { ok: true, output: 'openjdk version "21.0.11" 2026-04-21\n' }
    if (command === 'xcodebuild') return { ok: true, output: 'Xcode 27.2\nBuild version 27B52\n' }
    if (command === 'xcrun') {
      return { ok: true, output: JSON.stringify({ runtimes: [{ name: 'iOS 27.0', version: '27.0', isAvailable: true }] }) }
    }
    if (args[0] === '--version') return { ok: true, output: '2.12.0\n' }
    return { ok: false, output: '' }
  }

  test('no pinned install and no override fails and names deja engine install', async () => {
    const home = await temp('deja-doctor-home-')
    const report = await runDoctor({ home, env: {}, probe: healthy, platform: 'darwin' })
    expect(report.ok).toBe(false)
    expect(report.checks[0]).toMatchObject({ name: 'engine', status: 'fail' })
    expect(report.checks[0].detail).toContain('deja engine install')
  })

  test('an override in range, with Java 21, Xcode 27 and an iOS 27 runtime, is ready', async () => {
    const report = await runDoctor({
      env: { DEJA_MAESTRO: '/opt/maestro' }, home: '/nonexistent', probe: healthy, platform: 'darwin',
    })
    expect(report.ok).toBe(true)
    expect(report.checks.map(c => `${c.name}:${c.status}`)).toEqual([
      'engine:ok', 'java:ok', 'xcode:ok', 'ios-runtime:ok',
    ])
    const text = renderDoctor(report)
    expect(text).toContain('override via DEJA_MAESTRO')
    expect(text).toContain('Maestro 2.12.0')
    expect(text).toContain('Doctor: ready')
  })

  test('an engine outside the range fails, unless allowUntested, which warns', async () => {
    const old: DoctorProbe = async (command, args) =>
      args[0] === '--version' ? { ok: true, output: '2.11.0\n' } : healthy(command, args)
    const failed = await runDoctor({ env: { DEJA_MAESTRO: '/opt/maestro' }, probe: old, platform: 'linux' })
    expect(failed.ok).toBe(false)
    expect(failed.checks[0]).toMatchObject({ name: 'engine', status: 'fail' })
    expect(failed.checks[0].detail).toContain('outside the tested range')
    const allowed = await runDoctor({ env: { DEJA_MAESTRO: '/opt/maestro' }, probe: old, platform: 'linux', allowUntested: true })
    expect(allowed.ok).toBe(true)
    expect(allowed.checks[0].status).toBe('warn')
  })

  test('old Java fails; missing Xcode and runtimes only warn', async () => {
    const weak: DoctorProbe = async (command, args) => {
      if (command === 'java') return { ok: true, output: 'java version "1.8.0_402"\n' }
      if (command === 'xcodebuild' || command === 'xcrun') return { ok: false, output: 'not found' }
      return healthy(command, args)
    }
    const report = await runDoctor({ env: { DEJA_MAESTRO: '/opt/maestro' }, probe: weak, platform: 'darwin' })
    const status = Object.fromEntries(report.checks.map(c => [c.name, c.status]))
    expect(status).toEqual({ engine: 'ok', java: 'fail', xcode: 'warn', 'ios-runtime': 'warn' })
    expect(report.ok).toBe(false)
  })

  test('deja doctor prints the report and exits 0 or 1', async () => {
    const run = capture()
    const code = await runCli(['doctor', '--json', '--config', '/nonexistent/deja.config.json'], run.io, {
      env: { DEJA_MAESTRO: '/opt/maestro' }, doctorProbe: healthy,
    })
    expect(code).toBe(0)
    expect(JSON.parse(run.out.join('')).ok).toBe(true)
  })
})
