import type {
  CandidateAction,
  ElementRole,
  ExplorationDriver,
  FingerprintRules,
  LaunchProfile,
  Observation,
  RawElement,
  Segment,
  StepResult,
  SystemUiState,
} from '@deja-run/core'
import { normalizeElements, normalizedSnapshot } from '@deja-run/core'
import { parseAllDocuments } from 'yaml'
import { tapSegment } from './selectors.ts'

/** Result of one hierarchy read. `raw` is whatever the transport got back, parsed from JSON. */
export type MaestroHierarchyResult = { raw: unknown; durationMs: number }

export type MaestroFlowResult = {
  passed: boolean
  exitCode: number | null
  /** Last lines of Maestro output, for a failure report. Never stored by the driver. */
  output: string
  durationMs: number
  timedOut: boolean
}

/**
 * How the driver talks to Maestro. Two kinds exist: a fresh `maestro` process per call, and one
 * long-running `maestro mcp` session. DEJA-18 measures both (spikes/004-ios-observation).
 */
export interface MaestroTransport {
  readonly kind: 'process' | 'mcp'
  hierarchy(): Promise<MaestroHierarchyResult>
  /** Run a complete YAML flow (with its `appId` header). */
  runFlow(yaml: string, env?: Record<string, string>): Promise<MaestroFlowResult>
  close(): Promise<void>
}

// ---------------------------------------------------------------------------------------------
// Hierarchy parsing. Two shapes: `maestro hierarchy` (full `attributes` objects) and MCP
// `inspect_screen` (`ui_schema` abbreviations and defaults).

const FULL_KEYS = {
  id: 'resource-id',
  label: 'accessibilityText',
  title: 'title',
  value: 'value',
  text: 'text',
  hint: 'hintText',
}

function bool(value: unknown, fallback: boolean): boolean {
  if (typeof value === 'boolean') return value
  if (value === 'true') return true
  if (value === 'false') return false
  return fallback
}

function parseBounds(value: unknown): RawElement['bounds'] {
  if (typeof value !== 'string') return null
  const match = /^\[(-?\d+),(-?\d+)\]\[(-?\d+),(-?\d+)\]$/.exec(value)
  return match ? [Number(match[1]), Number(match[2]), Number(match[3]), Number(match[4])] : null
}

function str(value: unknown): string | null {
  return typeof value === 'string' && value.length > 0 ? value : null
}

/** Role from ID shape only: the hierarchy has no element class. `other` when unsure. */
export function roleFromId(id: string | null): ElementRole {
  if (!id) return 'other'
  if (/(^|\.)(secure|password)(\.|$)/i.test(id)) return 'secure-field'
  if (/\.(field|input|answer)$/i.test(id)) return 'text-field'
  if (/^tab\./.test(id)) return 'tab'
  if (/\.(switch|toggle)$/i.test(id)) return 'switch'
  if (/\.(cell|row)(\.|$)/i.test(id)) return 'cell'
  return 'other'
}

function rawElement(id: string | null, label: string | null, source: {
  bounds: unknown; enabled: unknown; focused: unknown; selected: unknown; checked: unknown
}): RawElement {
  const role = roleFromId(id)
  return {
    rawId: id,
    label,
    bounds: parseBounds(source.bounds),
    enabled: bool(source.enabled, true),
    focused: bool(source.focused, false),
    selected: bool(source.selected, false),
    checked: bool(source.checked, false),
    role,
    interactive: role !== 'other' && role !== 'text',
  }
}

/** Flatten either hierarchy shape into raw elements, depth first. Nodes with no ID and no label are skipped. */
export function parseMaestroHierarchy(raw: unknown): RawElement[] {
  const out: RawElement[] = []
  const visitFull = (node: any) => {
    if (!node || typeof node !== 'object') return
    const attributes = node.attributes ?? {}
    const id = str(attributes[FULL_KEYS.id])
    const label = str(attributes[FULL_KEYS.label]) ?? str(attributes[FULL_KEYS.text]) ??
      str(attributes[FULL_KEYS.value]) ?? str(attributes[FULL_KEYS.title])
    if (id || label) out.push(rawElement(id, label, attributes))
    for (const child of node.children ?? []) visitFull(child)
  }
  const visitAbbreviated = (node: any, defaults: Record<string, unknown>) => {
    if (!node || typeof node !== 'object') return
    const merged = { ...defaults, ...node }
    const id = str(merged.rid)
    const label = str(merged.a11y) ?? str(merged.txt) ?? str(merged.val)
    if (id || label) out.push(rawElement(id, label, { ...merged, bounds: merged.b }))
    for (const child of node.c ?? []) visitAbbreviated(child, defaults)
  }
  const shape = raw as { ui_schema?: { defaults?: Record<string, unknown> }; elements?: unknown[] }
  if (shape && Array.isArray(shape.elements)) {
    const defaults = shape.ui_schema?.defaults ?? {}
    for (const element of shape.elements) visitAbbreviated(element, defaults)
  } else {
    visitFull(raw)
  }
  return out
}

// ---------------------------------------------------------------------------------------------
// iOS fingerprint rules

/** UIKit keyboard key names are used as accessibility IDs, so they are the same in every locale. */
const KEYBOARD_KEY = /^(space|return|shift|delete|more|dictation|emoji|globe|Next keyboard|[A-Za-z0-9])$/
const SHARE_SHEET_IDS = new Set(['ActivityListView', 'ActivityContentView', 'UIActivityContentView'])
/** IDs the system adds that say nothing about the app screen. */
const SYSTEM_CONTAINER_IDS = new Set(['Toolbar', 'NavigationBar', 'TabBar', 'wifi'])
/** Keyboard accessory views seen with the software keyboard up (iOS 27.2 simulator). */
const KEYBOARD_ACCESSORY_IDS = new Set(['SystemInputAssistantView', 'inputView', 'CenterPageView'])
const MAX_ID_LENGTH = 120

export const iosFingerprintRules: FingerprintRules = {
  normalizeId(raw) {
    const id = raw.trim()
    if (!id || id.length > MAX_ID_LENGTH) return null
    if (SYSTEM_CONTAINER_IDS.has(id) || id.startsWith('com.apple.')) return null
    // Stable IDs are code-like tokens. Anything with spaces is a label that leaked into the ID slot.
    if (!/^[A-Za-z0-9][A-Za-z0-9._:-]*$/.test(id)) return null
    return id === 'BackButton' ? 'nav.back' : id
  },
  isOverlay(element) {
    return element.rawId !== null &&
      (KEYBOARD_KEY.test(element.rawId) || KEYBOARD_ACCESSORY_IDS.has(element.rawId))
  },
  isSystemUi(element) {
    return element.rawId !== null && SHARE_SHEET_IDS.has(element.rawId)
  },
}

/** English labels of the standard permission buttons. Best effort; not verified on a real prompt. */
const PERMISSION_LABELS = new Set([
  'Allow', 'Don’t Allow', "Don't Allow", 'Allow Once', 'Allow While Using App',
  'Allow Full Access', 'Allow Access to All Photos', 'Limit Access…',
])

function detectSystemUi(observation: Pick<Observation, 'elements' | 'keyboardVisible'>): SystemUiState | null {
  const ids = new Set(observation.elements.map(element => element.id))
  for (const id of SHARE_SHEET_IDS) {
    if (ids.has(id)) return { kind: 'share-sheet', evidence: [id] }
  }
  const permission = observation.elements.find(element =>
    element.label !== null && PERMISSION_LABELS.has(element.label))
  if (permission) return { kind: 'permission', evidence: ['permission-button'] }
  if (observation.keyboardVisible) return { kind: 'keyboard', evidence: ['space'] }
  return null
}

// ---------------------------------------------------------------------------------------------
// The driver

export type IosExplorationDriverOptions = {
  transport: MaestroTransport
  /** Pattern that marks a bottom sheet in normalized IDs, for the swipe-down back action. */
  sheetPattern?: RegExp
  now?: () => Date
}

const PASS = (durationMs: number): StepResult => ({ passed: true, failure: null, durationMs, crashed: false })

/** The most telling line of Maestro output: a failure message if one is there, else the last line. */
function failingLine(output: string): string | null {
  const lines = output.trim().split('\n').map(line => line.trim()).filter(Boolean)
  const telling = [...lines].reverse().find(line =>
    /FAILED|not found|not visible|Assertion|Exception|Error|timed out/i.test(line))
  return (telling ?? lines.pop() ?? null)?.slice(0, 300) ?? null
}

function quote(value: string): string {
  return JSON.stringify(value)
}

function validateProfile(profile: LaunchProfile) {
  if (typeof profile.appId !== 'string' || !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(profile.appId)) {
    throw new TypeError('LaunchProfile.appId must be a bundle ID')
  }
  if (typeof profile.clearState !== 'boolean') throw new TypeError('LaunchProfile.clearState must be a boolean')
  for (const [key, value] of Object.entries(profile.arguments)) {
    if (typeof value !== 'boolean' && typeof value !== 'string') {
      throw new TypeError(`LaunchProfile.arguments.${key} must be a string or boolean`)
    }
  }
  for (const [key, value] of Object.entries(profile.permissions ?? {})) {
    if (value !== 'allow' && value !== 'deny' && value !== 'unset') {
      throw new TypeError(`LaunchProfile.permissions.${key} must be allow, deny or unset`)
    }
  }
}

/** Commands a live-app segment must never hold: they relaunch, reset, rebind or run outside code. */
const FORBIDDEN_COMMANDS = new Set([
  'launchApp', 'stopApp', 'killApp', 'clearState', 'clearKeychain', 'openLink', 'appId',
  'runScript', 'evalScript',
])

/**
 * Walk the parsed segment. Any forbidden command name used as a key, or as a bare command string in
 * a list, is refused at any depth (`repeat`, `retry`, `when`, inline `runFlow` commands). `runFlow`
 * is allowed only with inline `commands`: a file reference could hold anything. Text values (such as
 * an `inputText` block scalar) are data and pass.
 */
function forbiddenIn(node: unknown, inList = false): string | null {
  if (typeof node === 'string') return inList && FORBIDDEN_COMMANDS.has(node.trim()) ? node.trim() : null
  if (Array.isArray(node)) {
    for (const item of node) {
      const found = forbiddenIn(item, true)
      if (found) return found
    }
    return null
  }
  if (node && typeof node === 'object') {
    for (const [key, value] of Object.entries(node as Record<string, unknown>)) {
      if (FORBIDDEN_COMMANDS.has(key)) return key
      if (key === 'runFlow') {
        const inline = typeof value === 'object' && value !== null && !Array.isArray(value) &&
          Array.isArray((value as Record<string, unknown>).commands) && !('file' in (value as object))
        if (!inline) return 'runFlow with a file'
      }
      const found = forbiddenIn(value)
      if (found) return found
    }
  }
  return null
}

function checkSegment(segment: Segment) {
  const documents = parseAllDocuments(segment.yaml)
  if (documents.length !== 1 || documents[0].errors.length > 0) {
    throw new TypeError(`Segment "${segment.label}" is not one valid YAML document`)
  }
  const found = forbiddenIn(documents[0].toJS())
  if (found) {
    throw new TypeError(`Segment "${segment.label}" must not use ${found}: a live-app segment never relaunches, rebinds or runs code`)
  }
}

function launchFlow(profile: LaunchProfile): string {
  validateProfile(profile)
  const lines = [`appId: ${quote(profile.appId)}`, '---', '- launchApp:']
  lines.push(`    clearState: ${profile.clearState}`)
  const args = Object.entries(profile.arguments)
  if (args.length > 0) {
    lines.push('    arguments:')
    for (const [key, value] of args) lines.push(`      ${quote(key)}: ${typeof value === 'boolean' ? value : quote(value)}`)
  }
  const permissions = Object.entries(profile.permissions ?? {})
  if (permissions.length > 0) {
    lines.push('    permissions:')
    for (const [key, value] of permissions) lines.push(`      ${quote(key)}: ${quote(value)}`)
  }
  return `${lines.join('\n')}\n`
}

export function iosExplorationDriver(options: IosExplorationDriverOptions): ExplorationDriver & {
  /** The app ID of the last `reset`, used to wrap segments that carry no header. */
  readonly appId: string | null
} {
  const { transport } = options
  const sheetPattern = options.sheetPattern ?? /(^|\.)(sheet|daySheet)$/i
  const now = options.now ?? (() => new Date())
  let appId: string | null = null

  function failed(output: string, result: MaestroFlowResult): StepResult {
    return {
      passed: false,
      failure: {
        category: result.timedOut ? 'timeout' : 'runner',
        advisory: true,
        repairEligible: false,
        failingStep: failingLine(output),
        rawArtifactPath: null,
      },
      durationMs: result.durationMs,
      crashed: false,
    }
  }

  async function runSegment(yaml: string, env?: Record<string, string>): Promise<StepResult> {
    const result = await transport.runFlow(yaml, env)
    return result.passed ? PASS(result.durationMs) : failed(result.output, result)
  }

  return {
    platform: 'ios',
    rules: iosFingerprintRules,
    get appId() {
      return appId
    },

    async reset(profile, statePaths) {
      validateProfile(profile)
      appId = profile.appId
      const started = performance.now()
      const launched = await runSegment(launchFlow(profile))
      if (!launched.passed) return launched
      for (const path of statePaths) {
        for (const segment of path) {
          const step = await this.act(segment)
          if (!step.passed) return { ...step, durationMs: Math.round(performance.now() - started) }
        }
      }
      return PASS(Math.round(performance.now() - started))
    },

    async observe() {
      const read = await transport.hierarchy()
      const raw = parseMaestroHierarchy(read.raw)
      const keyboardVisible = raw.some(element => element.rawId === 'space')
      const normalized = normalizeElements(raw, iosFingerprintRules)
      // The keyboard's own "Done" button has a plain label as its ID; it is part of the overlay.
      const elements = normalized.elements.map(element =>
        keyboardVisible && element.id === 'Done' ? { ...element, overlay: true } : element)
      const truncated = normalized.truncated
      const base = { elements, keyboardVisible, truncated }
      const snapshot = normalizedSnapshot(base)
      return {
        ...base,
        systemUi: detectSystemUi(base),
        snapshot: { path: '', sha256: snapshot.sha256 },
        capturedAt: now().toISOString(),
        durationMs: read.durationMs,
      }
    },

    async act(segment: Segment, env) {
      if (!appId) throw new Error('act() needs reset() first, so the app ID is known')
      checkSegment(segment)
      return runSegment(`appId: ${quote(appId)}\n---\n${segment.yaml}`, env)
    },

    backActions(observation) {
      const actions: CandidateAction[] = []
      const ids = observation.elements.filter(element => !element.overlay).map(element => element.id)
      if (ids.includes('nav.back')) {
        actions.push({
          kind: 'back', targetId: 'nav.back', via: 'nav-bar',
          segment: { ...tapSegment('BackButton'), label: 'back:nav-bar' },
        })
        actions.push({
          kind: 'back', targetId: null, via: 'edge-swipe',
          segment: { label: 'back:edge-swipe', yaml: '- swipe:\n    start: 1%, 50%\n    end: 70%, 50%\n' },
        })
      }
      if (ids.some(id => id !== null && sheetPattern.test(id))) {
        actions.push({
          kind: 'dismiss', targetId: null, via: 'sheet-swipe-down',
          segment: { label: 'back:sheet-swipe-down', yaml: '- swipe:\n    start: 50%, 40%\n    end: 50%, 95%\n' },
        })
      }
      return actions
    },

    systemUi(observation) {
      return detectSystemUi(observation)
    },
  }
}
