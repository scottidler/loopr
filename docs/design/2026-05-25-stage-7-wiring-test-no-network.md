# Stop `plan_create_exercises_stage_7_spawn_path_without_crash` from hitting the network

- Crates touched: loopr (test only)
- Status: Implemented

## Resolution

After Architect review, the chosen option was **Option 5 — delete the test** (the Architect's addition, not in the original three options). Verifications:

- `crates/loopr/src/transport/handler.rs:229-236`: the decomposer-error branch short-circuits with a `warn!` and falls through; `Ok(works)` (containing the spawn loop) is never reached when the decomposer fails.
- `crates/decomposer/src/decompose.rs:199`: `return Err(DecomposerError::LlmFailed(retry_err))` on the retried 401. The decomposer does NOT return an empty `Vec<Work>` for the no-key path — the test docstring's "(the expected outcome with no API key set)" framing was incorrect.
- `crates/loopr/src/transport/handler/tests.rs:217` (`plan_create_with_failing_llm_still_persists_plan_and_leaves_works_empty`) already covers the Err-branch behavior the integration test was actually exercising, using the same `http://127.0.0.1:1` closed-loopback pattern.
- The sibling `plan_create_daemon_shutdown_drains_implementer_tasks_cleanly` in the same file covers the fork-daemon + drain lifecycle.

The integration test was a placebo: its docstring claimed Stage 7 spawn-path coverage; the actual code path short-circuited before any spawn happened. Deleting it loses zero genuine coverage. The real `Ok(works)` spawn-path coverage remains deferred to Stage 9's first-gate run, exactly as the deleted test's own docstring already admitted.

What shipped:

1. Deleted the `plan_create_exercises_stage_7_spawn_path_without_crash` function body from `crates/loopr/tests/stage_7_wiring.rs`. Left a tombstone comment in its place pointing here and to the empirical evidence.
2. Updated the file's top docstring to describe what the file actually guards now (just the drain path).
3. No changes to `decomposer`, `llm`, or production `loopr` code. No new config knobs. None of Options 1, 2, 3, or 4 applied.

Q5 (the placeholder-API-key design in `resolve_api_key`) is **not** addressed by this change — Architect flagged it as out of scope for this flake fix. Worth tracking separately: "construction succeeds, call-time 401" is the design surface that lets daemon plumbing tests accidentally hit `api.anthropic.com`. A future cleanup could make missing-key fail fast at daemon startup instead, but that's a real product surface change, not a test fix.

## The problem

`crates/loopr/tests/stage_7_wiring.rs::plan_create_exercises_stage_7_spawn_path_without_crash` failed in the latest `otto ci` run with:

```
Error: ipc client error: request timed out after 10s
command=`.../target/debug/loopr -C /tmp/.tmpzOsLPM plan create "toy stage 7 goal"`
```

The sibling test in the same file documents the cause inline:

> "The earlier shape used `loopr plan create` for this, but plan.create routes through the decomposer (real LLM call without an API key takes 7-15s before the deterministic fallback), which intermittently exceeded the client's 10s `client-request-secs` cap."

The mechanism is in `crates/loopr/src/config.rs:165-169`, where `resolve_api_key` returns a placeholder string when `ANTHROPIC_API_KEY` is unset:

> "returns a placeholder string that makes `AnthropicClient::new` succeed but any actual LLM call will fail with 401."

So with no API key, the daemon constructs an `AnthropicClient` with a placeholder, the decomposer fires a real HTTPS request to `api.anthropic.com`, gets a 401 back after the network round-trip + reqwest's internal retries, then the decomposer falls back to deterministic output. The whole sequence intermittently exceeds 10s.

The sibling drain test was de-flaked in `91b3108` by switching its auto-fork command from `loopr plan create` to `loopr plans` (a fast read-only call that doesn't invoke the decomposer). That fix is not available here because **this** test specifically exists to exercise the `plan create` -> Stage 7 spawn path. Switching away loses the regression coverage.

## What this test actually exists for

From the test's own docstring (`crates/loopr/tests/stage_7_wiring.rs:1-19`):

> The Stage 7 design doc's full E2E exit criterion ... requires a live or mocked Anthropic backend to script both the decomposer and the implementer responses. That deep-E2E is deferred to Stage 9's first-gate run.
>
> What THIS test guards:
> - `handle_plan_create` invokes the Stage 7 spawn path without crashing the daemon, even when the decomposer returns no Works (the expected outcome with no API key set).
> - `drain_implementer_tasks` is reached during daemon shutdown without deadlocking against an empty JoinSet.
> - The new DaemonContext fields (`context_builder`, `implementer_config`, `worktree_cleanup_policy`, `implementer_tasks`) are initialized at daemon startup without panic.

The test is a regression guard for **daemon plumbing**, not for LLM behavior. It asserts:

- The daemon doesn't crash on `plan create`.
- The Stage 7 spawn path is reached.
- The drain path is reached on shutdown.
- A Plan is persisted with the goal text.
- `.loopr/taskstore/` exists.

It explicitly does *not* assert anything about decomposition correctness, what Works get spawned, or anything LLM-related.

## Why "test a failed Anthropic call" is the wrong shape

The current test, by accident, also asserts:

- That `AnthropicClient::new` succeeds with the placeholder API key.
- That the decomposer's no-API-key code path actually catches the resulting 401 and falls back deterministically.
- That the full 401 round-trip + reqwest retry sequence completes inside `client-request-secs`.

None of those assertions are in the test's stated scope. All three are LLM-layer concerns, not Stage 7 wiring concerns. The third is the one that flakes — and it's testing a behavior the test docstring explicitly says is out of scope.

Worse: this test currently makes a real HTTPS request to `api.anthropic.com` from CI every time it runs. That's:

- A network dependency in a test that's supposed to verify daemon-internal plumbing.
- Rate-limit-able by Anthropic.
- Flaky on slow/segmented CI networks.
- An external observable side effect (CI machines pinging Anthropic from random IPs).

The fix is to stop the network call. The question is how.

## Options

### Option 1 — Point the LLM at a fast-failing localhost URL via test config

The test writes `<target>/.loopr/config.yml` before invoking `loopr plan create`:

```yaml
llm:
  api-base-url: "http://127.0.0.1:1"
```

`validate_api_base_url` (in `crates/llm/src/anthropic.rs:412`) accepts this — it requires a non-empty URL, no control chars, no trailing slash, http/https scheme, and a host. `http://127.0.0.1:1` passes all four checks (verified).

At call time, `reqwest` gets `ECONNREFUSED` from the closed loopback port in milliseconds. The decomposer hits its no-LLM fallback path immediately. The full `plan create` IPC round-trip completes well inside 10s.

Pros:
- Single file change. Test-only.
- Solves the root cause (no network call).
- No production code touched.
- The closed loopback port is OS-portable (Linux/macOS both refuse connections to port 1 by default).

Cons:
- Encodes a "use a deliberately broken URL" trick in a test, which a future reader has to recognize.
- Still goes through the decomposer's no-LLM fallback path, so the test still incidentally asserts that path exists and works. (Architect: is that ok, or is the test still over-asserting?)
- If `reqwest` ever decides ECONNREFUSED is retryable (it shouldn't, but the LLM crate may layer its own retry on top — verify), the 401-path replays as a connection-refused retry storm and we're back where we started.

### Option 2 — Give the decomposer a "no-LLM" mode

Add a config knob (or env var) that tells the decomposer to skip the LLM entirely and return zero Works without constructing an HTTP client.

```yaml
# .loopr/config.yml in the test's target dir
decomposer:
  mode: "skip-llm"   # or "deterministic-only"
```

The decomposer's entry point reads the mode and short-circuits to the deterministic-fallback path without ever calling `AnthropicClient::new` or `complete_with_tool`.

Pros:
- Test asserts what it claims to assert: daemon plumbing on the no-Works branch.
- The "deterministic decomposition" path becomes a first-class supported mode, not an implicit consequence of a 401 failure.
- The knob may be useful outside tests: a developer running loopr against a target without an API key gets fast feedback rather than 7-15s of 401 wait per `plan create`.

Cons:
- Bigger blast radius: a new config field in `decomposer` config, a new branch in the decomposer entry point, a precedence rule (env vs. config), and a serialization/deserialization test.
- The "decomposer deterministic-only mode" is a real product surface, not just a test hook. Probably needs an option name that doesn't lie ("skip-llm" sounds like a debug flag; what should an operator using this without an LLM see in their summary?).

### Option 3 — Redesign the test to not invoke `plan create`

Stage 7 wiring is exercised by the **daemon's** spawn path, which the IPC layer triggers on `plan.create`. There's no other CLI verb that fires the same spawn path; calling `plans` (the sibling test's choice) auto-forks the daemon but doesn't invoke `handle_plan_create`.

Pros:
- Removes the LLM call entirely.

Cons:
- Loses the actual Stage 7 spawn-path coverage. The test exists *because* `plan create` is the trigger. Switching to a cheaper verb makes the test a different test (just "does the daemon fork and drain cleanly," which is the sibling's job).
- This option is effectively "delete the test."

### Implicit Option 4 — Bump `client-request-secs` for this test only

What I almost did before the user stopped me. Write `transport: client-request-secs: 30` into `<target>/.loopr/config.yml`. The decomposer still makes the 401 call; the client just waits longer.

Pros:
- Smallest possible patch.

Cons:
- Treats the symptom (timeout) not the disease (unwanted network call from a wiring test).
- Test still makes an HTTPS request to Anthropic in CI.
- Test still incidentally asserts the 401 fallback path works.
- 30s is a guess; if Anthropic's edge gets slower or reqwest adds another retry layer the flake comes back.

Listed only to be explicit about why it isn't on the table.

## Verifications the Architect should run before opining

1. Read `crates/decomposer/src/lib.rs` (or wherever the decomposer entry point lives) and confirm the actual no-API-key code path: does it really catch a 401 and fall back, or does it propagate the error to the daemon and just *log* the failure? If the latter, the test's `plans.jsonl` assertion may be passing for a different reason than the docstring suggests.
2. Confirm the LLM crate doesn't layer retry-on-ECONNREFUSED logic on top of `reqwest`'s defaults. Grep for retry/backoff in `crates/llm/src/`. Option 1 only works if ECONNREFUSED is a fast terminal failure.
3. Read `validate_api_base_url` (`crates/llm/src/anthropic.rs:412`) and confirm `http://127.0.0.1:1` actually passes — I claimed it does after eyeballing the code; verify against the rules.
4. Check `crates/decomposer/CLAUDE.md` (or scope-doc equivalent) for any existing "no-LLM mode" pattern. Option 2's product surface may already be partially designed.
5. Check whether any other integration test in the repo already uses Option 1's pattern (closed loopback port) — if so, this should match that pattern.

## Specific questions for the Architect

1. Is Option 1 (closed loopback port via test config) acceptable as a "test hack," or does the implicit assertion of the no-LLM fallback path make it a confused test that should be Option 2 instead?
2. If Option 2: where does the config knob live — `decomposer.mode`, `llm.enabled`, top-level `dry-run`, or an env var? What's the operator-facing story for "loopr against a target without an LLM"?
3. Is there a fifth option (mock at a different seam, fixture file, etc.) that neither Claude nor the prior comment in the sibling test considered?
4. Should this test even exist in its current form, or is it pretending to test Stage 7 wiring while actually being a duplicate of the sibling drain test plus a network call? Concretely: is the spawn-path coverage real, or does the no-API-key fallback short-circuit before the spawn even fires?
5. Hardest question: should `resolve_api_key` ever return a placeholder string that lets `AnthropicClient::new` succeed? The current design "succeeds at construction, fails at call time" is precisely what creates this test-flake surface. Should the missing-key path fail at startup instead?
