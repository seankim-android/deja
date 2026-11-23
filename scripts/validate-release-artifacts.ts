#!/usr/bin/env bun
import { access, readFile } from 'node:fs/promises'
import { spawnSync } from 'node:child_process'
import { dirname, resolve } from 'node:path'

function git(args: string[]) {
  const result = spawnSync('git', args, { encoding: 'utf8' })
  if (result.status !== 0) throw new Error(`git ${args.join(' ')} failed: ${result.stderr}`)
  return result.stdout
}

const files = git(['ls-files', '-z']).split('\0').filter(Boolean)
const forbiddenPaths = [
  /(^|\/)\.deja\//,
  /(^|\/)\.tmp\//,
  /(^|\/)node_modules\//,
  /(^|\/)dist\//,
  /(^|\/)\.gradle\//,
  /\.(apk|aab|class|pyc)$/,
  /(^|\/)(build|dist|out)\/.*\.jar$/,
  /(^|\/)demo\/assets\//,
  /(^|\/)demo\/rehearsal\/scene-[0-9]+\.png$/,
]
const forbiddenTracked = files.filter(path => forbiddenPaths.some(pattern => pattern.test(path)))
if (forbiddenTracked.length) {
  throw new Error(`Generated or ignored artifacts are tracked: ${forbiddenTracked.join(', ')}`)
}

let jsonFiles = 0
let textFiles = 0
const leaks: string[] = []
const brokenMarkdownLinks: string[] = []
let markdownLinks = 0
const forbiddenContents = [
  { label: 'private home path', pattern: /\/Users\/seankim/ },
  { label: 'private key', pattern: /BEGIN [A-Z ]*PRIVATE KEY/ },
  { label: 'GitHub credential', pattern: /\bgh[opusr]_[A-Za-z0-9]{20,}/ },
  { label: 'OpenAI-shaped credential', pattern: /\bsk-[A-Za-z0-9_-]{24,}/ },
]
for (const path of files) {
  const bytes = await readFile(path)
  if (bytes.includes(0)) continue // Curated video/images only; their source text is checked below.
  const contents = bytes.toString('utf8')
  textFiles += 1
  if (path.endsWith('.md')) {
    for (const match of contents.matchAll(/!?\[[^\]\n]*\]\(([^)\n]+)\)/g)) {
      const target = match[1].trim().replace(/^<|>$/g, '').split(/\s+/)[0]
      if (!target || /^[a-z][a-z0-9+.-]*:/i.test(target) || target.startsWith('#') || target.includes('${') || target.includes('*')) continue
      const localPath = target.split('#')[0]
      if (!localPath) continue
      markdownLinks += 1
      const resolved = localPath.startsWith('/')
        ? resolve(`.${localPath}`)
        : resolve(dirname(path), localPath)
      try {
        await access(resolved)
      } catch {
        brokenMarkdownLinks.push(`${path}: ${target}`)
      }
    }
  }
  // TypeScript tsconfig files are JSONC (comments/trailing commas) and are validated by bun run build.
  if (path.endsWith('.json') && !path.startsWith('tsconfig')) {
    JSON.parse(contents)
    jsonFiles += 1
  }
  for (const { label, pattern } of forbiddenContents) {
    if (pattern.test(contents)) leaks.push(`${path}: ${label}`)
  }
}
if (leaks.length) throw new Error(`Potential secret/machine leaks:\n${leaks.join('\n')}`)
if (brokenMarkdownLinks.length) {
  throw new Error(`Broken local Markdown links:\n${brokenMarkdownLinks.join('\n')}`)
}

const dirty = git(['status', '--porcelain']).trim()
if (dirty) throw new Error(`Release checkout changed during portable checks:\n${dirty}`)
const head = git(['rev-parse', 'HEAD']).trim()
console.log(JSON.stringify({
  result: 'pass', head, trackedFiles: files.length, textFiles, jsonFiles,
  generatedArtifactsAbsent: true, markdownLinks, markdownLinksValid: true,
  secretAndMachinePathScan: 'pass', checkoutUnchanged: true,
}, null, 2))
