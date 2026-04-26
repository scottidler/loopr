# Python Link Harvester

## Problem Statement

The project needs an HTML link harvester in Python. It reads all `.html` files from
an `input/` directory, extracts every `<a href>` link (href, link text, source
filename), stores results in SQLite, and generates a markdown report showing total
links, unique domains, and the top 10 domains by link count. HTML fixtures are
pre-provided in `input/`. Validation: `docker compose run --rm test`.

## Goals

- The harvester processes all `.html` files in the configured input directory
- Each link's href, text, and source filename are stored in SQLite
- A markdown report is generated with total links, unique domains, and top 10 domains
- A pytest suite covers parsing, storage, report generation, and directory harvesting
- The harvester builds and runs in Docker via `docker compose`

## Requirements

| As a... | I want to... | So that... |
|---------|-------------|-----------|
| user | run `docker compose run harvester` | all links from `input/*.html` are extracted and stored |
| user | read `report.md` after harvesting | I see total link count, unique domain count, and top 10 domains |
| developer | set `DATABASE_PATH` env var | the harvester reads and writes to that SQLite file |
| developer | set `INPUT_DIR` env var | the harvester reads from that directory |
| developer | set `REPORT_PATH` env var | the report is written to that file path |
| developer | `docker compose run --rm test` | all 7 pytest tests pass in a clean container |

## Scope

- `database.py`: SQLite storage module using stdlib `sqlite3`
- `scraper.py`: HTML link extraction using stdlib `html.parser`
- `report.py`: markdown report generator
- `main.py`: entry point that orchestrates harvest and report generation
- `test_scraper.py`: pytest suite covering all modules

## Constraints

- Python 3.12. Standard library only for parsing and storage: `html.parser`, `sqlite3`,
  `urllib.parse`. `pytest` from `requirements.txt`.
- `input/` fixtures (page-a.html, page-b.html, page-c.html) are pre-provided in the scaffold.
- Environment variables: `INPUT_DIR` (default `input`), `DATABASE_PATH` (default
  `data/links.db`), `REPORT_PATH` (default `report.md`).
- Containerized: `Dockerfile` + `docker-compose.yml` are pre-provided.
- Test isolation: all tests pass a `tmp_path`-based db path; never mutate global state.

## Contracts

### Data Model

Links stored in SQLite as:

```
links table:
  id          INTEGER PRIMARY KEY AUTOINCREMENT
  source_file TEXT NOT NULL
  href        TEXT NOT NULL
  text        TEXT NOT NULL DEFAULT ''
```

Row dicts returned by `list_links` have keys: `id`, `source_file`, `href`, `text`.

### Processing Pipeline

```
html files in INPUT_DIR
  -> scraper.extract_links(html_content, source_file) -> list of {source_file, href, text}
  -> database.insert_links(db_path, links)
  -> database.domain_counts(db_path) -> [(domain, count), ...] sorted descending
  -> report.generate_report(db_path, output_path) -> markdown string
```

`domain_counts` extracts domains via `urllib.parse.urlparse(href).netloc`.
Hrefs with no netloc (relative links, anchors, `mailto:`) are excluded.
Report shows top 10 domains only.

## Acceptance Criteria

| Given... | When... | Then... |
|----------|---------|---------|
| an HTML string with one `<a href>` | `extract_links` runs | returns a list with one dict containing `source_file`, `href`, and `text` |
| an `<a href="">` (empty href) | `extract_links` runs | the empty href is excluded from results |
| a link with surrounding whitespace | `extract_links` runs | `text` is stripped |
| links inserted into the database | `count_links` runs | returns the correct total count |
| a mix of absolute and relative hrefs | `domain_counts` runs | relative and anchor hrefs are excluded from domain counts |
| the pre-provided `input/` directory | `harvest_directory` runs | returns a positive integer count of inserted links |
| a harvested database | `generate_report` runs | the markdown file contains `Total links` and `Top Domains` sections |
| pytest runs | `docker compose run --rm test` | all 7 tests pass and exit code is 0 |

### Final Validation

```
docker compose run --rm test
```

## Specs

- **Parser and database layer** - `scraper.py` and `database.py`:
  - `database.py`: `init_db(db_path)`, `insert_links(db_path, links)`, `count_links(db_path)`,
    `list_links(db_path)`, `domain_counts(db_path)`. `init_db` creates the parent directory
    if needed.
  - `scraper.py`: `LinkParser(HTMLParser)` collects `(href, text)` pairs. `extract_links(html, source_file)`
    returns list of dicts, skipping empty hrefs. `harvest_directory(input_dir, db_path)` reads
    all `.html` files sorted, calls `extract_links`, calls `database.insert_links`, returns total count.

- **Report and entry point** - `report.py` and `main.py`:
  - `report.py`: `generate_report(db_path, output_path)` writes a markdown report with a
    Summary section (total links, unique domains) and a Top Domains table (top 10 by count).
    Returns the markdown string.
  - `main.py`: reads env vars, calls `scraper.harvest_directory`, calls `report.generate_report`,
    prints progress messages. Used as Docker CMD.

- **Test suite** - `test_scraper.py`: pytest using `tmp_path` for all database paths.
  Tests: `test_extract_links_finds_hrefs`, `test_extract_links_skips_empty_href`,
  `test_extract_links_captures_text`, `test_insert_and_count`, `test_domain_counts`,
  `test_harvest_directory` (uses pre-provided `input/`), `test_generate_report`.

## Dependencies

- `pytest>=8.3` (in `requirements.txt`)
- Docker + docker compose (pre-installed in the scaffold environment)
- HTML fixture files `input/page-a.html`, `input/page-b.html`, `input/page-c.html`
  (pre-provided in the scaffold)
