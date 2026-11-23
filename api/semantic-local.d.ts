// typescript 7.0.2
// entry . (./src/index.ts)
// exports: LOCAL_LEXICAL_EMBEDDING_DIMENSIONS, LOCAL_LEXICAL_EMBEDDING_PROVIDER, LocalLexicalEmbeddingProvider, canonicalGoalTokens
export declare function canonicalGoalTokens(goal: string): string[];
export declare const LOCAL_LEXICAL_EMBEDDING_DIMENSIONS = 128;
export declare const LOCAL_LEXICAL_EMBEDDING_PROVIDER = "local-lexical-v1";
export declare class LocalLexicalEmbeddingProvider implements GoalEmbeddingProvider {
    readonly id = "local-lexical-v1";
    isCollisionSafeMatch(requestedGoal: string, candidateGoal: string): boolean;
    embed(goal: string): Promise<readonly number[]>;
}
