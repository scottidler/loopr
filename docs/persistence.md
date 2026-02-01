# Persistence

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec

---

## Summary

Loopr uses TaskStore (JSONL + SQLite) for all persistent state. Multiple collections store different record types. The daemon owns the TaskStore; TUI reads state via daemon IPC.

---

## Storage Location

Per-project storage at:

```
~/.loopr/<project-hash>/
```

Project hash computed from git repo root path:

```rust
fn project_hash(repo_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_root.to_string_lossy().as_bytes());
    hex::encode(&hasher.finalize()[..8])  // First 8 bytes = 16 hex chars
}
```

---

## Directory Structure

```
~/.loopr/<project-hash>/
├── .taskstore/
│   ├── loops.jsonl           # Loop records
│   ├── signals.jsonl         # Coordination signals
│   ├── tool_jobs.jsonl       # Tool execution history
│   ├── events.jsonl          # Event stream (debugging)
│   └── taskstore.db          # SQLite index cache
├── loops/
│   └── <loop-id>/
│       ├── iterations/
│       │   └── 001/
│       │       ├── prompt.md
│       │       ├── conversation.jsonl
│       │       ├── validation.log
│       │       └── artifacts/
│       ├── stdout.log
│       ├── stderr.log
│       └── current -> iterations/NNN/
├── archive/                   # Invalidated loops
│   └── <loop-id>/
└── worktrees/                 # Git worktrees (transient)
    └── <loop-id>/
```

---

## Collections

### loops

All loops (Plan, Spec, Phase, Code). There is no separate "Loop" - `Loop` IS the record.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Loop {
    pub id: String,
    pub loop_type: LoopType,
    pub parent_id: Option<String>,
    pub input_artifact: Option<PathBuf>,
    pub output_artifacts: Vec<PathBuf>,
    pub prompt_path: PathBuf,
    pub validation_command: String,
    pub max_iterations: u32,
    pub worktree: PathBuf,
    pub iteration: u32,
    pub status: LoopStatus,
    pub progress: String,
    pub context: serde_json::Value,
    pub created_at: i64,
    pub updated_at: i64,
}
```

**Indexed fields:** `loop_type`, `status`, `parent_id`

### signals

Coordination signals between loops.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalRecord {
    pub id: String,
    pub signal_type: SignalType,
    pub source_loop: Option<String>,
    pub target_loop: Option<String>,
    pub target_selector: Option<String>,
    pub reason: String,
    pub payload: Option<Value>,
    pub created_at: i64,
    pub acknowledged_at: Option<i64>,
}
```

**Indexed fields:** `signal_type`, `target_loop`, `acknowledged_at`

### tool_jobs

Tool execution history for observability and replay.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolJobRecord {
    pub id: String,
    pub loop_id: String,
    pub tool_name: String,
    pub lane: String,
    pub command: String,
    pub cwd: PathBuf,
    pub status: ToolExitStatus,
    pub exit_code: Option<i32>,
    pub output_bytes: usize,
    pub was_timeout: bool,
    pub was_cancelled: bool,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_ms: Option<u64>,
}
```

**Indexed fields:** `loop_id`, `tool_name`, `status`, `lane`

### events

Event stream for debugging and replay.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: String,
    pub event_type: String,
    pub loop_id: Option<String>,
    pub tool_job_id: Option<String>,
    pub payload: Value,
    pub created_at: i64,
}
```

**Indexed fields:** `event_type`, `loop_id`

---

## TaskStore Design

TaskStore implements the Bead Store pattern:

- **JSONL** is source of truth (git-friendly, append-only)
- **SQLite** is derived cache (fast queries, rebuildable)
- **Write-through**: JSONL first, then SQLite

```rust
impl TaskStore {
    pub fn create<T: Record>(&self, record: &T) -> Result<()> {
        // 1. Append to JSONL
        let collection = T::collection_name();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path.join(format!("{}.jsonl", collection)))?;
        writeln!(file, "{}", serde_json::to_string(record)?)?;

        // 2. Insert into SQLite
        self.db.insert(collection, record)?;

        Ok(())
    }

    pub fn query<T: Record>(&self, filters: &[Filter]) -> Result<Vec<T>> {
        // Query from SQLite (fast)
        self.db.query(T::collection_name(), filters)
    }

    pub fn rebuild_index(&self) -> Result<()> {
        // Regenerate SQLite from JSONL files
        for collection in &["loops", "signals", "tool_jobs", "events"] {
            let file = self.path.join(format!("{}.jsonl", collection));
            if file.exists() {
                for line in BufReader::new(File::open(&file)?).lines() {
                    let record: Value = serde_json::from_str(&line?)?;
                    self.db.insert(collection, &record)?;
                }
            }
        }
        Ok(())
    }
}
```

---

## Daemon Responsibilities

- Open TaskStore on startup
- Write records on state changes
- Handle concurrent writes (file locking)
- Sync on shutdown

```rust
impl Daemon {
    async fn on_loop_status_change(&self, loop_id: &str, status: LoopStatus) {
        let mut record: Loop = self.store.get(loop_id)?.unwrap();
        record.status = status;
        record.updated_at = now_ms();
        self.store.update(&record)?;

        // Log event
        self.store.create(&EventRecord {
            id: generate_event_id(),
            event_type: "loop.status_change".to_string(),
            loop_id: Some(loop_id.to_string()),
            payload: json!({ "status": status }),
            created_at: now_ms(),
            ..Default::default()
        })?;

        // Notify TUIs
        self.notify_tuis(DaemonEvent::LoopUpdated(record));
    }
}
```

---

## TUI Access

TUI does **not** read TaskStore directly. All state comes from daemon:

```rust
impl TuiClient {
    pub async fn get_loops(&self) -> Result<Vec<Loop>> {
        let response = self.request("loop.list", json!({})).await?;
        Ok(serde_json::from_value(response["loops"].clone())?)
    }

    // State also pushed via events
    pub async fn handle_event(&mut self, event: DaemonEvent) {
        match event.event.as_str() {
            "loop.updated" => {
                let record: Loop = serde_json::from_value(event.data)?;
                self.state.update_loop(record);
            }
            _ => {}
        }
    }
}
```

---

## Cleanup and Retention

### Archive Cleanup

Invalidated loops are archived but not deleted immediately:

```rust
async fn cleanup_old_archives(&self) -> Result<()> {
    let archive_dir = self.path.join("archive");
    let retention = Duration::from_secs(7 * 24 * 60 * 60); // 7 days

    for entry in std::fs::read_dir(&archive_dir)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let age = metadata.modified()?.elapsed()?;

        if age > retention {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}
```

### Signal Pruning

Acknowledged signals can be pruned:

```rust
async fn prune_old_signals(&self) -> Result<()> {
    let cutoff = now_ms() - (7 * 24 * 60 * 60 * 1000); // 7 days

    self.store.delete_where::<SignalRecord>(&[
        Filter::is_not_null("acknowledged_at"),
        Filter::lt("acknowledged_at", cutoff),
    ])?;

    Ok(())
}
```

---

## Recovery

### On Daemon Start

```rust
impl Daemon {
    async fn recover(&mut self) -> Result<()> {
        // Find loops that were running when daemon stopped
        let interrupted = self.store.query::<Loop>(&[
            Filter::eq("status", "running"),
        ])?;

        for record in interrupted {
            tracing::info!(loop_id = %record.id, "Found interrupted loop");

            // Check if worktree still exists
            let worktree = self.worktree_path(&record.id);
            if worktree.exists() {
                // Can resume - mark as pending
                let mut updated = record.clone();
                updated.status = LoopStatus::Pending;
                self.store.update(&updated)?;
            } else {
                // Can't resume - mark as failed
                let mut updated = record.clone();
                updated.status = LoopStatus::Failed;
                self.store.update(&updated)?;
            }
        }

        Ok(())
    }
}
```

### SQLite Corruption

If SQLite index is corrupted, rebuild from JSONL:

```rust
impl TaskStore {
    pub fn ensure_valid(&self) -> Result<()> {
        match self.db.query_test() {
            Ok(_) => Ok(()),
            Err(_) => {
                tracing::warn!("SQLite index corrupted, rebuilding...");
                std::fs::remove_file(self.path.join("taskstore.db"))?;
                self.rebuild_index()
            }
        }
    }
}
```

---

## References

- [domain-types.md](domain-types.md) - Full record schemas
- [loop-coordination.md](loop-coordination.md) - Signal usage
- [architecture.md](architecture.md) - System overview
