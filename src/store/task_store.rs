//! TaskStore implementation with JSONL append log and SQLite index.
//!
//! The TaskStore provides persistence for loop records using a dual-storage approach:
//! - **JSONL file**: Append-only log of all record changes (source of truth)
//! - **SQLite database**: Query index for fast lookups (rebuilt from JSONL on startup)
//!
//! This design ensures durability (JSONL is simple and crash-safe) while enabling
//! efficient queries (SQLite indexes on loop_type, status, parent_loop).

use crate::store::records::{IndexValue, LoopRecord, LoopStatus, LoopType};
use eyre::{Context, Result};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// TaskStore manages loop records with JSONL persistence and SQLite indexing.
pub struct TaskStore {
    /// Base directory for this project's store
    base_dir: PathBuf,

    /// Path to the JSONL file
    jsonl_path: PathBuf,

    /// SQLite connection for queries
    db: Connection,
}

impl TaskStore {
    /// Open or create a TaskStore for the given project directory.
    ///
    /// The store is created at `~/.loopr/<project-hash>/.taskstore/`.
    pub fn open(project_dir: &Path) -> Result<Self> {
        let project_hash = compute_project_hash(project_dir)?;
        let loopr_dir = dirs::home_dir()
            .ok_or_else(|| eyre::eyre!("Cannot determine home directory"))?
            .join(".loopr")
            .join(&project_hash);

        Self::open_at(&loopr_dir)
    }

    /// Open or create a TaskStore at the specified directory.
    ///
    /// Useful for testing with custom paths.
    pub fn open_at(base_dir: &Path) -> Result<Self> {
        let store_dir = base_dir.join(".taskstore");
        fs::create_dir_all(&store_dir)
            .with_context(|| format!("Failed to create store directory: {}", store_dir.display()))?;

        let jsonl_path = store_dir.join("loops.jsonl");
        let db_path = store_dir.join("taskstore.db");

        // Open SQLite database
        let db = Connection::open(&db_path)
            .with_context(|| format!("Failed to open SQLite database: {}", db_path.display()))?;

        // Initialize schema
        Self::init_schema(&db)?;

        let mut store = Self {
            base_dir: base_dir.to_path_buf(),
            jsonl_path,
            db,
        };

        // Rebuild index from JSONL if needed
        store.rebuild_index_if_needed()?;

        Ok(store)
    }

    /// Initialize the SQLite schema.
    fn init_schema(db: &Connection) -> Result<()> {
        db.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS loops (
                id TEXT PRIMARY KEY,
                loop_type TEXT NOT NULL,
                status TEXT NOT NULL,
                parent_loop TEXT,
                iteration INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                json_data TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_loops_type ON loops(loop_type);
            CREATE INDEX IF NOT EXISTS idx_loops_status ON loops(status);
            CREATE INDEX IF NOT EXISTS idx_loops_parent ON loops(parent_loop);
            CREATE INDEX IF NOT EXISTS idx_loops_created ON loops(created_at);

            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )
        .context("Failed to initialize database schema")?;

        Ok(())
    }

    /// Rebuild the SQLite index from the JSONL file if needed.
    fn rebuild_index_if_needed(&mut self) -> Result<()> {
        if !self.jsonl_path.exists() {
            return Ok(());
        }

        // Check if we need to rebuild (compare line counts)
        let jsonl_lines = self.count_jsonl_lines()?;
        let db_count: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM loops", [], |row| row.get(0))
            .unwrap_or(0);

        // Simple heuristic: if JSONL has more entries, rebuild
        // In a real implementation, we'd track a checksum
        if jsonl_lines as i64 > db_count || db_count == 0 {
            self.rebuild_index()?;
        }

        Ok(())
    }

    /// Count lines in the JSONL file.
    fn count_jsonl_lines(&self) -> Result<usize> {
        let file = File::open(&self.jsonl_path)?;
        let reader = BufReader::new(file);
        Ok(reader.lines().count())
    }

    /// Rebuild the entire SQLite index from the JSONL file.
    fn rebuild_index(&mut self) -> Result<()> {
        // Clear existing data
        self.db.execute("DELETE FROM loops", [])?;

        if !self.jsonl_path.exists() {
            return Ok(());
        }

        let file = File::open(&self.jsonl_path)?;
        let reader = BufReader::new(file);

        // Use a HashMap to track the latest version of each record
        let mut records: std::collections::HashMap<String, LoopRecord> = std::collections::HashMap::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            let record: LoopRecord = serde_json::from_str(&line).context("Failed to parse JSONL line")?;
            records.insert(record.id.clone(), record);
        }

        // Insert all records into SQLite
        let tx = self.db.transaction()?;
        for record in records.values() {
            Self::insert_record_into_db(&tx, record)?;
        }
        tx.commit()?;

        Ok(())
    }

    /// Insert a record into the SQLite database.
    fn insert_record_into_db(db: &Connection, record: &LoopRecord) -> Result<()> {
        let json_data = serde_json::to_string(record)?;

        db.execute(
            r#"
            INSERT OR REPLACE INTO loops
            (id, loop_type, status, parent_loop, iteration, created_at, updated_at, json_data)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                record.id,
                record.loop_type.as_str(),
                record.status.as_str(),
                record.parent_loop,
                record.iteration,
                record.created_at,
                record.updated_at,
                json_data,
            ],
        )?;

        Ok(())
    }

    /// Save a new loop record.
    pub fn save(&mut self, record: &LoopRecord) -> Result<()> {
        // Append to JSONL
        let json = serde_json::to_string(record)?;
        let mut file = OpenOptions::new().create(true).append(true).open(&self.jsonl_path)?;
        writeln!(file, "{}", json)?;

        // Update SQLite index
        Self::insert_record_into_db(&self.db, record)?;

        Ok(())
    }

    /// Update an existing loop record.
    pub fn update(&mut self, record: &LoopRecord) -> Result<()> {
        // Same as save - JSONL is append-only, SQLite uses REPLACE
        self.save(record)
    }

    /// Get a loop record by ID.
    pub fn get(&self, id: &str) -> Result<Option<LoopRecord>> {
        let result = self
            .db
            .query_row("SELECT json_data FROM loops WHERE id = ?1", [id], |row| {
                let json: String = row.get(0)?;
                Ok(json)
            });

        match result {
            Ok(json) => {
                let record: LoopRecord = serde_json::from_str(&json)?;
                Ok(Some(record))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List all loop records.
    pub fn list_all(&self) -> Result<Vec<LoopRecord>> {
        let mut stmt = self.db.prepare("SELECT json_data FROM loops ORDER BY created_at")?;
        let rows = stmt.query_map([], |row| {
            let json: String = row.get(0)?;
            Ok(json)
        })?;

        let mut records = Vec::new();
        for row in rows {
            let json = row?;
            let record: LoopRecord = serde_json::from_str(&json)?;
            records.push(record);
        }

        Ok(records)
    }

    /// List loop records by status.
    pub fn list_by_status(&self, status: LoopStatus) -> Result<Vec<LoopRecord>> {
        let mut stmt = self
            .db
            .prepare("SELECT json_data FROM loops WHERE status = ?1 ORDER BY created_at")?;
        let rows = stmt.query_map([status.as_str()], |row| {
            let json: String = row.get(0)?;
            Ok(json)
        })?;

        let mut records = Vec::new();
        for row in rows {
            let json = row?;
            let record: LoopRecord = serde_json::from_str(&json)?;
            records.push(record);
        }

        Ok(records)
    }

    /// List loop records by type.
    pub fn list_by_type(&self, loop_type: LoopType) -> Result<Vec<LoopRecord>> {
        let mut stmt = self
            .db
            .prepare("SELECT json_data FROM loops WHERE loop_type = ?1 ORDER BY created_at")?;
        let rows = stmt.query_map([loop_type.as_str()], |row| {
            let json: String = row.get(0)?;
            Ok(json)
        })?;

        let mut records = Vec::new();
        for row in rows {
            let json = row?;
            let record: LoopRecord = serde_json::from_str(&json)?;
            records.push(record);
        }

        Ok(records)
    }

    /// List child loops of a parent.
    pub fn list_children(&self, parent_id: &str) -> Result<Vec<LoopRecord>> {
        let mut stmt = self
            .db
            .prepare("SELECT json_data FROM loops WHERE parent_loop = ?1 ORDER BY created_at")?;
        let rows = stmt.query_map([parent_id], |row| {
            let json: String = row.get(0)?;
            Ok(json)
        })?;

        let mut records = Vec::new();
        for row in rows {
            let json = row?;
            let record: LoopRecord = serde_json::from_str(&json)?;
            records.push(record);
        }

        Ok(records)
    }

    /// List pending loops that are ready to run.
    pub fn list_runnable(&self) -> Result<Vec<LoopRecord>> {
        let mut stmt = self
            .db
            .prepare("SELECT json_data FROM loops WHERE status IN ('pending', 'paused') ORDER BY created_at")?;
        let rows = stmt.query_map([], |row| {
            let json: String = row.get(0)?;
            Ok(json)
        })?;

        let mut records = Vec::new();
        for row in rows {
            let json = row?;
            let record: LoopRecord = serde_json::from_str(&json)?;
            records.push(record);
        }

        Ok(records)
    }

    /// Count loops by status.
    pub fn count_by_status(&self, status: LoopStatus) -> Result<usize> {
        let count: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM loops WHERE status = ?1",
            [status.as_str()],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Get the base directory for this store.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Get the path to the loops directory.
    pub fn loops_dir(&self) -> PathBuf {
        self.base_dir.join("loops")
    }

    /// Get the path for a specific loop's directory.
    pub fn loop_dir(&self, loop_id: &str) -> PathBuf {
        self.loops_dir().join(loop_id)
    }
}

/// Compute a hash of the project directory path for storage isolation.
pub fn compute_project_hash(project_dir: &Path) -> Result<String> {
    let canonical = project_dir
        .canonicalize()
        .with_context(|| format!("Failed to canonicalize path: {}", project_dir.display()))?;

    let path_str = canonical.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(path_str.as_bytes());
    let result = hasher.finalize();

    // Take first 16 chars of hex
    Ok(hex::encode(&result[..8]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_temp_store() -> (TaskStore, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let store = TaskStore::open_at(temp_dir.path()).unwrap();
        (store, temp_dir)
    }

    #[test]
    fn test_open_creates_directories() {
        let temp_dir = TempDir::new().unwrap();
        let _store = TaskStore::open_at(temp_dir.path()).unwrap();

        assert!(temp_dir.path().join(".taskstore").exists());
        assert!(temp_dir.path().join(".taskstore/taskstore.db").exists());
    }

    #[test]
    fn test_save_and_get() {
        let (mut store, _temp) = create_temp_store();

        let record = LoopRecord::new_plan("Test task", 10);
        let id = record.id.clone();

        store.save(&record).unwrap();

        let retrieved = store.get(&id).unwrap().unwrap();
        assert_eq!(retrieved.id, id);
        assert_eq!(retrieved.loop_type, LoopType::Plan);
        assert_eq!(retrieved.context["task"], "Test task");
    }

    #[test]
    fn test_get_nonexistent() {
        let (store, _temp) = create_temp_store();
        let result = store.get("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_update_record() {
        let (mut store, _temp) = create_temp_store();

        let mut record = LoopRecord::new_plan("Test task", 10);
        let id = record.id.clone();

        store.save(&record).unwrap();

        record.status = LoopStatus::Running;
        record.iteration = 1;
        store.update(&record).unwrap();

        let retrieved = store.get(&id).unwrap().unwrap();
        assert_eq!(retrieved.status, LoopStatus::Running);
        assert_eq!(retrieved.iteration, 1);
    }

    #[test]
    fn test_list_all() {
        let (mut store, _temp) = create_temp_store();

        let record1 = LoopRecord::new_plan("Task 1", 10);
        let record2 = LoopRecord::new_ralph("Task 2", 5);

        store.save(&record1).unwrap();
        store.save(&record2).unwrap();

        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_list_by_status() {
        let (mut store, _temp) = create_temp_store();

        let mut record1 = LoopRecord::new_plan("Task 1", 10);
        record1.status = LoopStatus::Running;

        let record2 = LoopRecord::new_plan("Task 2", 10);

        store.save(&record1).unwrap();
        store.save(&record2).unwrap();

        let pending = store.list_by_status(LoopStatus::Pending).unwrap();
        assert_eq!(pending.len(), 1);

        let running = store.list_by_status(LoopStatus::Running).unwrap();
        assert_eq!(running.len(), 1);
    }

    #[test]
    fn test_list_by_type() {
        let (mut store, _temp) = create_temp_store();

        let record1 = LoopRecord::new_plan("Plan task", 10);
        let record2 = LoopRecord::new_ralph("Ralph task", 5);

        store.save(&record1).unwrap();
        store.save(&record2).unwrap();

        let plans = store.list_by_type(LoopType::Plan).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].context["task"], "Plan task");

        let ralphs = store.list_by_type(LoopType::Ralph).unwrap();
        assert_eq!(ralphs.len(), 1);
    }

    #[test]
    fn test_list_children() {
        let (mut store, _temp) = create_temp_store();

        let parent = LoopRecord::new_plan("Parent", 10);
        let parent_id = parent.id.clone();

        let child1 = LoopRecord::new_spec(&parent_id, "Content 1", 10);
        let child2 = LoopRecord::new_spec(&parent_id, "Content 2", 10);
        let orphan = LoopRecord::new_ralph("Orphan", 5);

        store.save(&parent).unwrap();
        store.save(&child1).unwrap();
        store.save(&child2).unwrap();
        store.save(&orphan).unwrap();

        let children = store.list_children(&parent_id).unwrap();
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn test_list_runnable() {
        let (mut store, _temp) = create_temp_store();

        let mut running = LoopRecord::new_plan("Running", 10);
        running.status = LoopStatus::Running;

        let pending = LoopRecord::new_plan("Pending", 10);

        let mut paused = LoopRecord::new_plan("Paused", 10);
        paused.status = LoopStatus::Paused;

        let mut complete = LoopRecord::new_plan("Complete", 10);
        complete.status = LoopStatus::Complete;

        store.save(&running).unwrap();
        store.save(&pending).unwrap();
        store.save(&paused).unwrap();
        store.save(&complete).unwrap();

        let runnable = store.list_runnable().unwrap();
        assert_eq!(runnable.len(), 2); // pending + paused
    }

    #[test]
    fn test_count_by_status() {
        let (mut store, _temp) = create_temp_store();

        let record1 = LoopRecord::new_plan("Task 1", 10);
        let record2 = LoopRecord::new_plan("Task 2", 10);

        let mut running = LoopRecord::new_plan("Running", 10);
        running.status = LoopStatus::Running;

        store.save(&record1).unwrap();
        store.save(&record2).unwrap();
        store.save(&running).unwrap();

        assert_eq!(store.count_by_status(LoopStatus::Pending).unwrap(), 2);
        assert_eq!(store.count_by_status(LoopStatus::Running).unwrap(), 1);
    }

    #[test]
    fn test_jsonl_persistence() {
        let temp_dir = TempDir::new().unwrap();

        // Create and save
        {
            let mut store = TaskStore::open_at(temp_dir.path()).unwrap();
            let record = LoopRecord::new_plan("Persistent task", 10);
            store.save(&record).unwrap();
        }

        // Reopen and verify
        {
            let store = TaskStore::open_at(temp_dir.path()).unwrap();
            let all = store.list_all().unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].context["task"], "Persistent task");
        }
    }

    #[test]
    fn test_rebuild_index() {
        let temp_dir = TempDir::new().unwrap();

        // Create some records
        {
            let mut store = TaskStore::open_at(temp_dir.path()).unwrap();
            store.save(&LoopRecord::new_plan("Task 1", 10)).unwrap();
            store.save(&LoopRecord::new_plan("Task 2", 10)).unwrap();
        }

        // Delete the SQLite file to force rebuild
        let db_path = temp_dir.path().join(".taskstore/taskstore.db");
        fs::remove_file(&db_path).unwrap();

        // Reopen - should rebuild from JSONL
        {
            let store = TaskStore::open_at(temp_dir.path()).unwrap();
            let all = store.list_all().unwrap();
            assert_eq!(all.len(), 2);
        }
    }

    #[test]
    fn test_compute_project_hash() {
        let temp_dir = TempDir::new().unwrap();
        let hash = compute_project_hash(temp_dir.path()).unwrap();

        // Hash should be 16 hex characters
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

        // Same path should produce same hash
        let hash2 = compute_project_hash(temp_dir.path()).unwrap();
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_loop_dir_paths() {
        let (store, temp) = create_temp_store();

        assert_eq!(store.base_dir(), temp.path());
        assert_eq!(store.loops_dir(), temp.path().join("loops"));
        assert_eq!(store.loop_dir("12345"), temp.path().join("loops").join("12345"));
    }
}
