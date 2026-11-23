#!/usr/bin/env bun
import { readdir, readFile, writeFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import { spawnSync } from 'node:child_process'

/*
 * Builds every published package to `dist/` (ESM `.js` and `.d.ts`) with `tsc -b`.
 * `rewriteRelativeImportExtensions` turns `./x.ts` into `./x.js` in the JavaScript, but it leaves
 * `.ts` in the declaration files, so those are rewritten here. The rewrite is safe to repeat.
 */

export const repo = resolve(import.meta.dir, '..')
export const publishedDirectories = [
  'packages/core',
  'packages/maestro',
  'packages/store-local',
  'packages/store-sqlite',
  'packages/store-conformance',
  'packages/agent-openclaw',
  'packages/repair',
  'packages/semantic-local',
  'apps/cli',
]

async function declarationFiles(directory: string): Promise<string[]> {
  const entries = await readdir(directory, { withFileTypes: true })
  const nested = await Promise.all(entries.map(entry => {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) return declarationFiles(path)
    return entry.name.endsWith('.d.ts') ? [path] : []
  }))
  return nested.flat()
}

export async function buildPackages() {
  const result = spawnSync('bunx', ['tsc', '-b', 'tsconfig.build.json'], { cwd: repo, stdio: 'inherit' })
  if (result.status !== 0) throw new Error('tsc -b tsconfig.build.json failed')
  const relativeTsSpecifier = /(\bfrom\s+|\bimport\s*\(\s*|\bimport\s+)(['"])(\.{1,2}\/[^'"]*)\.ts\2/g
  for (const directory of publishedDirectories) {
    for (const file of await declarationFiles(join(repo, directory, 'dist'))) {
      const before = await readFile(file, 'utf8')
      const after = before.replace(relativeTsSpecifier, '$1$2$3.js$2')
      if (after !== before) await writeFile(file, after)
    }
  }
}

if (import.meta.main) await buildPackages()
