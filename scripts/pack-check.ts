#!/usr/bin/env bun
import { mkdtemp, mkdir, readdir, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { spawnSync } from 'node:child_process'
import { buildPackages, publishedDirectories, repo } from './build-packages.ts'

/*
 * Pack-and-install check (DEJA-13). Builds every published package, packs each one, installs only
 * the tarballs into an empty project, type-checks an import of every export with `tsc`, then
 * imports every export on Bun and on Node.
 *
 * Tarballs come from `bun pm pack`: `npm pack` leaves `workspace:*` ranges in the manifest, which
 * npm cannot install. Publish the same way (never with a bare `npm publish` from a package folder).
 *
 * Known gap (DEJA-14): `bun:sqlite`, `bun:test` and `Bun.*` do not exist on Node. A Node import
 * that fails for that reason is reported, not failed. Any other Node failure fails the check.
 */

type Manifest = {
  name: string
  version: string
  exports?: Record<string, unknown>
  bin?: Record<string, string>
  dependencies?: Record<string, string>
  devDependencies?: Record<string, string>
}

function run(command: string, args: string[], cwd: string, env: Record<string, string> = {}) {
  const result = spawnSync(command, args, { cwd, encoding: 'utf8', env: { ...process.env, ...env } })
  return { ok: result.status === 0, output: `${result.stdout}${result.stderr}`.trim() }
}

async function sourceSpecifiersLeft(directory: string): Promise<string[]> {
  const found: string[] = []
  for (const entry of await readdir(directory, { withFileTypes: true, recursive: true })) {
    if (!entry.isFile() || !/\.(js|d\.ts)$/.test(entry.name)) continue
    const path = join(entry.parentPath, entry.name)
    if (/\bfrom\s+['"]\.{1,2}\/[^'"]*\.ts['"]|\bimport\s*\(\s*['"]\.{1,2}\/[^'"]*\.ts['"]/.test(await readFile(path, 'utf8'))) found.push(path)
  }
  return found
}

function must(command: string, args: string[], cwd: string) {
  const result = run(command, args, cwd)
  if (!result.ok) throw new Error(`${command} ${args.join(' ')} failed in ${cwd}:\n${result.output}`)
  return result.output
}

await buildPackages()

const work = await mkdtemp(join(tmpdir(), 'deja-pack-check-'))
const tarballDirectory = join(work, 'tarballs')
const project = join(work, 'project')
await mkdir(tarballDirectory)
await mkdir(project)

try {
  const packages: { directory: string; manifest: Manifest; tarball: string; specifiers: string[] }[] = []
  for (const directory of publishedDirectories) {
    const folder = join(repo, directory)
    const manifest = JSON.parse(await readFile(join(folder, 'package.json'), 'utf8')) as Manifest
    const lines = must('bun', ['pm', 'pack', '--destination', tarballDirectory, '--quiet'], folder).split('\n')
    const subpaths = Object.keys(manifest.exports ?? {})
    packages.push({
      directory,
      manifest,
      tarball: lines[lines.length - 1].trim(),
      specifiers: subpaths.map(subpath => subpath === '.' ? manifest.name : `${manifest.name}/${subpath.slice(2)}`),
    })
  }

  // Assert the pack rewrote `workspace:*`. The install below uses `overrides`, which would hide a leftover.
  const failures: string[] = []
  const versions = new Map(packages.map(({ manifest }) => [manifest.name, manifest.version]))
  for (const { directory, manifest, tarball } of packages) {
    const packed = JSON.parse(must('tar', ['-xzOf', tarball, 'package/package.json'], work)) as Manifest
    if (JSON.stringify(packed).includes('workspace:')) failures.push(`${manifest.name}: packed manifest still has a workspace: range`)
    for (const [name, range] of Object.entries({ ...packed.dependencies, ...packed.devDependencies })) {
      if (versions.has(name) && range !== versions.get(name)) failures.push(`${manifest.name}: ${name} is ${range}, expected ${versions.get(name)}`)
    }
    for (const left of await sourceSpecifiersLeft(join(repo, directory, 'dist'))) failures.push(`.ts import specifier left in ${left}`)
  }
  console.log(`manifests: ${failures.length ? 'FAILED' : 'workspace: ranges rewritten, sibling ranges match packed versions'}`)

  // `overrides` makes npm resolve `@deja-qa/*` dependencies to the tarballs, never the registry.
  const overrides = Object.fromEntries(packages.map(({ manifest, tarball }) => [manifest.name, `file:${tarball}`]))
  await writeFile(join(project, 'package.json'), JSON.stringify({
    name: 'deja-pack-check-consumer',
    private: true,
    type: 'module',
    dependencies: overrides,
    overrides,
  }, null, 2))
  // The only registry fetch is @modelcontextprotocol/sdk and its tree. Pin the SDK to the version
  // the repo is locked to, prefer the cache and retry, so a registry blip or a new release does not
  // fail an unrelated PR. skipLibCheck stays false so a broken import inside our own packed
  // `.d.ts` cannot silently become `any`. If a third-party type ever breaks it, add a pass that
  // fails only on errors under node_modules/@deja-qa/.
  const sdkVersion = (JSON.parse(await readFile(join(repo, 'node_modules/@modelcontextprotocol/sdk/package.json'), 'utf8')) as { version: string }).version
  const consumer = JSON.parse(await readFile(join(project, 'package.json'), 'utf8'))
  consumer.dependencies['@modelcontextprotocol/sdk'] = sdkVersion
  consumer.overrides['@modelcontextprotocol/sdk'] = sdkVersion
  await writeFile(join(project, 'package.json'), JSON.stringify(consumer, null, 2))
  const install = () => run('npm', ['install', '--no-audit', '--no-fund', '--ignore-scripts', '--prefer-offline', '--fetch-retries=5'], project)
  let installed = install()
  for (let attempt = 2; !installed.ok && attempt <= 3; attempt += 1) {
    console.log(`npm install failed, retry ${attempt} of 3`)
    await new Promise(done => setTimeout(done, 5000 * attempt))
    installed = install()
  }
  if (!installed.ok) throw new Error(`npm install failed:\n${installed.output}`)

  const specifiers = packages.flatMap(({ specifiers }) => specifiers)
  const importList = specifiers.map((specifier, index) => `import * as m${index} from '${specifier}'`).join('\n')
  await writeFile(join(project, 'check.ts'), `${importList}\nexport const all = [${specifiers.map((_, i) => `m${i}`).join(', ')}]\n`)
  await writeFile(join(project, 'tsconfig.json'), JSON.stringify({
    compilerOptions: {
      target: 'es2023', module: 'nodenext', moduleResolution: 'nodenext', strict: true, noEmit: true,
      skipLibCheck: false, types: ['node'], typeRoots: [join(repo, 'node_modules/@types')],
    },
    files: ['check.ts'],
  }, null, 2))
  const typecheck = run(join(repo, 'node_modules/.bin/tsc'), ['-p', 'tsconfig.json'], project)
  console.log(`tsc (nodenext): ${typecheck.ok ? 'ok' : 'FAILED'} (${specifiers.length} exports)`)
  if (!typecheck.ok) console.log(typecheck.output)
  // A Bun project that sets customConditions: ["bun"] must still resolve `types` to dist, never src (TS5097).
  await writeFile(join(project, 'tsconfig.bun.json'), JSON.stringify({
    compilerOptions: {
      target: 'es2023', module: 'esnext', moduleResolution: 'bundler', customConditions: ['bun'], strict: true,
      noEmit: true, skipLibCheck: false, types: ['node'], typeRoots: [join(repo, 'node_modules/@types')],
    },
    files: ['check.ts'],
  }, null, 2))
  const bunConditionCheck = run(join(repo, 'node_modules/.bin/tsc'), ['-p', 'tsconfig.bun.json'], project)
  console.log(`tsc (bundler, customConditions bun): ${bunConditionCheck.ok ? 'ok' : 'FAILED'}`)
  if (!bunConditionCheck.ok) console.log(bunConditionCheck.output)

  if (!typecheck.ok) failures.push('type-check (nodenext)')
  if (!bunConditionCheck.ok) failures.push('type-check (bundler with customConditions bun)')

  const importScript = (specifier: string) => `await import(${JSON.stringify(specifier)})`
  const nodeGaps: string[] = []
  console.log('\npackage / export                                installs  tsc  bun   node')
  for (const { manifest, specifiers: own } of packages) {
    for (const specifier of own) {
      const onBun = run('bun', ['-e', importScript(specifier)], project)
      const onNode = run('node', ['--input-type=module', '-e', importScript(specifier)], project)
      const bunOnly = !onNode.ok && /bun:|\bBun\b/.test(onNode.output)
      if (!onBun.ok) failures.push(`bun import ${specifier}: ${onBun.output}`)
      if (!onNode.ok && !bunOnly) failures.push(`node import ${specifier}: ${onNode.output}`)
      if (bunOnly) nodeGaps.push(`${specifier}: ${onNode.output.split('\n').find(line => /bun/i.test(line)) ?? onNode.output.split('\n')[0]}`)
      console.log(`${specifier.padEnd(46)}  yes       ${typecheck.ok ? 'ok ' : 'ERR'}  ${onBun.ok ? 'ok ' : 'ERR'}  ${onNode.ok ? 'ok' : bunOnly ? 'bun-only (DEJA-14)' : 'ERR'}`)
    }
    const bin = manifest.bin?.deja
    if (bin) {
      const help = run(join(project, 'node_modules/.bin/deja'), ['--help'], project)
      console.log(`${manifest.name} bin deja --help on Bun: ${help.ok ? 'ok' : 'FAILED'}`)
      if (!help.ok || !/deja/i.test(help.output)) failures.push(`bin deja --help failed or printed no usage: ${help.output}`)
    }
  }
  if (nodeGaps.length) console.log(`\nNode import gaps from Bun-only APIs (tracked in DEJA-14):\n${nodeGaps.map(line => `  ${line}`).join('\n')}`)
  if (failures.length) {
    console.error(`\npack-check FAILED:\n${failures.join('\n')}`)
    process.exitCode = 1
  } else {
    console.log('\npack-check passed')
  }
} finally {
  await rm(work, { recursive: true, force: true })
}
