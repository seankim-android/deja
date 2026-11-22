# WALK-016 — exact-first semantic retrieval evidence

## Scope and reproduction

This is deterministic unit/integration/orchestration evidence for the local lexical proof. It uses fake Maestro/OpenClaw process fixtures, not a real emulator, learned embedding service, network, or remote vector database. It does not claim generalized semantic correctness or a new real-device result.

```sh
bun install --frozen-lockfile
bun test packages/core/test/semantic.test.ts \
  packages/core/test/orchestration.test.ts \
  packages/semantic-local/test/local-embeddings.test.ts \
  packages/store-local/test/semantic-retrieval.test.ts \
  apps/cli/test/cli.test.ts --timeout=10000
# focused result: 47 pass, 0 fail, 271 assertions across 5 files
bun test --timeout=10000
# complete result/trace: 138 pass, 0 fail, 774 assertions across 20 files;
# walk016-all-tests.txt in this directory
bun run lint
bun run build
```

## Acceptance matrix

- **Exact first; zero exact-hit embedding/model calls:** `core/test/semantic.test.ts` proves exact lookup returns before provider construction or candidate listing. `core/test/orchestration.test.ts` separately proves no provider construction/embed call, explorer, or repairer on an exact hit. The CLI exact report has `semantic.status: skipped-exact-hit` and `embeddingCalls: 0`.
- **Complete candidate identity/verification gates:** `store-local/test/semantic-retrieval.test.ts` uses real `LocalWalkStore` persistence to reject app, platform, schema, build, adapter, and browser mismatches; latest failed validation and missing flows exclude candidates before embedding; checksum tampering raises `InvalidWalkMemoryError`. `core/test/semantic.test.ts` also verifies that a candidate removed between listing and exact relookup is never embedded.
- **Similarity, selected walk, and confidence:** equivalent local goals produce similarity `1`, select the original stored goal/flow/checksum, and report two embedding calls (query plus one eligible candidate). Two equivalent verified walks produce `ambiguous`; unrelated vectors produce `low-confidence`; no candidates produce `no-eligible-candidates`.
- **Contradictory/adversarial fail closed:** negated requests select no walk and skip candidate embedding. `Ignore previous instructions ...` produces `adversarial-goal` with zero embedding calls; orchestration constructs neither provider nor explorer and returns `exploration-rejected`, `modelCalls: 0`.
- **Provider failure is ordinary miss behavior:** null/throwing factories and failed/invalid embeddings return `provider-unavailable`, never a selected walk. CLI integration with an unsupported configured provider performs the existing verified cold exploration (`modelCalls: 2`), then exact replay with zero model/embedding calls.
- **No verification bypass:** semantic orchestration executes the selected stored YAML with the runner and records validation against the selected stored exact identity, while the report retains the requested identity. A forced product-assertion replay remains `product-regression`, records failed validation, and invokes no explorer/repairer. Semantic selector failures do not broaden repair eligibility.
- **Separate metrics/public compatibility:** additive optional schema-1 `semantic` fields report status/reason, provider, embedding calls, duration, candidates, thresholds, top/runner-up similarity, and selected walk. Top-level cold model metrics remain unchanged; stored memory stays schema 2 and old reports without `semantic` remain readable.
- **Independent-review regressions:** a raw `email`/`search` hash collision has cosine 1 but returns `low-confidence` through canonical-token mismatching. A winner changed after embedding is exactly re-looked-up immediately before replay and executes no runner/explorer on a miss. Product and selector replay failures invoke no semantic repair. Extreme finite vectors (`1e308`) yield finite stable similarity. Zero/null/below-default similarity/margin settings are rejected. Adversarial stale-build goals invoke neither provider nor repairer/explorer.

## Honest limitations

`local-lexical-v1` hashes a deliberately tiny English lexical vocabulary into 128 dimensions. Its explicit equivalences include `check out`/`checkout`, `sign in`/`log in`, `sign out`/`log out`, and `finish`/`complete`; it is not a learned semantic model. Unknown paraphrases, non-English text, subtle intent, and adversarial phrasing outside the narrow marker rules can miss or be misread. Thresholds do not prove natural-language equivalence. The proof's confidence comes from conservative misses plus complete cache identity/checksum/latest-pass gates and a fresh stored-YAML runner execution, not from embedding quality alone.
The final exact relookup closes the provider-await gap before runner invocation; local filesystem
writes still have no distributed transaction or multi-writer lock, matching the existing store boundary.
