#!/usr/bin/env bash
# E2E target: Express + better-sqlite3 notes REST API in Docker

TARGET_TIMEOUT=1200

scaffold() {
    mkdir -p "${TARGET}"

    if ! command -v node &>/dev/null; then
        err "node is not installed"
        exit 1
    fi
    if ! command -v docker &>/dev/null; then
        err "docker is not installed"
        exit 1
    fi
    if ! docker compose version &>/dev/null; then
        err "docker compose is not available"
        exit 1
    fi

    log "Found: node $(node --version), $(docker compose version)"

    cat > "${TARGET}/package.json" <<'PKG'
{
  "name": "notes-api",
  "version": "0.1.0",
  "scripts": {
    "start": "node server.js",
    "test": "jest --forceExit"
  },
  "dependencies": {
    "better-sqlite3": "^9.6.0",
    "express": "^4.21.0"
  },
  "devDependencies": {
    "jest": "^29.7.0",
    "supertest": "^7.0.0"
  }
}
PKG

    cat > "${TARGET}/Dockerfile" <<'DOCKER'
FROM node:20-alpine
WORKDIR /app
COPY package*.json ./
RUN npm ci
COPY . .
CMD ["node", "server.js"]
DOCKER

    cat > "${TARGET}/docker-compose.yml" <<'COMPOSE'
services:
  api:
    build: .
    ports:
      - "8082:3000"
    volumes:
      - ./data:/app/data
    environment:
      - DATABASE_PATH=/app/data/notes.db

  test:
    build: .
    command: npm test
    environment:
      - DATABASE_PATH=/tmp/test_notes.db
COMPOSE

    # Stub app.js so docker compose build works before agents write routes
    cat > "${TARGET}/app.js" <<'APP'
const express = require('express');
const app = express();
app.use(express.json());
app.get('/health', (_req, res) => res.json({ status: 'ok' }));
module.exports = app;
APP

    cat > "${TARGET}/server.js" <<'SERVER'
const app = require('./app');
const PORT = process.env.PORT || 3000;
app.listen(PORT, () => console.log(`Notes API listening on port ${PORT}`));
SERVER

    cat > "${TARGET}/README.md" <<'README'
# Notes API

A REST API for managing notes, built with Express and SQLite.

## Endpoints

- GET    /health      - health check
- GET    /notes       - list all notes
- POST   /notes       - create a note
- GET    /notes/{id}  - get a note by ID
- PUT    /notes/{id}  - update a note
- DELETE /notes/{id}  - delete a note

## Note schema

- id: integer (auto-assigned)
- title: string (required)
- content: string (optional, default "")
- tags: string (comma-separated, optional, default "")

## Run

    docker compose up api
    curl http://localhost:8082/health
    curl http://localhost:8082/notes

## Test

    docker compose run --rm test
README

    mkdir -p "${TARGET}/test" "${TARGET}/data"

    # Minimal stub so Jest doesn't exit 1 with "No tests found" before agents write the real tests
    cat > "${TARGET}/test/notes.test.js" <<'TEST'
const app = require('../app');
describe('stub', () => {
  test('health returns ok', async () => {
    const supertest = require('supertest');
    const res = await supertest(app).get('/health');
    expect(res.body.status).toBe('ok');
  });
});
TEST

    log "Installing npm dependencies..."
    (cd "${TARGET}" && npm install --silent 2>&1 | /usr/bin/tail -3)

    (
        cd "${TARGET}"
        git init -q
        printf "node_modules/\ndata/*.db\n.env\n" > .gitignore
        git add -A
        git commit -q -m "init: Express skeleton, Docker config, npm deps"
    )
    ok "Node API target ready at ${TARGET}"
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
    echo "Build a notes REST API with Express and SQLite (better-sqlite3). Endpoints: GET /health, GET /notes, POST /notes, GET /notes/:id, PUT /notes/:id, DELETE /notes/:id. Note: id (int, auto), title (str), content (str, default ''), tags (str, comma-separated, default ''). Read database path from DATABASE_PATH env var, default 'data/notes.db'. Include Jest + supertest tests in test/notes.test.js. Validate with: docker compose run --rm test."
}

target_plan() {
    echo "${LOOPR_ROOT}/bin/e2e-targets/node-api.md"
}

collect_results() {
    for f in db.js app.js test/notes.test.js; do
        if [[ -f "${TARGET}/${f}" ]]; then
            echo ""
            log "Target ${f}:"
            cat "${TARGET}/${f}"
        fi
    done
}

verify() {
    local pass=true

    for f in db.js app.js test/notes.test.js; do
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
    if curl -sf http://localhost:8082/health | grep -q "ok"; then
        ok "GET /health returned ok"
        CREATED=$(curl -sf -X POST http://localhost:8082/notes \
            -H "Content-Type: application/json" \
            -d '{"title":"E2E Test","content":"hello","tags":"e2e"}' 2>/dev/null || true)
        if echo "${CREATED}" | grep -q "E2E Test"; then
            ok "POST /notes created a note"
            if curl -sf http://localhost:8082/notes | grep -q "E2E Test"; then
                ok "GET /notes returned the created note"
            else
                warn "GET /notes did not return expected note"
                pass=false
            fi
        else
            warn "POST /notes did not return expected data"
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
    fi
}
