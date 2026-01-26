# Loopr Implementation Guide

You are building **Loopr** - an autonomous coding agent. You are running in a Ralph Wiggum loop with fresh context each iteration. Your memory persists ONLY through files.

---

## CRITICAL: Read This Protocol

You have **NO MEMORY** of previous iterations. You MUST follow this exact sequence:

### 1. READ STATE FIRST (Before Anything Else)

```bash
# Read the progress file - this is your memory
cat .loopr-progress

# What's been committed?
git log --oneline | head -20

# What exists?
ls -la src/ 2>/dev/null || echo "src/ not created yet"
```

**DO NOT SKIP THIS STEP.** The progress file tells you exactly where you are.

### 2. DO WORK

Based on `.loopr-progress`, continue from where the last iteration left off:
- If `current_phase` is set, work on that phase
- If `current_phase` is null, determine the next phase from `phases_remaining`

**Work → Validate → Commit cycle:**
```bash
# 1. Implement the phase (write code, tests)

# 2. Run validation
otto ci

# 3. If validation passes, commit
git add <specific files>  # NOT .loopr-progress
git commit -m "feat(scope): description

Phase N: <phase name>
- what was done"

# 4. If validation fails, fix issues and repeat
```

**Important:** Commit BEFORE updating `.loopr-progress`. The progress file records that the commit happened.

### 3. UPDATE STATE LAST (Before Iteration Ends)

**YOU MUST UPDATE `.loopr-progress` BEFORE THE ITERATION ENDS.**

```bash
# Update the progress file with current state
cat > .loopr-progress << 'EOF'
status: "in_progress"  # or "complete" only when ALL phases done
iteration: <increment this>
current_phase: "<what you're working on>"
phases_completed:
  - "Phase 1: Foundation"
  - "Phase 2: LLM Client"
  # ... etc
phases_remaining:
  - "Phase 3: Single Loop"
  # ... etc
last_action: "<brief description of what you did this iteration>"
blockers: []  # any issues preventing progress
notes: "<anything the next iteration needs to know>"
EOF
```

---

## Project: Loopr

### What You're Building

Read these docs to understand the architecture:
- **[docs/README.md](docs/README.md)** - Master overview, all phases defined here
- **[docs/loop-architecture.md](docs/loop-architecture.md)** - Core concepts
- **[docs/domain-types.md](docs/domain-types.md)** - Data model
- **[docs/execution-model.md](docs/execution-model.md)** - How loops run

### Implementation Phases

The phases are defined in `docs/README.md`. Read that file to understand:
- What each phase delivers
- Dependencies between phases
- File structure targets

**Do not hardcode phase knowledge here.** Always reference `docs/README.md` as the source of truth.

### How to Check Phase Completion

For each phase, verify:
1. Required modules/files exist (check `ls`)
2. Code compiles (`cargo check`)
3. Tests pass (`cargo test`)
4. Committed to git with proper message

### Validation

Run before each commit:
```bash
otto ci  # runs: cargo check, clippy, fmt --check, test
```

### Commit Format

```
feat(scope): description

Phase N: <phase name>
- bullet points of what was done
```

---

## Completion Criteria

**YOU MAY ONLY CREATE `.loopr-complete` WHEN ALL OF THESE ARE TRUE:**

1. ALL phases from `docs/README.md` are implemented
2. ALL phases are committed to git (verify with `git log`)
3. `otto ci` passes
4. `.loopr-progress` shows `phases_remaining: []` (empty)
5. The binary builds and runs: `cargo build --release && ./target/release/loopr --help`
6. **Code cleanup complete:**
   - No `#[allow(dead_code)]` remaining (grep to verify)
   - No `_underscore` variables that should be used
   - All tests pass with no warnings

**Pre-completion cleanup check:**
```bash
# Verify no temporary allowances remain
grep -r "allow(dead_code)" src/ && echo "FAIL: dead_code found" || echo "OK: no dead_code"
grep -r "let _" src/ --include="*.rs" | grep -v "let _ =" && echo "WARN: check _vars" || echo "OK"
```

**To signal completion:**
```bash
# Only after ALL criteria above are verified
echo "Build complete - $(date)" > .loopr-complete
echo "Final validation: otto ci passed" >> .loopr-complete
echo "All phases implemented per docs/README.md" >> .loopr-complete
echo "Cleanup verified: no dead_code, no unused _vars" >> .loopr-complete
```

---

## Rules

### DO

- Read `.loopr-progress` FIRST every iteration
- Update `.loopr-progress` LAST every iteration
- Use `cargo add` for dependencies (never manual versions)
- Write tests for all public functions
- Commit after completing meaningful work
- Reference docs for implementation details

### DO NOT

- Skip reading the progress file
- Exit without updating the progress file
- Create `.loopr-complete` until ALL phases are done
- Guess what was done - always check git log and files
- Manually write dependency versions in Cargo.toml
- Skip validation before commits

---

## Rust Conventions

1. **Async everywhere** - Use tokio, async fn
2. **Dependency injection** - Accept traits, not concrete types
3. **Structured errors** - thiserror for types, eyre for propagation
4. **Return data** - Functions return `Result<T>`, minimize side effects

### Testing Requirements

**Every module must have tests.** When you write code, write tests for it.

```rust
// At the bottom of each module
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_something() {
        // ...
    }
}
```

- Test public functions and key internal logic
- Use `#[tokio::test]` for async tests
- Aim for meaningful coverage, not 100% coverage theater

### Temporary Allowances (Development Only)

During development, these are acceptable to keep `cargo check` passing:

```rust
#[allow(dead_code)]  // OK temporarily
fn not_yet_used() { }

let _unused_var = something();  // OK temporarily
```

**BUT: These must be cleaned up before completion.** The final code should have:
- No `#[allow(dead_code)]` attributes
- No `_underscore` prefixed variables (unless genuinely unused by design)
- All public APIs actually used or removed

---

## If You Get Stuck

1. Update `.loopr-progress` with the blocker
2. Set `blockers: ["description of issue"]`
3. The next iteration will see this and can try a different approach

**NEVER just exit.** Always update progress first.

---

## Quick Reference

| File | Purpose |
|------|---------|
| `.loopr-progress` | Your memory between iterations |
| `.loopr-complete` | Completion marker (only create when DONE) |
| `docs/README.md` | Phase definitions, architecture |
| `docs/*.md` | Detailed implementation specs |

---

## Start of Iteration Checklist

1. [ ] Read `.loopr-progress`
2. [ ] Run `git log --oneline | head -10`
3. [ ] Run `ls src/` to see current state
4. [ ] Determine what to work on next
5. [ ] Do the work (implement code, write tests)
6. [ ] Run `otto ci`
7. [ ] **If passes:** `git add <files>` then `git commit` (NOT .loopr-progress)
8. [ ] **If fails:** Fix issues, go back to step 6
9. [ ] Update `.loopr-progress` (record what was committed)
10. [ ] Check if ALL phases complete → create `.loopr-complete`

**Note:** `.loopr-progress` and `.loopr-complete` should NOT be committed - they are loop control files.
