import type { RunResult, StoredValidation } from '@deja-qa/core'

export type ViewerReportState = 'ready' | 'malformed' | 'missing' | 'in-progress'

export type ViewerReportEntry = {
  path: string
  state: ViewerReportState
  report: RunResult | null
  message: string | null
  modifiedAt: string | null
}

export type ViewerRunList = {
  state: 'ready' | 'unavailable'
  reports: ViewerReportEntry[]
  message: string | null
}

export type ViewerHistory = {
  state: 'ready' | 'empty' | 'missing' | 'malformed' | 'unavailable'
  entries: StoredValidation[]
  message: string | null
}

export type ViewerArtifact = {
  state: 'ready'
  path: string
  contents: string
}
