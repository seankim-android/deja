import { afterEach, beforeEach, describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import {
  listWorkspaceReports,
  readDeclaredArtifact,
  readValidationHistory,
  safeArtifactPath,
} from '../local-server.ts'
import { redactViewerJson, redactViewerText } from '../redaction.ts'
import { fixtures } from '../../src/viewer/fixtures.ts'

let root: string

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), 'deja-viewer-'))
})

afterEach(async () => {
  await rm(root, { recursive: true, force: true })
})

async function put(relative: string, contents: string) {
  const path = join(root, relative)
  await mkdir(dirname(path), { recursive: true })
  await writeFile(path, contents)
  return path
}

const report = fixtures.warm.entry.report!
const reportPath = '.deja/reports/latest.json'
const yamlPath = report.yamlPath!

describe('read-only local viewer API', () => {
  test('a missing report directory is an empty index, never a fabricated pass', async () => {
    expect(await listWorkspaceReports(root)).toEqual({ state: 'ready', reports: [], message: null })
  })

  test('valid, malformed, missing, and in-progress reports retain their states', async () => {
    await put(reportPath, JSON.stringify(report))
    await put('.deja/reports/bad.json', '{nope')
    await put('.deja/reports/current.json', JSON.stringify({ schemaVersion: 1, endedAt: null }))
    const listed = await listWorkspaceReports(root)
    expect(listed.reports.find(entry => entry.path === reportPath)?.report?.passed).toBe(true)
    expect(listed.reports.find(entry => entry.path.endsWith('bad.json'))?.state).toBe('malformed')
    expect(listed.reports.find(entry => entry.path.endsWith('current.json'))?.state).toBe('in-progress')
    await rm(join(root, reportPath))
    const refreshed = await listWorkspaceReports(root)
    expect(refreshed.reports.some(entry => entry.path === reportPath)).toBe(false)
  })

  test('only an artifact declared by its authorizing report can be read', async () => {
    await put(reportPath, JSON.stringify(report))
    await put(yamlPath, 'appId: dev.deja.checkout\n')
    await put('.deja/flows/checkout/undeclared.yaml', 'do not load\n')
    const artifact = await readDeclaredArtifact(root, reportPath, yamlPath)
    expect(artifact.contents).toContain('dev.deja.checkout')
    await expect(readDeclaredArtifact(root, reportPath, '.deja/flows/checkout/undeclared.yaml')).rejects.toThrow('not declared')
  })

  test('arbitrary paths, traversal, raw logs, and credential-like paths are rejected', async () => {
    for (const path of ['/etc/passwd', '.deja/../secret.yaml', '.deja/secrets/run.yaml', '.deja/reports/output.log']) {
      expect(safeArtifactPath(path)).toBe(false)
      await expect(readDeclaredArtifact(root, reportPath, path)).rejects.toThrow()
    }
  })

  test('a declared symlink that escapes .deja is never followed', async () => {
    await put(reportPath, JSON.stringify(report))
    const outside = await put('outside.yaml', 'outside\n')
    await mkdir(join(root, '.deja/flows/checkout'), { recursive: true })
    await symlink(outside, join(root, yamlPath))
    await expect(readDeclaredArtifact(root, reportPath, yamlPath)).rejects.toThrow()
  })

  test('validation history returns recorded outcomes and rejects malformed memory', async () => {
    expect((await readValidationHistory(root, 'checkout')).state).toBe('missing')
    await put('.deja/memory/walks.json', JSON.stringify({
      schemaVersion: 2,
      walks: [{ goalKey: 'checkout', validationHistory: [
        { passed: true, at: report.endedAt, flowSha256: report.yamlSha256 },
        { passed: false, at: report.endedAt, flowSha256: report.yamlSha256 },
      ] }],
    }))
    const history = await readValidationHistory(root, 'checkout')
    expect(history.entries.map(entry => entry.passed)).toEqual([true, false])
    await put('.deja/memory/walks.json', '{bad')
    expect((await readValidationHistory(root, 'checkout')).state).toBe('malformed')
  })
})

describe('viewer redaction', () => {
  test('sensitive keys, bearer values, CLI secrets, and local paths are redacted', () => {
    const result = redactViewerText(`token: abc123\nAuthorization: Bearer abc.def.ghi\n--password value\n${root}/run.yaml`, root)
    expect(result).not.toContain('abc123')
    expect(result).not.toContain('abc.def.ghi')
    expect(result).not.toContain('--password value')
    expect(result).not.toContain(root)
    expect(result).toContain('<redacted>')
    expect(result).toContain('<local-path>')
  })

  test('JSON redaction recursively protects credential-bearing fields', () => {
    const value = redactViewerJson({ nested: { apiKey: 'do-not-show', message: 'safe' } }, root)
    expect(JSON.stringify(value)).not.toContain('do-not-show')
    expect(value.nested.message).toBe('safe')
  })
})
