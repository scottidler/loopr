# Design Document: Case-Insensitive Status Enum Parsing

**Author:** Scott A. Idler
**Date:** 2026-03-31
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Eliminate the recurring class of bugs where LLM agents send status values in the wrong case (e.g., `"ready"` vs `"Ready"`) by creating a derive macro that makes all status enums case-insensitive at the deserialization boundary. Status enum variants become the single source of truth - defined once in Rust, interpolated into prompts, parsed forgivingly from LLM output.

## Problem Statement

### Background

Loopr's agent architecture has LLMs producing JSON actions with status values (e.g., `"target_status": "ready"`). These strings are deserialized into Rust enums via `serde_json::from_value()`. The enums use default serde (PascalCase), but prompts document lowercase values, and LLMs produce whatever casing they feel like.

### Problem

The same bug keeps happening: an LLM sends `"ready"` but `WorkStatus` expects `"Ready"`. The handler returns `"invalid target_status"`, the coordinator retries identically, hits the lifeguard circuit breaker, fails, gets restarted by the supervisor, and loops. This has caused multiple E2E timeouts and has been identified as a problem at least twice before without being fixed at the root.

The root cause: status string values are defined in multiple places (Rust enums, .pmt prompt files, executor format strings) with no mechanism to keep them in sync.

### Goals

- Case-insensitive deserialization for all status enums at the handler boundary
- A single `#[derive(FlexibleEnum)]` macro that adds this behavior to any enum
- Prompts interpolate valid values from the enum itself - no hardcoded strings
- Eliminate this entire class of bug permanently

### Non-Goals

- Changing the serialization format (output remains PascalCase or whatever Display produces)
- Adding a template engine (simple `str::replace` with generated values is sufficient)
- Changing how AgentAction stores target_status (remains `String` - the fix is at parse time)

## Proposed Solution

### Overview

Three layers:

1. **Derive macro** (`FlexibleEnum`) - generates case-insensitive `FromStr` and `VARIANT_NAMES` const
2. **Handler update** - swap `serde_json::from_value` to use `FromStr` via a generic helper
3. **Prompt interpolation** - inject `VARIANT_NAMES` into .pmt templates, eliminating hardcoded strings

### Layer 1: The `FlexibleEnum` Derive Macro

Create a proc-macro crate `loopr-derive/` as a path dependency. The macro generates:

```rust
// Input:
#[derive(FlexibleEnum)]
pub enum WorkStatus {
    Draft,
    Ready,
    InProgress,
    Blocked,
    InReview,
    Integrated,
    Done,
    Abandoned,
}

// Generated:
impl std::str::FromStr for WorkStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains('_') || s.contains('-') {
            return Err(format!(
                "invalid WorkStatus: '{}' - underscores and hyphens are not allowed (valid: Draft, Ready, InProgress, Blocked, InReview, Integrated, Done, Abandoned)",
                s
            ));
        }
        let normalized = s.to_lowercase();
        match normalized.as_str() {
            "draft" => Ok(Self::Draft),
            "ready" => Ok(Self::Ready),
            "inprogress" => Ok(Self::InProgress),
            "blocked" => Ok(Self::Blocked),
            "inreview" => Ok(Self::InReview),
            "integrated" => Ok(Self::Integrated),
            "done" => Ok(Self::Done),
            "abandoned" => Ok(Self::Abandoned),
            _ => Err(format!(
                "invalid WorkStatus: '{}' (valid: Draft, Ready, InProgress, Blocked, InReview, Integrated, Done, Abandoned)",
                s
            )),
        }
    }
}

impl WorkStatus {
    /// All variant names as they should appear in prompts/docs
    pub const VARIANT_NAMES: &'static [&'static str] = &[
        "Draft", "Ready", "InProgress", "Blocked",
        "InReview", "Integrated", "Done", "Abandoned",
    ];
}
```

The normalization strategy: lowercase only, with separator rejection. Inputs containing underscores or hyphens are rejected upfront with a descriptive error listing valid values - this teaches the LLM the correct format rather than silently accepting bad input. After the separator check, `s.to_lowercase()` handles pure case differences: `"ready"`, `"Ready"`, `"READY"` all match. `"InProgress"` and `"inprogress"` both match. `"in_progress"` and `"in-progress"` are rejected with guidance.

### Layer 2: Handler Update

Replace the repeated pattern in all 6 handlers:

```rust
// Before (every handler):
let target_status: WorkStatus = match req.params.get("target_status") {
    Some(v) => match serde_json::from_value(v.clone()) {
        Ok(s) => s,
        Err(_) => {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("invalid target_status"),
            ));
        }
    },
    None => {
        return Ok(DaemonResponse::err(
            req.id,
            RpcError::invalid_params("target_status is required"),
        ));
    }
};

// After:
let target_status = parse_status_param::<WorkStatus>(&req, "target_status")?;
```

Where `parse_status_param` is a generic helper in `src/daemon/handlers/mod.rs`:

```rust
fn parse_status_param<T: std::str::FromStr<Err = String>>(
    req: &DaemonRequest,
    param: &str,
) -> Result<T, DaemonResponse> {
    match req.params.get(param).and_then(|v| v.as_str()) {
        Some(s) => s.parse::<T>().map_err(|e| {
            DaemonResponse::err(req.id, RpcError::invalid_params(&e))
        }),
        None => Err(DaemonResponse::err(
            req.id,
            RpcError::invalid_params(&format!("{} is required", param)),
        )),
    }
}
```

### Layer 3: Prompt Interpolation

Update .pmt files to use `{work_status_values}` placeholders. At prompt load time, replace with the canonical list from `WorkStatus::VARIANT_NAMES`.

```
// coordinator.pmt (before):
16. `override_work`   {"action": "override_work", "work_id": "...", "target_status": "ready|abandoned", "reason": "..."}

// coordinator.pmt (after):
16. `override_work`   {"action": "override_work", "work_id": "...", "target_status": "<WorkStatus>", "reason": "..."}
    Valid target_status values: {work_override_statuses}
```

In `src/prompts.rs`, after loading the template:

```rust
let content = load(filename, default)
    .replace("{work_status_values}", &WorkStatus::VARIANT_NAMES.join(", "))
    .replace("{bundle_status_values}", &BundleStatus::VARIANT_NAMES.join(", "))
    .replace("{work_override_statuses}", "Ready, Abandoned, InReview");
```

The override statuses are a subset - they come from the transition table, not the full enum. This can be a const on the coordinator module or derived from `work_override_transitions()`.

### Affected Enums

| Enum | File | Current Serde | Needs Macro |
|------|------|--------------|-------------|
| `WorkStatus` | `src/domain/work.rs` | PascalCase (default) | Yes |
| `BundleStatus` | `src/domain/bundle.rs` | PascalCase (default) | Yes |
| `HierarchyStatus` | `src/domain/plan.rs` | lowercase + aliases | Yes (replace aliases with macro) |
| `TickStatus` | `src/domain/tick.rs` | PascalCase (default) | Yes |
| `LockStatus` | `src/domain/lock.rs` | lowercase | Yes |
| `Role` | `src/domain/role.rs` | lowercase | Yes |

### Affected Handlers

| Handler | File |
|---------|------|
| `handle_work_transition` | `src/daemon/handlers/work.rs` |
| `handle_bundle_transition` | `src/daemon/handlers/bundle.rs` |
| `handle_plan_transition` | `src/daemon/handlers/plan.rs` |
| `handle_spec_transition` | `src/daemon/handlers/spec.rs` |
| `handle_phase_transition` | `src/daemon/handlers/phase.rs` |
| `handle_tick_transition` | `src/daemon/handlers/tick.rs` |

### Implementation Plan

**Phase 1: Derive macro crate**
- Create `loopr-derive/` proc-macro crate as a path dependency (not a workspace - loopr is a single crate)
- Implement `FlexibleEnum` derive: generates `FromStr` (case-insensitive) and `VARIANT_NAMES` const
- No `serde_json` dependency in generated code - handlers use `FromStr` via the generic `parse_status_param` helper
- Do NOT override serde `Deserialize` - keep existing serde for TaskStore compatibility. The macro only adds `FromStr` for the LLM boundary.
- Tests: case variations, rejected underscores/hyphens, invalid input, error messages with valid values listed

**Phase 2: Apply to all enums**
- Add `#[derive(FlexibleEnum)]` to all 6 status enums + Role
- Remove manual `#[serde(alias)]` from HierarchyStatus (macro handles it)
- Keep existing `#[serde(rename_all)]` for serialization output format
- Verify existing tests still pass

**Phase 3: Update handlers**
- Add `parse_status_param` helper to `src/daemon/handlers/mod.rs`
- Replace all 6 handlers' deserialization blocks with the helper
- Also update Role parsing anywhere it uses the same pattern

**Phase 4: Prompt interpolation**
- Add `{status_values}` placeholders to .pmt files
- Update `prompts::init()` to replace placeholders with `VARIANT_NAMES`
- Update prompt content tests to verify interpolation

## Alternatives Considered

### Alternative 1: `#[serde(rename_all = "snake_case")]` + aliases on every variant
- **Description:** Add `#[serde(alias = "...")]` for every casing variant manually
- **Pros:** No proc-macro crate needed
- **Cons:** Verbose, error-prone, doesn't handle arbitrary casing, must be maintained per-variant
- **Why not chosen:** Doesn't actually solve the problem - just shifts the hardcoding

### Alternative 2: Normalize strings in executor before sending to handler
- **Description:** Add a `normalize_status()` function that converts common formats to PascalCase
- **Pros:** Simple, no new crates
- **Cons:** Still relies on knowing the mapping, doesn't help if new enums are added, doesn't generate prompt values
- **Why not chosen:** Band-aid, not a fix. The derive macro solves it structurally.

### Alternative 3: Use `strum` crate
- **Description:** Use `strum::EnumString` with `serialize_all = "lowercase"` and `ascii_case_insensitive`
- **Pros:** Battle-tested, no custom proc-macro
- **Cons:** `ascii_case_insensitive` only handles pure case differences, not `in_progress` vs `InProgress` (underscore vs PascalCase). Would still need custom handling for compound words.
- **Why not chosen:** Close but doesn't handle the underscore/hyphen normalization we need. Could revisit if strum adds this feature.

## Technical Considerations

### Dependencies

- `syn`, `quote`, `proc-macro2` for the derive macro (standard proc-macro deps)
- No new runtime dependencies

### Performance

- `FromStr` does two `contains()` checks + one `to_lowercase()` + one match - negligible
- `VARIANT_NAMES` is a static slice - zero cost

### Testing Strategy

- Macro tests: every variant of every enum with lowercase, PascalCase, UPPERCASE
- Handler integration tests: verify `parse_status_param` returns correct enum for various casings
- Prompt tests: verify no `{..._values}` placeholders remain after interpolation
- E2E: re-run python-todo - the override_work failure should be gone

### Rollout Plan

1. Create `loopr-derive` crate, implement and test macro
2. Apply macro to all enums, verify existing tests pass
3. Update handlers to use `parse_status_param`
4. Update prompts with interpolation
5. Run E2E

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Proc-macro compile time increase | Low | Low | It's a tiny macro, adds seconds at most |
| Existing serde serialization format changes | Medium | High | Keep `Serialize` derive unchanged - macro only affects `FromStr` and adds methods |
| Prompt placeholder not replaced | Low | Medium | Test in `prompts.rs` that no `{..._values}` patterns remain |
| LLM sends unexpected separator format (e.g. `in_progress`) | Low | Low | Macro rejects underscores/hyphens with a descriptive error listing valid values - LLM self-corrects on retry |

## Resolved Questions

- **Serde Deserialize:** Do NOT override. Keep existing serde `Deserialize` for TaskStore compatibility. Use `FromStr` only at the LLM handler boundary. This is a hard constraint.
- **VARIANT_NAMES format:** Use the Debug representation (PascalCase for most enums). This matches what Display already produces for WorkStatus/BundleStatus/TickStatus (they use `write!(f, "{:?}", self)`). For HierarchyStatus and Role (which have custom lowercase Display), `VARIANT_NAMES` still uses PascalCase since the FromStr normalization makes casing irrelevant.

## Open Questions

- [ ] None remaining

## References

- Memory: `project_prompt_ssot.md` - prior documentation of this problem
- `docs/design/2026-02-28-agent-guidance-system.md` line 435 - called out this exact bug as needing a manual test
- `prompts/coordinator.pmt` line 42 - the hardcoded values that caused the latest failure
