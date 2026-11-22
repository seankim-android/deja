import { describe, expect, test } from 'bun:test'
import { cosineSimilarity } from '../../core/src/index.ts'
import {
  LOCAL_LEXICAL_EMBEDDING_DIMENSIONS,
  LOCAL_LEXICAL_EMBEDDING_PROVIDER,
  LocalLexicalEmbeddingProvider,
  canonicalGoalTokens,
} from '../src/index.ts'

describe('deterministic local lexical embedding proof', () => {
  test('produces stable bounded vectors without configuration or a service', async () => {
    const provider = new LocalLexicalEmbeddingProvider()
    const first = await provider.embed('  Please FINISH the checkout!  ')
    const second = await provider.embed('  Please FINISH the checkout!  ')
    expect(provider.id).toBe(LOCAL_LEXICAL_EMBEDDING_PROVIDER)
    expect(first).toEqual(second)
    expect(first).toHaveLength(LOCAL_LEXICAL_EMBEDDING_DIMENSIONS)
    expect(first.some(component => component > 0)).toBe(true)
    expect(first.every(Number.isFinite)).toBe(true)
  })

  test('recognizes only the explicitly documented narrow paraphrase normalizations', async () => {
    const provider = new LocalLexicalEmbeddingProvider()
    expect(canonicalGoalTokens('Check out')).toEqual(['checkout'])
    expect(canonicalGoalTokens('Sign in')).toEqual(['login'])
    expect(canonicalGoalTokens('Log out')).toEqual(['logout'])
    expect(cosineSimilarity(
      await provider.embed('Complete the checkout'),
      await provider.embed('Please finish checkout'),
    )).toBe(1)
    expect(canonicalGoalTokens('Purchase the items')).not.toEqual(canonicalGoalTokens('Buy the items'))
  })

  test('unrelated goals remain below the default retrieval confidence floor', async () => {
    const provider = new LocalLexicalEmbeddingProvider()
    const similarity = cosineSimilarity(
      await provider.embed('Finish checkout'),
      await provider.embed('Delete the account'),
    )
    expect(similarity).toBeLessThan(0.90)
  })

  test('extra unconstrained intent reduces similarity instead of being discarded', async () => {
    const provider = new LocalLexicalEmbeddingProvider()
    const base = await provider.embed('Complete checkout')
    const paraphrase = await provider.embed('Finish checkout')
    const extra = await provider.embed('Finish checkout and delete account')
    expect(cosineSimilarity(base, paraphrase)).toBe(1)
    expect(cosineSimilarity(base, extra)).toBeLessThan(0.90)
  })

  test('raw hash collisions cannot pass the local canonical-token equivalence guard', async () => {
    const provider = new LocalLexicalEmbeddingProvider()
    const email = await provider.embed('email')
    const search = await provider.embed('search')
    expect(cosineSimilarity(email, search)).toBe(1)
    expect(provider.isCollisionSafeMatch('email', 'search')).toBe(false)
    expect(provider.isCollisionSafeMatch('complete checkout', 'finish checkout')).toBe(true)
    expect(provider.isCollisionSafeMatch('checkout', 'check out')).toBe(true)
  })
})
