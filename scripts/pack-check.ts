#!/usr/bin/env bun
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises'
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

type Manifest = { name: string; exports?: Record<string, unknown>; bin?: Record<string, string> }

function run(command: string, args: string[], cwd: string, env: Record<string, string> = {}) {
  const result = spawnSync(command, args, { cwd, encoding: 'utf8', env: { ...process.env, ...env } })
  return { ok: result.status === 0, output: `${result.stdout}${result.stderr}`.trim() }
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
  const packages: { manifest: Manifest; tarball: string; specifiers: string[] }[] = []
  for (const directory of publishedDirectories) {
    const folder = join(repo, directory)
    const manifest = JSON.parse(await readFile(join(folder, 'package.json'), 'utf8')) as Manifest
    const lines = must('bun', ['pm', 'pack', '--destination', tarballDirectory, '--quiet'], folder).split('\n')
    const subpaths = Object.keys(manifest.exports ?? {})
    packages.push({
      manifest,
      tarball: lines[lines.length - 1].trim(),
      specifiers: subpaths.map(subpath => subpath === '.' ? manifest.name : `${manifest.name}/${subpath.slice(2)}`),
    })
  }

  // `overrides` makes npm resolve `@deja-qa/*` dependencies to the tarballs, never the registry.
  const overrides = Object.fromEntries(packages.map(({ manifest, tarball }) => [manifest.name, `file:${tarball}`]))
  await writeFile(join(project, 'package.json'), JSON.stringify({
    name: 'deja-pack-check-consumer',
    private: true,
    type: 'module',
    dependencies: overrides,
    overrides,
  }, null, 2))
  must('npm', ['install', '--no-audit', '--no-fund', '--ignore-scripts'], project)

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
  console.log(`tsc: ${typecheck.ok ? 'ok' : 'FAILED'} (${specifiers.length} exports)`)
  if (!typecheck.ok) console.log(typecheck.output)

  const failures: string[] = []
  if (!typecheck.ok) failures.push('type-check')

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
      const help = run('bun', [join(project, 'node_modules', manifest.name, bin), '--help'], project)
      console.log(`${manifest.name} bin deja --help on Bun: ${help.ok ? 'ok' : 'exit non-zero'}`)
      if (!/deja/i.test(help.output)) failures.push(`bin deja printed no usage: ${help.output}`)
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
