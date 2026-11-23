import { afterEach, describe, expect, test } from 'bun:test'
import { createHash } from 'node:crypto'
import { mkdir, mkdtemp, readdir, readFile, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { spawnSync } from 'node:child_process'
import {
  MAESTRO_ENGINE_PIN,
  MaestroEngineError,
  assertSafeEntries,
  checkMaestroRange,
  dejaHome,
  installMaestroEngine,
  normalizeEngineVersion,
  parseMaestroVersion,
  pinnedExecutablePath,
  resolveMaestroEngine,
  verifyInstalledEngine,
  type MaestroEnginePin,
  type ZipExtractor,
} from '../src/index.ts'

const temporaries: string[] = []
afterEach(async () => {
  while (temporaries.length) await rm(temporaries.pop()!, { recursive: true, force: true })
})

async function temp(prefix: string) {
  const directory = await mkdtemp(join(tmpdir(), prefix))
  temporaries.push(directory)
  return directory
}

/** A fake Maestro release: a real zip with maestro/bin/maestro, and a pin that matches it. */
async function fakeRelease(extra?: (root: string) => Promise<void>) {
  const source = await temp('deja-engine-src-')
  await mkdir(join(source, 'maestro', 'bin'), { recursive: true })
  await writeFile(join(source, 'maestro', 'bin', 'maestro'), '#!/bin/sh\necho 2.12.0\n', { mode: 0o755 })
  await extra?.(source)
  const zip = join(source, '..', `${source.split('/').at(-1)}.zip`)
  const built = spawnSync('zip', ['-qr', zip, 'maestro'], { cwd: source })
  if (built.status !== 0) throw new Error(`zip failed: ${built.stderr}`)
  temporaries.push(zip)
  const bytes = new Uint8Array(await readFile(zip))
  const pin: MaestroEnginePin = {
    version: '2.12.0',
    sha256: createHash('sha256').update(bytes).digest('hex'),
    url: 'https://example.invalid/maestro.zip',
  }
  let calls = 0
  const fetch = async () => {
    calls += 1
    return new Response(bytes, { status: 200 })
  }
  return { pin, fetch, calls: () => calls, bytes }
}

async function entries(home: string) {
  return readdir(join(home, 'maestro')).catch(() => [] as string[])
}

describe('the pin', () => {
  test('is Maestro 2.12.0 with the official zip sha256 and release URL', () => {
    expect(MAESTRO_ENGINE_PIN.version).toBe('2.12.0')
    expect(MAESTRO_ENGINE_PIN.sha256).toBe('a6044a84542f522f434c5dc3d609e60c7446d423f3ed1b2ae513f5c48663ec5e')
    expect(MAESTRO_ENGINE_PIN.url).toBe(
      'https://github.com/mobile-dev-inc/Maestro/releases/download/cli-2.12.0/maestro.zip',
    )
    expect(Object.isFrozen(MAESTRO_ENGINE_PIN)).toBe(true)
  })

  test('DEJA_HOME moves the install root; the default is under the user home', () => {
    expect(dejaHome({ DEJA_HOME: '/tmp/x' })).toBe('/tmp/x')
    expect(dejaHome({})).toMatch(/\.local\/share\/deja$/)
  })
})

describe('version range', () => {
  test('accepts 2.12.x only', () => {
    expect(checkMaestroRange('2.12.0')).toBe('in-range')
    expect(checkMaestroRange('2.12.9')).toBe('in-range')
    expect(checkMaestroRange('2.11.0')).toBe('out-of-range')
    expect(checkMaestroRange('2.13.0')).toBe('out-of-range')
    expect(checkMaestroRange('3.0.0')).toBe('out-of-range')
    expect(checkMaestroRange('nonsense')).toBe('unknown')
    expect(checkMaestroRange(null)).toBe('unknown')
  })

  test('parses the first x.y.z and normalizes to strict semver', () => {
    expect(parseMaestroVersion('Maestro CLI 2.12.0\n')).toEqual({ major: 2, minor: 12, patch: 0 })
    expect(normalizeEngineVersion('2.12.0\n')).toBe('2.12.0')
    expect(normalizeEngineVersion('v2.12.0-rc1')).toBe('2.12.0')
    expect(normalizeEngineVersion('no version')).toBeNull()
  })
})

describe('installMaestroEngine against a fake download', () => {
  test('downloads, checks the sha256, extracts and renames into the versioned folder', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    const result = await installMaestroEngine({ home, pin: release.pin, fetch: release.fetch })
    expect(result.status).toBe('installed')
    expect(result.executable).toBe(pinnedExecutablePath(home, release.pin))
    expect(await readFile(result.executable, 'utf8')).toContain('2.12.0')
    expect(await verifyInstalledEngine(home, release.pin)).toBe(true)
    expect(await entries(home)).toEqual(['2.12.0'])
    expect(await readdir(join(home, 'maestro', '2.12.0'))).toContain('maestro.zip')
  })

  test('a second install is skipped without a download', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    await installMaestroEngine({ home, pin: release.pin, fetch: release.fetch })
    const again = await installMaestroEngine({ home, pin: release.pin, fetch: release.fetch })
    expect(again.status).toBe('skipped')
    expect(release.calls()).toBe(1)
  })

  test('a checksum mismatch fails closed and leaves nothing behind', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    const wrong = { ...release.pin, sha256: 'f'.repeat(64) }
    await expect(installMaestroEngine({ home, pin: wrong, fetch: release.fetch }))
      .rejects.toThrow(/Checksum mismatch.*Nothing was installed/)
    expect(await entries(home)).toEqual([])
    expect(await verifyInstalledEngine(home, wrong)).toBe(false)
  })

  test('an HTTP error fails closed and leaves nothing behind', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    await expect(installMaestroEngine({
      home, pin: release.pin, fetch: async () => new Response('nope', { status: 404 }),
    })).rejects.toThrow(/HTTP 404/)
    expect(await entries(home)).toEqual([])
  })

  test('a network error fails closed and leaves nothing behind', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    await expect(installMaestroEngine({
      home, pin: release.pin, fetch: async () => { throw new Error('offline') },
    })).rejects.toThrow('offline')
    expect(await entries(home)).toEqual([])
  })

  test('never replaces an existing folder that does not match the pin', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    const folder = join(home, 'maestro', '2.12.0')
    await mkdir(folder, { recursive: true })
    await writeFile(join(folder, 'precious.txt'), 'keep me')
    await expect(installMaestroEngine({ home, pin: release.pin, fetch: release.fetch }))
      .rejects.toThrow(/will not replace it/)
    expect(await readFile(join(folder, 'precious.txt'), 'utf8')).toBe('keep me')
    expect(release.calls()).toBe(0)
    expect(await entries(home)).toEqual(['2.12.0'])
  })

  test('a folder whose kept zip was tampered with is not valid and is not replaced', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    await installMaestroEngine({ home, pin: release.pin, fetch: release.fetch })
    await writeFile(join(home, 'maestro', '2.12.0', 'maestro.zip'), 'tampered')
    expect(await verifyInstalledEngine(home, release.pin)).toBe(false)
    await expect(installMaestroEngine({ home, pin: release.pin, fetch: release.fetch }))
      .rejects.toThrow(/will not replace it/)
    expect(await readFile(join(home, 'maestro', '2.12.0', 'maestro.zip'), 'utf8')).toBe('tampered')
  })

  test('rejects zip-slip entry names before extracting anything', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    let extracted = false
    const extractor: ZipExtractor = {
      list: async () => ['maestro/', 'maestro/../../evil'],
      extract: async () => { extracted = true },
    }
    await expect(installMaestroEngine({ home, pin: release.pin, fetch: release.fetch, extractor }))
      .rejects.toThrow(/unsafe path/)
    expect(extracted).toBe(false)
    expect(await entries(home)).toEqual([])
    expect(() => assertSafeEntries(['/etc/passwd'])).toThrow(MaestroEngineError)
    expect(() => assertSafeEntries(['a\\..\\b'])).toThrow(MaestroEngineError)
    expect(() => assertSafeEntries(['C:/x'])).toThrow(MaestroEngineError)
    expect(() => assertSafeEntries(['maestro/bin/maestro', 'maestro/lib/a.jar'])).not.toThrow()
  })

  test('rejects a symlink that leaves the install folder', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    const extractor: ZipExtractor = {
      list: async () => ['maestro/bin/maestro', 'maestro/link'],
      extract: async (_zip, destination) => {
        await mkdir(join(destination, 'maestro', 'bin'), { recursive: true })
        await writeFile(join(destination, 'maestro', 'bin', 'maestro'), 'x')
        await symlink('/etc', join(destination, 'maestro', 'link'))
      },
    }
    await expect(installMaestroEngine({ home, pin: release.pin, fetch: release.fetch, extractor }))
      .rejects.toThrow(/link that leaves/)
    expect(await entries(home)).toEqual([])
  })

  test('rejects an archive without maestro/bin/maestro', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    const extractor: ZipExtractor = {
      list: async () => ['readme.txt'],
      extract: async (_zip, destination) => { await writeFile(join(destination, 'readme.txt'), 'x') },
    }
    await expect(installMaestroEngine({ home, pin: release.pin, fetch: release.fetch, extractor }))
      .rejects.toThrow(/no maestro\/bin\/maestro/)
    expect(await entries(home)).toEqual([])
  })

  test('a failing extractor leaves nothing behind', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    const extractor: ZipExtractor = {
      list: async () => ['maestro/bin/maestro'],
      extract: async () => { throw new Error('unzip crashed') },
    }
    await expect(installMaestroEngine({ home, pin: release.pin, fetch: release.fetch, extractor }))
      .rejects.toThrow('unzip crashed')
    expect(await entries(home)).toEqual([])
  })
})

describe('resolveMaestroEngine', () => {
  const probe = (output: string, calls: string[] = []) => async (executable: string) => {
    calls.push(executable)
    return output
  }

  test('defaults to the pinned install and does not execute it', async () => {
    const release = await fakeRelease()
    const home = await temp('deja-home-')
    await installMaestroEngine({ home, pin: release.pin, fetch: release.fetch })
    const calls: string[] = []
    const engine = await resolveMaestroEngine({ home, pin: release.pin, env: {}, probeVersion: probe('9.9.9', calls) })
    expect(engine).toEqual({
      executable: pinnedExecutablePath(home, release.pin), source: 'pinned', version: '2.12.0', range: 'in-range',
    })
    expect(calls).toEqual([])
  })

  test('refuses to resolve a missing pinned install and names the fix', async () => {
    const home = await temp('deja-home-')
    await expect(resolveMaestroEngine({ home, env: {} })).rejects.toThrow(/Run "deja engine install"/)
  })

  test('maestro.executable beats DEJA_MAESTRO, and DEJA_MAESTRO beats the pin', async () => {
    const home = await temp('deja-home-')
    const configured = await resolveMaestroEngine({
      home, configExecutable: '/opt/a', env: { DEJA_MAESTRO: '/opt/b' }, probeVersion: probe('2.12.0'),
    })
    expect(configured).toMatchObject({ executable: '/opt/a', source: 'override', overrideVia: 'config', version: '2.12.0' })
    const fromEnv = await resolveMaestroEngine({ home, env: { DEJA_MAESTRO: '/opt/b' }, probeVersion: probe('2.11.0') })
    expect(fromEnv).toMatchObject({ executable: '/opt/b', overrideVia: 'env', version: '2.11.0', range: 'out-of-range' })
  })

  test('an override with no readable version is unknown; one that cannot start is unlaunchable', async () => {
    const unknown = await resolveMaestroEngine({ env: { DEJA_MAESTRO: '/opt/b' }, probeVersion: probe('garbage') })
    expect(unknown).toMatchObject({ version: null, range: 'unknown' })
    expect(unknown.unlaunchable).toBeUndefined()
    const broken = await resolveMaestroEngine({
      env: { DEJA_MAESTRO: '/opt/b' },
      probeVersion: async () => { throw new Error('ENOENT') },
    })
    expect(broken).toMatchObject({ version: null, range: 'unknown', unlaunchable: true })
  })
})
