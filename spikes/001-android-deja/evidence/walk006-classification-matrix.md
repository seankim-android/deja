# WALK-006 deterministic classification matrix

All rows use the real `classifyMaestroFailure` function and checked-in raw-output fixtures under
`packages/maestro/test/fixtures/classification/`. Negative rows must not receive the row heading's
category; their expected category is explicit. Each classification is advisory, retains the supplied
raw artifact reference, and never changes runner pass/fail or exit status.

| Fixture | Expected category | Failing step retained | Repair eligible |
| --- | --- | --- | --- |
| `selector-positive.log` | `selector` | Checkout ID tap | yes |
| `selector-negative.log` | `product_assertion` | Wrong quoted order assertion | no |
| `navigation-positive.log` | `navigation` | App launch | yes |
| `navigation-negative.log` | `selector` | Checkout ID tap, not navigation | yes |
| `product-assertion-positive.log` | `product_assertion` | Wrong quoted order assertion | no |
| `product-assertion-negative.log` | `unknown` | ID assertion with element missing | no |
| `runner-positive.log` | `runner` | none; structured launch error | no |
| `runner-negative.log` | `product_assertion` | Assertion output is not a runner error | no |
| `timeout-positive.log` | `timeout` | Structured timeout takes precedence | no |
| `timeout-negative.log` | `selector` | Same output without timeout | yes |
| `unknown-positive.log` | `unknown` | Unrecognized proprietary action | no |
| `unknown-negative.log` | `navigation` | Recognized launch failure, not catch-all unknown | yes |

Additional deterministic cases prove passing output returns no classification, mixed selector and
assertion diagnostics stay `unknown`, and multiple failed steps stay `unknown` with a null failing
step. `unknown` and `product_assertion` are never repair-eligible. A separate CLI orchestration test
proves a failing cache hit has `modelCalls: 0`, preserves known-good flow bytes, records the failed
validation, and becomes a `failed-validation` miss with zero model calls on the next lookup.

## Real Android corroboration

- [`walk006-selector-android-output.log`](walk006-selector-android-output.log): 16 preceding steps
  completed; only Checkout failed. The report is a cache hit with zero model calls and `selector`.
- [`walk006-product-assertion-android-output.log`](walk006-product-assertion-android-output.log): all
  19 preceding steps completed, including Checkout and the confirmation screen; only the deliberately
  wrong order assertion failed. The report remains failing with `product_assertion` and no repair
  eligibility.

Exact build/run configuration, hashes, generated raw references, and WALK-001 preservation hashes are
in [`walk006-android-evidence.json`](walk006-android-evidence.json). See the focused and full recorded
test outputs for the executed counts; no wall-clock speedup or generalized diagnosis is claimed.
