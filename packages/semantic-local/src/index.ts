import { createHash } from 'node:crypto'
import type { GoalEmbeddingProvider } from '../../core/src/index.ts'

export const LOCAL_LEXICAL_EMBEDDING_PROVIDER = 'local-lexical-v1'
export const LOCAL_LEXICAL_EMBEDDING_DIMENSIONS = 128

const STOP_WORDS = new Set(['a', 'an', 'the', 'please'])
const TOKEN_EQUIVALENTS: Readonly<Record<string, string>> = {
  finish: 'complete',
  finished: 'complete',
  completes: 'complete',
  completed: 'complete',
}

function asciiWords(goal: string) {
  return goal.normalize('NFKD').toLowerCase().replace(/[’]/g, "'")
    .match(/[a-z0-9]+/g) ?? []
}

/**
 * Deliberately tiny English lexical proof vocabulary. Unknown words stay distinct; there is no
 * learned model, network call, remote vector index, or claim of generalized semantic equivalence.
 */
export function canonicalGoalTokens(goal: string) {
  const raw = asciiWords(goal)
  const tokens: string[] = []
  for (let index = 0; index < raw.length; index += 1) {
    const pair = `${raw[index]} ${raw[index + 1] ?? ''}`
    if (pair === 'check out') {
      tokens.push('checkout')
      index += 1
      continue
    }
    if (pair === 'log in' || pair === 'sign in') {
      tokens.push('login')
      index += 1
      continue
    }
    if (pair === 'log out' || pair === 'sign out') {
      tokens.push('logout')
      index += 1
      continue
    }
    const word = raw[index]
    if (!STOP_WORDS.has(word)) tokens.push(TOKEN_EQUIVALENTS[word] ?? word)
  }
  return tokens
}

function featureIndex(feature: string) {
  const digest = createHash('sha256').update(feature).digest()
  return digest.readUInt32BE(0) % LOCAL_LEXICAL_EMBEDDING_DIMENSIONS
}

export class LocalLexicalEmbeddingProvider implements GoalEmbeddingProvider {
  readonly id = LOCAL_LEXICAL_EMBEDDING_PROVIDER

  isCollisionSafeMatch(requestedGoal: string, candidateGoal: string) {
    const requested = canonicalGoalTokens(requestedGoal)
    const candidate = canonicalGoalTokens(candidateGoal)
    return requested.length === candidate.length &&
      requested.every((token, index) => token === candidate[index])
  }

  async embed(goal: string): Promise<readonly number[]> {
    const tokens = canonicalGoalTokens(goal)
    const features = new Map<string, number>()
    for (let index = 0; index < tokens.length; index += 1) {
      features.set(`u:${tokens[index]}`, 2)
      if (index + 1 < tokens.length) features.set(`b:${tokens[index]}:${tokens[index + 1]}`, 1)
    }
    const embedding = Array<number>(LOCAL_LEXICAL_EMBEDDING_DIMENSIONS).fill(0)
    for (const [feature, weight] of features) embedding[featureIndex(feature)] += weight
    return embedding
  }
}
