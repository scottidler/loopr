# Rust Notes CLI

## Problem Statement

The project needs a multi-subcommand `notes` CLI tool in Rust backed by SQLite.
This is Loopr's Rust CLI target: it exercises database modeling with rusqlite,
argument parsing with clap derive, and a comprehensive cargo test suite covering
both unit and integration tests. Validation: `cargo test`.

## Goals

- Users can add, list, get, delete, and search notes from the command line
- Notes persist in SQLite with the database path configurable via a `--db` flag
- A `src/db.rs` unit test suite covers all database operations
- A `tests/cli.rs` integration test suite covers CLI behavior end-to-end
- `cargo test` passes all unit and integration tests

## Requirements

| As a... | I want to... | So that... |
|---------|-------------|-----------|
| user | `notes add "My Note"` | a new note is created with an auto-assigned id |
| user | `notes list` | I see all notes with id and title |
| user | `notes get <id>` | I retrieve a specific note by id |
| user | `notes delete <id>` | the note is removed permanently |
| user | `notes search <query>` | I see notes whose title or content contains the query |
| user | `--db <path>` on any subcommand | notes are read from and written to that SQLite file |
| developer | `cargo test` | all unit tests and integration tests pass |

## Scope

- `src/db.rs`: `Db` struct and `Note` struct with SQLite CRUD operations
- `src/main.rs`: clap derive CLI with `add`, `list`, `get`, `delete`, `search` subcommands
- `tests/cli.rs`: integration tests that run the compiled binary with temp databases

## Constraints

- Rust stable. Dependencies: `clap` (derive feature), `rusqlite` (bundled feature),
  `eyre` for `main`. Dev dependency: `tempfile` for integration test isolation.
- Use `rusqlite` with the `bundled` feature - no system SQLite required.
- Database path via `--db <path>` global flag, default `notes.db` in current directory.
- Use `cargo add` for all dependencies - versions are defined in `Cargo.toml`.

## Contracts

### Data Model

```
notes table:
  id      INTEGER PRIMARY KEY AUTOINCREMENT
  title   TEXT NOT NULL
  content TEXT NOT NULL DEFAULT ''
  tags    TEXT NOT NULL DEFAULT ''
```

`Note` struct fields: `id: i64`, `title: String`, `content: String`, `tags: String`.

### CLI Contract

```
notes --db <path>  add <title> [--content <text>] [--tags <text>]
notes --db <path>  list
notes --db <path>  get <id>
notes --db <path>  delete <id>
notes --db <path>  search <query>
```

Output: `add` prints `[{id}] {title}`. `list` prints each note as `[{id}] {title}`
or `No notes.` if empty. `get` prints note fields or `Note {id} not found.`.
`delete` prints `Deleted note {id}.` or `Note {id} not found.`. `search` prints
matching notes or `No notes matching '{query}'.`.

## Acceptance Criteria

| Given... | When... | Then... |
|----------|---------|---------|
| an empty database | `notes add "Hello"` runs | output contains `Hello` and the assigned id |
| an empty database | `notes list` runs | output contains `No notes.` |
| a note exists | `notes list` runs | output contains the note's id and title |
| a note exists | `notes get <id>` runs | output contains the note's title |
| an unknown id | `notes get 99999` runs | output contains `not found` |
| a note exists | `notes delete <id>` runs | output contains `Deleted` |
| an unknown id | `notes delete 99999` runs | output contains `not found` |
| two notes with different titles | `notes search <term>` runs | only the matching note appears |
| `cargo test` runs | all tests execute | all 6 unit tests + all 9 integration tests pass |

### Final Validation

```
cargo test
```

## Specs

- **Database layer** - `src/db.rs`: `Db` struct wrapping a `rusqlite::Connection`.
  `Db::open(path: &str)` creates the table if not present. Methods: `create`, `list`,
  `get` (returns `Option<Note>`), `update` (returns `bool`), `delete` (returns `bool`),
  `search`. Unit tests in `#[cfg(test)] mod tests` use `Db::open(":memory:")` for
  isolation: `test_create_and_list`, `test_get_existing`, `test_get_missing`,
  `test_delete_existing`, `test_delete_missing`, `test_search_matches`.

- **CLI subcommands** - `src/main.rs`: clap derive `Cli` struct with global `--db` flag
  and `Commands` enum with `Add`, `List`, `Get`, `Delete`, `Search` variants. `main`
  opens `Db` from `cli.db` and dispatches to the matching command handler. Uses `eyre`
  for error propagation.

- **Integration tests** - `tests/cli.rs`: uses `std::process::Command` and `tempfile`
  for isolated per-test databases. Tests: `test_add_creates_note`, `test_list_empty`,
  `test_list_after_add`, `test_get_note`, `test_get_missing`, `test_delete_note`,
  `test_delete_missing`, `test_search_finds_match`, `test_search_no_match`.

## Dependencies

- `clap` with `derive` feature (`cargo add clap --features derive`)
- `rusqlite` with `bundled` feature (`cargo add rusqlite --features bundled`)
- `eyre` (`cargo add eyre`)
- `tempfile` as dev dependency (`cargo add --dev tempfile`)
