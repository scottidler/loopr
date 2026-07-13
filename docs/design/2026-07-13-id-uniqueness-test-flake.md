# ID-uniqueness test flake: findings and options

Crates touched: domain (tests), store (ticks id pre-check)
Status: Implemented
Shipped: 2026-07-13 (see "Resolution" below)

## Purpose

Full accounting of a pre-existing CI flake surfaced while fixing the
log-capture flakes (follow-up #1 of the failure-paths recovery chain), plus
the options as I see them, for review-board decision. This doc does NOT
change code; it asks which option to build.

## The finding

`crates/domain/src/id/tests.rs` has two tests that intermittently fail:

- `generate_id_uniqueness_1000` (line 25)
- `work_id_uniqueness_1000` (line 127)

Both draw 1000 freshly-minted ids and assert all 1000 are distinct:

```rust
let ids: HashSet<String> = (0..1000).map(|_| generate_id("wk")).collect();
assert_eq!(ids.len(), 1000, "expected 1000 distinct ids");
```

Measured flake rate: 2/150 full-binary runs (~1.3%) for `generate_id`; the
`work_id` twin is the same class. Combined, full `otto ci` reds ~1.6% of runs
on these two tests. They pass on isolated rerun. This is unrelated to the
log-capture flakes (different crate, different mechanism) and pre-dates this
session's work; `domain` was untouched by the log-capture fix.

## Root cause

An id is `{prefix}-{5 base36 chars}` (8 chars total). The body space is
36^5 = 60,466,176. Drawing N=1000 ids from a ~60.5M space, the birthday
collision probability is:

```
P(collision) ≈ 1 - e^(-N^2 / 2M) = 1 - e^(-1000^2 / (2·60,466,176)) ≈ 0.82%
```

That matches the observed ~1.3% (per-test, over 150 runs is a small sample).
The generator is behaving exactly as designed — random 5-char base36. The
tests assert a property (1000/1000 distinct) that is *statistically false*
for a random generator over this space. This is a test defect, not a
generator defect.

The design comment in `id.rs` is explicit that 60M is a deliberate choice:

> `generate_id(prefix)` produces `{prefix}-{5-char-base36}` — an 8-char ID
> readable in logs and sized to 36^5 ≈ 60M per-prefix entropy, sufficient
> for any single repo's record cardinality.

## Production reality (grounded in the code, not assumed)

The collision that the test frets over is handled at the persistence seam,
fail-closed — with **one gap the review panel caught** (now fixed; see
Resolution):

- `WorksStore`, `PlansStore`, `BundlesStore`, `NotesStore`, `CheckRunsStore`,
  `ReviewsStore` all pre-check the incoming id against the store and return
  `StoreError::AlreadyExists` on a hit, rather than letting SQLite's
  `INSERT OR REPLACE` silently overwrite. See `crates/store/src/works.rs:34`,
  `plans.rs:45`, `bundles.rs:40`, `notes.rs:40`, etc. Each carries a comment
  naming the `INSERT OR REPLACE` overwrite it prevents.
- **`TicksStore::create` did NOT** (as originally written, `ticks.rs:70`). It
  guarded only the *semantic* `(plan_id, bundles-set)` identity via
  `DuplicateTick`, then called `self.inner.create` with no *id* pre-check, so
  a `TickId` collision would silently `INSERT OR REPLACE`-overwrite a prior
  Tick. The first version of this doc wrongly listed `ticks` among the
  fail-closed stores; the Staff Engineer flagged it and I confirmed it. Fixed
  in this change (id pre-check mirroring the siblings, under the existing
  `tick_lock`), plus a break-to-prove regression test.
- The highest-volume mint path — a decomposition's Work batch — goes further:
  `persist_works_with_remint` (`crates/loopr/src/transport/handler.rs:464`)
  catches `AlreadyExists`, **re-mints every id in the batch, remaps the
  dependency edges, and retries** (bounded to 5 attempts). Work collisions are
  transparent to the operator UNTIL the 5-attempt bound is exhausted, at which
  point it returns `AlreadyExists` with a synthetic id and the Plan stalls
  (`handler.rs:378`). At ~0.8%/batch that bound is astronomically unlikely to
  exhaust, but "transparent" is bounded, not unconditional. There is an
  explicit "~0.8% by 1k records" comment at `works.rs:53-59` acknowledging the
  math.

So in production a colliding id is either transparently re-minted (Works,
within the retry bound) or surfaces as a loud `AlreadyExists` error (single
creates, ticks now included) — never data loss. The ~0.8%/1000 number is a
real property of the id space, but it is a solved problem at the layer that
matters.

## What the tests are actually worth

The `*_uniqueness_1000` tests do NOT exercise the store's collision handling
(they call the raw generator, not `create`). Their only real signal is
"the generator isn't grossly broken" (constant output, stuck RNG, or far
lower entropy than intended). That signal is:

- Partly redundant with `generate_id_format` and `generate_id_base36_chars`
  (format + charset), which already run.
- Delivered by an assertion that is statistically guaranteed to false-fail
  ~0.8% of the time per test.

## Options

### Option A — tolerance assertion (keep N=1000)
Assert `ids.len() >= 1000 - K` (or a distinct-ratio ≥ threshold) instead of
`== 1000`. Keeps the sample size. Downsides: still statistical, K is
arbitrary, and a K set tight enough to catch a subtly-degraded generator can
still false-fail; K set loose enough to never false-fail catches little.

### Option B — shrink the sample
Reduce to N where collision is astronomically unlikely yet gross breakage is
still caught (e.g. N=100 → P ≈ 0.0083%; a stuck/constant/low-entropy
generator still collides heavily at 100). Downside: weakens the "1000" intent
and the false-fail rate is smaller but non-zero.

### Option C — deterministic seeded RNG
Give `generate_id` a seedable/injectable RNG for tests (e.g. a
`generate_id_with<R: Rng>` seam, prod calls it with `rand::rng()`). The test
seeds a fixed RNG and asserts an exact, deterministic distinct count.
Fully deterministic; zero false-fail. Downside: a production-API/test-seam
change to `domain` for a test-only benefit.

### Option D — test the real invariant, drop the statistical one
Replace the `*_uniqueness_1000` tests with a deterministic distribution check
that catches the failure modes we actually care about (constant output,
stuck RNG, degenerate charset) without any birthday risk. Two sub-variants:

- **D1**: assert each of the 5 body positions yields >1 distinct char across a
  modest sample, and the charset is the full base36 set — catches stuck/low-
  entropy generators deterministically.
- **D2**: drop the `*_uniqueness_1000` tests entirely as statistically-invalid
  and redundant; rely on `generate_id_format` + `generate_id_base36_chars`
  for the generator, and the store-level `AlreadyExists` / re-mint tests for
  collision behavior where it actually matters.

### The separate (non-blocking) design question — is 60M entropy right?
Given fail-closed + re-mint, 60M is correct for *correctness* today. The only
cost of collisions is extra re-mint retries at high cardinality, which is
bounded and currently invisible. Widening the body (6-7 base36 chars) would
cut the collision rate by 36×/1296× and shrink re-mint churn, at the cost of
longer, less-readable ids and a format change rippling through id parsers,
fixtures, and the `id.len() == 8` assertion. I do not think we have evidence
to justify this yet ("make it be a problem first"), but it belongs on the
record as the road not taken.

## My recommendation

**Option D2** (drop the two `*_uniqueness_1000` tests) or **D1** (replace with
a deterministic distribution check) — I lean D1 so we keep a real signal on
the generator that can't false-fail. Keep production at 60M entropy: the
persistence layer already makes collisions safe (fail-closed + transparent
re-mint), so there is no correctness reason to change the id format now.

Rejected: A and B keep a statistical assertion in a unit test, which is the
root defect; C adds a production seam for a test-only gain that D1 achieves
without touching prod.

## Open questions for the board

1. D1 vs D2: keep a deterministic generator-health test (D1), or is that
   redundant with format+charset and better dropped (D2)?
2. Is there any appetite to revisit the 60M entropy now, or is
   "make it be a problem first" the right posture given fail-closed + re-mint?
3. Scope: fold this into the same session as the log-capture fix (already
   committed `b1b076ed`), or track as its own tiny follow-up commit?

## Acceptance criteria

- A constant/stuck generator (any body position collapsed to one char) fails
  the new `*_body_positions_are_not_stuck` tests.
- The domain lib test binary passes across many consecutive full-binary runs
  with no flake (verified: 0/300).
- **D1 done** = both `*_uniqueness_1000` tests replaced by a per-position
  not-stuck check with an explicit fixed `SAMPLES`; no `assert_eq!(len, N)`
  uniqueness assertion remains in `id/tests.rs`.
- **Ticks gap done** = `TicksStore::create` returns `AlreadyExists` on an id
  collision that is NOT a `(plan_id, bundles-set)` duplicate; a break-to-prove
  test fails when the guard is removed (verified) and passes with it.
- Every collection's create path is verified to fail closed on id collision,
  or the exception is documented (none remain after the ticks fix).

## Review-panel addendum (2026-07-13)

Design Review, both reviewers with full codebase read access.

- **Convergence:** root-cause analysis correct (36^5 ≈ 60.4M, ~0.82% at
  N=1000, tests hit the raw generator not the store); keep 60M entropy
  ("make it be a problem first"); separate follow-up commit; the doc's first
  version lacked acceptance criteria.
- **Architect (Gemini):** verified the birthday math, fail-closed for
  works/plans/bundles/notes/checks/reviews, and `persist_works_with_remint`
  (retry x5). Did not check ticks. Answers: D1, do not revisit 60M, separate
  commit. Noted: confirm the remint-collision `warn!` is wired to an
  observable metric, else "make it be a problem first" has no tripwire.
- **Staff Engineer (Codex):** found the `TicksStore::create` id-collision gap
  (verified: `ticks.rs:70` checks only the `(plan_id, bundles-set)` pair, then
  `INSERT OR REPLACE`); flagged that "never data loss" was overstated; noted
  D1 is not *literally* deterministic (false-fail ~10^-28, not 0 — only
  Option C's seam is literal-zero) and that "full charset appears" is
  redundant with `generate_id_base36_chars`; named the remint-exhaustion
  unhappy path (`handler.rs:378`). Answers: D2 (unless D1 is trimmed to a
  truly-tight per-position check), do not revisit 60M until ticks is fixed,
  separate commit.
- **Reconciled:** **D1 trimmed** (per-position not-stuck only; charset clause
  dropped) — keeps a real "RNG isn't broken" signal at a false-fail rate ~26
  orders of magnitude below the flake. **Fix the ticks gap now** (top
  finding). **Keep 60M**, legitimate once ticks is closed. **Separate commit.**

Follow-up not taken this change: wiring the remint-collision `warn!`
(`handler.rs:473`) to a counter/metric so a future entropy revisit has a
tripwire. Recorded here as the observability gap the Architect named; not
blocking, tracked for whoever adds run-level metrics.

## Resolution (2026-07-13)

Owner accepted the reconciled advice. Shipped:

1. **Ticks id-collision gap fixed** — `TicksStore::create` now id-pre-checks
   under `tick_lock` and returns `AlreadyExists`; break-to-prove test
   `colliding_id_different_bundles_rejected_as_already_exists`
   (`crates/store/src/ticks/tests.rs`) fails without the guard, passes with
   it. This was the review's top finding and the real bug the doc surfaced.
2. **Test flake fixed via D1-trimmed** — `generate_id_uniqueness_1000` and
   `work_id_uniqueness_1000` replaced by `*_body_positions_are_not_stuck`
   (fixed `SAMPLES = 256`, per-position `distinct > 1`, no charset clause).
   Verified 0/300 full-binary domain runs (was ~1.3%/test).
3. **60M entropy unchanged** — fail-closed everywhere (ticks now included) +
   transparent Work re-mint make collisions safe; no format change.

Deferred (road not taken): widening the id body; wiring the remint-collision
metric. Both recorded above.
