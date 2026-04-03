# CLI Shakedown Report: lua-todo (e2e artifact)

**Runtime:** Lua 5.1.5
**App location:** `/tmp/loopr-e2e/lua-todo/latest/`
**Shakedown dir:** `/tmp/lua-todo-shakedown/`
**Date:** 2026-04-03

---

## Summary

| Metric | Count |
|--------|-------|
| Commands discovered | 4 (add, list, done, delete) |
| Commands tested | 4 |
| Commands passed | 4 |
| Commands failed | 0 |
| Commands skipped | 0 |
| Pipelines tested | 4 |
| Edge cases tested | 8 |
| Test suite | 5/5 PASS |

---

## Invocation

The tool is a pure Lua script. It has **no `--version` flag** and **no `--help` flag** (usage is printed on unknown/missing commands). Invocation requires running from a directory containing `json.lua` and `todo.lua`:

```bash
cd /path/to/app && lua cli.lua <command> [arg]
```

> **Note:** `require("todo")` and `require("json")` resolve relative to CWD, not the script's location. Running `lua /abs/path/cli.lua` from a different directory fails with a module-not-found error.

---

## Command Results

### `(no args)` — Usage

```
$ lua cli.lua
Usage: lua cli.lua <command> [argument]
Commands:
  add <title>      Add a new todo
  list [filter]    List todos (filter: all, active, done)
  done <id>        Mark a todo as done
  delete <id>      Delete a todo
EXIT: 0
```

**Pass.** Behavior is clean. Minor note: exit code is 0 on missing command (many CLIs use 1 here, but this is not wrong).

---

### `add <title>`

```
$ lua cli.lua add "Buy groceries"
Added: 1 Buy groceries
EXIT: 0

$ lua cli.lua add "Write unit tests"
Added: 2 Write unit tests
EXIT: 0

$ lua cli.lua add "Deploy to production"
Added: 3 Deploy to production
EXIT: 0
```

**Pass.** IDs are sequential integers starting at 1. Output format: `Added: <id> <title>`.

---

### `list [filter]`

```
$ lua cli.lua list
  [ ] 1: Buy groceries
  [ ] 2: Write unit tests
  [ ] 3: Deploy to production
EXIT: 0

$ lua cli.lua list all      # explicit 'all' — identical to default
  [ ] 1: Buy groceries
  ...

$ lua cli.lua list active
  [ ] 1: Buy groceries
  [ ] 2: Write unit tests
  [ ] 3: Deploy to production
EXIT: 0

$ lua cli.lua list done
No todos found.
EXIT: 0
```

**Pass.** All three filter values work. Default (no arg) is equivalent to `all`.

After marking two done:

```
$ lua cli.lua list all
  [x] 1: Buy groceries
  [x] 2: Write unit tests
  [ ] 3: Deploy to production

$ lua cli.lua list active
  [ ] 3: Deploy to production

$ lua cli.lua list done
  [x] 1: Buy groceries
  [x] 2: Write unit tests
```

**Pass.** Filters are correct.

---

### `done <id>`

```
$ lua cli.lua done 1
Done: 1
EXIT: 0

$ lua cli.lua done 999
Not found: 999
EXIT: 0
```

**Pass.** Correct output for both found and not-found cases.

---

### `delete <id>`

```
$ lua cli.lua delete 1
Deleted: 1
EXIT: 0

$ lua cli.lua delete 999
Not found: 999
EXIT: 0
```

**Pass.** Correct output for both cases. After delete, the ID is not reused (next add gets `next_id`).

---

## Output Format Matrix

The tool outputs plain text only. No `--json`, `--csv`, or `--format` flags exist.

| Command | Plain text | JSON | CSV |
|---------|-----------|------|-----|
| add     | ✅        | n/a  | n/a |
| list    | ✅        | n/a  | n/a |
| done    | ✅        | n/a  | n/a |
| delete  | ✅        | n/a  | n/a |

---

## Failures & Bugs

### BUG: Non-numeric ID crashes with Lua traceback

**Severity:** Bug (unhandled error)

```
$ lua cli.lua done notanumber
lua: cli.lua:40: attempt to concatenate local 'id' (a nil value)
stack traceback:
    cli.lua:40: in main chunk
    [C]: ?
EXIT: 1
```

Same crash on `delete notanumber`. Root cause: `tonumber("notanumber")` returns `nil`, and `nil` is then used in string concatenation on the error path before the nil check on the store call happens.

**Expected:** `Error: id must be a number` and exit 1.

---

### BUG: Unknown filter silently returns empty

**Severity:** Cosmetic / confusing

```
$ lua cli.lua list invalidfilter
No todos found.
EXIT: 0
```

Even with active todos present, an invalid filter silently returns nothing.

**Expected:** `Error: unknown filter 'invalidfilter'. Use: all, active, done` and exit 1.

---

### NOTE: Unknown command returns exit 0

```
$ lua cli.lua unknowncmd
Usage: lua cli.lua <command> [argument]
...
EXIT: 0
```

Shows usage but exits 0. Conventional behavior is exit 1 for unknown commands. Not a crash, but may confuse scripts checking exit codes.

---

### NOTE: CWD-relative module loading

Running `lua /abs/path/cli.lua` from outside the app directory fails:

```
lua: /tmp/lua-todo-shakedown/cli.lua:1: module 'todo' not found: no file './todo.lua'
```

The app must be invoked from its own directory. This is a Lua convention, not strictly a bug, but worth documenting.

---

## Pipeline Recipes

```bash
# Count total todos
lua cli.lua list all | wc -l

# Count done
lua cli.lua list all | grep '^\s*\[x\]' | wc -l

# Count active
lua cli.lua list all | grep '^\s*\[ \]' | wc -l

# Get first active ID and mark done (chained pipeline)
ID=$(lua cli.lua list active | grep -o '^\s*\[ \] [0-9]\+' | grep -o '[0-9]\+' | head -1)
lua cli.lua done "$ID"

# Inspect raw JSON state
cat todos.json | jq .

# Bulk-add todos from a list
echo -e "Task one\nTask two\nTask three" | while read t; do lua cli.lua add "$t"; done
```

---

## Formatting Quality

- Output uses consistent 2-space indentation, `[x]`/`[ ]` markers, id-colon-title format
- Long titles display inline without truncation or column overflow (flat text, not tabular)
- Mixed done/active entries render correctly side-by-side
- No alignment columns to break — plain line-per-item format holds up fine

---

## Persistence

`todos.json` is written to CWD on every mutating operation. Format:

```json
{"todos":[{"id":2,"done":true,"title":"Write unit tests"}],"next_id":4}
```

- IDs are monotonically increasing integers (not reused after delete)
- `next_id` is persisted so counter survives restarts
- Special characters (`<>&"`) round-trip correctly through JSON

---

## Test Suite

```
$ lua test_todo.lua
PASS: test_add
PASS: test_done
PASS: test_delete
PASS: test_list_filter
PASS: test_persistence
5/5 tests passed
EXIT: 0
```

All 5 tests pass from the shakedown workspace.

---

## Observations

1. **No `--version` or `--help`** — Not needed for a simple script, but worth noting for discoverability.
2. **No `--file` flag** — The JSON data file path is hardcoded to `todos.json` in CWD. Parameterizing it would make the tool more scriptable (multiple lists).
3. **No `list` count** — The footer `No todos found.` is shown for empty lists; a `(N todos)` count would be handy for non-empty.
4. **Exit codes** — Missing command and unknown command exit 0; convention is exit 1. Low priority.
5. **Extra artifact:** `src/todo.rs` was committed to the repo (a stray Rust implementation from the orchestrator). It doesn't affect the Lua app but is unexpected.
