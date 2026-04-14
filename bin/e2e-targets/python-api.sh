#!/usr/bin/env bash
# E2E target: FastAPI + SQLite bookmarks REST API in Docker

TARGET_TIMEOUT=1200

scaffold() {
    mkdir -p "${TARGET}"

    if ! command -v docker &>/dev/null; then
        err "docker is not installed"
        exit 1
    fi
    if ! docker compose version &>/dev/null; then
        err "docker compose is not available"
        exit 1
    fi

    log "Found: $(docker compose version)"

    cat > "${TARGET}/pyproject.toml" <<'PYPROJECT'
[project]
name = "bookmarks-api"
version = "0.1.0"
requires-python = ">=3.12"
dependencies = [
    "fastapi>=0.115",
    "uvicorn[standard]>=0.32",
    "httpx>=0.28",
    "pytest>=8.3",
]
PYPROJECT

    cat > "${TARGET}/Dockerfile" <<'DOCKER'
FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim
WORKDIR /app
COPY pyproject.toml .
RUN uv sync
COPY . .
CMD ["uv", "run", "uvicorn", "main:app", "--host", "0.0.0.0", "--port", "8080"]
DOCKER

    cat > "${TARGET}/docker-compose.yml" <<'COMPOSE'
services:
  api:
    build: .
    ports:
      - "8081:8080"
    volumes:
      - ./data:/app/data
    environment:
      - DATABASE_PATH=/app/data/bookmarks.db

  test:
    build: .
    command: uv run pytest test_api.py -v
    environment:
      - DATABASE_PATH=/tmp/test_bookmarks.db
COMPOSE

    # Minimal stub so pytest doesn't crash before agents write the actual test suite
    cat > "${TARGET}/test_api.py" <<'TEST'
def test_stub():
    pass
TEST

    # Minimal stub so docker compose build works before agents write main.py
    cat > "${TARGET}/main.py" <<'MAIN'
from fastapi import FastAPI

app = FastAPI()

@app.get("/health")
def health():
    return {"status": "ok"}
MAIN

    cat > "${TARGET}/README.md" <<'README'
# Bookmarks API

A REST API for managing bookmarks, built with FastAPI and SQLite.

## Endpoints

- GET    /health          - health check
- GET    /bookmarks       - list all bookmarks
- POST   /bookmarks       - create a bookmark
- GET    /bookmarks/{id}  - get a bookmark by ID
- PUT    /bookmarks/{id}  - update a bookmark
- DELETE /bookmarks/{id}  - delete a bookmark

## Bookmark schema

- id: integer (auto-assigned)
- title: string (required)
- url: string (required)
- tags: string (comma-separated, optional, default "")

## Run

    docker compose up api
    curl http://localhost:8081/health
    curl http://localhost:8081/bookmarks

## Test

    docker compose run --rm test
README

    mkdir -p "${TARGET}/data"

    (
        cd "${TARGET}"
        git init -q
        printf "data/*.db\n__pycache__/\n.pytest_cache/\n*.pyc\n" > .gitignore
        git add -A
        git commit -q -m "init: FastAPI skeleton and Docker config"
    )
    ok "Python API target ready at ${TARGET}"
}

target_validation_commands() {
    cat <<'CMDS'
    - "docker compose run --rm test"
CMDS
}

target_tools() {
    cat <<'TOOLS'
  - name: "test"
    command: "docker compose run --rm test"
    timeout_secs: 300
    worktree: true
  - name: "fmt"
    command: "echo 'no local fmt; Docker validates'"
    timeout_secs: 5
    worktree: false
TOOLS
}

target_goal() {
    echo "Build a bookmarks REST API with FastAPI and SQLite. Endpoints: GET /health, GET /bookmarks, POST /bookmarks, GET /bookmarks/{id}, PUT /bookmarks/{id}, DELETE /bookmarks/{id}. Bookmark: id (int, auto), title (str), url (str), tags (str, comma-separated, default ''). Read database path from DATABASE_PATH env var, default 'data/bookmarks.db'. Include a pytest test suite in test_api.py. Use uv + pyproject.toml (NOT pip or requirements.txt). Validate with: docker compose run --rm test."
}

target_plan() {
    echo "${LOOPR_ROOT}/bin/e2e-targets/python-api.md"
}

collect_results() {
    for f in database.py main.py test_api.py; do
        if [[ -f "${TARGET}/${f}" ]]; then
            echo ""
            log "Target ${f}:"
            cat "${TARGET}/${f}"
        fi
    done
}

verify() {
    local pass=true

    for f in database.py main.py test_api.py; do
        if [[ -f "${TARGET}/${f}" ]]; then
            ok "${f} exists"
        else
            warn "${f} missing"
            pass=false
        fi
    done

    echo ""
    if (cd "${TARGET}" && docker compose build 2>&1 | /usr/bin/tail -5); then
        ok "docker compose build succeeded"
    else
        warn "docker compose build failed"
        pass=false
    fi

    echo ""
    if (cd "${TARGET}" && docker compose run --rm test 2>&1 | /usr/bin/tail -20); then
        ok "docker compose run --rm test passed"
    else
        warn "tests had failures"
        pass=false
    fi

    echo ""
    (cd "${TARGET}" && docker compose up -d api 2>&1 | /usr/bin/tail -3) || true
    sleep 3
    if curl -sf http://localhost:8081/health | grep -q "ok"; then
        ok "GET /health returned ok"
        CREATED=$(curl -sf -X POST http://localhost:8081/bookmarks \
            -H "Content-Type: application/json" \
            -d '{"title":"E2E Test","url":"https://example.com","tags":"e2e"}' 2>/dev/null || true)
        if echo "${CREATED}" | grep -q "E2E Test"; then
            ok "POST /bookmarks created a bookmark"
            if curl -sf http://localhost:8081/bookmarks | grep -q "E2E Test"; then
                ok "GET /bookmarks returned the created bookmark"
            else
                warn "GET /bookmarks did not return expected bookmark"
                pass=false
            fi
        else
            warn "POST /bookmarks did not return expected data"
            pass=false
        fi
    else
        warn "GET /health failed (API may not have started)"
        pass=false
    fi
    (cd "${TARGET}" && docker compose down 2>/dev/null || true)

    if [[ "${pass}" == "true" ]]; then
        ok "All verification checks passed"
    else
        warn "Some verification checks failed"
        return 1
    fi
}
