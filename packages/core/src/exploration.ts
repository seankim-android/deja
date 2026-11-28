/*
 * Platform-neutral explorer contract and screen fingerprints v2 (docs/PATH_MEMORY_AND_EXPLORATION.md
 * sections 3, 4.3, 4.4). Core does no IO and imports nothing from a platform package: a driver
 * (for example `iosExplorationDriver` in `@deja-run/maestro`) supplies hierarchy capture, fingerprint
 * rules and action execution. Nothing here calls a model.
 */
import { createHash } from 'node:crypto'
import type { WalkFailure } from './contracts.ts'

export const SCREEN_FINGERPRINT_V2 = 2
/** Upper bound on elements in one observation. A larger screen sets `truncated`. */
export const MAX_OBSERVED_ELEMENTS = 400

/** A Maestro YAML segment run on the live app. It never holds a `launchApp`. */
export type Segment = {
  /** Short name for reports, for example `tap:tab.feed`. */
  label: string
  yaml: string
}

export type LaunchProfile = {
  appId: string
  /** Launch arguments (iOS process arguments, Android intent extras). */
  arguments: Record<string, string | boolean>
  clearState: boolean
  permissions?: Record<string, 'allow' | 'deny' | 'unset'>
}

export type ElementRole =
  'button' | 'text-field' | 'secure-field' | 'cell' | 'switch' | 'tab' | 'text' | 'other'

/** One element as the platform reports it, before any filtering. Labels stay in memory only. */
export type RawElement = {
  rawId: string | null
  label: string | null
  /** `[left, top, right, bottom]`, or null when the platform gave none. */
  bounds: [number, number, number, number] | null
  enabled: boolean
  focused: boolean
  selected: boolean
  checked: boolean
  role: ElementRole
  interactive: boolean
}

export type ElementObservation = {
  /** Normalized stable ID. */
  id: string | null
  role: ElementRole
  enabled: boolean
  interactive: boolean
  /** Kept only in memory for this run. Stored only when `config.snapshots.includeLabels` is on. */
  label: string | null
  overlay: boolean
}

export type SystemUiKind = 'alert' | 'permission' | 'share-sheet' | 'keyboard' | 'other'

export type SystemUiState = {
  kind: SystemUiKind
  /** Raw IDs or other platform tokens that identified the state. No visible text. */
  evidence: string[]
}

export type Observation = {
  elements: ElementObservation[]
  systemUi: SystemUiState | null
  keyboardVisible: boolean
  /** True when a bound cut elements. */
  truncated: boolean
  /** Stored normalized snapshot (section 4.4). `path` is empty until a store writes it. */
  snapshot: { path: string; sha256: string }
  capturedAt: string
  durationMs: number
}

export type StepResult = {
  passed: boolean
  failure: WalkFailure | null
  durationMs: number
  /** Explicit crash signal only. */
  crashed: boolean
}

export type CandidateAction = {
  kind: 'tap' | 'back' | 'dismiss' | 'swipe' | 'other'
  /** Normalized ID the action targets, or null for gestures such as an edge swipe. */
  targetId: string | null
  /** How a back action works, for reports: `nav-bar`, `edge-swipe`, `sheet-swipe-down`. */
  via?: string
  segment: Segment
}

export interface FingerprintRules {
  /** Platform ID to a stable token, or null to drop it. */
  normalizeId(raw: string): string | null
  /** Keyboard, toasts. */
  isOverlay(element: RawElement): boolean
  /** Alerts, permission sheets, share sheet. */
  isSystemUi(element: RawElement): boolean
}

/** One per platform. The only platform-specific code the explorer calls. */
export interface ExplorationDriver {
  readonly platform: 'ios' | 'android' | 'web'
  readonly rules: FingerprintRules
  /** Clean launch with a profile, then replay any state paths. */
  reset(profile: LaunchProfile, statePaths: Segment[][]): Promise<StepResult>
  /** Read the current screen. Never relaunches. */
  observe(): Promise<Observation>
  /** Run one segment on the live app. Never relaunches. */
  act(segment: Segment, env?: Record<string, string>): Promise<StepResult>
  /** The platform's own way back, if any. */
  backActions(observation: Observation): CandidateAction[]
  /** Platform system UI the harness may handle without the model. */
  systemUi(observation: Observation): SystemUiState | null
}

export type ActionChoice = {
  action: CandidateAction | null
  /** Why. Deterministic choosers give a rule name. */
  reason: string
}

/** Picks the next action. A scripted chooser needs no model; a model chooser lives elsewhere. */
export interface ActionChooser {
  choose(input: {
    observation: Observation
    candidates: CandidateAction[]
    goal: string | null
  }): Promise<ActionChoice>
}

/** Agent-facing observation result: never guesses (section 7.8). */
export type ObservationReport = {
  /** The match status itself: only `matched` and `matched-scrolled` mean a known screen. */
  status: 'matched' | 'matched-scrolled' | 'ambiguous' | 'changed' | 'unknown' | 'gap'
  ambiguous: boolean
  candidates: { nodeId: string; marker: string | null }[]
  truncated: boolean
}

// ---------------------------------------------------------------------------------------------
// Normalizing raw elements

/** Normalize, classify and bound a raw hierarchy. Order is kept; nothing is reordered here. */
export function normalizeElements(
  raw: RawElement[],
  rules: FingerprintRules,
  limit = MAX_OBSERVED_ELEMENTS,
): { elements: ElementObservation[]; truncated: boolean } {
  const elements: ElementObservation[] = []
  for (const element of raw) {
    const id = element.rawId ? rules.normalizeId(element.rawId) : null
    const overlay = rules.isOverlay(element) || rules.isSystemUi(element)
    // Nothing to identify it by and nothing to read: skip.
    if (id === null && !element.label) continue
    elements.push({
      id,
      role: element.role,
      enabled: element.enabled,
      interactive: element.interactive,
      label: element.label && element.label.length > 0 ? element.label : null,
      overlay,
    })
  }
  return elements.length > limit
    ? { elements: elements.slice(0, limit), truncated: true }
    : { elements, truncated: false }
}

// ---------------------------------------------------------------------------------------------
// Fingerprint v2

export type FingerprintConfig = {
  /** Regular expression source. A matching ID is the screen marker (Blankaday: `^screen\.`). */
  screenMarkerPattern?: string
  /** IDs to ignore, exact or with a trailing `*` prefix wildcard. */
  ignore?: string[]
  /** IDs present on most screens in the graph, for example `tab.today`. See `findChromeIds`. */
  chromeIds?: string[]
  /**
   * What to do when more than one marker matches. Default: the screen is a gap (`marker-conflict`).
   * `last-marker`: the foreground screen is the one whose marker comes last in document order, and
   * only elements from that marker on count. Measured on Blankaday iOS (DEJA-18): a hidden page's
   * elements come first. Opt-in, because it is a platform observation, not a rule.
   */
  markerScope?: 'last-marker'
}

export type ScreenFingerprintV2 = {
  version: typeof SCREEN_FINGERPRINT_V2
  /** The one marker ID, or null when none or more than one matched. */
  marker: string | null
  /** Every marker ID that matched. More than one means the platform leaked another screen. */
  markers: string[]
  /** Sorted stable IDs left after every filter, marker excluded. */
  anchors: string[]
  /** SHA-256 of version, markers and anchors. Never of visible text. */
  fingerprint: string
  /** False when nothing stable is left or the markers conflict. Such a screen is never a node; it is a gap. */
  stable: boolean
  /** Why `stable` is false. */
  unstableReason: 'no-stable-ids' | 'marker-conflict' | null
}

const DYNAMIC_SEGMENT = [
  /^\d+$/,
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i,
  /^\d{4}-\d{2}-\d{2}([T_ ].*)?$/,
  /[_-]\d+$/,
]

/** True when a dot-separated ID part is a number, UUID or date, or ends in `_<n>` or `-<n>`. */
export function isDynamicId(id: string): boolean {
  return id.split('.').some(part => DYNAMIC_SEGMENT.some(pattern => pattern.test(part)))
}

function ignored(id: string, ignore: string[]): boolean {
  return ignore.some(entry => entry.endsWith('*') ? id.startsWith(entry.slice(0, -1)) : id === entry)
}

/**
 * Fingerprint v2 (section 4.3). Input is already normalized, so overlays and system UI are marked
 * and IDs are stable tokens. Steps: drop overlays and system UI and ignored IDs, drop dynamic IDs,
 * drop chrome IDs, split off the marker. Several markers make the screen unstable (a gap).
 * Visible text is never read.
 */
export function fingerprintScreenV2(
  elements: ElementObservation[],
  config: FingerprintConfig = {},
): ScreenFingerprintV2 {
  const marker = config.screenMarkerPattern ? new RegExp(config.screenMarkerPattern) : null
  const chrome = new Set(config.chromeIds ?? [])
  const ignore = config.ignore ?? []
  // Every filter runs first, so markers are found among the IDs that count. A marker that is
  // dynamic, ignored or chrome is not a marker.
  const counts = (element: ElementObservation): element is ElementObservation & { id: string } =>
    element.id !== null && !element.overlay &&
    !ignored(element.id, ignore) && !isDynamicId(element.id) && !chrome.has(element.id)
  let scoped = elements.filter(counts)
  if (marker && config.markerScope === 'last-marker') {
    const at = scoped.flatMap((element, index) => marker.test(element.id) ? [index] : [])
    if (new Set(at.map(index => scoped[index].id)).size > 1) scoped = scoped.slice(at[at.length - 1])
  }
  const kept = new Set<string>(scoped.map(element => element.id))
  const markers = marker ? [...kept].filter(id => marker.test(id)).sort() : []
  const markerSet = new Set(markers)
  const anchors = [...kept].filter(id => !markerSet.has(id)).sort()
  const fingerprint = createHash('sha256').update(JSON.stringify({
    version: SCREEN_FINGERPRINT_V2,
    markers,
    anchors,
  })).digest('hex')
  return {
    version: SCREEN_FINGERPRINT_V2,
    marker: markers.length === 1 ? markers[0] : null,
    markers,
    anchors,
    fingerprint,
    // More than one marker means the platform's hierarchy also holds another screen. Nothing says
    // which one is on top, so the screen is a gap rather than a guess.
    stable: markers.length <= 1 && markers.length + anchors.length > 0,
    unstableReason: markers.length > 1 ? 'marker-conflict'
      : markers.length + anchors.length === 0 ? 'no-stable-ids' : null,
  }
}

/** IDs present on at least `fraction` of the given screens (needs `minScreens` of them). */
export function findChromeIds(
  screens: { anchors: string[]; markers?: string[] }[],
  options: { fraction?: number; minScreens?: number } = {},
): string[] {
  const fraction = options.fraction ?? 0.6
  const minScreens = options.minScreens ?? 3
  if (screens.length < minScreens) return []
  const counts = new Map<string, number>()
  for (const screen of screens) {
    for (const id of new Set([...screen.anchors, ...(screen.markers ?? [])])) {
      counts.set(id, (counts.get(id) ?? 0) + 1)
    }
  }
  return [...counts].filter(([, count]) => count / screens.length >= fraction)
    .map(([id]) => id).sort()
}

/** Normalized snapshot for storage: IDs, roles and flags; labels only when allowed (4.4). */
export function normalizedSnapshot(
  observation: Pick<Observation, 'elements' | 'keyboardVisible' | 'truncated'>,
  options: { includeLabels?: boolean } = {},
) {
  const body = {
    keyboardVisible: observation.keyboardVisible,
    truncated: observation.truncated,
    elements: observation.elements.map(element => ({
      id: element.id,
      role: element.role,
      enabled: element.enabled,
      interactive: element.interactive,
      overlay: element.overlay,
      ...(options.includeLabels ? { label: element.label } : {}),
    })),
  }
  const json = JSON.stringify(body)
  return { body, json, sha256: createHash('sha256').update(json).digest('hex') }
}

// ---------------------------------------------------------------------------------------------
// Matching

export type StoredScreen = {
  nodeId: string
  marker: string | null
  anchors: string[]
  fingerprint: string
}

export type ScreenMatchV2 = {
  status: 'matched' | 'matched-scrolled' | 'ambiguous' | 'changed' | 'unknown' | 'gap'
  fingerprint: string
  candidateNodeIds: string[]
  /** `ambiguous: true` always comes with the candidates and never a pick. */
  ambiguous: boolean
}

/**
 * Fails closed. An unstable screen is a gap, never a node. An exact fingerprint is `matched`.
 * The same marker with a strict subset of a stored node's anchors is `matched-scrolled`, which is
 * never a hit by itself. More than one candidate is `ambiguous` and picks none. With an expected
 * node, anything else is `changed`; without one it is `unknown`.
 */
export function matchScreenV2(
  stored: StoredScreen[],
  current: ScreenFingerprintV2,
  expectedNodeId?: string,
): ScreenMatchV2 {
  const result = (
    status: ScreenMatchV2['status'],
    candidateNodeIds: string[],
  ): ScreenMatchV2 => ({
    status,
    fingerprint: current.fingerprint,
    candidateNodeIds: [...candidateNodeIds].sort(),
    ambiguous: status === 'ambiguous',
  })
  if (!current.stable) return result('gap', [])
  const exact = stored.filter(node => node.fingerprint === current.fingerprint).map(node => node.nodeId)
  if (exact.length > 1) return result('ambiguous', exact)
  if (exact.length === 1) {
    return expectedNodeId !== undefined && exact[0] !== expectedNodeId
      ? result('changed', exact)
      : result('matched', exact)
  }
  if (current.marker !== null) {
    const seen = new Set(current.anchors)
    const scrolled = stored.filter(node =>
      node.marker === current.marker &&
      seen.size < node.anchors.length &&
      [...seen].every(anchor => node.anchors.includes(anchor))).map(node => node.nodeId)
    if (scrolled.length > 1) return result('ambiguous', scrolled)
    if (scrolled.length === 1) {
      return expectedNodeId !== undefined && scrolled[0] !== expectedNodeId
        ? result('changed', scrolled)
        : result('matched-scrolled', scrolled)
    }
  }
  return result(expectedNodeId === undefined ? 'unknown' : 'changed', [])
}

/** Agent-facing JSON for a match (section 7.8). */
export function observationReport(
  match: ScreenMatchV2,
  stored: StoredScreen[],
  truncated: boolean,
): ObservationReport {
  return {
    status: match.status,
    ambiguous: match.ambiguous,
    candidates: match.ambiguous
      ? match.candidateNodeIds.map(nodeId => ({
        nodeId,
        marker: stored.find(node => node.nodeId === nodeId)?.marker ?? null,
      }))
      : [],
    truncated,
  }
}
