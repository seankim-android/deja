import { createHash } from 'node:crypto'
import { readFile, readdir } from 'node:fs/promises'
import { relative, resolve, sep } from 'node:path'

export type MaestroPlatform = 'android' | 'web'

export type AdapterIdentityInput = { buildFingerprint: string }
export type AdapterCacheIdentity = AdapterIdentityInput & {
  adapter?: string
  browser?: string
}

async function fingerprintFile(root: string, buildPath: string) {
  return createHash('sha256').update(await readFile(resolve(root, buildPath))).digest('hex')
}

async function filesBelow(directory: string): Promise<string[]> {
  const entries = await readdir(directory, { withFileTypes: true })
  const files: string[] = []
  for (const entry of entries) {
    const path = resolve(directory, entry.name)
    if (entry.isDirectory()) files.push(...await filesBelow(path))
    else if (entry.isFile()) files.push(path)
    else throw new Error('Web build tree cannot contain symbolic links or special files')
  }
  return files.sort()
}

async function fingerprintTree(root: string, buildPath: string) {
  const directory = resolve(root, buildPath)
  const hash = createHash('sha256')
  const paths = await filesBelow(directory)
  if (paths.length === 0) throw new Error('Web build tree must contain at least one file')
  for (const path of paths) {
    hash.update(relative(directory, path).split(sep).join('/'))
    hash.update(Buffer.from([0]))
    hash.update(await readFile(path))
    hash.update(Buffer.from([0]))
  }
  return hash.digest('hex')
}

/** The composition-root adapter owns runner selection and its share of exact cache identity. */
export interface MaestroExecutionAdapter {
  readonly id: string
  readonly platform: MaestroPlatform
  readonly browser: string | null
  readonly device: string
  cacheIdentity(input: AdapterIdentityInput): AdapterCacheIdentity
  buildFingerprint(workspaceRoot: string, buildPath: string): Promise<string>
  commandArguments(flowPath: string): string[]
}

export type AndroidAdapterOptions = { device: string }
export type WebAdapterOptions = {
  device?: string
  headless?: boolean
  screenSize?: string
  adapterId?: string
}

export function androidMaestroAdapter(options: AndroidAdapterOptions): MaestroExecutionAdapter {
  if (!options.device.trim()) throw new TypeError('An Android device is required')
  return {
    id: 'maestro-android',
    platform: 'android',
    browser: null,
    device: options.device,
    // Omit new fields for Android so validated pre-WALK-010 cache entries remain reproducible.
    cacheIdentity: ({ buildFingerprint }) => ({ buildFingerprint }),
    buildFingerprint: fingerprintFile,
    commandArguments: flowPath => ['--no-ansi', '--device', options.device, 'test', flowPath],
  }
}

export function webMaestroAdapter(options: WebAdapterOptions = {}): MaestroExecutionAdapter {
  const device = options.device ?? 'chromium'
  const headless = options.headless ?? true
  if (device !== 'chromium') throw new TypeError('WALK-010 supports only the chromium Web device')
  const screenSize = options.screenSize ?? '1280x800'
  if (!/^[1-9][0-9]{2,4}x[1-9][0-9]{2,4}$/.test(screenSize)) {
    throw new TypeError('Web screenSize must use WIDTHxHEIGHT')
  }
  const id = options.adapterId ?? 'maestro-web/v1'
  return {
    id,
    platform: 'web',
    browser: 'chromium',
    device,
    cacheIdentity: ({ buildFingerprint }) => ({ buildFingerprint, adapter: id, browser: 'chromium' }),
    buildFingerprint: fingerprintTree,
    commandArguments: flowPath => [
      '--no-ansi', '--platform', 'web', '--device', device,
      'test', ...(headless ? ['--headless'] : []), '--screen-size', screenSize, flowPath,
    ],
  }
}
