import { spawn } from 'node:child_process'
import { createHash, randomBytes } from 'node:crypto'
import { createReadStream, createWriteStream } from 'node:fs'
import { lstat, mkdir, readdir, readlink, realpath, rename, rm, stat } from 'node:fs/promises'
import { homedir } from 'node:os'
import { dirname, isAbsolute, join, resolve, sep } from 'node:path'
import { Readable } from 'node:stream'
import { pipeline } from 'node:stream/promises'
import { maestroChildEnv } from './runtime-home.ts'

/**
 * The one Maestro release this Déjà release runs by default. No silent upgrades: a new pin is a
 * Déjà release with iOS evidence. The sha256 is the digest of the release asset `maestro.zip` on
 * https://github.com/mobile-dev-inc/Maestro/releases/tag/cli-2.12.0.
 */
export const MAESTRO_ENGINE_PIN = Object.freeze({
  version: '2.12.0',
  sha256: 'a6044a84542f522f434c5dc3d609e60c7446d423f3ed1b2ae513f5c48663ec5e',
  url: 'https://github.com/mobile-dev-inc/Maestro/releases/download/cli-2.12.0/maestro.zip',
})

export type MaestroEnginePin = { version: string; sha256: string; url: string }

/** Tested range: the pinned minor release. Anything else is refused unless explicitly allowed. */
export const MAESTRO_TESTED_RANGE = Object.freeze({ min: '2.12.0', maxExclusive: '2.13.0' })

const MAX_DOWNLOAD_BYTES = 1_000_000_000

export class MaestroEngineError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'MaestroEngineError'
  }
}

export function dejaHome(env: NodeJS.ProcessEnv = process.env) {
  const configured = env.DEJA_HOME?.trim()
  return configured ? resolve(configured) : join(homedir(), '.local', 'share', 'deja')
}

export function engineDirectory(home: string, version: string) {
  return join(home, 'maestro', version)
}

export function pinnedExecutablePath(home: string, pin: MaestroEnginePin = MAESTRO_ENGINE_PIN) {
  return join(engineDirectory(home, pin.version), 'maestro', 'bin', 'maestro')
}

/** Parses `2.12.0` (optionally followed by text) into numbers; null when there is no x.y.z. */
export function parseMaestroVersion(text: string) {
  const match = /(?:^|[^0-9.])(\d{1,6})\.(\d{1,6})\.(\d{1,6})(?![0-9])/.exec(text)
  if (!match) return null
  return { major: Number(match[1]), minor: Number(match[2]), patch: Number(match[3]) }
}

function compare(a: ReturnType<typeof parseMaestroVersion> & {}, b: typeof a) {
  return a.major - b.major || a.minor - b.minor || a.patch - b.patch
}

export type EngineRangeStatus = 'in-range' | 'out-of-range' | 'unknown'

export function checkMaestroRange(
  version: string | null,
  range: { min: string; maxExclusive: string } = MAESTRO_TESTED_RANGE,
): EngineRangeStatus {
  const parsed = version ? parseMaestroVersion(version) : null
  if (!parsed) return 'unknown'
  const min = parseMaestroVersion(range.min)!
  const max = parseMaestroVersion(range.maxExclusive)!
  return compare(parsed, min) >= 0 && compare(parsed, max) < 0 ? 'in-range' : 'out-of-range'
}

/** The version string recorded in reports and the summary line: strict semver or null. */
export function normalizeEngineVersion(text: string | null | undefined) {
  if (!text) return null
  const parsed = parseMaestroVersion(text)
  return parsed ? `${parsed.major}.${parsed.minor}.${parsed.patch}` : null
}

export type ZipExtractor = {
  /** Entry names exactly as stored in the archive. */
  list(zipPath: string): Promise<string[]>
  extract(zipPath: string, destination: string): Promise<void>
}

function run(command: string, args: string[]) {
  return new Promise<string>((resolveRun, reject) => {
    const child = spawn(command, args, { stdio: ['ignore', 'pipe', 'pipe'] })
    let out = ''
    let err = ''
    child.stdout.setEncoding('utf8')
    child.stderr.setEncoding('utf8')
    child.stdout.on('data', (chunk: string) => { out += chunk })
    child.stderr.on('data', (chunk: string) => { err += chunk })
    child.once('error', reject)
    child.once('close', code => {
      if (code === 0) resolveRun(out)
      else reject(new Error(`${command} exited with ${String(code)}: ${err.trim().slice(0, 300)}`))
    })
  })
}

/** Uses the system `unzip`. No library, so the CLI gains no dependency. */
export const systemUnzip: ZipExtractor = {
  async list(zipPath) {
    return (await run('unzip', ['-Z1', zipPath])).split('\n').filter(Boolean)
  },
  async extract(zipPath, destination) {
    await run('unzip', ['-q', zipPath, '-d', destination])
  },
}

/** Rejects entry names that could write outside the extraction directory. */
export function assertSafeEntries(entries: string[]) {
  if (entries.length === 0) throw new MaestroEngineError('The Maestro archive is empty')
  for (const entry of entries) {
    const normalized = entry.replaceAll('\\', '/')
    if (isAbsolute(entry) || normalized.startsWith('/') || /^[A-Za-z]:/.test(normalized) ||
        normalized.split('/').includes('..') || entry.includes('\0')) {
      throw new MaestroEngineError(`The Maestro archive has an unsafe path: ${JSON.stringify(entry)}`)
    }
  }
}

/** After extraction: every entry must be a regular file or directory, or a symlink that stays inside. */
async function assertContained(root: string) {
  const canonicalRoot = await realpath(root)
  async function visit(directory: string) {
    for (const name of await readdir(directory)) {
      const path = join(directory, name)
      const info = await lstat(path)
      if (info.isSymbolicLink()) {
        const target = resolve(dirname(path), await readlink(path))
        if (target !== canonicalRoot && !target.startsWith(`${canonicalRoot}${sep}`)) {
          throw new MaestroEngineError(`The Maestro archive has a link that leaves the install directory: ${name}`)
        }
      } else if (info.isDirectory()) {
        await visit(path)
      }
    }
  }
  await visit(canonicalRoot)
}

async function sha256File(path: string) {
  const hash = createHash('sha256')
  for await (const chunk of createReadStream(path)) hash.update(chunk as Buffer)
  return hash.digest('hex')
}

export type FetchLike = (url: string) => Promise<Response>

export type InstallEngineOptions = {
  home?: string
  pin?: MaestroEnginePin
  fetch?: FetchLike
  extractor?: ZipExtractor
}

export type InstallEngineResult = {
  version: string
  directory: string
  executable: string
  /** "skipped": a checksum-valid install was already there, nothing was downloaded. */
  status: 'installed' | 'skipped'
}

async function exists(path: string) {
  return stat(path).then(() => true, () => false)
}

/** A folder is valid when its kept zip matches the pin and the executable is there. */
export async function verifyInstalledEngine(home: string, pin: MaestroEnginePin = MAESTRO_ENGINE_PIN) {
  const directory = engineDirectory(home, pin.version)
  const zip = join(directory, 'maestro.zip')
  if (!(await exists(pinnedExecutablePath(home, pin))) || !(await exists(zip))) return false
  return (await sha256File(zip)) === pin.sha256
}

/**
 * Installs the pinned Maestro. Download and extraction happen in a temp folder beside the target;
 * the folder is renamed into place only after the checksum and the archive checks pass, so a
 * failure never leaves a partial install. An existing folder is never replaced or deleted.
 */
export async function installMaestroEngine(options: InstallEngineOptions = {}): Promise<InstallEngineResult> {
  const home = options.home ?? dejaHome()
  const pin = options.pin ?? MAESTRO_ENGINE_PIN
  const fetchImpl: FetchLike = options.fetch ?? (url => fetch(url))
  const extractor = options.extractor ?? systemUnzip
  const directory = engineDirectory(home, pin.version)
  const executable = pinnedExecutablePath(home, pin)

  if (await exists(directory)) {
    if (await verifyInstalledEngine(home, pin)) {
      return { version: pin.version, directory, executable, status: 'skipped' }
    }
    throw new MaestroEngineError(
      `${directory} exists but does not match Maestro ${pin.version} (checksum or files). ` +
      'Déjà will not replace it. Move or remove that folder, then run "deja engine install" again.',
    )
  }

  const parent = dirname(directory)
  await mkdir(parent, { recursive: true })
  const temporary = join(parent, `.install-${pin.version}-${process.pid}-${randomBytes(4).toString('hex')}`)
  await mkdir(temporary)
  try {
    const zip = join(temporary, 'maestro.zip')
    const response = await fetchImpl(pin.url)
    if (!response.ok || !response.body) {
      throw new MaestroEngineError(`Download failed: HTTP ${response.status} for ${pin.url}`)
    }
    const declared = Number(response.headers.get('content-length') ?? 0)
    if (declared > MAX_DOWNLOAD_BYTES) throw new MaestroEngineError('Download is larger than the 1 GB limit')
    const hash = createHash('sha256')
    let received = 0
    const source = Readable.fromWeb(response.body as import('node:stream/web').ReadableStream<Uint8Array>)
    source.on('data', (chunk: Buffer) => {
      received += chunk.length
      hash.update(chunk)
      if (received > MAX_DOWNLOAD_BYTES) source.destroy(new MaestroEngineError('Download is larger than the 1 GB limit'))
    })
    await pipeline(source, createWriteStream(zip))
    const actual = hash.digest('hex')
    if (actual !== pin.sha256) {
      throw new MaestroEngineError(
        `Checksum mismatch for Maestro ${pin.version}: expected ${pin.sha256}, got ${actual}. Nothing was installed.`,
      )
    }

    assertSafeEntries(await extractor.list(zip))
    await extractor.extract(zip, temporary)
    await assertContained(temporary)
    if (!(await exists(join(temporary, 'maestro', 'bin', 'maestro')))) {
      throw new MaestroEngineError('The Maestro archive has no maestro/bin/maestro')
    }

    try {
      await rename(temporary, directory)
    } catch (error) {
      // Another install may have won the race. Accept it only if it is valid.
      if (await verifyInstalledEngine(home, pin)) {
        await rm(temporary, { recursive: true, force: true })
        return { version: pin.version, directory, executable, status: 'skipped' }
      }
      throw error
    }
    return { version: pin.version, directory, executable, status: 'installed' }
  } catch (error) {
    await rm(temporary, { recursive: true, force: true })
    throw error
  }
}

export type EngineSource = 'pinned' | 'override'

export type ResolvedEngine = {
  executable: string
  source: EngineSource
  /** Where an override came from. */
  overrideVia?: 'config' | 'env'
  /** Strict x.y.z, or null when it could not be read. */
  version: string | null
  range: EngineRangeStatus
  /** The override could not be started for `--version`. The run then fails as an ordinary launch failure. */
  unlaunchable?: boolean
}

export type ResolveEngineOptions = {
  /** `maestro.executable` from the config file. */
  configExecutable?: string
  env?: NodeJS.ProcessEnv
  home?: string
  pin?: MaestroEnginePin
  /** Runs `<executable> --version` and returns its output. Seam for tests. */
  probeVersion?: (executable: string) => Promise<string>
}

/** The JVM prints "Picked up JAVA_TOOL_OPTIONS: ..." (with our runtime path) on stderr. It is not version text. */
export function withoutJvmNotices(output: string) {
  return output.split('\n').filter(line => !/^(NOTE: )?Picked up (JAVA_TOOL_OPTIONS|JDK_JAVA_OPTIONS|_JAVA_OPTIONS)/.test(line)).join('\n')
}

export function probeMaestroVersion(executable: string, timeoutMs = 20_000) {
  return new Promise<string>((resolveProbe, reject) => {
    const child = spawn(executable, ['--version'], {
      stdio: ['ignore', 'pipe', 'pipe'],
      env: maestroChildEnv(executable),
    })
    let out = ''
    child.stdout.setEncoding('utf8')
    child.stderr.setEncoding('utf8')
    child.stdout.on('data', (chunk: string) => { out += chunk })
    child.stderr.on('data', (chunk: string) => { out += chunk })
    const timer = setTimeout(() => {
      child.kill('SIGKILL')
      reject(new Error(`"${executable} --version" did not finish within ${timeoutMs} ms`))
    }, timeoutMs)
    child.once('error', error => { clearTimeout(timer); reject(error) })
    child.once('close', () => { clearTimeout(timer); resolveProbe(withoutJvmNotices(out)) })
  })
}

/**
 * Chooses the engine: `maestro.executable`, then `DEJA_MAESTRO`, then the pinned install. The
 * pinned engine is not executed: its version is the pin. An override is asked for its version.
 */
export async function resolveMaestroEngine(options: ResolveEngineOptions = {}): Promise<ResolvedEngine> {
  const env = options.env ?? process.env
  const pin = options.pin ?? MAESTRO_ENGINE_PIN
  const configured = options.configExecutable?.trim()
  const fromEnv = env.DEJA_MAESTRO?.trim()
  const override = configured || fromEnv
  if (override) {
    let version: string | null = null
    let unlaunchable = false
    try {
      version = normalizeEngineVersion(await (options.probeVersion ?? probeMaestroVersion)(override))
    } catch {
      unlaunchable = true
    }
    return {
      executable: override,
      source: 'override',
      overrideVia: configured ? 'config' : 'env',
      version,
      range: checkMaestroRange(version),
      ...(unlaunchable ? { unlaunchable } : {}),
    }
  }
  const home = options.home ?? dejaHome(env)
  const executable = pinnedExecutablePath(home, pin)
  if (!(await verifyInstalledEngine(home, pin))) {
    throw new MaestroEngineError(
      `Maestro ${pin.version} is not installed in ${engineDirectory(home, pin.version)}. Run "deja engine install".`,
    )
  }
  return { executable, source: 'pinned', version: pin.version, range: 'in-range' }
}
