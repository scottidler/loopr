# TaskStore Data Architecture

## Gospel: JSONL is truth, SQLite is cache

**TaskStore (JSONL files) is the authoritative data store.**
**SQLite is a high-speed read cache rebuilt from JSONL.**

This is not a preference. It is the architectural invariant of this codebase.

### What this means in practice

- JSONL is written first, always. If JSONL write fails, stop — do not write SQLite.
- SQLite is derived state. It can be dropped and rebuilt at any time from the JSONL files.
- The in-memory `HashMap` stores in `Stores` are a runtime cache of what is in JSONL.
  They are populated at daemon startup by reading JSONL, and kept in sync on every write.
- Never treat in-memory state as authoritative. It is a read replica.

### Write ordering

Every write to any domain record must follow this sequence:

1. Acquire the in-memory store lock (`write_agent_sessions()`, `write_works()`, etc.)
2. Check any preconditions (pool count, dedup, status transitions) — all under the lock
3. **While still holding the lock**, acquire the TaskStore lock and call `store.create()` or `store.update()`
4. If `store.create()` succeeds, insert/update the in-memory map
5. Drop the lock

Steps 3 and 4 must be atomic under the same lock. Never release the in-memory lock between
the TaskStore write and the in-memory insert.

### Why

If the in-memory insert happens before `store.create()`, and the process crashes between the
two, the record is lost — it exists in memory but not in JSONL. On the next daemon startup,
JSONL is replayed and the record is gone. Memory state was never durable.

If `store.create()` happens before the in-memory insert and the process crashes between the
two, the record exists in JSONL. On the next startup, JSONL replay restores it. JSONL state
was durable.

### Enforcement

When writing any new handler or modifying an existing one:

- Do not call `sessions.insert()`, `works.insert()`, or any in-memory map mutation
  before calling `store.create()` / `store.update()`.
- Do not release the in-memory write lock between the `store` call and the map mutation.
- If you see code that violates this ordering, fix it — do not add more code on top of it.

### Rebuilding SQLite from JSONL

SQLite can always be rebuilt:

```bash
loopr store rebuild   # (planned) replays JSONL → repopulates SQLite
```

Until that command exists: delete `taskstore.db`, restart the daemon. JSONL replay
re-populates the cache on startup.

### Scope

This applies to all domain records managed through `Stores`:
- `agent_sessions`, `works`, `bundles`, `plans`, `specs`, `phases`
- `coordinator_goals`, `coordinator_states`, `ticks`, `locks`, `docs`

It does NOT apply to read-only operations, event broadcasting, or config loading.
