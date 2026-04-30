# Design Document: Rename Role::Coordinator to Role::Reactor (code)

**Author:** Scott A. Idler
**Date:** 2026-04-30
**Status:** Implemented
**Review Passes Completed:** 3/5 (Draft, Correctness, Clarity; Edge Cases folded into Risks; Excellence skipped — mechanical refactor doc, 3 is appropriate)
**Crates touched:** agents, context, derive, domain, integrator, loopr, store, worktree

## Summary

Mechanical follow-up to commit `305bcdb` (doc rename per ADR-0002). Renames `Role::Coordinator` to `Role::Reactor` everywhere in `crates/`, including FSM tables, test function names, serialized strings, and span fields. Uses the `replace` shell function for the bulk in three scoped sweeps; reserves Edit for prose-context comments that need rewording rather than blind substitution. Removes the "code rename pending" banners from CONTEXT.md and roles-and-states.md once `cargo check && cargo test && otto ci` are green.

## Problem Statement

### Background

ADR-0002 decided the `Coordinator` Role variant is a v3 fossil that keeps inviting confusion (most recently the "Coordinator agent" entry in `deferred-roadmap.md` Tier 1.2). The doc rename landed in `305bcdb` with a banner explicitly noting the code lags. This doc plans the code follow-up.

### Problem

`Role::Coordinator` appears in 8 crates: 36 files reference it directly (uppercase), and 14 additional files reference the kebab-case serialized form `"coordinator"` (lowercase) — test fixtures pinning the strum-generated string, test function names like `transition_draft_pending_by_coordinator`, and a small set of prose comments. A naive global sed across the entire repo would also touch the 30GB workspace `target/` directory, hidden caches, and design-doc historical records that should remain accurate to the period when they were written.

### Goals

- Zero `Coordinator` references in `crates/` (uppercase or lowercase) after the rename.
- `cargo check --workspace` clean.
- `cargo test --workspace` clean (test fixtures align with the new strum kebab-case `"reactor"`).
- `otto ci` clean at repo root.
- Banners in `CONTEXT.md` and `docs/roles-and-states.md` removed once code matches.
- Workspace `target/` not touched by the rename mechanism.

### Non-Goals

- Renaming `crates/tools::LaneRouter`. Unrelated; different layer.
- Touching `docs/design/*.md` historical records. Those describe shipped code under the old name; preserve as historical truth. The two living docs already updated (`docs/roles-and-states.md`, `CONTEXT.md`) and `deferred-roadmap.md` do not need a second pass.
- Rewriting v3/v4 file-path references like `~/repos/scottidler/loopr/src/agents/coordinator.rs`. Those are real files in other repos; the path stays accurate.
- Changing the strum derive or kebab-case serialization rule. The variant rename alone produces `"reactor"` automatically.

## Proposed Solution

### Overview

Three-sweep `replace` strategy, scoped tightly to avoid `target/` and to separate uppercase (always safe) from lowercase (mostly safe; one prose case needs review). Followed by a manual pass for context-sensitive comment wording, then verification.

### `replace` semantics (recap)

The shell function: `replace FIND [REPL] [DIR]`. Implemented as:

```
find "$DIR" -path ./.git -prune -o -type f -exec sed -i "s|$FIND|$REPL|g" {} \;
```

Important properties for this rename:

1. **Recursive into `$DIR`** — every file under the dir is processed.
2. **Skips `.git/` only** — does not respect `.gitignore`, `target/`, or hidden dirs.
3. **No text/binary discrimination** — `sed` will rewrite anything it can open.
4. **Sed delimiter `|`** — the strings in this rename (`Coordinator`, `Reactor`, `coordinator`, `reactor`) are letter-only, so no escaping needed.
5. **Case-sensitive by default** — `Coordinator` and `coordinator` are independent passes.

The DIR scope is the load-bearing safety control. `crates/` excludes the workspace `target/` (at repo root, 30GB). Per-crate `target/` directories do not exist in this workspace; the only `crates/.../target` path (`crates/loopr/src/target/`) is a source submodule, not a build artifact.

### Architecture

The rename touches three distinct surfaces and they need different treatment:

| Surface | Pattern | Treatment |
|---|---|---|
| **Variant + uppercase usages** | `Coordinator` in Rust source, FSM macro tables, comments, test bodies | `replace Coordinator Reactor crates/` — fully mechanical |
| **Strum-serialized fixtures** | `"coordinator"` strings, `_by_coordinator` test function names, `(coordinator)` in expected docstrings | `replace coordinator reactor crates/<scope>/tests` — scoped to test trees |
| **Prose comments** | `// Stage 7's reactive coordinator`, `//! coordinator scans...` in non-test source | Edit per-file after grep; reword (do not blindly substitute) |

### Implementation Plan

#### Phase 1: Pre-flight inventory and scope check
**Model:** sonnet

- Confirm working tree clean: `git status` reports no uncommitted changes from the previous doc commit follow-ups.
- Record baseline counts inside `crates/`:
  - `grep -rln 'Coordinator' crates/ | wc -l` → expect 36
  - `grep -rln 'coordinator' crates/ | wc -l` → expect 14
- Confirm `crates/` is the complete scope. Repo-root sweep with the right exclusions:
  - `grep -rln 'Coordinator' --exclude-dir=target --exclude-dir=.git --exclude-dir=docs --exclude-dir=crates .` → expect only `CONTEXT.md` (intentional; the lineage paragraph). Anything else in `bin/`, `Cargo.toml`, `clippy.toml`, `.otto.yml`, `README.md` would need to be added to Phase 2's scope.
- Confirm no per-crate `target/` build artifacts exist:
  - `find crates/ -type d -name target` → expect only `crates/loopr/src/target/` (source submodule; verify with `file crates/loopr/src/target/*` showing "Rust Source file").
- Record the prose-comment hits that Phase 4 will need to handle:
  - `grep -rn 'coordinator' crates/*/src/ > /tmp/lowercase-src-hits.txt`
  - Manually review the file list; it should be small (≤10 hits, mostly `// reactive coordinator`-style comments in `crates/{agents,context,domain,loopr,worktree}/src/`).
- No file mutations in this phase. Output is a clean baseline.

#### Phase 2: Bulk uppercase replace
**Model:** sonnet

- Run `replace Coordinator Reactor crates/`.
- Verify: `grep -rn 'Coordinator' crates/` returns zero hits.
- `cargo check --workspace` to catch any structural breakage early. Expected: clean. The rename is the variant name + every reference; `cargo check` only typechecks, so it does not exercise the test fixtures with hardcoded `"coordinator"` strings — that's deliberate. `cargo test --workspace` would *fail* between Phase 2 and Phase 3 because the strum-derived `Display` impl now produces `"reactor"` while fixtures still expect `"coordinator"`. Don't run tests yet.
- `git diff --stat` to sanity-check that only `.rs`, `.toml`, `.yml`, `.md` files were touched, and no binary files snuck in.
- Do NOT commit yet. Phase 3 lands together.

#### Phase 3: Scoped lowercase replace in test trees
**Model:** sonnet

- Identify which test trees have lowercase hits. From baseline: `crates/derive/tests/`, `crates/domain/tests/` (and any I missed; phase 1 inventory is authoritative).
- Per-tree: `replace coordinator reactor crates/<crate>/tests/`
  - Hits: serialized `"coordinator"` strings in test fixtures, test function names like `_by_coordinator`, expected-output strings like `"valid from A (override): C (coordinator)"`.
- Verify: `grep -rn 'coordinator' crates/<crate>/tests/` returns zero hits per tree.
- `cargo test -p domain` and `cargo test -p derive` to confirm fixtures align with strum's kebab-case serialization (which now produces `"reactor"`).

#### Phase 4: Manual prose-comment review
**Model:** sonnet

- Review the residual `crates/*/src/` lowercase hits saved in Phase 1.
- For each hit, decide between three actions:
  1. **Plain rename** — if the text is `coordinator` referring to the v5 daemon's role and "reactor" reads cleanly, use Edit to substitute.
  2. **Reword** — if blind substitution produces awkward phrasing, reword via Edit. Concrete example: `crates/domain/src/work.rs:4` says `//! Stage 7's reactive coordinator` — naive substitution gives `//! Stage 7's reactive reactor` which is awful. Reword to `//! Stage 7's Reactor (the daemon's mechanical FSM router)` or simply `//! the Reactor` if the surrounding context already disambiguates.
  3. **Leave** — if the text references v3 history or a real v3 file path, leave it. (Should be rare in `crates/*/src/`; more common in design docs which we're not touching.)
- Re-grep: `grep -rn 'coordinator' crates/` should return zero hits in `src/` or only the v3-history exceptions if any.

#### Phase 5: Full verification and banner removal
**Model:** sonnet

- `cargo check --workspace` clean.
- `cargo test --workspace` clean.
- `cargo fmt --check` clean.
- `otto ci` at repo root clean.
- `git diff --stat` review: confirm the touched files are all expected (the 36 + 14 + a handful of Phase 4 manual edits + the three living docs from this phase). No binary files. No unintended `target/` paths.
- Edit `CONTEXT.md`: under the "Reactor" definition, drop the "Renamed from `Coordinator` per ADR-0002 to remove the agent-flavored connotation; code rename across `crates/` follows in a later commit." sentence and replace with "Renamed from `Coordinator` per ADR-0002." The lineage paragraph in the "Lineage of the Reactor name" section stays as-is.
- Edit `docs/roles-and-states.md`: remove the entire `> **Naming note (ADR-0002):**` blockquote at the top of the file (the one that says "code rename pending"). Keep the inline note in the Reactor section ("The name change from `Coordinator` (ADR-0002) was specifically to remove the agent-flavored connotation.") and the "v3 had only one top role (named `Coordinator` then)" lineage paragraph in "Why Reactor and Director are Both Roles."
- Edit `docs/adr/0002-rename-role-coordinator-to-reactor.md`: under `## Status`, replace "Doc rename lands first (this commit). Code rename across `crates/domain/src/{role,plan,work}.rs` and 49 referencing files lands as a follow-up; until then, `roles-and-states.md` and CONTEXT.md describe the post-rename naming with an explicit 'code rename pending' note." with "Decided. Doc rename: commit `305bcdb`. Code rename: commit `<this-commit-sha>`."
- Commit. Single commit; this is one logical change. Commit message references ADR-0002 and the design doc.

## Alternatives Considered

### Alternative 1: Per-file `Edit` for everything

- **Description:** Use the Edit tool on each of the ~50 files individually.
- **Pros:** Maximal precision; every change is reviewed before it lands.
- **Cons:** Massive effort for a mechanical change; high risk of skipping a file or fat-fingering one of dozens of edits; the FSM tables in `crates/domain/src/{plan,work}.rs` have ~40 `Coordinator` instances each, all identical, all needing the same substitution.
- **Why not chosen:** The bulk is mechanical and `replace` shines on exactly this shape. Per-file Edit is reserved for Phase 4's prose-context cases.

### Alternative 2: Single `replace` pass with combined uppercase + lowercase

- **Description:** Run `replace Coordinator Reactor crates/` and `replace coordinator reactor crates/` back-to-back from the repo root, no scope.
- **Pros:** Two commands.
- **Cons:** The unscoped lowercase pass would rewrite prose comments that need rewording (`reactive coordinator` → `reactive reactor` is awkward); also the Phase 4 review would be retrofitted onto a tree where the changes already landed, mixing mechanical and judgment changes.
- **Why not chosen:** Separating the lowercase replace into a test-tree scope (Phase 3) and a manual review (Phase 4) keeps the diff readable and the judgment cases isolated.

### Alternative 3: `find . -name '*.rs' -exec sed ...` directly

- **Description:** Skip `replace`; use raw `find + sed`.
- **Pros:** Maximum control over which file types get touched.
- **Cons:** Reinvents `replace`; the rule explicitly says to use `replace` when the pattern is literal, unique, and the scope is well-bounded — this rename is exactly that case for the uppercase pass.
- **Why not chosen:** `replace` is the documented tool; the scoping concerns (target/, prose context) are addressed by phasing, not by reaching for a different command.

### Alternative 4: Workspace-wide `replace` from repo root

- **Description:** `replace Coordinator Reactor .`
- **Pros:** One command.
- **Cons:** Touches `target/` (30GB of build artifacts), `docs/design/*.md` historical records, and `docs/adr/` records. Some of those should not change; some are write-protected by intent (historical records preserve the period naming). It'd also rewrite the ADR-0002 doc itself in confusing ways.
- **Why not chosen:** The non-goals explicitly preserve `docs/design/` historical records and the ADR's textual references to the old name.

## Technical Considerations

### Dependencies

No new dependencies. The `replace` shell function is defined in the user's dotfiles (`~/repos/scottidler/dotfiles`); it's part of the standard environment.

### Performance

- `replace` on `crates/` (≈400 source files, mostly small): a few seconds end-to-end.
- `cargo check --workspace`: bound by the workspace's incremental-compile state. After the rename, `domain` and every dependent crate (essentially all of them) recompile. First check post-rename: probably 30–90 seconds.
- `cargo test --workspace`: bound by test count; this workspace's tests are unit + seam tests, no E2E. Probably 1–3 minutes total.

### Security

None. Mechanical text substitution on local files. The patterns (`Coordinator`, `coordinator`) are not security-sensitive.

### Testing Strategy

- **Phase 2 verification**: `grep -rn 'Coordinator' crates/` returns zero hits, `cargo check --workspace` passes.
- **Phase 3 verification**: `grep -rn 'coordinator' crates/<crate>/tests/` returns zero per scoped tree, `cargo test -p <crate>` passes for each test tree touched.
- **Phase 5 verification**: full `cargo test --workspace` plus `otto ci`.
- The strum-derived `Display` impl produces `"reactor"` automatically once the variant is renamed. Tests that hardcoded `"coordinator"` strings in fixtures will fail until Phase 3 lands; this is the expected ordering — we deliberately don't compile-and-test between Phase 2 and Phase 3.

### Rollout Plan

Single commit on the `v5` branch. The branch is the long-lived v5 working branch; tags happen at workspace version bumps, not on every commit. Code-rename commit follows `305bcdb` directly.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `replace` touches the workspace `target/` directory | Low | High (30GB rewrite, slow, possibly corrupts cached builds) | Scope to `crates/`, not `.`; verified pre-flight that no per-crate `target/` directories exist (`crates/loopr/src/target/` is a source submodule, not build) |
| Lowercase `replace` on test trees rewrites a comment that should reword instead | Low | Low | Phase 3 is scoped to `tests/` only, where lowercase hits are serialization fixtures and test names — neither needs rewording. Source-tree lowercase hits are deferred to Phase 4's manual pass |
| A `Coordinator` reference exists outside `crates/` (e.g., in `bin/`, `Cargo.toml`, root `.otto.yml`) | Low | Medium | Pre-flight grep also runs at repo root: `grep -rn 'Coordinator' --include='*.rs' --include='*.toml' --include='*.yml' .` minus `target/` and `docs/`. Any hits outside `crates/` get added to the rename plan before Phase 2 |
| `cargo check` fails post-Phase-2 due to a missed reference | Medium | Low | Expected; the cargo errors point to the missed reference, fix-forward with Edit, re-run |
| Test fixtures have `"coordinator"` strings I haven't found | Low | Medium | Phase 1's grep enumerates all of them; Phase 3 sweeps every test tree where they live; Phase 5's `cargo test --workspace` is the catch-all |
| A historical comment in `crates/*/src/` references "v3's Coordinator" (capital C) and gets blanket-renamed in Phase 2 | Low | Low | Spot-check `git diff` after Phase 2 for any context that reads weirdly. Most v3 references in v5 source are in `~/repos/scottidler/loopr/...` paths inside comments, which contain lowercase `coordinator.rs` and don't get touched in Phase 2 |
| `replace` on `crates/` accidentally rewrites a per-crate `CLAUDE.md` in a way that contradicts a documented invariant | Low | Low | These files use `Coordinator` consistently with the role meaning, so the rename is correct; verify via `git diff crates/*/CLAUDE.md` |

## Open Questions

- [ ] **Is the source-tree lowercase pass needed at all, or can Phase 4's manual review cover it?** Lean: keep them separate. Phase 4 reads small (1-handful of files); skipping a phase doesn't simplify it materially.
- [ ] **Should the FSM-test fixtures use `Role::Reactor.to_string()` instead of the literal `"reactor"` string?** Slightly more robust against future rename. Out of scope for this commit; flag as a follow-up if Phase 5's test sweep reveals brittleness.
- [ ] **Do we want a `git revert` rollback plan?** This is a single commit; revert works trivially. Don't write a special procedure.

## References

- [`docs/adr/0002-rename-role-coordinator-to-reactor.md`](../adr/0002-rename-role-coordinator-to-reactor.md) — the decision.
- Commit `305bcdb` — doc rename (CONTEXT.md, roles-and-states.md, deferred-roadmap.md, ADR-0002).
- [`docs/roles-and-states.md`](../roles-and-states.md) — canonical doc; carries the "code rename pending" banner that Phase 5 removes.
- `~/repos/scottidler/claude/HOME/repos/.claude/rules/refactor.md` — `replace` shell function semantics, scoping rules, and the "use Edit for single-file" rule that motivates Phase 4.
- `~/repos/scottidler/claude/HOME/repos/.claude/rules/dealing-with-large-files.md` — sed-on-large-files risk; not applicable here (Rust source files in this workspace are well under the 1500-line threshold), but worth keeping in mind for future reference.
