# Ralph Wiggum Loop - ONE TASK THEN EXIT

You are in a Ralph Wiggum loop. You have NO MEMORY of previous runs.
Your state persists ONLY in `progress.txt`.

## CRITICAL RULES

1. **READ progress.txt FIRST** - It tells you what was done
2. **DO ONE SMALL THING** - Not a phase. One file, one fix, one test.
3. **EXIT IMMEDIATELY** - Do not retry. Do not fix errors. Just exit.

The bash loop will restart you with fresh context. That's the whole point.
The bash loop runs validation EXTERNALLY - you do NOT run tests or validation.

---

## Your Workflow

```bash
# 1. Read state
cat progress.txt
git log --oneline -10

# 2. Do ONE small task (examples):
#    - Add ONE file
#    - Fix ONE error
#    - Add ONE test

# 3. Record what you did
echo "Iteration N: <what you did>" >> progress.txt

# 4. If ALL work is complete, signal completion:
echo "<promise>COMPLETE</promise>"

# 5. EXIT - do nothing else
```

## MANDATORY: Unit Tests

You MUST add unit tests that prove the correctness of the implementation.
- Every function/module you implement needs corresponding tests
- Tests must actually verify behavior, not just exist
- Zero tests passing means INCOMPLETE - keep adding tests
- The validation step checks that tests exist AND pass

## What is ONE task?

**YES - do these:**
- Add `src/foo.rs` with basic struct
- Fix the compile error on line 42
- Add one test for `parse_config`
- Add unit tests for the function you just wrote

**NO - too much:**
- Implement Phase 3
- Add multiple modules
- Fix errors then add features

## On Previous Validation Failure

If progress.txt shows a FAIL from the previous iteration:
1. Read what failed
2. Fix that ONE thing
3. Record what you fixed
4. EXIT immediately

The bash loop runs validation after you exit. You will see results in progress.txt next iteration.

## Completion

Output `<promise>COMPLETE</promise>` when you believe:
- ALL phases in `docs/implementation-phases.md` are done
- Code is complete and should pass validation
- Unit tests exist that prove the correctness of the implementation

The bash loop will verify this externally. If validation fails, you'll be restarted.

---

## Project: Loopr

Read `docs/implementation-phases.md` for what to build.
Each phase lists files and validation criteria.

## Now: Read progress.txt and do ONE thing
