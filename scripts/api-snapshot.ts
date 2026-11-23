/*
 * Public API snapshot. For each published package, `tsc --emitDeclarationOnly` emits `.d.ts` files.
 * The snapshot keeps only what the package exports from its `package.json` `exports` entry points,
 * plus the same-package declarations those exports reference. Other internal declarations never
 * appear, so internal changes do not fail the check. Result: `api/<package>.d.ts`.
 *
 * TypeScript 7 has no stable compiler API, so this reads the emitted declaration text. The text is
 * the declaration printer's output, which can change between TypeScript releases. The header names
 * the version that wrote the snapshot, and the check says so when the version differs. Regenerate
 * the snapshots in the same PR as any TypeScript or `@types/node` bump.
 *
 *   bun scripts/api-snapshot.ts            # check, exit 1 on drift
 *   bun scripts/api-snapshot.ts --update   # rewrite api/*.d.ts
 */
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'

export const repo = resolve(import.meta.dir, '..')

/** Published packages (docs/SDK_PLAN.md "Package layout") and the CLI's exported modules. */
export const PUBLIC_PACKAGES = [
  'packages/core', 'packages/maestro', 'packages/store-local', 'packages/store-sqlite',
  'packages/store-conformance', 'packages/agent-openclaw', 'packages/repair',
  'packages/semantic-local', 'apps/cli',
] as const

export function packageName(directory: string) {
  return directory.split('/').at(-1)!
}

export function snapshotPath(directory: string) {
  return join(repo, 'api', `${packageName(directory)}.d.ts`)
}

export function typescriptVersion(): string {
  return JSON.parse(readFileSync(join(repo, 'node_modules/typescript/package.json'), 'utf8')).version
}

const HEADER = /^\/\/ typescript (\S+)\n/

export function recordedVersion(snapshot: string): string | null {
  return HEADER.exec(snapshot)?.[1] ?? null
}

function filesBelow(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name)
    return entry.isDirectory() ? filesBelow(path) : [path]
  })
}

type Statement = { names: string[]; text: string; exported: boolean }
type Module = {
  statements: Statement[]
  /** local name -> where it is imported from (same package only) */
  imports: Map<string, { file: string; name: string }>
  /** `export * from` targets and `export { a as b } from` items, resolved to emitted files */
  starExports: string[]
  namedReexports: { file: string; name: string; as: string }[]
  /** `export { a as b }` of local names */
  localExports: { name: string; as: string }[]
}

const DECLARATION = /^(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:class|interface|type|function|const|let|var|enum|namespace)\s+([A-Za-z_$][\w$]*)/

function splitItems(list: string) {
  return list.split(',').map(item => item.trim()).filter(Boolean).map(item => {
    const cleaned = item.replace(/^type\s+/, '')
    const [name, alias] = cleaned.split(/\s+as\s+/)
    return { name: name.trim(), as: (alias ?? name).trim() }
  })
}

function parseModule(file: string, files: Map<string, string>): Module {
  const source = files.get(file)!
  const module: Module = {
    statements: [], imports: new Map(), starExports: [], namedReexports: [], localExports: [],
  }
  const resolveSpecifier = (specifier: string) => {
    if (!specifier.startsWith('.')) return null
    const target = resolve(dirname(file), specifier.replace(/\.ts$/, '')) + '.d.ts'
    return files.has(target) ? target : null
  }
  // Top-level statements start at column 0; indented lines and closing braces continue them.
  const chunks: string[] = []
  for (const line of source.split('\n')) {
    if (line.trim() === '') continue
    if (/^[}\])]/.test(line) || /^\s/.test(line)) chunks[chunks.length - 1] += `\n${line}`
    else chunks.push(line)
  }
  for (const text of chunks) {
    const flat = text.replace(/\s+/g, ' ')
    let match = /^import\s+(?:type\s+)?\{([^}]*)\}\s+from\s+'([^']+)'/.exec(flat)
    if (match) {
      const target = resolveSpecifier(match[2])
      if (target) for (const item of splitItems(match[1])) module.imports.set(item.as, { file: target, name: item.name })
      continue
    }
    if (flat.startsWith('import ')) continue
    match = /^export\s+\*\s+from\s+'([^']+)'/.exec(flat)
    if (match) {
      const target = resolveSpecifier(match[1])
      if (target) module.starExports.push(target)
      continue
    }
    match = /^export\s+(?:type\s+)?\{([^}]*)\}\s+from\s+'([^']+)'/.exec(flat)
    if (match) {
      const target = resolveSpecifier(match[2])
      if (target) for (const item of splitItems(match[1])) module.namedReexports.push({ file: target, ...item })
      continue
    }
    match = /^export\s+(?:type\s+)?\{([^}]*)\}\s*;?$/.exec(flat)
    if (match) {
      module.localExports.push(...splitItems(match[1]))
      continue
    }
    match = DECLARATION.exec(flat)
    if (match) {
      module.statements.push({ names: [match[1]], text: text.trimEnd(), exported: /^export\s/.test(flat) })
    }
  }
  return module
}

/** Public export name -> declaring file and local name, following re-exports inside the package. */
function exportsOf(file: string, files: Map<string, string>, cache: Map<string, Module>,
  seen = new Set<string>()): Map<string, { file: string; name: string }> {
  const result = new Map<string, { file: string; name: string }>()
  if (seen.has(file)) return result
  seen.add(file)
  if (!cache.has(file)) cache.set(file, parseModule(file, files))
  const module = cache.get(file)!
  for (const statement of module.statements) {
    if (statement.exported) result.set(statement.names[0], { file, name: statement.names[0] })
  }
  for (const item of module.localExports) result.set(item.as, { file, name: item.name })
  for (const target of module.starExports) {
    for (const [name, origin] of exportsOf(target, files, cache, seen)) result.set(name, origin)
  }
  for (const item of module.namedReexports) {
    const origin = exportsOf(item.file, files, cache, new Set()).get(item.name)
    result.set(item.as, origin ?? { file: item.file, name: item.name })
  }
  return result
}

function entryFiles(packageDirectory: string): { key: string; source: string }[] {
  const manifest = JSON.parse(readFileSync(join(repo, packageDirectory, 'package.json'), 'utf8'))
  return Object.entries(manifest.exports as Record<string, unknown>)
    .filter((entry): entry is [string, string] => typeof entry[1] === 'string')
    .map(([key, source]) => ({ key, source }))
}

export function emitSnapshot(packageDirectory: string): string {
  // Inside the repo so `types: ["node", "bun"]` resolves from the repo's node_modules.
  const scratchRoot = join(repo, 'node_modules', '.tmp')
  mkdirSync(scratchRoot, { recursive: true })
  const scratch = mkdtempSync(join(scratchRoot, 'api-'))
  try {
    const directory = join(repo, packageDirectory)
    const entries = entryFiles(packageDirectory)
    const tsconfig = join(scratch, 'tsconfig.json')
    writeFileSync(tsconfig, JSON.stringify({
      extends: join(directory, 'tsconfig.json'),
      compilerOptions: {
        noEmit: false,
        declaration: true,
        emitDeclarationOnly: true,
        removeComments: true,
        incremental: false,
        rootDir: repo,
        outDir: join(scratch, 'out'),
      },
      include: entries.map(entry => join(directory, entry.source)),
    }))
    const run = Bun.spawnSync([join(repo, 'node_modules/.bin/tsc'), '-p', tsconfig], { cwd: repo })
    if (run.exitCode !== 0) {
      throw new Error(`tsc failed for ${packageDirectory}:\n${run.stdout.toString()}${run.stderr.toString()}`)
    }
    const out = join(scratch, 'out', packageDirectory)
    const files = new Map<string, string>()
    for (const file of filesBelow(out)) files.set(file, readFileSync(file, 'utf8'))
    const cache = new Map<string, Module>()
    const moduleOf = (file: string) => {
      if (!cache.has(file)) cache.set(file, parseModule(file, files))
      return cache.get(file)!
    }

    const sections: string[] = []
    for (const entry of entries) {
      const entryFile = join(out, entry.source.replace(/\.ts$/, '.d.ts'))
      const exported = exportsOf(entryFile, files, cache)
      const names = [...exported.keys()].sort()
      // Worklist: exported declarations plus the same-package declarations they mention.
      const included = new Map<string, { name: string; file: string; statement: Statement; public: boolean }>()
      const queue: { file: string; name: string; public: boolean }[] =
        [...exported.values()].map(origin => ({ ...origin, public: true }))
      while (queue.length > 0) {
        const { file, name, public: isPublic } = queue.shift()!
        const module = moduleOf(file)
        const imported = module.imports.get(name)
        if (imported && !module.statements.some(statement => statement.names[0] === name)) {
          queue.push({ ...imported, public: isPublic })
          continue
        }
        for (const statement of module.statements.filter(item => item.names[0] === name)) {
          const key = `${file}\0${statement.text}`
          const known = included.get(key)
          if (known) {
            known.public ||= isPublic
            continue
          }
          included.set(key, { name, file, statement, public: isPublic })
          const words = new Set(statement.text.match(/[A-Za-z_$][\w$]*/g) ?? [])
          for (const word of words) {
            if (word === name) continue
            if (module.statements.some(item => item.names[0] === word) || module.imports.has(word)) {
              queue.push({ file, name: word, public: false })
            }
          }
        }
      }
      const ordered = [...included.values()].sort((a, b) =>
        Number(b.public) - Number(a.public) || a.name.localeCompare(b.name))
      sections.push([
        `// entry ${entry.key} (${entry.source})`,
        `// exports: ${names.join(', ')}`,
        ...ordered.map(item => item.public ? item.statement.text : `// referenced\n${item.statement.text}`),
      ].join('\n'))
    }
    return `// typescript ${typescriptVersion()}\n${sections.join('\n\n')}\n`
  } finally {
    rmSync(scratch, { recursive: true, force: true })
  }
}

function main() {
  const update = process.argv.includes('--update')
  const drift: string[] = []
  if (update) mkdirSync(join(repo, 'api'), { recursive: true })
  for (const directory of PUBLIC_PACKAGES) {
    const next = emitSnapshot(directory)
    if (update) {
      writeFileSync(snapshotPath(directory), next)
      continue
    }
    let current = ''
    try {
      current = readFileSync(snapshotPath(directory), 'utf8')
    } catch {
      current = ''
    }
    if (current !== next) drift.push(relative(repo, snapshotPath(directory)))
    const recorded = recordedVersion(current)
    if (current !== next && recorded && recorded !== typescriptVersion()) {
      console.error(`${directory}: snapshot written by typescript ${recorded}, installed ${typescriptVersion()}.`)
    }
  }
  if (drift.length > 0) {
    console.error(`Public API changed: ${drift.join(', ')}.`)
    console.error('Review the change, then run: bun scripts/api-snapshot.ts --update')
    process.exit(1)
  }
  console.log(update ? 'API snapshots updated.' : 'API snapshots match.')
}

if (import.meta.main) main()
