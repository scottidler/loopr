#!/usr/bin/env bash
# E2E target: Python HTML link harvester with SQLite storage and Docker

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

    cat > "${TARGET}/requirements.txt" <<'REQ'
beautifulsoup4>=4.12
pytest>=8.3
REQ

    cat > "${TARGET}/Dockerfile" <<'DOCKER'
FROM python:3.12-slim
WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY . .
CMD ["python", "main.py"]
DOCKER

    cat > "${TARGET}/docker-compose.yml" <<'COMPOSE'
services:
  harvester:
    build: .
    volumes:
      - ./data:/app/data
      - ./report.md:/app/report.md
    environment:
      - INPUT_DIR=/app/input
      - DATABASE_PATH=/app/data/links.db
      - REPORT_PATH=/app/report.md

  test:
    build: .
    command: python -m pytest test_scraper.py -v
    environment:
      - INPUT_DIR=/app/input
      - DATABASE_PATH=/tmp/test_links.db
      - REPORT_PATH=/tmp/test_report.md
COMPOSE

    mkdir -p "${TARGET}/input" "${TARGET}/data"

    # Pre-provide HTML fixture files so tests are deterministic
    cat > "${TARGET}/input/page-a.html" <<'HTML'
<!DOCTYPE html>
<html>
<head><title>Page A</title></head>
<body>
  <h1>Links from Page A</h1>
  <a href="https://python.org">Python</a>
  <a href="https://docs.python.org/3/">Python Docs</a>
  <a href="https://pypi.org">PyPI</a>
  <a href="https://github.com/python/cpython">CPython on GitHub</a>
  <a href="#top">Back to top</a>
  <a href="">Empty href</a>
</body>
</html>
HTML

    cat > "${TARGET}/input/page-b.html" <<'HTML'
<!DOCTYPE html>
<html>
<head><title>Page B</title></head>
<body>
  <h1>Links from Page B</h1>
  <a href="https://github.com/rust-lang/rust">Rust</a>
  <a href="https://crates.io">Crates.io</a>
  <a href="https://doc.rust-lang.org">Rust Docs</a>
  <a href="https://github.com/tokio-rs/tokio">Tokio</a>
  <a href="https://github.com/serde-rs/serde">Serde</a>
  <a href="mailto:admin@example.com">Contact</a>
</body>
</html>
HTML

    cat > "${TARGET}/input/page-c.html" <<'HTML'
<!DOCTYPE html>
<html>
<head><title>Page C</title></head>
<body>
  <h1>Links from Page C</h1>
  <a href="https://nodejs.org">Node.js</a>
  <a href="https://npmjs.com">npm</a>
  <a href="https://github.com/expressjs/express">Express</a>
  <a href="https://github.com/facebook/react">React</a>
  <a href="https://python.org/downloads">Python Downloads</a>
</body>
</html>
HTML

    cat > "${TARGET}/README.md" <<'README'
# Link Harvester

Reads HTML files from the `input/` directory, extracts all `<a href>` links,
stores them in SQLite, and generates a markdown report.

## How it works

1. Scan all `.html` files in `input/`
2. Extract links (href, text, source file)
3. Store in SQLite (`data/links.db`)
4. Generate `report.md` with domain statistics

## Run

    docker compose run harvester
    cat report.md

## Test

    docker compose run --rm test
README

    (
        cd "${TARGET}"
        git init -q
        printf "data/*.db\nreport.md\n__pycache__/\n.pytest_cache/\n*.pyc\n" > .gitignore
        git add -A
        git commit -q -m "init: fixtures, Dockerfile, and Docker config"
    )
    ok "Python scraper target ready at ${TARGET}"
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
    echo "Build an HTML link harvester in Python. It reads all .html files from an 'input/' directory, extracts every <a href> link (href + link text + source filename), stores results in SQLite, and writes a markdown report to 'report.md' showing: total links found, unique domains, and top 10 domains by link count (descending). HTML fixtures are pre-provided in input/. Use only stdlib (html.parser, sqlite3, urllib.parse) plus beautifulsoup4 if needed. Include a pytest test suite in test_scraper.py. Validate with: docker compose run --rm test."
}

target_plan() {
    echo "${LOOPR_ROOT}/bin/e2e-targets/python-scraper.md"
}

collect_results() {
    for f in scraper.py database.py report.py main.py test_scraper.py; do
        if [[ -f "${TARGET}/${f}" ]]; then
            echo ""
            log "Target ${f}:"
            cat "${TARGET}/${f}"
        fi
    done

    if [[ -f "${TARGET}/report.md" ]]; then
        echo ""
        log "Generated report.md:"
        cat "${TARGET}/report.md"
    fi
}

verify() {
    local pass=true

    for f in scraper.py database.py report.py main.py test_scraper.py; do
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

    # Run the harvester and check the report
    echo ""
    if (cd "${TARGET}" && docker compose run --rm \
        -e DATABASE_PATH=/tmp/verify_links.db \
        -e REPORT_PATH=/tmp/verify_report.md \
        harvester 2>&1 | /usr/bin/tail -5); then
        ok "harvester ran without error"
    else
        warn "harvester run failed"
        pass=false
    fi

    if [[ -f "${TARGET}/report.md" ]]; then
        ok "report.md was generated"
        if grep -q "Total links" "${TARGET}/report.md"; then
            ok "report contains 'Total links'"
        else
            warn "report missing 'Total links' section"
            pass=false
        fi
        if grep -q "Top Domains" "${TARGET}/report.md"; then
            ok "report contains 'Top Domains'"
        else
            warn "report missing 'Top Domains' section"
            pass=false
        fi
    else
        warn "report.md was not generated"
        pass=false
    fi

    if [[ "${pass}" == "true" ]]; then
        ok "All verification checks passed"
    else
        warn "Some verification checks failed"
    fi
}
