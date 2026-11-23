#!/usr/bin/env bun
import { createHash } from 'node:crypto'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { spawnSync } from 'node:child_process'

/*
 * Standalone binary build (DEJA-15). `bun build --compile` bundles the CLI, its workspace packages
 * and the Bun runtime into one file, so a consumer needs no Bun and no node_modules.
 *
 * Output goes to dist-bin/ (gitignored): deja-<version>-<os>-<arch>, one .sha256 file for each and
 * a SHA256SUMS file for the whole release. The version and the git commit are baked in with
 * --define, which is what `deja --version` prints.
 *
 * Usage: bun scripts/build-binary.ts [--target darwin-arm64] [--target linux-x64] [--out dist-bin]
 * With no --target it builds both. A binary can only be run on its own platform, so the smoke
 * check (scripts/smoke-binary.ts) covers the one that matches the machine.
 */

const root = resolve(import.meta.dir, '..')
const targets = { 'darwin-arm64': 'bun-darwin-arm64', 'linux-x64': 'bun-linux-x64' } as const
type Target = keyof typeof targets

function option(name: string) {
  const values: string[] = []
  const argv = Bun.argv.slice(2)
  for (let index = 0; index < argv.length; index += 1) {
    if (argv[index] === name) values.push(argv[index + 1] ?? '')
  }
  return values
}

function git(args: string[]) {
  const result = spawnSync('git', args, { cwd: root, encoding: 'utf8' })
  if (result.status !== 0) throw new Error(`git ${args.join(' ')} failed: ${result.stderr}`)
  return result.stdout
}

// Changes to tracked or unignored files. A build must leave this exactly as it found it.
const treeBefore = git(['status', '--porcelain'])

function gitCommit() {
  if (process.env.DEJA_COMMIT) return process.env.DEJA_COMMIT
  // A binary built from uncommitted changes must not claim a clean commit.
  return `${git(['rev-parse', '--short=12', 'HEAD']).trim()}${treeBefore.trim() ? '-dirty' : ''}`
}

const requested = option('--target')
for (const target of requested) {
  if (!(target in targets)) throw new Error(`Unknown target ${target}. Use ${Object.keys(targets).join(' or ')}.`)
}
const selected = (requested.length ? requested : Object.keys(targets)) as Target[]
const outDirectory = resolve(root, option('--out')[0] ?? 'dist-bin')
const { version } = JSON.parse(await readFile(join(root, 'apps/cli/package.json'), 'utf8')) as { version: string }
const commit = gitCommit()

await rm(outDirectory, { recursive: true, force: true })
await mkdir(outDirectory, { recursive: true })

// `bun build --compile` leaves a .<hash>-00000000.bun-build runtime copy (about 85 MB) in its cwd.
// Run it in a throwaway directory so the repository stays clean.
const scratch = await mkdtemp(join(tmpdir(), 'deja-build-'))
const started = performance.now()
const sums: string[] = []
for (const target of selected) {
  const name = `deja-${version}-${target}`
  const file = join(outDirectory, name)
  const targetStarted = performance.now()
  const build = spawnSync('bun', [
    'build', '--compile', `--target=${targets[target]}`,
    '--define', `__DEJA_VERSION__=${JSON.stringify(version)}`,
    '--define', `__DEJA_COMMIT__=${JSON.stringify(commit)}`,
    join(root, 'apps/cli/deja.ts'), '--outfile', file,
  ], { cwd: scratch, encoding: 'utf8' })
  if (build.status !== 0) throw new Error(`bun build failed for ${target}:\n${build.stdout}${build.stderr}`)
  const bytes = await readFile(file)
  const digest = createHash('sha256').update(bytes).digest('hex')
  await writeFile(`${file}.sha256`, `${digest}  ${name}\n`)
  sums.push(`${digest}  ${name}`)
  console.log(`${name}  ${(bytes.length / 1024 / 1024).toFixed(1)} MiB  ${((performance.now() - targetStarted) / 1000).toFixed(1)}s  sha256 ${digest}`)
}
await rm(scratch, { recursive: true, force: true })
const treeAfter = git(['status', '--porcelain'])
if (treeAfter !== treeBefore) {
  throw new Error(`The build changed the working tree:\n${treeAfter}`)
}
await writeFile(join(outDirectory, 'SHA256SUMS'), `${sums.join('\n')}\n`)
console.log(`Built ${selected.length} binar${selected.length === 1 ? 'y' : 'ies'} for ${version} (${commit}) in ${((performance.now() - started) / 1000).toFixed(1)}s`)
