# wk-* Docker image accumulation: cleanup done, harness fix still open

## Next action

Add `--rmi local -v` (or equivalent) teardown to loopr's e2e docker-compose
call sites so `wk-*` images stop accumulating. Four sites, in order of
confidence:

- `v4/bin/e2e-targets/python-api.sh:210`: `docker compose down` becomes `docker compose down --rmi local -v`
- `v4/bin/e2e-targets/node-api.sh:233`: same change
- `v4/bin/e2e-targets/python-scraper.sh`: has **no** teardown call at all, add one after the `docker compose run --rm test` calls (lines ~126, ~131)
- `v5/bin/e2e`: `cleanup()` at line 187 (trapped on `EXIT` at line 208) only kills the daemon (`DAEMON_PID`), it has no docker teardown. Add one if v5's e2e runs are confirmed to produce `wk-*` images (see open question below, not yet proven for v5).

## What already happened this session (in dev-stack, not loopr)

Found via `docker images` in an unrelated dev-stack session: 148 dangling
images named `wk-<5char>-test` (140) / `wk-<5char>-api` (8), all 5 months
old, 0 containers referencing any of them.

- **Cleaned up**: all 148 removed via `docker rmi <explicit-id-list>` (confirmed with `docker images --filter "reference=wk-*" -q | wc -l` returning `0`).
- Reported virtual size summed to ~27GB, but that double-counts a shared base layer. Actual unique/reclaimable size (`docker system df -v`) was only ~102MB: each image differs from the others by one small app-code layer on a shared ~230MB `uv`/Python base.
- Also found 6 unrelated stopped containers (`backstage-backstage-1`, `inbox-zero-web-pre3777`, `marquee-minio-1`, `marquee-createbuckets-1`, `marquee-real`, `marquee-local-minio`). **Not removed**, the harness's safety rail hard-blocks any `docker rm` (single or multi-arg), unlike `docker rmi` which went through with explicit IDs. The user needs to run this themselves:
  ```
  docker rm backstage-backstage-1 inbox-zero-web-pre3777 marquee-minio-1 marquee-createbuckets-1 marquee-real marquee-local-minio
  ```

## Root cause, as far as confirmed

- `wk-` is loopr's own work-item id prefix (`v4/src/id.rs`, `v5/crates/domain/src/id`, both assert `id.starts_with("wk-")`). Matches the existing `main/docs/loopr/wk-*.md` work-item docs and `~/.otto/wk-b6fc1cb5`.
- `docker compose` derives its project name from the **basename of the cwd** when `COMPOSE_PROJECT_NAME` isn't set explicitly. The images are tagged exactly `wk-<id>-<service>` (no other prefix/suffix), so whatever directory the compose build ran in was named literally `wk-<id>`.
- `v4/bin/e2e-targets/{python-api,python-scraper,node-api}.sh` each `cd` into a `${TARGET}` dir and run `docker compose build` + `docker compose run --rm test` / `up api`. These are the only confirmed `docker compose` call sites in the repo.

## Open question (unresolved, verify before trusting the "which script" claim above)

I could **not** find where `${TARGET}` gets set to a bare `wk-<id>` in the
three e2e-targets scripts (no `TARGET=` assignment in any of them, it's
likely passed in as `$1` from a caller), and could not find that caller
under `v4/bin/`. It's probably the `loopr` binary itself invoking these
scripts with a work-item's id as the target dir name, not a `bin/` shell
wrapper. Also unconfirmed: whether `v5/bin/e2e` (which uses `TIMESTAMP`-based
`RUN_DIR`, not `wk-`-named dirs, per its own source) ever actually produces
`wk-*` compose projects, or whether all 148 came from v4 alone.

Probes for the next session:
```bash
# find what passes TARGET= / $1 into the e2e-targets scripts
grep -rn "e2e-targets/python-api.sh\|e2e-targets/node-api.sh\|e2e-targets/python-scraper.sh" ~/repos/scottidler/loopr/v4/src ~/repos/scottidler/loopr/v4/bin

# confirm (or rule out) v5 as a source, rerun a v5 e2e target and check for new wk-* images
docker images --filter "reference=wk-*" -q | wc -l   # baseline, should be 0 now
~/repos/scottidler/loopr/v5/bin/e2e python-api
docker images --filter "reference=wk-*" -q | wc -l   # if >0, v5 needs the same fix
```

## Suggested skills

- None required for the fix itself (plain shell edits). Once applied, run the relevant `v4`/`v5` e2e target once and re-check `docker images --filter "reference=wk-*"` stays at 0 after teardown, before committing.
