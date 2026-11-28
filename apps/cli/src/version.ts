import { readFileSync } from 'node:fs'
import { CONTRACT_VERSION } from '@deja-run/core'

// `bun build --compile --define` replaces these two names in a release binary (scripts/build-binary.ts).
// A normal run leaves them undefined, so the version comes from package.json and the commit is "dev".
declare const __DEJA_VERSION__: string | undefined
declare const __DEJA_COMMIT__: string | undefined

function packageVersion() {
  // src/ and dist/src/ sit at different depths below the package root.
  for (const relative of ['../package.json', '../../package.json']) {
    try {
      const parsed = JSON.parse(readFileSync(new URL(relative, import.meta.url), 'utf8')) as { name?: string, version?: string }
      if (parsed.name === '@deja-run/cli' && parsed.version) return parsed.version
    } catch {
      // Try the next location.
    }
  }
  return 'unknown'
}

export function versionNumber() {
  return typeof __DEJA_VERSION__ === 'string' ? __DEJA_VERSION__ : packageVersion()
}

export function versionLine() {
  const version = versionNumber()
  const commit = typeof __DEJA_COMMIT__ === 'string' ? __DEJA_COMMIT__ : 'dev'
  return `deja ${version} (commit ${commit}, contract ${CONTRACT_VERSION})\n`
}

/** True inside a `bun build --compile` binary, where `process.execPath` is deja itself, not Bun. */
export function isCompiledBinary() {
  return typeof Bun !== 'undefined' && /\$bunfs|~BUN/.test(Bun.main)
}
