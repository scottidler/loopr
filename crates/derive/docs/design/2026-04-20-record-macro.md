# Design Document: `#[derive(Record)]` Proc Macro

**Author:** Scott A. Idler
**Date:** 2026-04-20
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

`#[derive(Record)]` generates the `taskstore_traits::Record` trait impl for a domain record struct (`Plan`, `Work`, `Spec`, `Phase`, `Bundle`, `Tick`, …). It inspects struct-level helper attributes (`#[record(collection = "…")]`) and field-level helper attributes (`#[record(indexed)]`) at macro expansion and emits the four trait methods (`id`, `updated_at`, `collection_name`, `indexed_fields`) as pure data dispatch. The derive replaces v4's hand-written `impl Record for X { … }` blocks, which ran to 10–20 lines per record and forced 11 near-identical impls across the `domain` tree.

**Where this fits in the roadmap:** Stage 5 — the second of four Stage 5 design docs. `Fsm` (shipped v0.5.9) handles the status-enum side; `Record` handles the struct side. Together they're the two derives every Stage 5+ record carries: `#[derive(Fsm)]` on `PlanStatus`, `#[derive(Record)]` on `Plan`. The subsequent `docs/design/records.md` (unwritten) defines the concrete `Plan` struct that consumes both derives; `store.md` (unwritten) then wraps the taskstore API in domain-shaped accessors.

## Problem Statement

### Background

`taskstore-traits::Record` (from `scottidler/taskstore`, branch `main`) declares:

```rust
pub trait Record: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static {
    fn id(&self) -> &str;
    fn updated_at(&self) -> i64;
    fn collection_name() -> &'static str where Self: Sized;
    fn indexed_fields(&self) -> HashMap<String, IndexValue> { HashMap::new() }
}
```

with `IndexValue` being `String | Int | Bool`. Every record type persisted through `taskstore::Store` must implement `Record`. v4 did this by hand across 11 files; a representative example from v4's `plan.rs`:

```rust
impl Record for Plan {
    fn id(&self) -> &str { &self.id }
    fn updated_at(&self) -> i64 { self.updated_at }
    fn collection_name() -> &'static str { "plans" }
    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m.insert("tier".into(), IndexValue::String(self.tier.to_string()));
        m
    }
}
```

Every v4 record repeats the same four methods with two–three parameters differing: the collection name string and the indexed-field names. Typos in the collection name silently land records in the wrong JSONL file; a forgotten indexed field means filter queries ignore that field without complaint. The derive target is exactly that boilerplate.

v3 did not have this derive. v4 did not have this derive. This is net-new codegen — net-new in the same sense `#[derive(Fsm)]` was: the need exists, the shape is agreed, nobody has written it yet.

### Problem

- Hand-written `impl Record` blocks are copy-paste-one-field-wrong bait. A typo in `collection_name()` returns the wrong string forever; the nearest correctness check is "do records end up in the right file on disk."
- `indexed_fields()` drifts silently. Add a new filterable column to `Plan`; forget to update `indexed_fields()`; `Filter::Eq("new_column", …)` returns nothing and no error fires.
- The trait's four-method shape repeated 11 times in v4 is a clear signal the boundary wants a derive. Every new v5 record type (Stage 5: `Plan`; Stage 6: `Work`; Stage 7: `Bundle`; Stage 8: `Tick`; Stage 6+ possibly `Spec`, `Phase`) will add another copy unless the derive exists first.
- Consumer code in `crates/domain/` must NOT `use` proc-macro tokens or runtime state from `derive`; the derive must generate code that references only the consumer's own fields and `::taskstore_traits::{Record, IndexValue}` via fully-qualified paths.

### Goals

- **A `#[derive(Record)]` macro** that attaches to a struct (never an enum), inspects `#[record(...)]` helper attributes on the struct and its fields, and generates `impl ::taskstore_traits::Record for StructName { … }` with the four required methods.
- **Default collection name** derived from the struct's ident: lowercase + append `"s"`. `Plan` → `"plans"`, `Work` → `"works"`, `Phase` → `"phases"`, `Bundle` → `"bundles"`, `Tick` → `"ticks"`. Overridable via `#[record(collection = "custom-name")]` for the v4 precedents where the default doesn't fit (v4's `Coverage` → `"coverage_reports"`, v4's `Validation` → `"validation_reports"`).
- **`id` field requirement**: the struct must have a field literally named `id`, whose type implements `AsRef<str>`. The generated `fn id(&self) -> &str` emits `self.id.as_ref()`. This works for `id: String` (stdlib impl) and for typed-ID newtypes `id: PlanId` where `PlanId: AsRef<str>` (standard pattern in `domain`). The macro does NOT verify the trait bound at expansion — that's rustc's job, and its error (`the trait bound PlanId: AsRef<str> is not satisfied`) is exactly what the consumer needs to see.
- **`updated_at` field requirement**: a field literally named `updated_at`, of type `i64`. The generated `fn updated_at(&self) -> i64` emits `self.updated_at`. As with `id`, the macro does not inspect the type — rustc's return-type mismatch error is the appropriate surface.
- **Indexed fields via `#[record(indexed)]` on field declarations**. The derive collects every field carrying the attribute, emits one `.insert(…)` per field into the returned `HashMap<String, IndexValue>`. Key is the field name as a string literal (snake_case, matching Rust field names and v4's map-key convention). Value is `::taskstore_traits::IndexValue::String(::std::string::ToString::to_string(&self.field))`. Rationale: every indexed field in v4 is coerced through `.to_string()` (via strum's kebab-case `Display` for enums and `String::clone()` for strings); using `ToString::to_string` unconditionally handles both and gives a single codegen path. Numeric and boolean indexing are not needed at first-gate and are left as future attribute options (see §Non-Goals).
- **Struct-only.** The derive rejects enums and unions at expansion with a spanned `compile_error!`. `taskstore_traits::Record` requires `Serialize + Deserialize + Clone + Send + Sync + 'static` — trivially satisfiable for a struct of owned serde types and impossible to meaningfully derive for an enum without tagging choices that belong to the consumer.
- **Unit structs and tuple structs are rejected**. Named-field structs only. A record with no id / no updated_at has no business being persistable; the attribute-driven indexed-field mechanism requires named fields to address.
- **No runtime dependency added to consumers** beyond what they already have. Consumers already depend on `taskstore-traits` (via `domain`'s Cargo.toml). The derive crate itself does not depend on `taskstore-traits`; generated code resolves `::taskstore_traits::Record` at the consumer's compile time, same pattern as `Fsm`'s `::domain::FsmError<S>`.
- **Compile-time validation** during expansion emits spanned `compile_error!` for the following:
    - Attached item is not a struct (enum, union, fn, etc.).
    - Struct is tuple-style or unit-style (not named-field).
    - Struct is generic. Stage 5+ records are all non-generic; earn it later.
    - No field literally named `id`.
    - No field literally named `updated_at`.
    - `#[record(...)]` appears more than once on the struct itself (merge into a single attribute, same rule as `Fsm`).
    - `#[record(collection = …)]` value is not a string literal, or is empty.
    - `#[record(indexed)]` appears on a field with the same name twice (name-collision across duplicate attributes; not a real case but trivially detectable).
    - Unknown keys inside `#[record(...)]` (exhaustive keyword match; typos like `#[record(collecton = "plans")]` fail with a clear hint).

### Non-Goals

- **Pluralization smarts.** The default collection name is lowercase struct ident + `"s"`. No English-plural rules (`child` → `"children"`, `person` → `"people"`), no `"y"` → `"ies"` (`Strategy` would naively become `"strategys"`, not `"strategies"`). For any record whose name doesn't end in a letter that takes a plain `"s"` plural, the consumer uses `#[record(collection = "…")]`. This mirrors v4's pattern, where 9/11 records matched simple-plural and 2 (Coverage, Validation) used explicit overrides.
- **Numeric / boolean indexed fields.** v4's entire indexed-field surface is strings. Adding `#[record(indexed_int)]` or `#[record(indexed_bool)]` is pure speculation at this stage; no Stage 5–9 caller needs them. Add when a concrete need shows up.
- **Custom id / updated_at field names.** Record fields are `id` and `updated_at`, exactly. A consumer that wants a different field name writes a manual `impl Record` — the derive's job is to collapse the common shape, not to accommodate every possible shape. If this bites within Stage 5, revisit.
- **Auto-derive of `Serialize` / `Deserialize` / `Clone`.** The Record trait requires these; consumers add them explicitly (`#[derive(Serialize, Deserialize, Clone, Record)]`). The derive does not emit them because doing so would pull serde / serde_derive into `derive`'s dep tree, which is both unnecessary (serde derives are already universal) and violates the "derive has no non-std runtime shape" spirit.
- **`#[record(skip)]` on fields.** Serde already supports `#[serde(skip)]` for storage; this derive is about the Record trait, not the serialization. No need to duplicate.
- **Relation to `#[derive(Fsm)]`.** The two derives are orthogonal. `Fsm` attaches to the status enum; `Record` attaches to the record struct that owns that status enum as a field. A single type never carries both. No shared attribute grammar.
- **Automatic ID generation.** Some records in v5 may want `id = format!("plan-{nanoid}")`-style synthesis at construction time. That's constructor-side logic, not derive-side. The derive only reads `self.id`; how the consumer populates `id` is entirely out of scope.
- **Graph / reference integrity.** v5's `parent_id` on `Work` is an indexed string; the derive doesn't validate that it points at an extant `Plan`. Referential integrity belongs to the `store` layer (future doc).

## Proposed Solution

### Overview

Standard proc-macro derive. Attaches only to named-field structs. Parses struct-level and field-level `#[record(...)]` helper attributes into a small IR; emits a single `impl ::taskstore_traits::Record for StructName { … }` block with the four methods as pure data dispatch.

The pipeline shape (parse → validate → emit as three sibling modules under `src/record/`) deliberately mirrors the `Fsm` derive's shape in `src/fsm/`. A contributor who has read the `Fsm` design will recognize the layout immediately.

No runtime types added to the derive's emission (unlike `Fsm`, which spawned `Transition`, `FsmError<S>`, etc. in `domain`). `Record` and `IndexValue` already live in `taskstore-traits`; generated code references them through fully-qualified paths so consumers don't need a `use` statement for the derive to work.

### Architecture

```
crates/derive/
├── src/
│   ├── lib.rs              + #[proc_macro_derive(Record, attributes(record))]
│   ├── fsm.rs              (existing)
│   ├── fsm/                (existing)
│   ├── record.rs           new: parse + validate + emit
│   └── record/
│       ├── parse.rs        DeriveInput -> RecordIr
│       ├── validate.rs     compile-time checks
│       └── emit.rs         quote!-driven codegen
└── tests/
    ├── fsm.rs              (existing)
    ├── smoke.rs            (existing)
    ├── trybuild.rs         (existing: gains entries under compile-fail/record-*.rs)
    └── record.rs           new: integration tests against a Plan-like fixture
```

`record.rs` + `record/` mirrors the shape of `fsm.rs` + `fsm/` — same parse→validate→emit three-stage pipeline, same module layout (2018+ style, per project rules).

The derive crate gains no new `[dependencies]`; `proc-macro2`, `syn`, `quote` are already there from Stage 5 Phase 2. Dev-dependency `taskstore-traits` is added so the integration test can verify generated `impl Record for …` blocks actually satisfy the trait.

### Data Model

**Contract with the consumer struct:**

A struct carrying `#[derive(Record)]` must have:

1. `#[derive(Serialize, Deserialize, Clone)]` (the `Record` trait's supertrait bounds).
2. A field literally named `id`, typed so that `self.id.as_ref()` returns `&str` — i.e., the type satisfies `AsRef<str>`. Works for `String` (stdlib impl) and for typed-ID newtypes like `struct PlanId(String)` where the consumer provides `impl AsRef<str>`.
3. A field literally named `updated_at`, typed `i64` (milliseconds since epoch; matches `taskstore_traits::Record::updated_at`'s signature).

The macro checks field *presence* at expansion time (spanned `compile_error!` if `id` or `updated_at` is missing). It does NOT check field *types* — rustc's native errors on the generated body (`trait bound AsRef<str> not satisfied`; `mismatched types: expected i64`) fire at the consumer with the correct span.

**Input (what the user writes):**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Record)]
#[record(collection = "plans")]          // optional; default is "plans" for Plan
pub struct Plan {
    pub id: String,
    pub updated_at: i64,

    #[record(indexed)]
    pub status: PlanStatus,              // PlanStatus: strum::Display -> "draft", "ready", …

    #[record(indexed)]
    pub tier: Tier,                      // Tier: strum::Display -> "tier1", "tier2", …

    pub goal: String,                    // not indexed, not mentioned
    pub created_at: i64,                 // not indexed, not mentioned
}
```

**Generated output (what the macro emits):**

Every path is fully qualified (`::std::…`, `::taskstore_traits::…`) so the generated code compiles inside any consumer crate regardless of what's already in scope. Readers unfamiliar with proc-macro hygiene should treat the verbosity as intentional, not a style choice to tidy up.

```rust
impl ::taskstore_traits::Record for Plan {
    fn id(&self) -> &str {
        ::std::convert::AsRef::<str>::as_ref(&self.id)
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "plans"
    }

    fn indexed_fields(&self) -> ::std::collections::HashMap<::std::string::String, ::taskstore_traits::IndexValue> {
        let mut m = ::std::collections::HashMap::new();
        m.insert(
            "status".to_string(),
            ::taskstore_traits::IndexValue::String(::std::string::ToString::to_string(&self.status)),
        );
        m.insert(
            "tier".to_string(),
            ::taskstore_traits::IndexValue::String(::std::string::ToString::to_string(&self.tier)),
        );
        m
    }
}
```

**Mini-grammar for the `#[record(...)]` helper attribute:**

```
record_struct_attr := "#[record(" struct_arg ("," struct_arg)* ","? ")]"

struct_arg         := "collection" "=" string_literal

record_field_attr  := "#[record(" field_arg ")]"

field_arg          := "indexed"
```

Argument vocabulary is intentionally tiny. If a key that isn't `collection` appears on the struct, or any key other than `indexed` appears on a field, the macro emits a spanned `compile_error!` pointing at the offending token with an explicit "expected one of: …" message.

**IR during macro expansion:**

```rust
struct RecordIr {
    struct_ident: syn::Ident,                 // e.g. "Plan"
    collection_name: String,                  // "plans" (either derived or from collection = "...")
    indexed_fields: Vec<syn::Ident>,          // fields carrying #[record(indexed)]
}
```

The IR carries `Ident` rather than a pre-computed string for indexed fields because `emit.rs` already needs the `Ident` for the `&self.#ident` reference and can trivially call `.to_string()` when building the map key. Pre-computing would duplicate state.

Notably absent: no `id_field` or `updated_at_field` fields on the IR. Their names are hardcoded (`id`, `updated_at`), so the IR carries only what varies across record types.

### API Design

**Generated `impl` block, signatures:**

```rust
impl ::taskstore_traits::Record for #StructName {
    fn id(&self) -> &str;
    fn updated_at(&self) -> i64;
    fn collection_name() -> &'static str;
    fn indexed_fields(&self) -> ::std::collections::HashMap<::std::string::String, ::taskstore_traits::IndexValue>;
}
```

All four methods are pure data dispatch. `id` indirects through `AsRef::<str>::as_ref` so both `String` and typed-ID newtypes work. `updated_at` is a plain field read (`i64: Copy`). `collection_name` returns a `'static` string literal. `indexed_fields` constructs and returns a `HashMap<String, IndexValue>` whose size is exactly the count of `#[record(indexed)]` attributes; empty HashMap when there are none (the trait's default impl already does this, but the derive overrides unconditionally to keep the generated body shape consistent).

**Method-name collisions.** `impl Record for X` adds four methods to `X`'s trait-impl surface. If the consumer has their own `impl X { fn id(&self) -> &str {…} }`, the trait method and the inherent method coexist (rustc resolves inherent methods first for direct calls); no collision error. If the consumer writes a conflicting `impl Record for X`, rustc emits "conflicting implementations" — correct diagnosis, not the derive's concern.

**Indexed-field key naming.** The HashMap key is the Rust field name verbatim. Field names in v4 record types are snake_case (`status`, `tier`, `parent_id`, `updated_at`). Callers writing `Filter::Eq("parent_id", …)` against a `Work` expect snake_case; the derive preserves it. No kebab-case translation on the map key (unlike role-name strings in the `Fsm` derive, which ARE translated to match wire format).

### Implementation Plan

Three phases, each a single-commit milestone. (Fewer than the Fsm derive's four because there are no new runtime types to add to `domain` — `Record` and `IndexValue` already live in `taskstore-traits`.)

#### Phase 1: `crates/derive/src/record.rs` — full parse + validate + emit

**Model:** opus

- Add `src/record.rs` as module entry (2018+ style).
- Add `src/record/parse.rs` — `DeriveInput` + struct / field `#[record(...)]` attrs → `RecordIr`. Hand-rolled `Parse` impls, mirroring `fsm/parse.rs`.
- Add `src/record/validate.rs` — the compile-time checks from §Goals, one spanned `compile_error!` per failure.
- Add `src/record/emit.rs` — `quote!`-driven emission of the full `impl ::taskstore_traits::Record for StructName` block including `indexed_fields` construction (`HashMap::insert` per `#[record(indexed)]` field, each wrapping `ToString::to_string(&self.field)` in `IndexValue::String`).
- Update `src/lib.rs` with `#[proc_macro_derive(Record, attributes(record))]` entry point alongside the existing `Fsm` entry.
- Rationale for one-phase (instead of parse/validate/emit split across commits): the Fsm derive proved the three-stage pipeline works and is debuggable; for a second derive in the same crate using the same patterns, splitting provides no additional safety and creates a window where the derive callable emits nothing (a form of "half-finished implementation").
- Model = opus because generated code must thread fully-qualified paths (`::taskstore_traits::*`, `::std::collections::HashMap`, `::std::string::ToString`) correctly across any consumer crate's import state. Path-hygiene bugs in proc macros are notoriously hard to see from outside rust-analyzer.

#### Phase 2: integration tests + trybuild compile-fail

**Model:** sonnet

- `crates/derive/tests/record.rs` — multiple fixture structs:
    - `Plan`-shaped happy path (`id: String`, `updated_at: i64`, `#[record(indexed)] status: FakeStatus`, `#[record(indexed)] tier: FakeTier`, a non-indexed `goal: String`). Tests: `Plan::collection_name() == "plans"`; `record.id()` returns the right slice; `record.updated_at()` returns the right i64; `record.indexed_fields()` has exactly two entries keyed `"status"` and `"tier"`.
    - Override fixture using `#[record(collection = "plans-v2")]`.
    - Zero-indexed fixture (no `#[record(indexed)]` anywhere) produces an empty `HashMap`.
    - Typed-ID fixture with `PlanId(String)` + `impl AsRef<str>`; `record.id()` returns the inner `&str`.
- `crates/derive/tests/compile-fail/record-*.rs` — one fixture per validation check, goldens via `TRYBUILD=overwrite`:
    - `record-on-enum.rs` — non-struct rejection.
    - `record-tuple-struct.rs` — tuple-variant rejection.
    - `record-unit-struct.rs` — unit-variant rejection.
    - `record-missing-id.rs` — no `id` field.
    - `record-missing-updated-at.rs` — no `updated_at` field.
    - `record-unknown-struct-key.rs` — `#[record(collecton = …)]` typo.
    - `record-unknown-field-key.rs` — `#[record(indexe)]` typo on a field.
    - `record-generic-struct.rs` — generic structs rejected.
    - `record-multi-attr.rs` — two `#[record(...)]` attributes on the struct.
- `tests/trybuild.rs` already globs `compile-fail/*.rs`; new fixtures pick up automatically.

#### Phase 3: status flip + roadmap alignment

**Model:** sonnet

- `docs/roadmap.md` Stage 5 entry — change "new `#[derive(Record)]`" to "`#[derive(Record)]` — shipped in vX.Y.Z"; mark "Status: Implemented" similar to how the Fsm doc's entry was updated after its Phase 4.
- This doc — flip `**Status:**` from Draft to Implemented.
- No CLAUDE.md changes expected; the existing §In-scope bullet for Record already matches the shipped shape.
- Commit this alongside the bump.

## Alternatives Considered

### Alternative 1: Hand-write `impl Record` for every record

- **Description:** Follow v4. Each record has `impl Record for X { fn id(…) fn updated_at(…) fn collection_name(…) fn indexed_fields(…) }`. No derive.
- **Pros:** Zero macro surface. Debugging is trivially easy — the code in front of you is the code that runs.
- **Cons:** 11 near-identical copies in v4; v5 would accrue the same dead weight. Typos in `collection_name` are silently wrong. Every new indexed field requires remembering to update `indexed_fields()`. The boundary is exactly the shape a derive exists to collapse.
- **Why not chosen:** the 11-file v4 precedent is the empirical case for a derive. Hand-writing is the fallback if the derive's debug overhead ever outweighs the boilerplate savings, but Stage 5 is already planning 6+ record types, comfortably past the threshold.

### Alternative 2: Function-like macro `record! { Plan with collection = "plans" indexed = [status, tier] }`

- **Description:** Not a derive; a function-like macro that generates both the struct and the impl.
- **Pros:** Single declarative blob describing the record.
- **Cons:** Forbidden by `crates/derive/CLAUDE.md` (same reasoning as in the `Fsm` design). Hides the struct definition from rust-analyzer, re-introduces a DSL, exactly the failure class v5 is built to avoid.
- **Why not chosen:** hard CLAUDE.md violation. Derive + helper attribute covers every concrete need without the cost.

### Alternative 3: Attribute macro `#[record(collection = "plans")] struct Plan { … }`

- **Description:** Use `#[record]` as an attribute macro (not a derive helper) that rewrites the struct body to add the Record impl.
- **Pros:** Attribute macro can inspect and rewrite both the struct definition and its impls in one sweep.
- **Cons:** Also forbidden by `crates/derive/CLAUDE.md`. Attribute macros that rewrite items are exactly what the scope guard targets. The `#[record(...)]` helper this design uses is a *derive helper attribute* scoped to `#[derive(Record, attributes(record))]`, which is explicitly allowed — a different beast from a free-standing attribute macro.
- **Why not chosen:** same CLAUDE.md reasoning as Alternative 2.

### Alternative 4: Blanket impl via a marker trait + generic `impl`

- **Description:** Declare `trait DomainRecord { fn collection_name() -> &'static str; fn indexed_fields(&self) -> HashMap<…>; }` in `domain`; consumers impl only `DomainRecord`; a blanket `impl<T: DomainRecord> Record for T { … }` in `taskstore-traits` does the forwarding.
- **Pros:** No proc macro at all. Consumers write slightly less code.
- **Cons:** `taskstore-traits` is a generic traits crate; adding a blanket impl keyed on a domain-specific helper trait inverts the dependency (traits crate depending on domain). The blanket would live in `domain` instead, but Rust's orphan rules forbid blanket-impl'ing an external trait (`taskstore_traits::Record`) for a local marker trait. Workable only if `Record` moves out of `taskstore-traits` or becomes sealed — both worse than keeping the derive.
- **Why not chosen:** orphan-rule wall. Theoretically tidy but uninstallable without breaking the crate boundary.

### Alternative 5: Runtime reflection via `erased-serde` or `typetag`

- **Description:** Skip compile-time derivation entirely. At startup, walk all types and reflectively extract indexed fields via serde introspection.
- **Pros:** Records need no explicit indexed-field markup; they're derived from serde tags.
- **Cons:** Huge runtime dependency footprint (`typetag`, `erased-serde`). Opaque failure modes; a mistyped serde attribute surfaces as an empty indexed-fields map at the wrong moment. v5's entire thesis is that compile-time typing beats runtime reflection.
- **Why not chosen:** violates the vision's "typed pipeline" principle. Any amount of macro debt beats runtime-reflection debt at this layer.

## Technical Considerations

### Dependencies

No new `[dependencies]` added to `crates/derive/Cargo.toml`. `proc-macro2`, `syn`, `quote` are already present from the Fsm derive.

`[dev-dependencies]`: `taskstore-traits = { workspace = true }` added so the integration test in `tests/record.rs` can import `Record` + `IndexValue` for assertion. This is a dev-dep only; the derive's non-test build still has zero runtime dep edges.

### Performance

- `id()`: one `AsRef::<str>::as_ref` call, inlined to a field deref.
- `updated_at()`: one `i64` field read.
- `collection_name()`: returns a `&'static str` literal; zero cost.
- `indexed_fields()`: allocates one `HashMap` + N `String` keys + N `String` values (via `ToString::to_string`). For v4's heaviest record (`Work`, 2 indexed fields), that's ~50 bytes of heap allocation per call. Callers cache the result when it matters; the derive does not try to cache because `IndexValue` values change with `self` state and can't be `const`. Benchmark beyond first-gate if it shows up in profiles.

Expected allocation count per `indexed_fields()` call: **1 HashMap + 2N Strings for N indexed fields**. For records with zero indexed fields, it's still 1 HashMap allocation (trivially skippable by preserving the trait's default impl, but keeping generation uniform is cleaner; the `HashMap::new()` inline from a default impl is 1 allocation either way).

### Security

Proc macros run at compile time with user privilege. This derive emits token streams that are fully data-driven (no user-controlled raw-token forwarding). Surface is equivalent to other workspace proc macros.

Generated code does no I/O, no subprocess execution, no secret handling. The Record trait reads record state; persistence happens in `taskstore::Store` (separate crate).

### Testing Strategy

- **Integration tests in `crates/derive/tests/record.rs`**: multiple Plan-shaped fixtures with varying `#[record(...)]` attribute combinations; exercises each generated method against expected outputs.
- **`trybuild` compile-fail tests** in `crates/derive/tests/compile-fail/record-*.rs`: one fixture per validation path. Goldens generated via `TRYBUILD=overwrite` and checked in.
- **Unit tests in `crates/derive/src/record/` are NOT added** because proc-macro crates can't unit-test their own macros. All coverage is integration / compile-fail.
- **Coverage target:** every public-facing generated method hit at least once. Every compile-time check has a dedicated compile-fail fixture. Every `#[record(...)]` attribute option hit at least once.
- **Regression overlap with `Fsm` tests:** zero. The two derives operate on disjoint item types (struct vs. enum); a consumer carrying both is a single type-level concern and won't be tested by `derive`'s own test suite — that surfaces in the later `records.md` / `store.md` tests.

### Rollout Plan

- Three phases land as sequential commits on `v5` branch.
- No tag between phases; single patch bump on the Phase 3 commit (same pattern as Fsm shipped at v0.5.8).
- Consumers (Stage 5+ record structs) carry `#[derive(Record)]` in their own design docs' commits, not here.
- No coexistence window. v4's hand-written `impl Record` blocks never land in v5.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `id` type constraint (`AsRef<str>`) is insufficient — some typed IDs might want `Into<&str>` or `Deref<Target=str>` instead | Low | Low | rustc's error on a bad `AsRef` bound is legible. If a real case surfaces, switch generated code to `&self.id[..]` (requires `Deref<Target=str>`) or accept both via a sealed helper trait. Don't pre-solve. |
| `updated_at: i64` locks the unit to milliseconds; if the project later wants `DateTime<Utc>` or `SystemTime`, the derive breaks | Low | Medium | `i64` ms matches `taskstore-traits::Record::updated_at`'s signature. Any unit change is a trait-crate change first; the derive reacts to that, not the other way around. |
| Hand-written `impl Record for X` and `#[derive(Record)]` on the same type → duplicate impl | Low | Low | rustc's "conflicting implementations" error is the right surface. Derive doesn't pre-check. |
| An indexed field's type does not implement `Display` → `ToString::to_string(&self.field)` fails to compile | Low | Low | rustc error points at the indexed attribute / field. Clear message ("Display is not implemented for T"). Fix is on the consumer. |
| Common-case `Option<T>` fields cannot be indexed (the `Display` rule above rejects them) | Medium | Medium | See §Open Questions. Current plan: flag at macro expansion with a clearer "use a concrete type, not Option, or add `#[record(indexed_optional)]` (not yet implemented)" message if we detect a path-segment match for `Option`. Since `syn::Type` can't reliably distinguish `std::option::Option<T>` from a user-aliased `type Option<T> = ...`, the detection is best-effort. Worst case: rustc's native "Display not implemented for Option<T>" error surfaces and the consumer files the fix. |
| Default collection-name pluralization bites (`Strategy` → `"strategys"`) | Medium | Low | Document the simple rule; offer `#[record(collection = "…")]` as the standard override. Every real v4 case either fit the simple rule or already used an override. |
| Derive emits a generated `impl` with a method-name collision against the consumer's own inherent impl on the same type | Low | Low | Inherent methods win the resolution fight for direct calls; trait method still reachable via `<X as Record>::collection_name()`. No compile error from the overlap. |
| `#[record(...)]` helper attribute usage creep — someone wants nested structure like `#[record(collection = "…", audit_log = true)]` | Medium | Low | Design says exactly two keys (`collection` struct-level, `indexed` field-level). Anything else is rejected at validate. Reserve grammar space for future expansion if a use case earns it; don't pre-build. |
| `ToString::to_string(&self.field)` allocates a `String` even for fields that are already `String` | Medium | Low | That's the cost of the uniform coercion path. Callers who care about zero-allocation index extraction build a custom lookup — derive is for the common case. |

## Open Questions

- [ ] **Pluralization rule refinement.** Lowercase + `"s"` handles every v4 record except two, and v5's initial six (Plan, Work, Spec, Phase, Bundle, Tick) all fit cleanly. If we ever hit a record name ending in `s`, `x`, `z`, `ch`, or `sh` (where English plural would add `es`), the default would produce an awkward `"boxs"` or similar. Current plan: don't preempt; require the consumer to use `#[record(collection = "…")]` if the default is awkward. Flagged here for future reviewers.
- [ ] **Should `#[record(indexed)]` accept a key override?** E.g., `#[record(indexed(key = "status_display"))]` to let the HashMap key diverge from the Rust field name. v4 always aligned them; v5 has no call for divergence yet. If a use case shows up (serde rename forcing a wire name different from the Rust field name?), extend the grammar.
- [ ] **Should the derive also emit `const COLLECTION_NAME: &str`?** A free-standing `const` lets callers write `taskstore.get::<Plan>(&Plan::COLLECTION_NAME, id)` instead of `Plan::collection_name()`. Marginal benefit; easy to add later. Not in v1.
- [ ] **Compile-time check for `i64` on `updated_at`?** Currently delegated to rustc's return-type mismatch on `fn updated_at(&self) -> i64`. Could add an explicit syn-level type-ident check for earlier / friendlier error. Deferred; rustc's message is already adequate.
- [ ] **Typed-ID ecosystem coordination with `records.md`.** This doc says "id: PlanId works if PlanId: AsRef<str>". The `records.md` design (not yet written) must land the actual `PlanId` / `WorkId` newtype structs with those impls. Noted here so `records.md` doesn't forget.
- [ ] **Serde-rename interaction with indexed-field keys.** `indexed_fields()` keys are the Rust field name verbatim. If the consumer later adds `#[serde(rename = "statusCode")]` or `#[serde(rename_all = "camelCase")]`, the JSONL file on disk and the `indexed_fields` map key will diverge (JSON: `"statusCode"`; index key: `"status_code"`). Callers filtering via `Filter::Eq("status_code", …)` get what they expect; callers who assume the index key matches the wire name get surprised. Current plan: document that indexed keys are Rust names, not wire names. If this bites, add `#[record(indexed(key = "…"))]` to override the map key independently of serde.
- [ ] **`Option<T>` fields marked `#[record(indexed)]`.** The generated code calls `ToString::to_string(&self.field)`. `Option<T>` does not implement `Display`, so indexing an `Option<String>` field produces a rustc error at expansion-downstream. v4 never hit this (`Work::parent_id` is `String`, not `Option<String>`). If v5 wants optional indexed fields (e.g., a `Work` with no parent for standalone tasks), we need a path: either (a) document that indexed fields must be non-optional, (b) introduce a second attribute `#[record(indexed_optional)]` that emits `if let Some(v) = &self.field { m.insert(…) }`, or (c) detect `Option<T>` at the `syn::Type` level and handle it automatically (fragile; syn can see the path `Option` but not its type alias). Defer until Stage 5+ shows a concrete need.
- [ ] **Indexing the `id` or `updated_at` field.** Nothing in the design forbids `#[record(indexed)] pub id: String`, which produces `m.insert("id", IndexValue::String(self.id.to_string()))` alongside the `id()` method. Semantically reasonable (some stores index by primary key for redundant fast lookup). Not restricted; flagged so future readers understand it's an intentional non-restriction, not an oversight.

## References

- [`docs/vision.md`](../../../../docs/vision.md) §domain: records layer and FSM tables.
- [`docs/roadmap.md`](../../../../docs/roadmap.md) Stage 5: lists this doc as the second of four Stage 5 design docs.
- [`docs/design/2026-04-20-fsm-macro.md`](../../../../docs/design/2026-04-20-fsm-macro.md): sibling derive; informs the parse / validate / emit pipeline shape used here.
- [`crates/derive/CLAUDE.md`](../../CLAUDE.md) §In-scope: `#[derive(Record)]` explicitly named as one of the two supported derives; §Out-of-scope: no function-like / attribute macros.
- [`crates/derive/docs/CLAUDE.md`](../CLAUDE.md) §What lives here: single-crate designs go in `crates/derive/docs/design/`. This doc qualifies (no types added to `domain`; all generated references resolve against the existing `taskstore_traits` dep).
- `taskstore-traits` crate (`scottidler/taskstore`, branch `main`, workspace dep): source for the `Record` trait signature and `IndexValue` shape.
- v4 reference: `~/repos/scottidler/loopr-v4/src/domain/{plan,work,tick,…}.rs` — 11 hand-written `impl Record for X` blocks. Same shape each time; target to collapse.
