import type {
  ViewerArtifact,
  ViewerHistory,
  ViewerRunList,
} from '../../viewer/contracts.ts'

const EMPTY_RUNS: ViewerRunList = { state: 'ready', reports: [], message: null }

async function responseJson(response: Response) {
  const value: unknown = await response.json()
  if (!response.ok) {
    const message = typeof value === 'object' && value !== null && 'message' in value &&
      typeof value.message === 'string' ? value.message : 'Local inspection is unavailable.'
    throw new Error(message)
  }
  return value
}

export async function loadRuns(signal?: AbortSignal): Promise<ViewerRunList> {
  const response = await fetch('/api/deja/runs', { signal, cache: 'no-store' })
  const value = await responseJson(response)
  if (typeof value !== 'object' || value === null || !('reports' in value) ||
      !Array.isArray(value.reports)) throw new Error('The local run index is malformed.')
  return value as ViewerRunList
}

export async function loadHistory(goalKey: string, signal?: AbortSignal): Promise<ViewerHistory> {
  const response = await fetch(`/api/deja/history?goal=${encodeURIComponent(goalKey)}`, {
    signal, cache: 'no-store',
  })
  return await responseJson(response) as ViewerHistory
}

export async function loadArtifact(
  reportPath: string,
  artifactPath: string,
  signal?: AbortSignal,
): Promise<ViewerArtifact> {
  const query = new URLSearchParams({ report: reportPath, path: artifactPath })
  const response = await fetch(`/api/deja/artifact?${query}`, { signal, cache: 'no-store' })
  return await responseJson(response) as ViewerArtifact
}

export function unavailableRuns(message: string): ViewerRunList {
  return { ...EMPTY_RUNS, state: 'unavailable', message }
}

/** UI gating is advisory; the server independently verifies report declaration and canonical paths. */
export function isViewableClientPath(path: string) {
  return /^\.deja\/(?:[a-zA-Z0-9._-]+\/)*[a-zA-Z0-9._-]+\.(?:yaml|yml|json|diff|patch|txt)$/.test(path) &&
    !path.split('/').includes('..') &&
    !path.split('/').some(segment =>
      /(?:^|[._-])(?:env|credential|credentials|secret|secrets|token|password|passwd|keystore|private-key|id_rsa)(?:[._-]|$)/i.test(segment))
}
