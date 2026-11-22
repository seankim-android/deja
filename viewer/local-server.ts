import { createServer as createHttpServer, type IncomingMessage, type ServerResponse } from 'node:http'
import { readdir, readFile, realpath, stat } from 'node:fs/promises'
import { extname, isAbsolute, resolve, sep } from 'node:path'
import type { StoredValidation } from '../packages/core/src/contracts.ts'
import type {
  ViewerArtifact,
  ViewerHistory,
  ViewerReportEntry,
  ViewerRunList,
} from './contracts.ts'
import { redactViewerJson, redactViewerText } from './redaction.ts'

const REPORT_DIRECTORY = '.walkcache/reports'
const MEMORY_PATH = '.walkcache/memory/walks.json'
const MAX_FILE_BYTES = 2_000_000
const MAX_REPORTS = 50
const VIEWABLE_KINDS = new Set(['yaml', 'diff', 'memory', 'model-metrics', 'repair-report'])
const VIEWABLE_EXTENSIONS = new Set(['.yaml', '.yml', '.json', '.diff', '.patch', '.txt'])
const FORBIDDEN_NAME = /(?:^|[._-])(?:env|credential|credentials|secret|secrets|token|password|passwd|keystore|private-key|id_rsa)(?:[._-]|$)/i
const COMMANDS = new Set(['explore', 'run', 'remember'])
const OUTCOMES = new Set([
  'passed', 'remembered', 'cold-recorded', 'repaired', 'rejected', 'exploration-rejected',
  'product-regression', 'repair-rejected', 'failed',
])
const CACHE_STATES = new Set(['hit', 'miss', 'not-checked'])

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function metric(value: unknown) {
  return value === null || (typeof value === 'number' && Number.isFinite(value) && value >= 0)
}

function timestamp(value: unknown): value is string {
  return typeof value === 'string' && Number.isFinite(Date.parse(value))
}

function safeReportPath(path: string) {
  return /^\.walkcache\/reports\/[a-zA-Z0-9][a-zA-Z0-9._-]*\.json$/.test(path) &&
    !path.split('/').some(segment => FORBIDDEN_NAME.test(segment))
}

export function safeArtifactPath(path: string) {
  if (path.length === 0 || path.length > 1_024 || path.includes('\0') || path.includes('\\') ||
      isAbsolute(path) || !path.startsWith('.walkcache/') || path.split('/').includes('..') ||
      !VIEWABLE_EXTENSIONS.has(extname(path).toLowerCase()) ||
      path.split('/').some(segment => FORBIDDEN_NAME.test(segment))) return false
  return path.split('/').every(segment => segment.length > 0 && segment !== '.' && segment !== '..')
}

function validArtifact(value: unknown) {
  return object(value) && typeof value.kind === 'string' && typeof value.path === 'string'
}

function validDiff(value: unknown) {
  return object(value) && (value.path === null || typeof value.path === 'string') &&
    Array.isArray(value.affectedFiles) && value.affectedFiles.every(file => typeof file === 'string') &&
    typeof value.contents === 'string' && value.autoCommit === false
}

function supportedReport(value: unknown): value is NonNullable<ViewerReportEntry['report']> {
  if (!object(value) || value.schemaVersion !== 1 || !COMMANDS.has(String(value.command)) ||
      !object(value.identity) || typeof value.identity.goal !== 'string' ||
      typeof value.identity.appId !== 'string' || typeof value.identity.platform !== 'string' ||
      typeof value.identity.flowSchemaVersion !== 'string' ||
      typeof value.identity.buildFingerprint !== 'string' || typeof value.goalKey !== 'string' ||
      !CACHE_STATES.has(String(value.cacheStatus)) || !OUTCOMES.has(String(value.outcome)) ||
      typeof value.passed !== 'boolean' || !timestamp(value.startedAt) || !timestamp(value.endedAt) ||
      !metric(value.durationMs) || value.durationMs === null || !metric(value.modelCalls) ||
      typeof value.modelCallBasis !== 'string' || !metric(value.assertions) ||
      (value.repairCount !== 0 && value.repairCount !== 1) ||
      (value.yamlPath !== null && typeof value.yamlPath !== 'string') ||
      (value.yamlSha256 !== null && typeof value.yamlSha256 !== 'string') ||
      !Array.isArray(value.artifacts) || value.artifacts.length > 100 || !value.artifacts.every(validArtifact) ||
      !Array.isArray(value.diffs) || value.diffs.length > 20 || !value.diffs.every(validDiff) ||
      !Array.isArray(value.errors) || !value.errors.every(error => typeof error === 'string')) return false
  return true
}

class ViewerError extends Error {
  readonly status: number
  readonly state: 'missing' | 'malformed' | 'restricted' | 'unavailable' | 'in-progress'

  constructor(status: number, state: ViewerError['state'], message: string) {
    super(message)
    this.status = status
    this.state = state
  }
}

function errorCode(error: unknown) {
  return (error as NodeJS.ErrnoException).code
}

async function readBoundedFile(path: string) {
  const details = await stat(path)
  if (!details.isFile()) throw new ViewerError(404, 'missing', 'The local artifact is not a file.')
  if (details.size > MAX_FILE_BYTES) {
    throw new ViewerError(413, 'restricted', 'The local artifact exceeds the 2 MB inspection limit.')
  }
  return readFile(path, 'utf8')
}

function parseReport(contents: string): NonNullable<ViewerReportEntry['report']> {
  let value: unknown
  try {
    value = JSON.parse(contents)
  } catch {
    throw new ViewerError(422, 'malformed', 'The report is not valid JSON.')
  }
  if (object(value) && (value.outcome === 'in-progress' || value.endedAt === null)) {
    throw new ViewerError(202, 'in-progress', 'The report is still being written; no outcome is assumed.')
  }
  if (!supportedReport(value)) {
    throw new ViewerError(422, 'malformed', 'The report does not match WalkCache run schema v1.')
  }
  return value
}

async function reportEntry(root: string, path: string, modifiedAt: string | null) {
  try {
    const canonical = await realpath(resolve(root, path))
    if (!canonical.startsWith(`${resolve(root, REPORT_DIRECTORY)}${sep}`)) {
      throw new ViewerError(403, 'restricted', 'Report path is outside the local report directory.')
    }
    const report = redactViewerJson(parseReport(await readBoundedFile(canonical)), root)
    return { path, state: 'ready', report, message: null, modifiedAt } satisfies ViewerReportEntry
  } catch (error) {
    if (error instanceof ViewerError && error.state === 'in-progress') {
      return { path, state: 'in-progress', report: null, message: error.message, modifiedAt } satisfies ViewerReportEntry
    }
    if (errorCode(error) === 'ENOENT') {
      return { path, state: 'missing', report: null, message: 'The report disappeared before it could be read.', modifiedAt } satisfies ViewerReportEntry
    }
    return {
      path,
      state: 'malformed',
      report: null,
      message: error instanceof ViewerError ? error.message : 'The report could not be read safely.',
      modifiedAt,
    } satisfies ViewerReportEntry
  }
}

export async function listWorkspaceReports(workspaceRoot: string): Promise<ViewerRunList> {
  const root = await realpath(resolve(workspaceRoot))
  try {
    const directory = resolve(root, REPORT_DIRECTORY)
    const entries = await readdir(directory, { withFileTypes: true })
    const reports = await Promise.all(entries
      .filter(entry => entry.isFile() && entry.name.endsWith('.json'))
      .map(entry => `.walkcache/reports/${entry.name}`)
      .filter(safeReportPath)
      .sort().reverse().slice(0, MAX_REPORTS)
      .map(async path => {
        let modifiedAt: string | null = null
        try { modifiedAt = (await stat(resolve(root, path))).mtime.toISOString() } catch { /* handled below */ }
        return reportEntry(root, path, modifiedAt)
      }))
    return { state: 'ready', reports, message: null }
  } catch (error) {
    if (errorCode(error) === 'ENOENT') return { state: 'ready', reports: [], message: null }
    return {
      state: 'unavailable', reports: [],
      message: 'Local reports are unavailable. Use the CLI fallback below.',
    }
  }
}

function declaredArtifacts(report: NonNullable<ViewerReportEntry['report']>) {
  const paths = new Map<string, string>()
  if (report.yamlPath) paths.set(report.yamlPath, 'yaml')
  for (const artifact of report.artifacts) paths.set(artifact.path, artifact.kind)
  for (const diff of report.diffs) if (diff.path) paths.set(diff.path, 'diff')
  return paths
}

async function readReportForArtifact(root: string, reportPath: string) {
  if (!safeReportPath(reportPath)) {
    throw new ViewerError(403, 'restricted', 'Only a direct .walkcache/reports JSON file can authorize artifacts.')
  }
  try {
    const canonical = await realpath(resolve(root, reportPath))
    if (!canonical.startsWith(`${resolve(root, REPORT_DIRECTORY)}${sep}`)) {
      throw new ViewerError(403, 'restricted', 'Report path is outside the local report directory.')
    }
    return parseReport(await readBoundedFile(canonical))
  } catch (error) {
    if (errorCode(error) === 'ENOENT') throw new ViewerError(404, 'missing', 'The authorizing report is missing.')
    throw error
  }
}

export async function readDeclaredArtifact(
  workspaceRoot: string,
  reportPath: string,
  artifactPath: string,
): Promise<ViewerArtifact> {
  const root = await realpath(resolve(workspaceRoot))
  if (!safeArtifactPath(artifactPath)) {
    throw new ViewerError(403, 'restricted', 'Only safe relative .walkcache YAML, JSON, text, and diff artifacts are viewable.')
  }
  const report = await readReportForArtifact(root, reportPath)
  const kind = declaredArtifacts(report).get(artifactPath)
  if (!kind || !VIEWABLE_KINDS.has(kind)) {
    throw new ViewerError(403, 'restricted', 'The artifact is not declared as a viewable artifact by this report.')
  }
  const candidate = resolve(root, artifactPath)
  let canonical: string
  try {
    canonical = await realpath(candidate)
  } catch (error) {
    if (errorCode(error) === 'ENOENT') throw new ViewerError(404, 'missing', 'The declared artifact is missing or still being written.')
    throw error
  }
  const allowedRoot = resolve(root, '.walkcache')
  if (!canonical.startsWith(`${allowedRoot}${sep}`)) {
    throw new ViewerError(403, 'restricted', 'The artifact resolves outside .walkcache; symlinks are not followed.')
  }
  return {
    state: 'ready', path: artifactPath,
    contents: redactViewerText(await readBoundedFile(canonical), root),
  }
}

export async function readValidationHistory(workspaceRoot: string, goalKey: string): Promise<ViewerHistory> {
  const root = await realpath(resolve(workspaceRoot))
  if (!/^[a-z0-9][a-z0-9 .:_/-]{0,159}$/i.test(goalKey)) {
    return { state: 'malformed', entries: [], message: 'A valid local goal key is required.' }
  }
  try {
    const canonical = await realpath(resolve(root, MEMORY_PATH))
    if (!canonical.startsWith(`${resolve(root, '.walkcache')}${sep}`)) {
      return { state: 'unavailable', entries: [], message: 'Validation memory resolves outside .walkcache.' }
    }
    const value: unknown = JSON.parse(await readBoundedFile(canonical))
    if (!object(value) || ![1, 2].includes(Number(value.schemaVersion)) || !Array.isArray(value.walks)) {
      return { state: 'malformed', entries: [], message: 'Local validation memory is malformed.' }
    }
    const walk = value.walks.find(entry => object(entry) && entry.goalKey === goalKey)
    if (!walk) return { state: 'empty', entries: [], message: 'No validation history is stored for this goal.' }
    if (!object(walk) || !Array.isArray(walk.validationHistory)) {
      return { state: 'malformed', entries: [], message: 'Validation history is malformed for this goal.' }
    }
    const entries: StoredValidation[] = walk.validationHistory.map(entry => {
      if (!object(entry) || typeof entry.passed !== 'boolean' || !timestamp(entry.at) ||
          typeof entry.flowSha256 !== 'string') {
        throw new ViewerError(422, 'malformed', 'Validation history contains an invalid entry.')
      }
      return { passed: entry.passed, at: entry.at, flowSha256: entry.flowSha256 }
    })
    return { state: entries.length ? 'ready' : 'empty', entries, message: null }
  } catch (error) {
    if (errorCode(error) === 'ENOENT') {
      return { state: 'missing', entries: [], message: 'Validation memory is not available for this run.' }
    }
    if (error instanceof ViewerError) return { state: error.state === 'unavailable' ? 'unavailable' : 'malformed', entries: [], message: error.message }
    return { state: 'malformed', entries: [], message: 'Validation memory is not valid JSON or cannot be read safely.' }
  }
}

function sendJson(response: ServerResponse, status: number, value: unknown) {
  response.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Cache-Control': 'no-store',
    'X-Content-Type-Options': 'nosniff',
  })
  response.end(JSON.stringify(value))
}

export function viewerHandler(workspaceRoot: string) {
  return async (request: IncomingMessage, response: ServerResponse) => {
    const url = new URL(request.url ?? '/', 'http://127.0.0.1')
    if (request.method !== 'GET') {
      sendJson(response, 405, { state: 'restricted', message: 'The local viewer API is read-only.' })
      return
    }
    try {
      if (url.pathname === '/api/walkcache/runs') {
        sendJson(response, 200, await listWorkspaceReports(workspaceRoot))
      } else if (url.pathname === '/api/walkcache/history') {
        sendJson(response, 200, await readValidationHistory(workspaceRoot, url.searchParams.get('goal') ?? ''))
      } else if (url.pathname === '/api/walkcache/artifact') {
        sendJson(response, 200, await readDeclaredArtifact(
          workspaceRoot,
          url.searchParams.get('report') ?? '',
          url.searchParams.get('path') ?? '',
        ))
      } else {
        sendJson(response, 404, { state: 'missing', message: 'Unknown local viewer endpoint.' })
      }
    } catch (error) {
      if (error instanceof ViewerError) {
        sendJson(response, error.status, { state: error.state, message: error.message })
      } else {
        sendJson(response, 500, { state: 'unavailable', message: 'Local inspection failed safely. Use the CLI fallback.' })
      }
    }
  }
}

/** Optional standalone read-only server; it never imports or starts the WalkCache lifecycle. */
export function createViewerServer(workspaceRoot: string) {
  const handler = viewerHandler(workspaceRoot)
  return createHttpServer((request, response) => {
    if (!(request.url ?? '').startsWith('/api/walkcache/')) {
      sendJson(response, 404, { state: 'missing', message: 'Use the Vite viewer for the local UI.' })
      return
    }
    void handler(request, response)
  })
}
