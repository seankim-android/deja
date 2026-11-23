#!/usr/bin/env bun
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import { join, relative } from 'node:path'
import { spawnSync } from 'node:child_process'

/*
 * `prepublishOnly` guard for every published package (DEJA-13). Run from the package folder.
 * It fails unless:
 *   - the DEJA-16 publish process set DEJA_PUBLISH_PROCESS=DEJA-16 (only that process may publish),
 *   - the git tree is clean,
 *   - every `types` and `default` target in `exports` and `bin` exists, and
 *   - `dist/` is newer than its sources and holds no output of a deleted source file.
 * `bun pm pack` does not run this, so pack-check and local packing are not blocked.
 */

const folder = process.cwd()
const failures: string[] = []
const manifest = JSON.parse(readFileSync(join(folder, 'package.json'), 'utf8')) as {
  name: string
  exports?: Record<string, Record<string, string>>
  bin?: Record<string, string>
}

if (process.env.DEJA_PUBLISH_PROCESS !== 'DEJA-16') {
  failures.push('only the DEJA-16 publish process may publish (it sets DEJA_PUBLISH_PROCESS=DEJA-16)')
}

const status = spawnSync('git', ['status', '--porcelain'], { cwd: folder, encoding: 'utf8' })
if (status.status !== 0 || status.stdout.trim()) failures.push('the git tree is not clean')

const targets = [
  ...Object.values(manifest.exports ?? {}).flatMap(conditions => [conditions.types, conditions.default]),
  ...Object.values(manifest.bin ?? {}),
]
for (const target of targets) {
  if (!existsSync(join(folder, target))) failures.push(`missing build output ${target}; run bun run build:packages`)
}

function files(directory: string): string[] {
  if (!existsSync(directory)) return []
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name)
    return entry.isDirectory() ? files(path) : [path]
  })
}
const sources = [...files(join(folder, 'src')), ...(existsSync(join(folder, 'deja.ts')) ? [join(folder, 'deja.ts')] : [])]
const outputs = files(join(folder, 'dist')).filter(path => /\.(js|d\.ts)$/.test(path))
const newestSource = Math.max(0, ...sources.map(path => statSync(path).mtimeMs))
const oldestOutput = Math.min(Infinity, ...outputs.map(path => statSync(path).mtimeMs))
if (outputs.length && oldestOutput < newestSource) failures.push('dist is older than src; rebuild with bun run build:packages')
const sourceStems = new Set(sources.map(path => relative(folder, path).replace(/\.ts$/, '')))
for (const output of outputs) {
  const stem = relative(join(folder, 'dist'), output).replace(/\.(d\.ts|js)$/, '')
  const stems = [stem, `src/${stem}`]
  if (!stems.some(candidate => sourceStems.has(candidate))) failures.push(`stale output with no source: dist/${stem}`)
}

if (failures.length) {
  console.error(`publish-guard: refusing to publish ${manifest.name}:\n${failures.map(line => `  - ${line}`).join('\n')}`)
  process.exit(1)
}
console.log(`publish-guard: ${manifest.name} ok`)
