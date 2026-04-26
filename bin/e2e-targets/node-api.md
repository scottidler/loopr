# Node.js Notes API

## Problem Statement

The project needs a notes REST API built with Express and better-sqlite3,
containerized with Docker. This is Loopr's Node.js web API target: it exercises
database modeling, REST endpoint design, Docker packaging, and Jest + supertest
coverage in a single contained project. Validation: `docker compose run --rm test`.

## Goals

- Users can create, read, update, and delete notes via a REST API
- Data persists in SQLite using better-sqlite3 with the path configurable via env var
- A health endpoint confirms the service is running
- A Jest + supertest suite covers all CRUD endpoints with per-test database isolation
- The service builds and runs in Docker via `docker compose`

## Requirements

| As a... | I want to... | So that... |
|---------|-------------|-----------|
| user | `GET /health` | I can confirm the API is running |
| user | `POST /notes` with title | a new note is created with an auto-assigned id |
| user | `GET /notes` | I see all notes in the database |
| user | `GET /notes/:id` | I retrieve a specific note by id |
| user | `PUT /notes/:id` with updated fields | the note is updated in place |
| user | `DELETE /notes/:id` | the note is removed permanently |
| developer | set `DATABASE_PATH` env var | the API reads and writes to that SQLite file |
| developer | `docker compose run --rm test` | all Jest tests pass in a clean container |

## Scope

- `db.js`: SQLite CRUD module using `better-sqlite3`
- `app.js`: Express application with all endpoints (no `app.listen` - that is in `server.js`)
- `server.js`: entry point that calls `app.listen`
- `test/notes.test.js`: Jest + supertest suite

## Constraints

- Node.js 20. `express`, `better-sqlite3` as runtime deps; `jest`, `supertest` as dev deps.
- Use `better-sqlite3` synchronous API - no async database calls.
- Database path from `DATABASE_PATH` env var, default `data/notes.db`.
- Containerized: `Dockerfile` + `docker-compose.yml` are pre-provided.
- Test isolation: each test uses a unique temp database path set via `DATABASE_PATH`.
- `app.js` must not call `app.listen()` - import/require must be side-effect-free for testing.

## Contracts

### Data Model

Note stored in SQLite as:

```
notes table:
  id      INTEGER PRIMARY KEY AUTOINCREMENT
  title   TEXT NOT NULL
  content TEXT NOT NULL DEFAULT ''
  tags    TEXT NOT NULL DEFAULT ''
```

Row objects returned by all `db.js` functions have keys: `id`, `title`, `content`, `tags`.

### API Contract

```
GET    /health                          -> { status: 'ok' }
GET    /notes                           -> array of note objects
POST   /notes  body: {title, content?, tags?}   -> created note, status 201; 400 if title missing
GET    /notes/:id                       -> note object, 404 { error: 'Not found' } if missing
PUT    /notes/:id  body: {title?, content?, tags?}  -> updated note, 404 if missing
DELETE /notes/:id                       -> { deleted: id }, 404 if missing
```

Request and response bodies are JSON. `app.use(express.json())` is required.

## Acceptance Criteria

| Given... | When... | Then... |
|----------|---------|---------|
| the API is running | `GET /health` | returns `{ status: 'ok' }` |
| an empty database | `POST /notes` with title | returns the created note with integer id, status 201 |
| `POST /notes` with no title | the request arrives | returns HTTP 400 |
| a created note | `GET /notes` | returns an array containing that note |
| a created note | `GET /notes/:id` | returns the note with correct fields |
| an unknown id | `GET /notes/99999` | returns HTTP 404 with `{ error: 'Not found' }` |
| a created note | `PUT /notes/:id` with new title | returns the updated note |
| an unknown id | `PUT /notes/99999` | returns HTTP 404 |
| a created note | `DELETE /notes/:id` | returns `{ deleted: id }` |
| an unknown id | `DELETE /notes/99999` | returns HTTP 404 |
| Jest runs | `docker compose run --rm test` | all 11 tests pass and exit code is 0 |

### Final Validation

```
docker compose run --rm test
```

## Specs

- **Database layer** - `db.js`: `getDbPath()`, `createNote(dbPath, title, content, tags)`,
  `listNotes(dbPath)`, `getNote(dbPath, id)`, `updateNote(dbPath, id, fields)`,
  `deleteNote(dbPath, id)`. Each function opens and closes the database internally using
  `better-sqlite3`. `updateNote` accepts an object with optional `title`, `content`, `tags`
  keys and updates only present fields. `module.exports` exports all functions.

- **API routes** - `app.js`: Express app with `express.json()` middleware. All six
  endpoints delegate to `db.js`. Returns 400 when `title` is missing on POST. Returns
  404 JSON `{ error: 'Not found' }` when a note is not found. Does NOT call `app.listen()`.

- **Test suite** - `test/notes.test.js`: Jest + supertest against the Express app.
  `beforeEach` sets `process.env.DATABASE_PATH` to a unique temp path.
  `afterEach` deletes the temp database file. Tests: health, create, create-missing-title,
  list-empty, list-after-create, get, get-not-found, update, update-not-found, delete,
  delete-not-found.

## Dependencies

- `express ^4.21.0` (in `package.json` dependencies)
- `better-sqlite3 ^9.6.0` (in `package.json` dependencies)
- `jest ^29.7.0` (in `package.json` devDependencies)
- `supertest ^7.0.0` (in `package.json` devDependencies)
- Docker + docker compose (pre-installed in the scaffold environment)
