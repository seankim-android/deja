# WALK-020 Android rejected takes

These real recordings are retained only under ignored `demo/generated/android/`. They are not used as
passing repair evidence and are not represented as successful mutations.

## `walk020-android-20261122T200956-97974`

- Cold `explore` and warm `run` passed with the same identity/YAML. The cold model-authored YAML has the
  desired direct fully qualified checkout tap and unchanged final order assertion.
- The goal was a fresh descriptive string, not the exact normalized `checkout` goal accepted by the
  configured production repairer. Its stale probe correctly built and installed, but the repair CLI
  refused the goal before diagnosis and did not emit a valid JSON receipt. No repair was attempted.
- The driver restored the baseline selector source and stable fixture mode. The original patch attempt
  using `git apply -R` also failed on trailing blank-line context; the exact reviewed two-file reversal
  was subsequently performed with BSD `patch --no-backup-if-mismatch -R -p1`.

## `walk020-android-20261122T201401-99133`

- Cold `explore` passed independently with 13 model round trips and 17 assertions; warm `run` was an
  exact cache hit with 0 model calls and byte-identical YAML.
- The model inserted `assertVisible(checkout_button)` immediately before the checkout tap. On the stale
  build, Maestro therefore failed at `Assert that id: …/checkout_button is visible`, not at the repair
  policy's only validated failing step, `Tap on id: dev.walkcache.checkout:id/checkout_button`.
- The production repairer classified the failure as `unknown`, returned `repair-rejected`, made 0 repair
  attempts and 0 model calls, and left source, memory, and YAML hashes byte-identical. The driver restored
  the normal source/APK. This take cannot demonstrate a successful bounded repair without deleting an
  assertion or broadening the classifier, neither of which was done.

## `walk020-android-checkout-03`

Cold `explore` passed independently with 16 model round trips and 13 assertions. Its first checkout step
is a direct tap, which avoids the extra assertion, but the model used short `checkout_button` IDs. The
resulting Maestro failure would be `Tap on id: checkout_button`, outside the configured fully qualified
selector diagnosis. No warm, stale mutation, or repair was run for this take.

## `walk020-android-checkout-04`

Cold `explore` passed independently with 14 model round trips and 15 assertions. It used fully qualified
IDs but added `assertVisible(checkout_button)` before the tap. No warm, stale mutation, or repair was run
for this incompatible take.

## `walk020-android-checkout-05`

Cold `explore` passed independently with 13 model round trips and 15 assertions. Its semantically
identical `Checkout` goal is allowed by the repairer's case-insensitive goal check, but its flow again
added `assertVisible(checkout_button)` before the fully qualified tap. No warm, stale mutation, or repair
was run for this incompatible take.

## `walk020-android-checkout-06` (fresh cold/warm only; not its repair fixture)

Cold `explore` passed independently with 13 model round trips and 14 assertions; warm `run` then passed
as an exact identity/YAML cache hit with 0 model calls. Its short-ID pre-tap assertion makes it unsuitable
for the configured repair diagnosis, so no stale mutation was attempted on that fresh cold flow. The
accepted demo uses this run's cold/warm recordings only for the fresh-cache lifecycle.

A separate, explicitly mapped repair lifecycle then uses production `remember checkout` to independently
verify the hash-preserved model-authored candidate from `walk020-android-20261122T200956-97974`. This
disclosure prevents the fresh cold/warm and repair YAML hashes from being misrepresented as one chain.

All rejected or restricted cold flows remain actual model-authored, independently verified recordings.
None was edited to remove an assertion, replace a selector, manufacture a repair result, or improve the
demo narrative.
