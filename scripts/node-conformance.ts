#!/usr/bin/env bun
import { spawnSync } from 'node:child_process'
import { join } from 'node:path'
import { buildPackages, repo } from './build-packages.ts'

/*
 * Runs the store conformance suites on Node (DEJA-14), through `node:test`, against the built
 * `dist/` of every package, the same files an npm consumer gets. Needs Node 22.13 or later:
 * `node:sqlite` is behind a flag before that. Building needs Bun; running does not.
 */

await buildPackages()
const result = spawnSync('node', ['--test', join(repo, 'packages/store-conformance/test/node/conformance.mjs')], {
  cwd: repo,
  stdio: 'inherit',
})
process.exitCode = result.status ?? 1
