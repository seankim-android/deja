import './viewer/test-setup.ts'
import { afterEach, beforeEach, describe, expect, test } from 'bun:test'
import { cleanup, fireEvent, render, within } from '@testing-library/react'
import App from './App.tsx'
import { isViewableClientPath } from './viewer/api.ts'

afterEach(() => cleanup())

let screen: ReturnType<typeof within>

function mockFetch(implementation: () => Promise<Response>) {
  globalThis.fetch = Object.assign(implementation, { preconnect: globalThis.fetch.preconnect })
}

beforeEach(() => {
  window.history.replaceState(null, '', '/')
  screen = within(document.body)
  mockFetch(async () => new Response(JSON.stringify({
    state: 'ready', reports: [], message: null,
  }), { status: 200, headers: { 'Content-Type': 'application/json' } }))
})

function metric(label: string) {
  return screen.getByText(label).parentElement?.textContent ?? ''
}

describe('WalkCache local viewer fixtures', () => {
  test('cold discovery shows exploration metrics without suggesting a warm hit', async () => {
    window.history.replaceState(null, '', '/?fixture=cold')
    render(<App />)
    expect(await screen.findByText('cold-recorded')).toBeDefined()
    expect(metric('Model calls')).toContain('13')
    expect(metric('Cache')).toContain('MISS')
    expect(metric('Assertions')).toContain('11')
    expect(metric('Elapsed')).toContain('1m 35.5s')
    expect(await screen.findByText(/launchApp:/)).toBeDefined()
    expect(screen.queryByText('FAIL')).toBeNull()
  })

  test('warm replay makes the zero-model cache hit explicit', async () => {
    window.history.replaceState(null, '', '/?fixture=warm')
    render(<App />)
    expect(await screen.findByText('Exact local hit → standalone YAML replay → validation recorded')).toBeDefined()
    expect(metric('Model calls')).toContain('0')
    expect(metric('Model calls')).toContain('explorer and model factories were not invoked')
    expect(metric('Cache')).toContain('HIT')
    expect(metric('Repairs')).toContain('0')
    fireEvent.click(screen.getByRole('tab', { name: 'Validation history' }))
    expect(await screen.findAllByText('Validation passed')).toHaveLength(2)
  })

  test('bounded repair exposes its proposed diff and preserved assertions', async () => {
    window.history.replaceState(null, '', '/?fixture=repair')
    render(<App />)
    expect(await screen.findByText('Selector issue resolved, product assertions intact.')).toBeDefined()
    expect(metric('Repairs')).toContain('1')
    expect(metric('Model calls')).toContain('0')
    fireEvent.click(screen.getByRole('tab', { name: 'Source diffs (1)' }))
    expect(await screen.findByText(/testTag = "checkout_button"/)).toBeDefined()
    expect(screen.getByText(/autoCommit = false/)).toBeDefined()
    expect(screen.getByText('AUTO-COMMIT: OFF')).toBeDefined()
  })

  test('a product regression stays failed with no repair or success fabrication', async () => {
    window.history.replaceState(null, '', '/?fixture=regression')
    render(<App />)
    expect(await screen.findByText('FAIL')).toBeDefined()
    expect(screen.getByText('product-regression')).toBeDefined()
    expect(screen.getByText(/product_assertion/)).toBeDefined()
    expect(metric('Repairs')).toContain('0')
    fireEvent.click(screen.getByRole('tab', { name: 'Source diffs' }))
    expect(screen.getByText('Repair was not attempted')).toBeDefined()
    expect(screen.queryByText('PASS')).toBeNull()
  })
})

describe('viewer availability and safety', () => {
  test('empty local memory is empty, not a pass, and the CLI fallback remains visible', async () => {
    render(<App />)
    expect(await screen.findByText('No local reports')).toBeDefined()
    expect(screen.getByText('This is empty, not a pass. Select a fixture or use the CLI.')).toBeDefined()
    expect(screen.getByText('The CLI does not need this viewer.')).toBeDefined()
    expect(screen.queryByText('PASS')).toBeNull()
  })

  test('malformed, missing, and in-progress reports never become successful states', async () => {
    const reports = [
      { path: '.walkcache/reports/malformed.json', state: 'malformed', report: null, message: 'Bad JSON', modifiedAt: null },
      { path: '.walkcache/reports/missing.json', state: 'missing', report: null, message: 'Missing', modifiedAt: null },
      { path: '.walkcache/reports/current.json', state: 'in-progress', report: null, message: 'Still running', modifiedAt: null },
    ]
    mockFetch(async () => new Response(JSON.stringify({ state: 'ready', reports, message: null }), {
      status: 200, headers: { 'Content-Type': 'application/json' },
    }))
    render(<App />)
    for (const [button, heading] of [
      [/malformed/, 'Malformed report'],
      [/missing/, 'Report not found'],
      [/in-progress/, 'Run in progress'],
    ] as const) {
      fireEvent.click(await screen.findByRole('button', { name: button }))
      expect(await screen.findByRole('heading', { name: heading })).toBeDefined()
      expect(screen.queryByText('PASS')).toBeNull()
    }
  })

  test('client path checks restrict arbitrary files, traversal, and credential-like paths', () => {
    expect(isViewableClientPath('.walkcache/flows/checkout/run.yaml')).toBe(true)
    expect(isViewableClientPath('/etc/passwd')).toBe(false)
    expect(isViewableClientPath('.walkcache/../outside.yaml')).toBe(false)
    expect(isViewableClientPath('.walkcache/secrets/run.yaml')).toBe(false)
    expect(isViewableClientPath('.walkcache/reports/raw.log')).toBe(false)
  })

  test('an unavailable viewer API leaves the headless fallback intact', async () => {
    mockFetch(async () => { throw new Error('server stopped') })
    render(<App />)
    expect(await screen.findByText('Viewer API unavailable')).toBeDefined()
    expect(screen.getByText('The CLI does not need this viewer.')).toBeDefined()
  })
})
