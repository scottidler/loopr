# Python Command-Line Todo Application

## Problem Statement

The project needs a working command-line todo application in Python with full
CRUD operations and JSON persistence. The app is the canonical verification
target for Loopr's Python implementation path: it exercises file I/O, data
modeling, argument parsing, and pytest coverage in one contained project.

## Goals

- Users can add, list, complete, and delete todo items from the command line
- Todo state persists across invocations via a JSON file
- A pytest suite covers all operations and edge cases
- All code is importable as a library and exercisable as a CLI

## Requirements

| As a... | I want to... | So that... |
|---------|-------------|-----------|
| user | run `python cli.py add "Buy milk"` | a new todo is created with a unique id |
| user | run `python cli.py list` | I see all todos with their id, title, and done status |
| user | run `python cli.py list --filter active` | I see only incomplete todos |
| user | run `python cli.py list --filter done` | I see only completed todos |
| user | run `python cli.py done 1` | todo with id 1 is marked as done |
| user | run `python cli.py delete 1` | todo with id 1 is removed permanently |
| developer | import `todo.py` | I can call add_todo, list_todos, done_todo, delete_todo without side effects |

## Scope

- `todo.py`: core library with all data operations and persistence
- `cli.py`: argument parsing and dispatch, calls `todo.py` functions
- `test_todo.py`: pytest suite covering all operations

## Constraints

- Python 3.10+. Standard library only: `json`, `sys`, `argparse`, `pathlib`.
- No third-party packages except `pytest` (already in requirements.txt).
- Persistence file: `todos.json` in the current working directory.
- IDs are auto-incrementing integers starting at 1. The next ID is
  `max(existing ids) + 1` or `1` if the list is empty.

## Contracts

### Data Model

TodoItem stored as a JSON object:

```
{
  "id":    integer,   -- unique, auto-assigned, never reused
  "title": string,    -- required, non-empty
  "done":  boolean    -- false on creation, set to true by done command
}
```

Persistence format: `todos.json` is a JSON array of TodoItem objects.
Empty state: `[]`.

### CLI API

```
python cli.py add <title>
python cli.py list [--filter all|active|done]   # default: all
python cli.py done <id>
python cli.py delete <id>
```

Exit code 0 on success. Exit code 1 with an error message on failure
(e.g., ID not found, empty title).

## Acceptance Criteria

| Given... | When... | Then... |
|----------|---------|---------|
| an empty todos.json | `cli.py add "Buy milk"` runs | todos.json contains one item with id=1, title="Buy milk", done=false |
| a list with id 1 | `cli.py done 1` runs | todos.json has done=true for id 1 |
| a list with id 1 | `cli.py delete 1` runs | todos.json no longer contains id 1 |
| a list with mixed done/active items | `cli.py list --filter active` | only incomplete items are printed |
| a list with mixed done/active items | `cli.py list --filter done` | only completed items are printed |
| `done` called with a nonexistent id | the command runs | exits with code 1 and an error message |
| pytest runs | `pytest test_todo.py` | all tests pass |

### Final Validation

```
pytest test_todo.py -v
```

## Specs

- **Core library** - `todo.py`: load/save JSON, add_todo, list_todos, done_todo,
  delete_todo. All functions operate on a file path parameter (default: todos.json).

- **CLI layer** - `cli.py`: argparse setup for add/list/done/delete subcommands.
  Calls functions from todo.py. Prints human-readable output. Handles errors and
  sets exit codes.

- **Test suite** - `test_todo.py`: pytest tests using a temporary file path.
  Tests for add, list with each filter, done, delete, and error cases.

## Dependencies

- `pytest` (already installed in `.venv`)
