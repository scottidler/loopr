# Python Bookmarks API

## Problem Statement

The project needs a bookmarks REST API built with FastAPI and SQLite, containerized
with Docker. This is Loopr's Python web API target: it exercises database modeling,
REST endpoint design, Docker packaging, and pytest coverage in a single contained
project. Validation: `docker compose run --rm test`.

## Goals

- Users can create, read, update, and delete bookmarks via a REST API
- Data persists in SQLite with the database path configurable via environment variable
- A health endpoint confirms the service is running
- A pytest suite covers all CRUD endpoints with test isolation
- The service builds and runs in Docker via `docker compose`

## Requirements

| As a... | I want to... | So that... |
|---------|-------------|-----------|
| user | `GET /health` | I can confirm the API is running |
| user | `POST /bookmarks` with title and url | a new bookmark is created with an auto-assigned id |
| user | `GET /bookmarks` | I see all bookmarks in the database |
| user | `GET /bookmarks/{id}` | I retrieve a specific bookmark by id |
| user | `PUT /bookmarks/{id}` with updated fields | the bookmark is updated in place |
| user | `DELETE /bookmarks/{id}` | the bookmark is removed permanently |
| developer | set `DATABASE_PATH` env var | the API reads and writes to that SQLite file |
| developer | `docker compose run --rm test` | all pytest tests pass in a clean container |

## Scope

- `database.py`: SQLite CRUD module using stdlib `sqlite3`
- `main.py`: FastAPI application with all endpoints
- `test_api.py`: pytest suite using FastAPI's TestClient

## Constraints

- Python 3.12. FastAPI, uvicorn, httpx, and pytest from `requirements.txt`.
- No ORM - use `sqlite3` stdlib directly.
- Database path from `DATABASE_PATH` env var, default `data/bookmarks.db`.
- Containerized: `Dockerfile` + `docker-compose.yml` are pre-provided.
- Test isolation: each test uses a `tmp_path`-based database path via `DATABASE_PATH`.

## Contracts

### Data Model

Bookmark stored in SQLite as:

```
bookmarks table:
  id    INTEGER PRIMARY KEY AUTOINCREMENT
  title TEXT NOT NULL
  url   TEXT NOT NULL
  tags  TEXT NOT NULL DEFAULT ''
```

Row dicts returned by all database functions have keys: `id`, `title`, `url`, `tags`.

### API Contract

```
GET    /health                        -> {"status": "ok"}
GET    /bookmarks                     -> list of bookmark dicts
POST   /bookmarks  body: {title, url, tags?}  -> created bookmark, status 201
GET    /bookmarks/{id}                -> bookmark dict, 404 if not found
PUT    /bookmarks/{id}  body: {title?, url?, tags?}  -> updated bookmark, 404 if not found
DELETE /bookmarks/{id}                -> {"deleted": id}, 404 if not found
```

Request and response bodies are JSON. Pydantic models: `BookmarkCreate` (title, url, tags="") and `BookmarkUpdate` (all Optional).

## Acceptance Criteria

| Given... | When... | Then... |
|----------|---------|---------|
| the API is running | `GET /health` | returns `{"status": "ok"}` |
| an empty database | `POST /bookmarks` with title and url | returns the created bookmark with an integer id |
| a created bookmark | `GET /bookmarks` | returns a list containing that bookmark |
| a created bookmark | `GET /bookmarks/{id}` | returns the bookmark with correct fields |
| an unknown id | `GET /bookmarks/99999` | returns HTTP 404 |
| a created bookmark | `PUT /bookmarks/{id}` with new title | returns the updated bookmark |
| an unknown id | `PUT /bookmarks/99999` | returns HTTP 404 |
| a created bookmark | `DELETE /bookmarks/{id}` | returns `{"deleted": id}` |
| an unknown id | `DELETE /bookmarks/99999` | returns HTTP 404 |
| pytest runs | `docker compose run --rm test` | all 10 tests pass and exit code is 0 |

### Final Validation

```
docker compose run --rm test
```

## Specs

- **Database layer** - `database.py`: `get_db_path()`, `init_db(db_path)`,
  `get_connection(db_path)`, `create_bookmark(db_path, title, url, tags)`,
  `list_bookmarks(db_path)`, `get_bookmark(db_path, id)`,
  `update_bookmark(db_path, id, title, url, tags)`, `delete_bookmark(db_path, id)`.
  All functions accept `db_path` as first argument; `get_connection` creates parent
  directories and initializes the schema.

- **API routes** - `main.py`: FastAPI `app` with Pydantic models `BookmarkCreate`
  and `BookmarkUpdate`. All six endpoints delegate to `database.py` functions. Raises
  `HTTPException(404)` when a bookmark is not found.

- **Test suite** - `test_api.py`: pytest using `TestClient(app)`. Fixture sets
  `DATABASE_PATH` to `str(tmp_path / "test.db")` and restores it after each test.
  Tests: health, create, list-empty, list-after-create, get, get-not-found, update,
  update-not-found, delete, delete-not-found.

## Dependencies

- `fastapi>=0.115` (in `requirements.txt`)
- `uvicorn[standard]>=0.32` (in `requirements.txt`)
- `httpx>=0.28` (required by FastAPI TestClient, in `requirements.txt`)
- `pytest>=8.3` (in `requirements.txt`)
- Docker + docker compose (pre-installed in the scaffold environment)
