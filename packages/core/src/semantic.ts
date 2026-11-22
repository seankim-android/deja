import {
  type GoalEmbeddingProvider,
  type LookupHit,
  type LookupMiss,
  type SemanticLookupReport,
  type SemanticRetrievalOptions,
  type StoredWalk,
  type WalkIdentity,
  type WalkStore,
} from './contracts.ts'

export const DEFAULT_MINIMUM_SIMILARITY = 0.90
export const DEFAULT_MINIMUM_MARGIN = 0.15

export type ExactFirstLookupResult = {
  status: 'hit'
  matchType: 'exact' | 'semantic'
  lookup: LookupHit
  semantic: SemanticLookupReport
} | {
  status: 'miss'
  miss: LookupMiss
  semantic: SemanticLookupReport
}

function inactiveReport(status: 'disabled' | 'skipped-exact-hit' | 'not-applicable') {
  return {
    status,
    provider: null,
    embeddingCalls: 0,
    durationMs: 0,
    candidates: 0,
    eligibleCandidates: 0,
    minimumSimilarity: null,
    minimumMargin: null,
    goalSimilarity: null,
    runnerUpSimilarity: null,
    selectedWalk: null,
  } satisfies SemanticLookupReport
}

function boundedThreshold(value: number | undefined, fallback: number, label: string) {
  const result = value === undefined ? fallback : value
  if (!Number.isFinite(result) || result < fallback || result > 1) {
    throw new TypeError(`${label} must be a finite number between ${fallback} and 1`)
  }
  return result
}

function words(goal: string) {
  return goal.toLowerCase().replace(/[’]/g, "'").match(/[a-z0-9]+(?:'[a-z]+)?/g) ?? []
}

/** Narrow prompt-injection markers, not a claim of general adversarial-language detection. */
export function hasAdversarialGoalMarker(goal: string) {
  const normalized = goal.toLowerCase().replace(/[’]/g, "'").replace(/[_-]+/g, ' ')
  return [
    /\bignore\s+(?:all\s+|any\s+|the\s+)?(?:previous|prior|above|system|developer)\s+instructions?\b/,
    /\b(?:system|developer)\s+(?:prompt|message|instructions?)\b/,
    /\b(?:jailbreak|prompt\s+injection)\b/,
    /\b(?:bypass|skip|disable|avoid)\s+(?:the\s+)?(?:verification|validation|checks?|tests?)\b/,
    /\b(?:trust\s+me|do\s+anything|without\s+verification)\b/,
    /\b(?:reveal|print|exfiltrate|steal)\s+(?:the\s+)?(?:secrets?|credentials?|tokens?|passwords?)\b/,
  ].some(pattern => pattern.test(normalized))
}

const NEGATION = new Set([
  'no', 'not', 'never', 'without', 'avoid', 'avoiding', 'skip', 'skipping',
  'except', 'instead', 'cannot', "can't", "don't", "won't", 'mustnt',
])

// Only explicit opposites participate. This list does not attempt to infer broader intent.
const OPPOSITES = [
  ['add', 'remove'], ['create', 'delete'], ['enable', 'disable'], ['open', 'close'],
  ['accept', 'reject'], ['confirm', 'cancel'], ['purchase', 'refund'],
  ['login', 'logout'], ['signin', 'signout'],
] as const

export function goalsAreContradictory(left: string, right: string) {
  const leftWords = new Set(words(left))
  const rightWords = new Set(words(right))
  const leftNegated = [...leftWords].some(word => NEGATION.has(word)) ||
    /\bdo\s+not\b/.test(left.toLowerCase())
  const rightNegated = [...rightWords].some(word => NEGATION.has(word)) ||
    /\bdo\s+not\b/.test(right.toLowerCase())
  if (leftNegated !== rightNegated) return true
  for (const [leftOpposite, rightOpposite] of OPPOSITES) {
    if ((leftWords.has(leftOpposite) && rightWords.has(rightOpposite)) ||
        (leftWords.has(rightOpposite) && rightWords.has(leftOpposite))) return true
  }
  return false
}

function identityMatches(walk: StoredWalk, identity: WalkIdentity) {
  return walk.appId === identity.appId && walk.platform === identity.platform &&
    walk.flowSchemaVersion === identity.flowSchemaVersion &&
    walk.buildFingerprint === identity.buildFingerprint &&
    (walk.adapter ?? null) === (identity.adapter ?? null) &&
    (walk.browser ?? null) === (identity.browser ?? null) && walk.lastValidation.passed
}

function selectedIdentity(identity: WalkIdentity, walk: StoredWalk): WalkIdentity {
  return {
    ...identity,
    goal: walk.goal,
    // Copy these explicitly from the selected record as well as comparing them above.
    appId: walk.appId,
    platform: walk.platform,
    flowSchemaVersion: walk.flowSchemaVersion,
    buildFingerprint: walk.buildFingerprint,
    ...(walk.adapter !== undefined ? { adapter: walk.adapter } : {}),
    ...(walk.browser !== undefined ? { browser: walk.browser } : {}),
  }
}

function vector(value: readonly number[]) {
  if (!Array.isArray(value) || value.length === 0 || value.length > 4096 ||
      value.some(component => !Number.isFinite(component))) {
    throw new Error('Embedding provider returned an invalid vector')
  }
  return value
}

export function cosineSimilarity(left: readonly number[], right: readonly number[]) {
  const leftVector = vector(left)
  const rightVector = vector(right)
  if (leftVector.length !== rightVector.length) {
    throw new Error('Embedding vectors must have the same non-zero dimension')
  }
  let leftScale = 0
  let rightScale = 0
  for (let index = 0; index < leftVector.length; index += 1) {
    leftScale = Math.max(leftScale, Math.abs(leftVector[index]))
    rightScale = Math.max(rightScale, Math.abs(rightVector[index]))
  }
  if (leftScale === 0 || rightScale === 0) return 0
  let dot = 0
  let leftNorm = 0
  let rightNorm = 0
  for (let index = 0; index < leftVector.length; index += 1) {
    const a = leftVector[index] / leftScale
    const b = rightVector[index] / rightScale
    dot += a * b
    leftNorm += a * a
    rightNorm += b * b
  }
  const result = dot / Math.sqrt(leftNorm * rightNorm)
  if (!Number.isFinite(result)) throw new Error('Embedding similarity is not finite')
  return Math.max(-1, Math.min(1, result))
}

type RankedCandidate = {
  lookup: LookupHit
  similarity: number
}

/**
 * Exact lookup always happens before the lazy semantic provider. Candidate goals are embedded only
 * after store.lookup has rechecked the full identity, latest pass, canonical path, and checksum.
 */
export class ExactFirstWalkRetriever {
  private readonly store: WalkStore
  private readonly semantic?: SemanticRetrievalOptions
  private readonly now: () => number

  constructor(options: {
    store: WalkStore
    semantic?: SemanticRetrievalOptions
    now?: () => number
  }) {
    this.store = options.store
    this.semantic = options.semantic
    this.now = options.now ?? Date.now
  }

  async lookup(identity: WalkIdentity): Promise<ExactFirstLookupResult> {
    const exact = await this.store.lookup(identity)
    if (exact.status === 'hit') {
      return {
        status: 'hit', matchType: 'exact', lookup: exact,
        semantic: inactiveReport('skipped-exact-hit'),
      }
    }
    if (!this.semantic) {
      return {
        status: 'miss', miss: exact,
        semantic: inactiveReport(exact.reason === 'not-found' ? 'disabled' : 'not-applicable'),
      }
    }
    // Opt-in goal safety also guards non-not-found misses before their repair/explore lifecycle.
    // This is lexical only and still performs no provider construction or embedding.
    if (hasAdversarialGoalMarker(identity.goal)) {
      return {
        status: 'miss', miss: exact,
        semantic: { ...inactiveReport('not-applicable'), status: 'miss', reason: 'adversarial-goal' },
      }
    }
    if (exact.reason !== 'not-found') {
      return { status: 'miss', miss: exact, semantic: inactiveReport('not-applicable') }
    }

    const minimumSimilarity = boundedThreshold(
      this.semantic.minimumSimilarity, DEFAULT_MINIMUM_SIMILARITY, 'minimumSimilarity',
    )
    const minimumMargin = boundedThreshold(
      this.semantic.minimumMargin, DEFAULT_MINIMUM_MARGIN, 'minimumMargin',
    )
    const startedAt = this.now()
    let provider: GoalEmbeddingProvider | null = null
    let embeddingCalls = 0
    let candidates = 0
    let eligibleCandidates = 0
    let queryVector: readonly number[] = []
    const ranked: RankedCandidate[] = []
    let contradictoryCandidates = 0
    const baseReport = () => ({
      provider: provider?.id ?? null,
      embeddingCalls,
      durationMs: Math.max(0, this.now() - startedAt),
      candidates,
      eligibleCandidates,
      minimumSimilarity,
      minimumMargin,
      goalSimilarity: null,
      runnerUpSimilarity: null,
      selectedWalk: null,
    })
    const unavailable = (): ExactFirstLookupResult => ({
      status: 'miss', miss: exact,
      semantic: { ...baseReport(), status: 'unavailable', reason: 'provider-unavailable' },
    })

    try {
      provider = this.semantic.createProvider()
      if (!provider || !provider.id.trim()) throw new Error('Embedding provider is unavailable')
      embeddingCalls += 1
      queryVector = vector(await provider.embed(identity.goal))
    } catch {
      return unavailable()
    }

    // Store failures are not provider failures: malformed/unsafe memory must retain its error path.
    const memory = await this.store.list()
    candidates = memory.length
    const constrained = memory.filter(walk => identityMatches(walk, identity))
    for (const walk of constrained) {
      // This exact store call is the safety gate: filtered metadata alone is never a cache hit.
      const verified = await this.store.lookup(selectedIdentity(identity, walk))
      if (verified.status !== 'hit') continue
      if (adversarialGoal(walk.goal)) continue
      if (goalsAreContradictory(identity.goal, walk.goal)) {
        contradictoryCandidates += 1
        continue
      }
      try {
        embeddingCalls += 1
        const candidateVector = vector(await provider.embed(walk.goal))
        const similarity = cosineSimilarity(queryVector, candidateVector)
        ranked.push({ lookup: verified, similarity })
        eligibleCandidates += 1
      } catch {
        return unavailable()
      }
    }

    if (ranked.length === 0) {
      return {
        status: 'miss', miss: exact,
        semantic: {
          ...baseReport(), status: 'miss',
          reason: contradictoryCandidates > 0 ? 'contradictory-goal' : 'no-eligible-candidates',
        },
      }
    }
    ranked.sort((left, right) => right.similarity - left.similarity ||
      left.lookup.walk.goalKey.localeCompare(right.lookup.walk.goalKey) ||
      left.lookup.walk.flowSha256.localeCompare(right.lookup.walk.flowSha256))
    const top = ranked[0]
    const runnerUp = ranked[1]
    const runnerUpSimilarity = runnerUp?.similarity ?? null
    const scoredReport = {
      ...baseReport(),
      goalSimilarity: top.similarity,
      runnerUpSimilarity,
    }
    const collisionSafe = provider.isCollisionSafeMatch?.(identity.goal, top.lookup.walk.goal) ?? true
    if (top.similarity < minimumSimilarity || !collisionSafe) {
      return {
        status: 'miss', miss: exact,
        semantic: { ...scoredReport, status: 'miss', reason: 'low-confidence' },
      }
    }
    if (runnerUp && top.similarity - runnerUp.similarity < minimumMargin) {
      return {
        status: 'miss', miss: exact,
        semantic: { ...scoredReport, status: 'miss', reason: 'ambiguous' },
      }
    }

    const walk = top.lookup.walk
    return {
      status: 'hit', matchType: 'semantic', lookup: top.lookup,
      semantic: {
        ...scoredReport,
        status: 'hit',
        selectedWalk: {
          goal: walk.goal,
          goalKey: walk.goalKey,
          flowPath: walk.flowPath,
          flowSha256: walk.flowSha256,
        },
      },
    }
  }
}

function adversarialGoal(goal: string) {
  return hasAdversarialGoalMarker(goal)
}
