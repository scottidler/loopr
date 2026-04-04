# Lua Command-Line Todo Application

## Problem Statement

The project needs a command-line todo application in pure Lua. This is Loopr's
Lua implementation target: it exercises file I/O, data modeling, argument
parsing, and a test suite in a language with no package manager or standard
testing framework. The project must use only the pre-provided helpers
(`json.lua`, `test.lua`) plus Lua's standard library.

## Goals

- Users can add, list, complete, and delete todo items from the command line
- Todo state persists across invocations as JSON via `json.lua`
- A test suite in `test_todo.lua` covers all operations using `test.lua`
- Logic is separated: `todo.lua` is the library, `cli.lua` is the entry point

## Requirements

| As a... | I want to... | So that... |
|---------|-------------|-----------|
| user | run `lua cli.lua add "Buy milk"` | a new todo is created with a unique id |
| user | run `lua cli.lua list` | I see all todos with id, title, and done status |
| user | run `lua cli.lua list active` | I see only incomplete todos |
| user | run `lua cli.lua list done` | I see only completed todos |
| user | run `lua cli.lua done 1` | todo with id 1 is marked as done |
| user | run `lua cli.lua delete 1` | todo with id 1 is removed permanently |
| developer | require("todo") | I can call add, list, done, delete without side effects |

## Scope

- `todo.lua`: core library with all data operations and persistence
- `cli.lua`: argument parsing and dispatch, requires `todo`
- `test_todo.lua`: test suite using `test.lua`, covers all operations

## Constraints

- Pure Lua. No external packages or package managers.
- Use pre-provided `json.lua` for persistence (`local json = require("json")`).
- Use pre-provided `test.lua` for tests (`local test = require("test")`).
- Persistence file: `todos.json` in the current working directory.
- IDs are auto-incrementing integers starting at 1.

## Contracts

### Data Model

TodoItem stored as a Lua table and serialized via `json.lua`:

```
{
  id    = integer,   -- unique, auto-assigned, never reused
  title = string,    -- required, non-empty
  done  = boolean    -- false on creation, set to true by done command
}
```

Persistence format: `todos.json` is a JSON array of TodoItem objects.
Empty state: `[]`.

### CLI API

```
lua cli.lua add <title>
lua cli.lua list [all|active|done]   -- default: all
lua cli.lua done <id>
lua cli.lua delete <id>
```

Prints a success message or error message. Exits with code 0 on success,
code 1 on error (ID not found, empty title).

## Acceptance Criteria

| Given... | When... | Then... |
|----------|---------|---------|
| an empty or missing todos.json | `cli.lua add "Buy milk"` runs | todos.json contains one item with id=1, title="Buy milk", done=false |
| a list with id 1 | `cli.lua done 1` runs | todos.json has done=true for id 1 |
| a list with id 1 | `cli.lua delete 1` runs | todos.json no longer contains id 1 |
| a list with mixed items | `cli.lua list active` | only incomplete items are printed |
| a list with mixed items | `cli.lua list done` | only completed items are printed |
| `done` called with a nonexistent id | the command runs | exits with error message |
| `lua test_todo.lua` runs | the test runner executes | all tests pass and summary shows N/N |

### Final Validation

```
lua test_todo.lua
```

## Specs

- **Core library** - `todo.lua`: load/save JSON via `json.lua`, add, list with
  filter, done (by id), delete (by id). Functions take a file path parameter
  (default: todos.json). Returns tables and error strings.

- **CLI layer** - `cli.lua`: parse `arg[1]` (subcommand) and remaining args.
  Requires `todo`. Prints human-readable output to stdout. Exits with code 1
  on error using `os.exit(1)`.

- **Test suite** - `test_todo.lua`: requires `test` and `todo`. Uses a temp
  file path to avoid touching todos.json. Tests add, list-all, list-active,
  list-done, done, delete, and error cases. Calls `test.summary()` at end.

## Dependencies

- `json.lua` (pre-provided in the repo root)
- `test.lua` (pre-provided in the repo root)
