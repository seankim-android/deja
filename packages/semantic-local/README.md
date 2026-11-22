# Deterministic local lexical embeddings (WALK-016)

`LocalLexicalEmbeddingProvider` is the hackathon proof provider, not a learned semantic service. It canonicalizes whitespace/punctuation, `check out` → `checkout`, `log in`/`sign in` → `login`,
`log out`/`sign out` → `logout`, and `finish`/`complete` → `complete`; hashes unigram/bigram features into a fixed 128-dimensional vector; and runs entirely in-process with no model, credentials, network, or vector database.

Raw cosine is not trusted by itself. `isCollisionSafeMatch` requires the two canonical token sequences
to be identical, so unrelated features that collide in the 128 buckets (for example `email` and
`search`) cannot produce a semantic hit even with raw cosine 1. This intentionally restricts the
proof to the explicit normalizations above.

This deliberately small vocabulary cannot establish general semantic correctness. Core still requires a 0.90 cosine floor, a 0.15 winner margin over the runner-up, complete cache identity/checksum/latest-pass gates, explicit contradiction/adversarial-marker rejection, and a fresh Maestro replay before a semantic hit can pass. Unknown paraphrases, negation outside the narrow marker list, and non-English semantics can miss or be misread; the conservative defaults favor a miss. Exact lookup and exact-miss reasons remain authoritative.

The provider is opt-in through `semantic.provider: "local-lexical-v1"`. Missing/unsupported providers never manufacture a semantic hit: they report `provider-unavailable` and use the ordinary miss lifecycle.
