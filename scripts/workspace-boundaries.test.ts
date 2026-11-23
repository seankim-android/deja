import { describe, expect, test } from 'bun:test'
import { readFileSync, readdirSync, statSync } from 'node:fs'
import { dirname, join, normalize, relative, resolve, sep } from 'node:path'

/*
 * Workspace boundary guard. Packages must reach each other only through `@deja-qa/*` package
 * imports, and only through dependencies they declare. Hoisted links in the root node_modules
 * would otherwise let an undeclared import resolve locally and then break once published.
 */

const repo = resolve(import.meta.dir, '..')
const workspaceDirectories = ['packages', 'apps'].flatMap(group =>
  readdirSync(join(repo, group)).map(name => join(group, name))
    .filter(path => statSync(join(repo, path)).isDirectory()))

type Manifest = {
  name: string
  dependencies?: Record<string, string>
  devDependencies?: Record<string, string>
}

const workspaces = workspaceDirectories.map(directory => ({
  directory,
  manifest: JSON.parse(readFileSync(join(repo, directory, 'package.json'), 'utf8')) as Manifest,
}))

function sourceFiles(directory: string): string[] {
  return readdirSync(join(repo, directory), { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) return entry.name === 'node_modules' || entry.name === 'fixtures' ? [] : sourceFiles(path)
    return /\.tsx?$/.test(entry.name) ? [path] : []
  })
}

const specifierPattern = /(?:from\s+|import\s*\(\s*|import\s+)['"]([^'"]+)['"]/g

function specifiers(file: string) {
  return [...readFileSync(join(repo, file), 'utf8').matchAll(specifierPattern)].map(match => match[1])
}

function owningWorkspace(path: string) {
  return workspaces.find(workspace => path === workspace.directory ||
    path.startsWith(`${workspace.directory}${sep}`))
}

describe('workspace boundaries', () => {
  test('every package and app has a private @deja-qa manifest', () => {
    expect(workspaces.length).toBeGreaterThan(0)
    for (const { directory, manifest } of workspaces) {
      expect({ directory, name: manifest.name.startsWith('@deja-qa/') }).toEqual({ directory, name: true })
    }
  })

  test('no relative import crosses into another package', () => {
    const violations: string[] = []
    for (const workspace of workspaces) {
      for (const file of sourceFiles(workspace.directory)) {
        for (const specifier of specifiers(file)) {
          if (!specifier.startsWith('.')) continue
          const target = normalize(relative(repo, resolve(repo, dirname(file), specifier)))
          const owner = owningWorkspace(target)
          if (owner && owner !== workspace) violations.push(`${file}: ${specifier}`)
        }
      }
    }
    expect(violations).toEqual([])
  })

  test('package imports are declared: src in dependencies, tests in either list', () => {
    const violations: string[] = []
    for (const { directory, manifest } of workspaces) {
      const runtime = new Set(Object.keys(manifest.dependencies ?? {}))
      const development = new Set(Object.keys(manifest.devDependencies ?? {}))
      for (const file of sourceFiles(directory)) {
        const isTest = file.split(sep).includes('test')
        for (const specifier of specifiers(file)) {
          if (!specifier.startsWith('@deja-qa/')) continue
          const name = specifier.split('/').slice(0, 2).join('/')
          if (name === manifest.name) {
            violations.push(`${file}: imports its own package ${name}`)
          } else if (!runtime.has(name) && !(isTest && development.has(name))) {
            violations.push(`${file}: ${name} is not declared in ${directory}/package.json`)
          }
        }
      }
    }
    expect(violations).toEqual([])
  })

  test('core depends on no other Déjà package', () => {
    const core = workspaces.find(workspace => workspace.manifest.name === '@deja-qa/core')!
    const declared = Object.keys({ ...core.manifest.dependencies, ...core.manifest.devDependencies })
    expect(declared.filter(name => name.startsWith('@deja-qa/'))).toEqual([])
    const imports = sourceFiles(core.directory).flatMap(file =>
      specifiers(file).filter(specifier => specifier.startsWith('@deja-qa/')).map(specifier => `${file}: ${specifier}`))
    expect(imports).toEqual([])
  })
})
