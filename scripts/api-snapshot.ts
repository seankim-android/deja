/*
 * Public API snapshot. For each published package, `tsc --emitDeclarationOnly` emits the `.d.ts`
 * of its `src`, and the files are joined into `api/<package>.d.ts`. A change to a public type or
 * signature changes the snapshot, so `--check` (the default) fails until `--update` is run and the
 * new snapshot is reviewed. No dependency beyond TypeScript.
 *
 *   bun scripts/api-snapshot.ts            # check, exit 1 on drift
 *   bun scripts/api-snapshot.ts --update   # rewrite api/*.d.ts
 */
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'

export const repo = resolve(import.meta.dir, '..')

/** Published packages (docs/SDK_PLAN.md "Package layout"): folder under `packages/`. */
export const PUBLIC_PACKAGES = [
  'core', 'maestro', 'store-local', 'store-sqlite', 'store-conformance',
  'agent-openclaw', 'repair', 'semantic-local',
] as const

function filesBelow(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name)
    return entry.isDirectory() ? filesBelow(path) : [path]
  })
}

export function snapshotPath(name: string) {
  return join(repo, 'api', `${name}.d.ts`)
}

export function emitSnapshot(name: string): string {
  // Inside the repo so `types: ["node", "bun"]` resolves from the repo's node_modules.
  const scratchRoot = join(repo, 'node_modules', '.tmp')
  mkdirSync(scratchRoot, { recursive: true })
  const scratch = mkdtempSync(join(scratchRoot, 'api-'))
  try {
    const packageDirectory = join(repo, 'packages', name)
    const tsconfig = join(scratch, 'tsconfig.json')
    writeFileSync(tsconfig, JSON.stringify({
      extends: join(packageDirectory, 'tsconfig.json'),
      compilerOptions: {
        noEmit: false,
        declaration: true,
        emitDeclarationOnly: true,
        incremental: false,
        rootDir: repo,
        outDir: join(scratch, 'out'),
      },
      include: [join(packageDirectory, 'src')],
    }))
    const run = Bun.spawnSync([join(repo, 'node_modules/.bin/tsc'), '-p', tsconfig], { cwd: repo })
    if (run.exitCode !== 0) {
      throw new Error(`tsc failed for ${name}:\n${run.stdout.toString()}${run.stderr.toString()}`)
    }
    const source = join(scratch, 'out', 'packages', name, 'src')
    return filesBelow(source).sort().map(file =>
      `// ${relative(source, file).split('\\').join('/')}\n${readFileSync(file, 'utf8').trimEnd()}\n`,
    ).join('\n')
  } finally {
    rmSync(scratch, { recursive: true, force: true })
  }
}

function main() {
  const update = process.argv.includes('--update')
  const drift: string[] = []
  if (update) mkdirSync(join(repo, 'api'), { recursive: true })
  for (const name of PUBLIC_PACKAGES) {
    const next = emitSnapshot(name)
    if (update) {
      writeFileSync(snapshotPath(name), next)
      continue
    }
    let current = ''
    try {
      current = readFileSync(snapshotPath(name), 'utf8')
    } catch {
      current = ''
    }
    if (current !== next) drift.push(name)
  }
  if (drift.length > 0) {
    console.error(`Public API changed for: ${drift.join(', ')}.`)
    console.error('Review the change, then run: bun scripts/api-snapshot.ts --update')
    process.exit(1)
  }
  console.log(update ? 'API snapshots updated.' : 'API snapshots match.')
}

if (import.meta.main) main()
