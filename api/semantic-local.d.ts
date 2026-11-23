// index.d.ts
import type { GoalEmbeddingProvider } from '@deja-qa/core';
export declare const LOCAL_LEXICAL_EMBEDDING_PROVIDER = "local-lexical-v1";
export declare const LOCAL_LEXICAL_EMBEDDING_DIMENSIONS = 128;
/**
 * Deliberately tiny English lexical proof vocabulary. Unknown words stay distinct; there is no
 * learned model, network call, remote vector index, or claim of generalized semantic equivalence.
 */
export declare function canonicalGoalTokens(goal: string): string[];
export declare class LocalLexicalEmbeddingProvider implements GoalEmbeddingProvider {
    readonly id = "local-lexical-v1";
    isCollisionSafeMatch(requestedGoal: string, candidateGoal: string): boolean;
    embed(goal: string): Promise<readonly number[]>;
}
