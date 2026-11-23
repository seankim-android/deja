import { createHash } from 'node:crypto'
import { readFile, readdir } from 'node:fs/promises'
import { relative, resolve, sep } from 'node:path'

export type MaestroPlatform = 'android' | 'web' | 'ios'

/** Per-invocation options. `junitPath` asks Maestro itself to write its JUnit report there. */
export type MaestroCommandOptions = { junitPath?: string }

function junitArguments(options?: MaestroCommandOptions) {
  return options?.junitPath ? ['--format=JUNIT', `--output=${options.junitPath}`] : []
}

export type AdapterIdentityInput = { buildFingerprint: string }
export type AdapterCacheIdentity = AdapterIdentityInput & {
  adapter?: string
  browser?: string
}

async function fingerprintFile(root: string, buildPath: string) {
  return createHash('sha256').update(await readFile(resolve(root, buildPath))).digest('hex')
}

async function filesBelow(directory: string, label: string): Promise<string[]> {
  const entries = await readdir(directory, { withFileTypes: true })
  const files: string[] = []
  for (const entry of entries) {
    const path = resolve(directory, entry.name)
    if (entry.isDirectory()) files.push(...await filesBelow(path, label))
    else if (entry.isFile()) files.push(path)
    else throw new Error(`${label} build tree cannot contain symbolic links or special files`)
  }
  return files.sort()
}

async function fingerprintTree(root: string, buildPath: string, label: string) {
  const directory = resolve(root, buildPath)
  const hash = createHash('sha256')
  const paths = await filesBelow(directory, label)
  if (paths.length === 0) throw new Error(`${label} build tree must contain at least one file`)
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
  commandArguments(flowPath: string, options?: MaestroCommandOptions): string[]
}

export type AndroidAdapterOptions = { device: string }
export type WebAdapterOptions = {
  device?: string
  headless?: boolean
  screenSize?: string
  adapterId?: string
}

export type IosAdapterOptions = {
  device: string
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
    commandArguments: (flowPath, run) => [
      '--no-ansi', '--device', options.device, 'test', ...junitArguments(run), flowPath,
    ],
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
    buildFingerprint: (root, buildPath) => fingerprintTree(root, buildPath, 'Web'),
    commandArguments: (flowPath, run) => [
      '--no-ansi', '--platform', 'web', '--device', device,
      'test', ...(headless ? ['--headless'] : []), '--screen-size', screenSize,
      ...junitArguments(run), flowPath,
    ],
  }
}

export function iosMaestroAdapter(options: IosAdapterOptions): MaestroExecutionAdapter {
  if (!options.device.trim()) throw new TypeError('An iOS simulator UDID is required')
  const id = options.adapterId ?? 'maestro-ios/v1'
  return {
    id,
    platform: 'ios',
    browser: null,
    device: options.device,
    cacheIdentity: ({ buildFingerprint }) => ({ buildFingerprint, adapter: id }),
    buildFingerprint: (root, buildPath) => fingerprintTree(root, buildPath, 'iOS'),
    commandArguments: (flowPath, run) => [
      '--no-ansi', '--platform', 'ios', '--device', options.device, 'test',
      ...junitArguments(run), flowPath,
    ],
  }
}
