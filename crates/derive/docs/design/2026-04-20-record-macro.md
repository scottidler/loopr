# Design Document: `#[derive(Record)]` Proc Macro

**Author:** Scott A. Idler
**Date:** 2026-04-20
**Status:** Implemented (shipped in v0.5.10)
**Review Passes Completed:** 5/5 + Architect

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
- **`updated_at` field requirement**: a field literally named `updated_at`, of type `i64`. The generated `fn updated_at(&self) -> i64` emits `self.updated_at`. Unlike the `id` field (whose type varies — `String`, typed IDs, etc.), `updated_at` has exactly one valid shape, and a wrong shape is always a consumer mistake rather than a new use case. The validator therefore performs a `syn::Type` path-segment check at expansion: the type must be `i64` (or a path ending in `i64`). A spanned `compile_error!` fires at the field declaration with "expected `i64` for `updated_at`" — higher-signal than rustc's generated-body return-type mismatch.
- **Indexed fields via `#[record(indexed)]` or `#[record(indexed(key = "…"))]` on field declarations**. The derive collects every field carrying either form, emits one `.insert(…)` per field into the returned `HashMap<String, IndexValue>`:
    - Map key: by default the Rust field name verbatim (e.g., `status`, `parent_id`). The `indexed(key = "…")` form overrides this with an explicit string literal; use when a consumer's field name diverges from the caller-facing index name (for example, after a `#[serde(rename_all = "camelCase")]` makes the JSON wire key camelCase while the Rust field stays snake_case — the `indexed(key = …)` override lets the index map align with the wire form). Without the override, the map key is always the Rust field name; callers writing `Filter::Eq("…", …)` must use that convention.
    - Map value: `::taskstore_traits::IndexValue::String(::std::string::ToString::to_string(&self.field))`. Every indexed field in v4 is coerced through `.to_string()` (via strum's kebab-case `Display` for enums and `String::clone()` for strings); using `ToString::to_string` unconditionally handles both and gives a single codegen path. Numeric and boolean indexing are not needed at first-gate — left as future attribute options (see §Non-Goals).
    - **`Option<T>` handling:** when the macro sees a field whose type is a `syn::Type::Path` ending in `Option` (best-effort detection; aliased `type Option<T> = MyOption<T>` would defeat it, which is the consumer's problem), it emits a conditional `if let Some(v) = &self.field { m.insert(key, IndexValue::String(v.to_string())); }`. `None` values are simply omitted from the map, matching the "absent means unset" semantic that filter queries naturally support. Consumers can index `parent_id: Option<PlanId>` directly — the common Stage 6+ case of orphan Works with no parent is covered without a separate attribute form.
- **Struct-only.** The derive rejects enums and unions at expansion with a spanned `compile_error!`. `taskstore_traits::Record` requires `Serialize + Deserialize + Clone + Send + Sync + 'static` — trivially satisfiable for a struct of owned serde types and impossible to meaningfully derive for an enum without tagging choices that belong to the consumer.
- **Unit structs and tuple structs are rejected**. Named-field structs only. A record with no id / no updated_at has no business being persistable; the attribute-driven indexed-field mechanism requires named fields to address.
- **No runtime dependency added to consumers** beyond what they already have. Consumers already depend on `taskstore-traits` (via `domain`'s Cargo.toml). The derive crate itself does not depend on `taskstore-traits`; generated code resolves `::taskstore_traits::Record` at the consumer's compile time, same pattern as `Fsm`'s `::domain::FsmError<S>`.
- **Compile-time validation** during expansion emits spanned `compile_error!` for the following:
    - Attached item is not a struct (enum, union, fn, etc.).
    - Struct is tuple-style or unit-style (not named-field).
    - Struct is generic. Stage 5+ records are all non-generic; earn it later.
    - No field literally named `id`.
    - No field literally named `updated_at`.
    - `updated_at` field's type path does not end in `i64` (per-field syn-level check; higher-signal than rustc's generated-body error).
    - `#[record(...)]` appears more than once on the struct itself (merge into a single attribute, same rule as `Fsm`).
    - `#[record(collection = …)]` value is not a string literal, or is empty.
    - Two `#[record(indexed)]` attributes on the same field (duplicate; trivially detectable).
    - `#[record(indexed(key = …))]` has an empty or non-string-literal value, or produces a map key that collides with another indexed field's key (duplicate keys would cause silent last-writer-wins in the HashMap).
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

### Earned-When-Needed Features

Deliberately NOT in v1, but pre-cleared as non-breaking to add when a real call site demands them. Listed here so the design carries an explicit signal that these were considered and rejected for now on cost/benefit, rather than overlooked.

- **Pluralization smarts beyond lowercase + `"s"`.** Handles every v5 record name in Stages 5–8 (Plan → `"plans"`, Work → `"works"`, Spec → `"specs"`, Phase → `"phases"`, Bundle → `"bundles"`, Tick → `"ticks"`) and 9/11 v4 records. The two v4 outliers (`Coverage`, `Validation`) used explicit `#[record(collection = "…")]` overrides, which remain the escape valve. If v5 ever grows a record whose name ends in `s`, `x`, `z`, `ch`, or `sh`, the default will produce something awkward (`"boxs"`, `"strategys"`) that surfaces loudly on first test run; the override fixes it in one line without touching the derive.
- **Numeric / boolean `IndexValue` variants.** The taskstore SQLite schema already has dedicated `field_value_int` and `field_value_bool` columns alongside `field_value_str` — v1 of the derive simply emits `IndexValue::String` for every indexed field. When a future record wants numeric range filtering (e.g., `retry_count > 3`, `updated_at` between two timestamps), add `#[record(indexed_int)]` / `#[record(indexed_bool)]` forms that dispatch to the matching column. Not speculative; the engine-side capability exists, the derive just hasn't exposed it yet.
- **`const COLLECTION_NAME: &str` alongside the trait method.** A free-standing `const` would let callers write `taskstore.get::<Plan>(Plan::COLLECTION_NAME, id)` without the function call syntax. Marginal ergonomic gain; trivially additive.

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

The macro checks field *presence* at expansion time (spanned `compile_error!` if `id` or `updated_at` is missing). For `updated_at`, it also performs a `syn::Type` path-segment check to enforce `i64` directly. For `id`, it does not inspect the type — rustc's native error on the generated body (`trait bound AsRef<str> not satisfied`) fires at the consumer with the correct span.

**Intentionally not restricted:** nothing prevents a consumer from attaching `#[record(indexed)]` to the `id` or `updated_at` field. That produces an extra entry in the `indexed_fields()` map keyed on `"id"` / `"updated_at"` alongside the `id()` / `updated_at()` methods. Semantically reasonable (some stores index the primary key for redundant fast lookup); flagged so readers don't mistake the permissiveness for an oversight.

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

    #[record(indexed(key = "parentId"))] // override: map key "parentId" (wire form), field name stays parent_id
    pub parent_id: Option<PlanId>,       // Option is detected; None omits the entry

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
        // Option<PlanId> — detected via syn::Type::Path ending in Option; conditional insert
        if let ::std::option::Option::Some(ref v) = self.parent_id {
            m.insert(
                "parentId".to_string(),   // key override from #[record(indexed(key = "parentId"))]
                ::taskstore_traits::IndexValue::String(::std::string::ToString::to_string(v)),
            );
        }
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
                    | "indexed" "(" "key" "=" string_literal ")"
```

Argument vocabulary is intentionally tiny. If a key that isn't `collection` appears on the struct, or any key other than `indexed` / `indexed(key = …)` appears on a field, the macro emits a spanned `compile_error!` pointing at the offending token with an explicit "expected one of: …" message.

**IR during macro expansion:**

```rust
struct RecordIr {
    struct_ident: syn::Ident,                 // e.g. "Plan"
    collection_name: String,                  // "plans" (derived or from collection = "...")
    indexed_fields: Vec<IndexedFieldIr>,
}

struct IndexedFieldIr {
    field_ident: syn::Ident,                  // the Rust field ident, for `&self.#ident`
    map_key: String,                          // map key in indexed_fields() HashMap
                                              //   — defaults to field_ident.to_string()
                                              //   — overridden by #[record(indexed(key = "..."))]
    is_optional: bool,                        // true when the field's type is syn-detected as
                                              // `Option<T>` (emit wraps insert in `if let Some`)
}
```

Notably absent: no `id_field` or `updated_at_field` fields on the IR. Their names are hardcoded (`id`, `updated_at`), so the IR carries only what varies across record types.

Option-detection rule for `is_optional`: the macro walks `syn::Type::Path` and marks the field optional if the last path segment's ident equals `"Option"` and it has exactly one angle-bracketed generic argument. This catches `Option<T>`, `std::option::Option<T>`, and `core::option::Option<T>` but misses user-level type aliases (`type MaybeId = Option<String>`). When the detection is wrong, the generated code either fails at rustc with a legible error (`Display not implemented for MaybeId`) or the consumer writes `Option<...>` directly. Consumers who alias `Option` are vanishingly rare; the fragility is acceptable.

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
- Add `src/record/parse.rs` — `DeriveInput` + struct / field `#[record(...)]` attrs → `RecordIr`. Hand-rolled `Parse` impls, mirroring `fsm/parse.rs`. Parses `indexed` and `indexed(key = "…")` into `IndexedFieldIr`. Option detection walks the field's `syn::Type::Path`.
- Add `src/record/validate.rs` — the compile-time checks from §Goals, one spanned `compile_error!` per failure. Includes the `updated_at: i64` type-path check and the duplicate-map-key check across `IndexedFieldIr.map_key` values.
- Add `src/record/emit.rs` — `quote!`-driven emission of the full `impl ::taskstore_traits::Record for StructName` block including `indexed_fields` construction. Each `IndexedFieldIr` produces either an unconditional `HashMap::insert(...)` or (when `is_optional`) an `if let Some(ref v) = self.field { m.insert(...) }` guard. Map keys come from `IndexedFieldIr.map_key` so the `indexed(key = "…")` override threads through correctly.
- Update `src/lib.rs` with `#[proc_macro_derive(Record, attributes(record))]` entry point alongside the existing `Fsm` entry.
- Rationale for one-phase (instead of parse/validate/emit split across commits): the Fsm derive proved the three-stage pipeline works and is debuggable; for a second derive in the same crate using the same patterns, splitting provides no additional safety and creates a window where the derive callable emits nothing (a form of "half-finished implementation").
- Model = opus because generated code must thread fully-qualified paths (`::taskstore_traits::*`, `::std::collections::HashMap`, `::std::string::ToString`, `::std::option::Option`) correctly across any consumer crate's import state. The Option-conditional codegen adds one more dimension of expansion-time correctness; path-hygiene bugs in proc macros are notoriously hard to see from outside rust-analyzer.

#### Phase 2: integration tests + trybuild compile-fail

**Model:** sonnet

- `crates/derive/tests/record.rs` — multiple fixture structs:
    - `Plan`-shaped happy path (`id: String`, `updated_at: i64`, `#[record(indexed)] status: FakeStatus`, `#[record(indexed)] tier: FakeTier`, a non-indexed `goal: String`). Tests: `Plan::collection_name() == "plans"`; `record.id()` returns the right slice; `record.updated_at()` returns the right i64; `record.indexed_fields()` has exactly two entries keyed `"status"` and `"tier"`.
    - Override fixture using `#[record(collection = "plans-v2")]`.
    - Zero-indexed fixture (no `#[record(indexed)]` anywhere) produces an empty `HashMap`.
    - Typed-ID fixture with `PlanId(String)` + `impl AsRef<str>`; `record.id()` returns the inner `&str`.
    - `Option<T>` fixture with `#[record(indexed)] parent_id: Option<String>`; `None` produces a map missing the key; `Some("x")` produces an entry `"parent_id" -> IndexValue::String("x")`.
    - Key-override fixture with `#[record(indexed(key = "parentId"))] parent_id: Option<String>`; verifies the map key is `"parentId"`, not `"parent_id"`.
- `crates/derive/tests/compile-fail/record-*.rs` — one fixture per validation check, goldens via `TRYBUILD=overwrite`:
    - `record-on-enum.rs` — non-struct rejection.
    - `record-tuple-struct.rs` — tuple-variant rejection.
    - `record-unit-struct.rs` — unit-variant rejection.
    - `record-missing-id.rs` — no `id` field.
    - `record-missing-updated-at.rs` — no `updated_at` field.
    - `record-wrong-updated-at-type.rs` — `updated_at: u64` or `updated_at: String` (validator catches the type mismatch at the field ident, not in the generated body).
    - `record-unknown-struct-key.rs` — `#[record(collecton = …)]` typo.
    - `record-unknown-field-key.rs` — `#[record(indexe)]` typo on a field.
    - `record-duplicate-map-key.rs` — two fields with `#[record(indexed(key = "same"))]`.
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
| `Option<T>` detection via `syn::Type::Path` segment match is defeated by user-level type aliases (`type Foo = Option<String>`) | Low | Low | Detection is best-effort; aliased Option produces either a rustc "Display not implemented for Foo" error (if the alias points at a user newtype) or a correct conditional insert (if rustc can see through the alias to `Option<T>`, which it usually can't at macro-expansion time). Consumers who need detection for an alias either inline `Option<T>` directly or write a manual `impl Record`. No known real case. |
| Default collection-name pluralization bites (`Strategy` → `"strategys"`) | Medium | Low | Document the simple rule; offer `#[record(collection = "…")]` as the standard override. Every real v4 case either fit the simple rule or already used an override. |
| Derive emits a generated `impl` with a method-name collision against the consumer's own inherent impl on the same type | Low | Low | Inherent methods win the resolution fight for direct calls; trait method still reachable via `<X as Record>::collection_name()`. No compile error from the overlap. |
| `#[record(...)]` helper attribute usage creep — someone wants nested structure like `#[record(collection = "…", audit_log = true)]` | Medium | Low | Design says exactly two keys (`collection` struct-level, `indexed` field-level). Anything else is rejected at validate. Reserve grammar space for future expansion if a use case earns it; don't pre-build. |
| `ToString::to_string(&self.field)` allocates a `String` even for fields that are already `String` | Medium | Low | That's the cost of the uniform coercion path. Callers who care about zero-allocation index extraction build a custom lookup — derive is for the common case. |

## References

- [`docs/vision.md`](../../../../docs/vision.md) §domain: records layer and FSM tables.
- [`docs/roadmap.md`](../../../../docs/roadmap.md) Stage 5: lists this doc as the second of four Stage 5 design docs.
- [`docs/design/2026-04-20-fsm-macro.md`](../../../../docs/design/2026-04-20-fsm-macro.md): sibling derive; informs the parse / validate / emit pipeline shape used here.
- [`crates/derive/CLAUDE.md`](../../CLAUDE.md) §In-scope: `#[derive(Record)]` explicitly named as one of the two supported derives; §Out-of-scope: no function-like / attribute macros.
- [`crates/derive/docs/CLAUDE.md`](../CLAUDE.md) §What lives here: single-crate designs go in `crates/derive/docs/design/`. This doc qualifies (no types added to `domain`; all generated references resolve against the existing `taskstore_traits` dep).
- `taskstore-traits` crate (`scottidler/taskstore`, branch `main`, workspace dep): source for the `Record` trait signature and `IndexValue` shape.
- v4 reference: `~/repos/scottidler/loopr-v4/src/domain/{plan,work,tick,…}.rs` — 11 hand-written `impl Record for X` blocks. Same shape each time; target to collapse.
- Cross-doc coordination: the (unwritten) `crates/domain/docs/design/records.md` owes `impl AsRef<str>` for the typed-ID newtypes it defines (`PlanId`, `WorkId`, etc.) so they satisfy the `id` field contract from §Data Model.

---

## Addendum: Architect Review (pre-implementation)

After converging at 5/5 Rule-of-Five passes, this doc went to an Architect consultation (Gemini via `~/.claude/skills/architect/script.sh`). Three of the Architect's findings were accepted and are now folded into the design above; two were pushed back on or deferred with explicit rationale. Logged here so future readers can see what was tried and why.

### Accepted and incorporated

1. **`Option<T>` as first-class indexed-field behavior (was: Non-Goal / Open Question).**
   - Architect critique: "The moment Stage 6 introduces a `Work` item with an optional `assigned_to` or `parent_id`, this macro fails entirely because `Option<String>` does not implement `Display`."
   - Accepted. The design no longer defers Option handling. The macro now walks `syn::Type::Path`, detects `Option<T>` as a best-effort path-segment match, and emits conditional `if let Some(ref v) = &self.field { m.insert(…) }` code. `None` values produce no map entry; `Some(v)` indexes the inner value. Open Question retired; non-alias consumers are covered. Documented in §Goals and §Data Model.

2. **`#[record(indexed(key = "…"))]` override promoted from Open Question to first-class grammar.**
   - Architect critique: "You are institutionalizing a split-brain schema where the derive macro and `serde` operate in uncoordinated silos." (referring to the drift between Rust field names and serde-rename'd wire names).
   - Accepted. Grammar extended in §Data Model; the `field_arg` rule now carries two forms, `indexed` and `indexed(key = "…")`. The input example in §Data Model shows the override in use. Open Question retired.

3. **`updated_at: i64` type check in `validate.rs` (was: deferred to rustc).**
   - Architect critique: "A simple `syn::Type` check during the validate phase would emit a much higher-signal error directly at the struct field definition."
   - Accepted. §Goals validation list now carries the check explicitly; §Implementation Plan Phase 1 scope includes it. A new compile-fail fixture `record-wrong-updated-at-type.rs` pins the behavior. Open Question retired.

### Pushed back on

1. **Allocation floor from `ToString::to_string` coercion.**
   - Architect framing: "the `Record` trait shape combined with brute-force `ToString` scales poorly … during mass operations (e.g., rebuilding indices on a node or syncing a large chunk of records), generating this map for thousands of `Work` records will repeatedly trash the allocator."
   - Pushback: verified against `scottidler/taskstore@64036fa` — `indexed_fields()` is called in exactly two places in the engine (`store.rs:275` inside `create_many`'s validation+prep phase, and `store.rs:735` inside `rebuild_indexes<T>` which is an explicit slow-path recovery operation). Never called during queries. At write time the HashMap allocation is dominated by `serde_json::to_string` + JSONL I/O + SQLite transaction commit; the map is not the bottleneck. Pre-optimizing the allocation path would require either a `&[u8]`-typed IndexValue surface (not what taskstore-traits exposes) or a zero-alloc intermediate representation (premature). Re-examine only if a benchmark surfaces it as a real cost, not based on a priori framing. Design stays with `ToString::to_string`.

### Deferred with explicit rationale

1. **Pluralization smarts for collection names.**
   - Architect framing: "`Status` → `'statuss'`, `Strategy` → `'strategys'` … developers forgetting to add `#[record(collection = "x")]` codifies typos into persistent storage paths."
   - Deferred. The fallback rule (lowercase struct ident + `"s"`) matches all six Stage 5 record names (Plan, Work, Spec, Phase, Bundle, Tick) and 9/11 v4 record names. The two v4 outliers (`Coverage`, `Validation`) used explicit overrides. Awkward defaults surface loudly on first test run — the file name in `.taskstore/` tells you immediately. The override attribute is discoverable and cheap. No v5 record in Stages 5–8 trips the edge case; no reason to add English-plural rules to the macro now. Flagged as an Open Question for future review.

2. **Typed-ID traits: `AsRef<str>` vs. `Deref<Target=str>`.**
   - Architect framing: "If a consumer uses a standard derive macro that provides `Deref<Target=str>` or `Into<String>` but not `AsRef`, the generated code fails opaquely."
   - Deferred. Standard newtype-ID ecosystems (`nutype`, `derive_more`, hand-rolled) emit `AsRef<str>` first because that's the common consumer of typed IDs. A consumer who lands on `Deref`-only gets a clear rustc error pointing at the generated `self.id.as_ref()` call; adding `impl AsRef<str>` is a one-liner. Not worth threading a more permissive trait surface (e.g., a sealed helper trait accepting both `AsRef` and `Deref`) at v1. Re-examine if a real consumer actually arrives with `Deref`-only.

### Net design diff vs. pre-review 5/5 draft

- §Goals: +1 validation bullet (`updated_at` type check), extended `indexed` bullet (grammar + Option handling).
- §Non-Goals: gained a §Earned-When-Needed Features subsection listing pluralization smarts, numeric/bool `IndexValue` variants, and `const COLLECTION_NAME` as non-breaking future additions. The numeric/bool entry was reframed from deferred speculation into a real earned feature (the taskstore schema already supports it at the engine layer).
- §Data Model: grammar gained `indexed(key = "…")`; input example gained an `Option<PlanId>` field using both the override and the Option path; generated output shows the conditional `if let Some` arm; IR grew an `IndexedFieldIr` struct; a non-restriction note added for indexing the `id` / `updated_at` fields.
- §Implementation Plan Phase 1: scope expanded to cover Option detection, key-override parsing, and the `updated_at` type check.
- §Risks: the "Option<T> fields cannot be indexed" row replaced with the narrower "syn detection defeated by type aliases" risk.
- §Open Questions: section removed entirely. The three Architect-surfaced questions (Option handling, serde-rename drift, indexed key override) are now settled behavior; the remaining items were misfiled — two (pluralization, numeric/bool variants, `const COLLECTION_NAME`) belonged in §Non-Goals as earned features; one (typed-ID coordination) belonged in §References as a cross-doc footnote; one (indexing id / updated_at) belonged in §Data Model as a non-restriction note. Prior commit had them as Open Questions because the Rule-of-Five template has the section, not because they were actually blocking questions.
- §Addendum: this section, capturing the review's audit trail.

### Architect items not addressed

None. Every Architect finding has either a corresponding design change or an explicit rationale above.
