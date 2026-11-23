import { useCallback, useEffect, useState } from 'react'
import type { RunArtifact, RunResult } from '@deja-qa/core'
import type {
  ViewerArtifact,
  ViewerHistory,
  ViewerReportEntry,
  ViewerRunList,
} from '../viewer/contracts.ts'
import './App.css'
import {
  isViewableClientPath,
  loadArtifact,
  loadHistory,
  loadRuns,
  unavailableRuns,
} from './viewer/api.ts'
import { fixtureFromSearch, fixtures, type FixtureName, type ViewerFixture } from './viewer/fixtures.ts'

type Selection = {
  entry: ViewerReportEntry
  fixture: ViewerFixture | null
}

type Tab = 'artifact' | 'history' | 'diffs'
type LoadState<T> =
  | { state: 'loading' | 'empty'; message: string | null; data: null }
  | { state: 'ready' | 'error'; message: string | null; data: T | null }

const loading = <T,>(): LoadState<T> => ({ state: 'loading', message: null, data: null })
const viewableKinds = new Set<RunArtifact['kind']>(['yaml', 'diff', 'memory', 'model-metrics', 'repair-report'])

function fixtureSelection(name: FixtureName): Selection {
  return { entry: fixtures[name].entry, fixture: fixtures[name] }
}

function formatDuration(milliseconds: number) {
  if (milliseconds < 1_000) return `${milliseconds} ms`
  const seconds = milliseconds / 1_000
  return seconds < 60 ? `${seconds.toFixed(2)} s` : `${Math.floor(seconds / 60)}m ${(seconds % 60).toFixed(1)}s`
}

function lifecycleCopy(report: RunResult) {
  if (report.replayedOnNewBuild) {
    return report.passed
      ? 'Stale build → last verified YAML replayed on an unverified build → stored for this build, replacing the earlier build walk (origin replayed-on-new-build)'
      : `Stale build → last verified YAML replayed on an unverified build → ${report.outcome}, nothing stored; the earlier build walk is unchanged`
  }
  switch (report.outcome) {
    case 'cold-recorded': return 'Cache miss → verified cold exploration → reusable walk stored'
    case 'passed': return 'Exact local hit → standalone YAML replay → validation recorded'
    case 'repaired': return 'Stale build → one bounded selector repair → independent verification passed'
    case 'product-regression': return 'Exact local hit → product assertion failed → stopped without repair'
    case 'remembered': return 'Existing YAML → independent verification → reusable walk stored'
    default: return `${report.cacheStatus} → ${report.outcome} → no successful state inferred`
  }
}

function StateMessage({ entry }: { entry: ViewerReportEntry }) {
  const copy = {
    malformed: ['Malformed report', 'This file does not match the supported run schema. Nothing here is treated as a pass.'],
    missing: ['Report not found', 'The report may have moved or disappeared while the local index was being read.'],
    'in-progress': ['Run in progress', 'The report has no final outcome. Wait for the CLI to finish, then refresh the read-only index.'],
    ready: ['', ''],
  }[entry.state]
  return (
    <section className="state-card" aria-label={copy[0]}>
      <span className={`state-symbol ${entry.state}`} aria-hidden="true">{entry.state === 'in-progress' ? '…' : '!'}</span>
      <p className="eyebrow">{entry.path}</p>
      <h2>{copy[0]}</h2>
      <p>{entry.message ?? copy[1]}</p>
      <p className="muted">{copy[1]}</p>
    </section>
  )
}

function Metric({ label, value, detail, tone = 'default' }: {
  label: string
  value: string
  detail?: string
  tone?: 'default' | 'green' | 'amber'
}) {
  return (
    <div className={`metric ${tone}`}>
      <span>{label}</span>
      <strong>{value}</strong>
      {detail && <small>{detail}</small>}
    </div>
  )
}

function ArtifactPanel({ selection }: { selection: Selection }) {
  const report = selection.entry.report!
  const initialPath = report.yamlPath ?? report.artifacts.find(artifact =>
    viewableKinds.has(artifact.kind) && isViewableClientPath(artifact.path))?.path ?? ''
  const [tab, setTab] = useState<Tab>('artifact')
  const [artifactPath, setArtifactPath] = useState(initialPath)
  const [artifact, setArtifact] = useState<LoadState<ViewerArtifact>>(() =>
    !artifactPath
      ? { state: 'empty', message: 'This report has no generated YAML.', data: null }
      : !isViewableClientPath(artifactPath)
        ? { state: 'error', message: 'This artifact path is restricted.', data: null }
        : loading())
  const [history, setHistory] = useState<LoadState<ViewerHistory>>(loading)

  useEffect(() => {
    const controller = new AbortController()
    if (!artifactPath || !isViewableClientPath(artifactPath)) return () => controller.abort()
    const fixtureContents = selection.fixture?.artifacts[artifactPath]
    const request = fixtureContents !== undefined
      ? Promise.resolve({ state: 'ready' as const, path: artifactPath, contents: fixtureContents })
      : selection.fixture
        ? Promise.reject(new Error('The fixture artifact is missing.'))
        : loadArtifact(selection.entry.path, artifactPath, controller.signal)
    void request.then(value => {
      if (!controller.signal.aborted) setArtifact({ state: 'ready', data: value, message: null })
    }).catch((error: unknown) => {
      if (!controller.signal.aborted) {
        setArtifact({
          state: 'error', data: null,
          message: error instanceof Error ? error.message : 'Artifact inspection is unavailable.',
        })
      }
    })
    return () => controller.abort()
  }, [artifactPath, selection])

  useEffect(() => {
    const controller = new AbortController()
    const request = selection.fixture
      ? Promise.resolve(selection.fixture.history)
      : loadHistory(report.goalKey, controller.signal)
    void request.then(value => {
      if (!controller.signal.aborted) setHistory({ state: 'ready', data: value, message: null })
    }).catch((error: unknown) => {
      if (!controller.signal.aborted) {
        setHistory({
          state: 'error', data: null,
          message: error instanceof Error ? error.message : 'Validation history is unavailable.',
        })
      }
    })
    return () => controller.abort()
  }, [report.goalKey, selection])

  return (
    <section className="artifact-section" aria-label="Run artifacts">
      <div className="tabs" role="tablist" aria-label="Artifact views">
        {([
          ['artifact', 'YAML & artifacts'],
          ['history', 'Validation history'],
          ['diffs', `Source diffs${report.diffs.length ? ` (${report.diffs.length})` : ''}`],
        ] as Array<[Tab, string]>).map(([id, label]) => (
          <button
            key={id} type="button" role="tab" aria-selected={tab === id} aria-controls="artifact-panel"
            className={tab === id ? 'active' : ''} onClick={() => setTab(id)}
          >{label}</button>
        ))}
      </div>
      <div id="artifact-panel" role="tabpanel" className="artifact-panel">
        {tab === 'artifact' && (
          <div className="artifact-layout">
            <div className="code-column">
              <div className="code-heading">
                <div>
                  <p className="eyebrow">{artifactPath === report.yamlPath ? 'Generated YAML' : 'Selected artifact'}</p>
                  <code>{artifactPath || 'none'}</code>
                </div>
                <span className="read-badge">READ ONLY</span>
              </div>
              {artifact.state === 'loading' && <p className="panel-message">Reading declared local artifact…</p>}
              {(artifact.state === 'empty' || artifact.state === 'error') && (
                <div className="inline-state" role="status">
                  <strong>{artifact.state === 'empty' ? 'No YAML artifact' : 'Artifact unavailable'}</strong>
                  <p>{artifact.message}</p>
                </div>
              )}
              {artifact.state === 'ready' && artifact.data && (
                <pre className="code-view" data-testid="artifact-contents"><code>{artifact.data.contents}</code></pre>
              )}
            </div>
            <aside className="artifact-list" aria-label="Declared artifacts">
              <p className="eyebrow">Declared artifacts</p>
              {report.artifacts.length === 0 && <p className="muted">No artifacts declared.</p>}
              {report.artifacts.map((artifactEntry, index) => {
                const viewable = viewableKinds.has(artifactEntry.kind) && isViewableClientPath(artifactEntry.path)
                return (
                  <div className="artifact-row" key={`${artifactEntry.path}-${index}`}>
                    <span className="artifact-kind">{artifactEntry.kind}</span>
                    {viewable ? (
                      <button
                        type="button" className={artifactPath === artifactEntry.path ? 'selected' : ''}
                        onClick={() => {
                          setArtifactPath(artifactEntry.path)
                          setArtifact(loading)
                          setTab('artifact')
                        }}
                      >{artifactEntry.path}</button>
                    ) : (
                      <span className="restricted-path" title="Raw logs and undeclared file types are never loaded">
                        {artifactEntry.path}<small>raw / restricted</small>
                      </span>
                    )}
                  </div>
                )
              })}
            </aside>
          </div>
        )}
        {tab === 'history' && (
          <div>
            <div className="code-heading">
              <div><p className="eyebrow">Validation history</p><h3>Recorded outcomes only</h3></div>
              <span className="read-badge">LOCAL MEMORY</span>
            </div>
            {history.state === 'loading' && <p className="panel-message">Reading validation memory…</p>}
            {history.state === 'error' && <div className="inline-state"><strong>History unavailable</strong><p>{history.message}</p></div>}
            {history.state === 'ready' && history.data && (
              history.data.entries.length > 0 ? (
                <ol className="history-list">
                  {history.data.entries.map((validation, index) => (
                    <li key={`${validation.at}-${index}`}>
                      <span className={`history-dot ${validation.passed ? 'pass' : 'fail'}`} aria-hidden="true" />
                      <div><strong>{validation.passed ? 'Validation passed' : 'Validation failed'}</strong><time dateTime={validation.at}>{validation.at}</time></div>
                      <code title={validation.flowSha256}>flow {validation.flowSha256.slice(0, 12)}</code>
                    </li>
                  ))}
                </ol>
              ) : <div className="inline-state"><strong>No recorded history</strong><p>{history.data.message ?? 'No validation entries are stored for this goal.'}</p></div>
            )}
          </div>
        )}
        {tab === 'diffs' && (
          <div>
            <div className="code-heading">
              <div><p className="eyebrow">Proposed source changes</p><h3>{report.diffs.length ? `${report.diffs.length} bounded diff` : 'No source diff'}</h3></div>
              <span className="read-badge">AUTO-COMMIT: OFF</span>
            </div>
            {report.diffs.length === 0 && (
              <div className="inline-state">
                <strong>{report.outcome === 'product-regression' ? 'Repair was not attempted' : 'No source changes proposed'}</strong>
                <p>{report.outcome === 'product-regression'
                  ? 'A meaningful product assertion is preserved as a failure, never patched into a pass.'
                  : 'This run did not modify fixture or application source.'}</p>
              </div>
            )}
            {report.diffs.map((diff, index) => (
              <div className="diff-block" key={`${diff.path ?? 'inline'}-${index}`}>
                <p className="diff-files">Affected: {diff.affectedFiles.join(', ')}</p>
                <pre className="code-view diff-view"><code>{diff.contents}</code></pre>
                <p className="diff-note">Proposed only · autoCommit = false · independently reviewed before any use</p>
              </div>
            ))}
          </div>
        )}
      </div>
    </section>
  )
}

function Report({ selection }: { selection: Selection }) {
  const report = selection.entry.report!
  const status = report.passed ? 'PASS' : 'FAIL'
  return (
    <>
      <section className="run-heading">
        <div>
          <p className="eyebrow">{report.command} / {report.identity.platform} / {report.startedAt}</p>
          <h1>{report.goalKey}<span className={`status-dot ${report.passed ? 'pass' : 'fail'}`} /></h1>
          <p className="app-id">{report.identity.appId} · build <code>{report.identity.buildFingerprint.slice(0, 12)}</code></p>
        </div>
        <div className={`run-status ${report.passed ? 'pass' : 'fail'}`}>
          <span>{status}</span><strong>{report.outcome}</strong>
        </div>
      </section>
      <section className={`outcome-banner ${report.passed ? 'success' : 'failure'}`} aria-label="Run explanation">
        <strong>{report.outcome === 'repaired' ? 'Selector issue resolved, product assertions intact.' :
          report.passed ? 'Verified run result.' : 'Failure preserved. No repair was substituted.'}</strong>
        <p>{lifecycleCopy(report)}</p>
        {report.failure && (
          <p className="failure-detail">
            {report.passed ? 'Resolved diagnosis' : 'Failure'}: <code>{report.failure.category}</code>
            {report.failure.failingStep ? ` · ${report.failure.failingStep}` : ''}
          </p>
        )}
        {report.errors.map((error, index) => <p className="error-copy" key={`${error}-${index}`}>{error}</p>)}
      </section>
      <section className="metrics" aria-label="Run metrics">
        <Metric label="Elapsed" value={formatDuration(report.durationMs)} detail={`${report.durationMs.toLocaleString()} ms total`} />
        <Metric label="Model calls" value={report.modelCalls?.toString() ?? 'unknown'} detail={report.modelCallBasis} tone={report.modelCalls === 0 ? 'green' : 'amber'} />
        <Metric label="Cache" value={report.cacheStatus.toUpperCase()} detail={report.cacheMissReason ?? (report.cacheStatus === 'hit' ? 'exact build + flow identity' : 'lookup not required')} />
        <Metric label="Assertions" value={report.assertions?.toString() ?? 'unknown'} detail="reported checks, not inferred" />
        <Metric label="Repairs" value={report.repairCount.toString()} detail={report.repairCount ? 'one bounded attempt' : 'no source repair'} tone={report.repairCount ? 'amber' : 'default'} />
      </section>
      <ArtifactPanel key={selection.entry.path} selection={selection} />
    </>
  )
}

export default function App() {
  const initialFixture = fixtureFromSearch(window.location.search)
  const [selection, setSelection] = useState<Selection | null>(initialFixture ? fixtureSelection(initialFixture) : null)
  const [runList, setRunList] = useState<ViewerRunList>({ state: 'ready', reports: [], message: null })
  const [indexLoading, setIndexLoading] = useState(true)

  const refresh = useCallback(async (signal?: AbortSignal) => {
    try {
      const value = await loadRuns(signal)
      if (signal?.aborted) return
      setRunList(value)
      setSelection(current => {
        if (!current || current.fixture) return current
        const replacement = value.reports.find(entry => entry.path === current.entry.path)
        return replacement ? { entry: replacement, fixture: null } : {
          entry: { ...current.entry, state: 'missing', report: null, message: 'The selected report is no longer in the local index.' },
          fixture: null,
        }
      })
    } catch (error: unknown) {
      if (!signal?.aborted) {
        setRunList(unavailableRuns(error instanceof Error ? error.message : 'The local report index is unavailable.'))
      }
    } finally {
      if (!signal?.aborted) setIndexLoading(false)
    }
  }, [])

  useEffect(() => {
    const controller = new AbortController()
    const timer = window.setTimeout(() => { void refresh(controller.signal) }, 0)
    return () => {
      window.clearTimeout(timer)
      controller.abort()
    }
  }, [refresh])

  function selectFixture(name: FixtureName) {
    setSelection(fixtureSelection(name))
    window.history.replaceState(null, '', `${window.location.pathname}?fixture=${name}`)
  }

  return (
    <div className="viewer-shell">
      <header className="topbar">
        <a className="brand" href="/" aria-label="Déjà local viewer home">
          <span className="brand-mark">WC</span>
          <span><strong>DEJA</strong><small>LOCAL RUN VIEWER</small></span>
        </a>
        <div className="topbar-meta"><span className="local-indicator">127.0.0.1</span><span className="read-badge">READ ONLY</span></div>
      </header>
      <div className="workspace">
        <aside className="sidebar">
          <section>
            <div className="sidebar-heading"><p className="eyebrow">Fixture gallery</p><span>4</span></div>
            <p className="sidebar-note">Deterministic, secret-free examples.</p>
            <nav className="run-nav" aria-label="Fixture runs">
              {(Object.keys(fixtures) as FixtureName[]).map(name => {
                const fixture = fixtures[name]
                const report = fixture.entry.report!
                return (
                  <button
                    type="button" key={name} onClick={() => selectFixture(name)}
                    className={selection?.fixture?.name === name ? 'selected' : ''}
                    aria-pressed={selection?.fixture?.name === name}
                  >
                    <span className={`nav-dot ${report.passed ? 'pass' : 'fail'}`} aria-hidden="true" />
                    <span><strong>{fixture.label}</strong><small>{report.cacheStatus} · {report.modelCalls ?? 'unknown'} model calls</small></span>
                    <span className="nav-arrow" aria-hidden="true">›</span>
                  </button>
                )
              })}
            </nav>
          </section>
          <section className="local-index">
            <div className="sidebar-heading"><p className="eyebrow">Workspace reports</p>
              <button className="refresh-button" type="button" onClick={() => {
                setIndexLoading(true)
                void refresh()
              }} disabled={indexLoading}>
                {indexLoading ? 'Reading…' : 'Refresh'}
              </button>
            </div>
            <p className="sidebar-note">Latest .deja/reports JSON, capped at 50.</p>
            {runList.state === 'unavailable' && <div className="sidebar-state"><strong>Viewer API unavailable</strong><p>{runList.message}</p></div>}
            {runList.state === 'ready' && runList.reports.length === 0 && !indexLoading && (
              <div className="sidebar-state"><strong>No local reports</strong><p>This is empty, not a pass. Select a fixture or use the CLI.</p></div>
            )}
            <nav className="run-nav local" aria-label="Workspace reports">
              {runList.reports.map(entry => (
                <button type="button" key={entry.path} onClick={() => setSelection({ entry, fixture: null })}
                  className={!selection?.fixture && selection?.entry.path === entry.path ? 'selected' : ''}>
                  <span className={`nav-dot ${entry.state === 'ready' ? entry.report?.passed ? 'pass' : 'fail' : 'unknown'}`} aria-hidden="true" />
                  <span><strong>{entry.report?.goalKey ?? entry.state}</strong><small>{entry.path.split('/').at(-1)}</small></span>
                  <span className="nav-arrow" aria-hidden="true">›</span>
                </button>
              ))}
            </nav>
          </section>
          <div className="security-note"><strong>Bounded inspection</strong><p>No run controls. No uploads. Only declared .deja artifacts are read.</p></div>
        </aside>
        <main>
          {selection ? (
            selection.entry.state === 'ready' && selection.entry.report
              ? <Report selection={selection} />
              : <StateMessage entry={selection.entry} />
          ) : (
            <section className="welcome-state">
              <p className="eyebrow">OPTIONAL OBSERVABILITY / ZERO LIFECYCLE DEPENDENCY</p>
              <h1>Inspect the walk.<br />Keep the evidence honest.</h1>
              <p>Cold discovery, model-free replay, bounded repair, and real product failures—without reading source or starting another run.</p>
              <div className="welcome-hint"><span>←</span><p><strong>Start with a fixture</strong>Select Cold discovery or Warm replay. Fixtures never touch local files.</p></div>
            </section>
          )}
          <section className="cli-fallback" aria-label="Headless CLI fallback">
            <div><p className="eyebrow">Headless fallback / always available</p><h2>The CLI does not need this viewer.</h2><p>Run from the configured workspace. Inspection is read-only; lifecycle commands stay explicit in the terminal.</p></div>
            <div className="cli-command"><span>$</span><code>bun run deja -- inspect .deja/reports/latest.json --config deja.config.json</code></div>
            <div className="cli-command"><span>$</span><code>bun run deja -- list --config deja.config.json</code></div>
            <p className="fallback-note">If the viewer is stopped or unavailable, these commands provide the same outcome, metrics, artifact paths, failures, and diffs.</p>
          </section>
        </main>
      </div>
      <footer><span>DEJA / SCHEMA V1</span><span>NO ARBITRARY FILE LOADS · NO COMMAND EXECUTION · NO AUTO-COMMIT</span></footer>
    </div>
  )
}
