use std::collections::HashSet;
use std::path::Path;

use indexfox_core::{normalize_search_text, ExtractedDocument, FileState, SearchMode, SourceKind};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::dictionary;
use crate::grams::{bigrams_for_query, short_grams_for_chunk, trigrams_for_query};
use crate::migrations;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("database schema version {found} is newer than supported {supported}")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("file id {0} not found")]
    FileNotFound(i64),
    #[error("source id {0} not found")]
    SourceNotFound(i64),
    #[error("index job {0} not found")]
    JobNotFound(i64),
    #[error("invalid index job state {0:?}; expected queued, running, failed or done")]
    InvalidJobState(String),
    #[error("invalid source state {0:?}; expected offline, failed, online or unknown")]
    InvalidSourceState(String),
    #[error("attempt_count must be non-negative, got {0}")]
    NegativeAttemptCount(i64),
    #[error("file {file_id} belongs to source {file_source_id}, not job source {job_source_id}")]
    JobFileSourceMismatch {
        file_id: i64,
        file_source_id: i64,
        job_source_id: i64,
    },
    #[error("search term contains a NUL character")]
    NulTerm,
}

pub type Result<T, E = IndexError> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFile {
    pub source_id: i64,
    pub path_key: String,
    pub display_path: String,
    pub filename: String,
    pub ext: String,
    pub size_bytes: i64,
    pub mtime_ns: Option<i64>,
    pub state: FileState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub id: i64,
    pub source_id: i64,
    pub path_key: String,
    pub display_path: String,
    pub filename: String,
    pub ext: String,
    pub size_bytes: i64,
    pub mtime_ns: Option<i64>,
    pub state: String,
    pub indexed_at: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRecord {
    pub id: i64,
    pub file_id: i64,
    pub ordinal: i64,
    /// Normalized text used for verification and matching.
    pub text: String,
    pub display_text: String,
    pub locator_json: String,
    pub origin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexemeCandidate {
    pub id: i64,
    pub text: String,
    pub length: i64,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexemeOccurrence {
    pub chunk_id: i64,
    pub file_id: i64,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRecord {
    pub id: i64,
    pub kind: SourceKind,
    pub root_path: String,
    pub enabled: bool,
    pub online_state: String,
    pub scan_mode: String,
    pub last_success_scan_at: Option<String>,
    pub config_json: String,
    pub scan_generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHistoryRecord {
    pub id: i64,
    pub query_text: String,
    pub search_mode: SearchMode,
    pub filters_json: String,
    pub last_used_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexJobRecord {
    pub id: i64,
    pub file_id: Option<i64>,
    pub source_id: i64,
    pub kind: String,
    pub state: String,
    pub attempt_count: i64,
    pub requested_at: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub sources: i64,
    pub files: i64,
    pub indexed_files: i64,
    pub skipped_files: i64,
    pub failed_files: i64,
    pub stale_files: i64,
    pub chunks: i64,
    pub short_grams: i64,
    pub lexemes: i64,
    pub lexeme_grams: i64,
    pub lexeme_occurrences: i64,
    /// Logical database size: `page_count * page_size`.
    ///
    /// Covers schema, `chunks`, FTS5, `short_grams`, all other tables and free
    /// pages, including pages already committed to the WAL but not yet
    /// checkpointed. Excludes WAL file framing/SHM overhead, uncommitted WAL
    /// frames and SQLite page-cache memory.
    pub db_size_bytes: i64,
    pub db_page_size: i64,
    pub db_page_count: i64,
}

pub type ChunkContext = (Option<ChunkRecord>, ChunkRecord, Option<ChunkRecord>);

pub struct IndexDb {
    conn: Connection,
}

impl IndexDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        configure(&conn)?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        migrations::migrate(&mut self.conn)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Insert a source or return the existing one for the same kind + root.
    pub fn ensure_source(&self, kind: SourceKind, root_path: &str) -> Result<i64> {
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT id FROM sources WHERE kind = ?1 AND root_path = ?2",
                params![kind.as_str(), root_path],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        {
            return Ok(id);
        }
        self.conn.execute(
            "INSERT INTO sources(id, kind, root_path)
             VALUES (1 + MAX(
                COALESCE((SELECT MAX(id) FROM sources), 0),
                COALESCE((SELECT CAST(value AS INTEGER) FROM app_meta
                          WHERE key = 'source_id_high_watermark'), 0)
             ), ?1, ?2)",
            params![kind.as_str(), root_path],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn set_source_online(&self, source_id: i64, online: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE sources SET online_state = ?2, last_success_scan_at = CASE WHEN ?2 = 'online' THEN ?3 ELSE last_success_scan_at END WHERE id = ?1",
            params![source_id, if online { "online" } else { "offline" }, now()],
        )?;
        Ok(())
    }

    /// Start a scan generation. The increment and the `scanning` state change
    /// are one atomic statement, so concurrent starts produce distinct
    /// generations and older generations can no longer commit.
    pub fn begin_source_scan(&self, source_id: i64) -> Result<i64> {
        let generation: Option<i64> = self
            .conn
            .query_row(
                "UPDATE sources
                 SET scan_generation = scan_generation + 1, online_state = 'scanning'
                 WHERE id = ?1
                 RETURNING scan_generation",
                params![source_id],
                |row| row.get(0),
            )
            .optional()?;
        generation.ok_or(IndexError::SourceNotFound(source_id))
    }

    /// Commit a completed scan generation. Only the source's current
    /// `scanning` generation may commit; stale or aborted generations return
    /// `None` and change nothing. The seen markers, removal of unseen files
    /// and their jobs, and the online/success timestamp share one transaction.
    pub fn finish_source_scan(
        &mut self,
        source_id: i64,
        generation: i64,
        seen: &HashSet<String>,
    ) -> Result<Option<usize>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(i64, String)> = tx
            .query_row(
                "SELECT scan_generation, online_state FROM sources WHERE id = ?1",
                params![source_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((current_generation, state)) = current else {
            return Err(IndexError::SourceNotFound(source_id));
        };
        if current_generation != generation || state != "scanning" {
            return Ok(None);
        }

        prepare_seen_keys(&tx, seen)?;
        tx.execute(
            "UPDATE files SET seen_generation = ?2
             WHERE source_id = ?1
               AND path_key IN (SELECT path_key FROM seen_path_keys)",
            params![source_id, generation],
        )?;
        let removed = remove_unseen_source_files(&tx, source_id)?;
        tx.execute(
            "UPDATE sources
             SET online_state = 'online', last_success_scan_at = ?2
             WHERE id = ?1 AND scan_generation = ?3 AND online_state = 'scanning'",
            params![source_id, now(), generation],
        )?;
        tx.execute("DROP TABLE seen_path_keys", [])?;
        tx.commit()?;
        Ok(Some(removed))
    }

    /// Abort the current scan generation without touching file rows or their
    /// index. Stale generations and non-`scanning` sources are no-ops.
    pub fn abort_source_scan(&self, source_id: i64, generation: i64, state: &str) -> Result<()> {
        if !matches!(state, "offline" | "failed" | "online" | "unknown") {
            return Err(IndexError::InvalidSourceState(state.to_string()));
        }
        let changed = self.conn.execute(
            "UPDATE sources SET online_state = ?3
             WHERE id = ?1 AND scan_generation = ?2 AND online_state = 'scanning'",
            params![source_id, generation, state],
        )?;
        if changed == 0 {
            let exists: i64 = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sources WHERE id = ?1)",
                params![source_id],
                |row| row.get(0),
            )?;
            if exists == 0 {
                return Err(IndexError::SourceNotFound(source_id));
            }
        }
        Ok(())
    }

    pub fn list_sources(&self) -> Result<Vec<SourceRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, root_path, enabled, online_state, scan_mode,
                    last_success_scan_at, config_json, scan_generation
             FROM sources ORDER BY id",
        )?;
        let rows = stmt.query_map([], row_to_source)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn source_by_id(&self, source_id: i64) -> Result<Option<SourceRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, kind, root_path, enabled, online_state, scan_mode,
                        last_success_scan_at, config_json, scan_generation
                 FROM sources WHERE id = ?1",
                params![source_id],
                row_to_source,
            )
            .optional()?)
    }

    /// Update user-editable source settings. Scanning and watching stay in
    /// other crates; this only persists the configuration row.
    pub fn update_source_config(
        &self,
        source_id: i64,
        enabled: bool,
        scan_mode: &str,
        config_json: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE sources SET enabled = ?2, scan_mode = ?3, config_json = ?4,
               online_state = CASE WHEN online_state = 'scanning' THEN 'unknown' ELSE online_state END
             WHERE id = ?1",
            params![source_id, enabled, scan_mode, config_json],
        )?;
        if changed == 0 {
            return Err(IndexError::SourceNotFound(source_id));
        }
        Ok(())
    }

    /// Delete a source and all of its SQLite rows. Never touches the
    /// filesystem. Jobs are removed by source or by their file's source so
    /// legacy rows that disagree with the owner cannot block the delete.
    /// Chunk rows, FTS and short grams follow `chunks.file_id ON DELETE
    /// CASCADE` when the files are deleted.
    pub fn delete_source(&mut self, source_id: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: i64 = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sources WHERE id = ?1)",
            params![source_id],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(IndexError::SourceNotFound(source_id));
        }
        // Saved search filters and background events can still reference deleted IDs.
        tx.execute(
            "INSERT INTO app_meta(key, value) VALUES ('source_id_high_watermark', ?1)
             ON CONFLICT(key) DO UPDATE SET value = MAX(CAST(value AS INTEGER), ?1)",
            [source_id],
        )?;
        tx.execute(
            "DELETE FROM index_jobs
             WHERE source_id = ?1
                OR file_id IN (SELECT id FROM files WHERE source_id = ?1)",
            params![source_id],
        )?;
        tx.execute("DELETE FROM files WHERE source_id = ?1", params![source_id])?;
        tx.execute("DELETE FROM sources WHERE id = ?1", params![source_id])?;
        dictionary::prune_orphan_lexemes(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Insert a search-history row, deduplicating on query/mode/filters.
    /// `INSERT OR REPLACE` gives a refreshed row a new, larger id, which also
    /// breaks `last_used_at` ties when several uses share one clock tick.
    /// Oldest rows beyond 100 are evicted in the same transaction.
    pub fn add_search_history(
        &mut self,
        query_text: &str,
        search_mode: SearchMode,
        filters_json: &str,
    ) -> Result<i64> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO search_history(query_text, search_mode, filters_json, last_used_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![query_text, search_mode.as_str(), filters_json, now()],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "DELETE FROM search_history
             WHERE id NOT IN (
               SELECT id FROM search_history
               ORDER BY last_used_at DESC, id DESC
               LIMIT 100
             )",
            [],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Most recent history first, capped at the 100 rows the schema keeps.
    pub fn search_history(&self) -> Result<Vec<SearchHistoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, query_text, search_mode, filters_json, last_used_at
             FROM search_history ORDER BY last_used_at DESC, id DESC LIMIT 100",
        )?;
        let rows = stmt.query_map([], row_to_search_history)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn clear_search_history(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM search_history", [])?)
    }

    pub fn delete_search_history_item(&self, id: i64) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM search_history WHERE id = ?1", [id])?)
    }

    /// Queue a job. A job may bind only a file owned by the same source; the
    /// ownership check and insert share one transaction.
    pub fn insert_index_job(
        &mut self,
        source_id: i64,
        file_id: Option<i64>,
        kind: &str,
    ) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let source_exists: i64 = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sources WHERE id = ?1)",
            params![source_id],
            |row| row.get(0),
        )?;
        if source_exists == 0 {
            return Err(IndexError::SourceNotFound(source_id));
        }
        if let Some(file_id) = file_id {
            let owner: Option<i64> = tx
                .query_row(
                    "SELECT source_id FROM files WHERE id = ?1",
                    params![file_id],
                    |row| row.get(0),
                )
                .optional()?;
            match owner {
                None => return Err(IndexError::FileNotFound(file_id)),
                Some(owner) if owner != source_id => {
                    return Err(IndexError::JobFileSourceMismatch {
                        file_id,
                        file_source_id: owner,
                        job_source_id: source_id,
                    })
                }
                Some(_) => {}
            }
        }
        tx.execute(
            "INSERT INTO index_jobs(file_id, source_id, kind, state, requested_at)
             VALUES (?1, ?2, ?3, 'queued', ?4)",
            params![file_id, source_id, kind, now()],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    pub fn index_jobs_for_source(&self, source_id: i64) -> Result<Vec<IndexJobRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_id, source_id, kind, state, attempt_count, requested_at, last_error
             FROM index_jobs WHERE source_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![source_id], row_to_index_job)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn update_index_job(
        &self,
        job_id: i64,
        state: &str,
        attempt_count: i64,
        last_error: Option<&str>,
    ) -> Result<()> {
        if !matches!(state, "queued" | "running" | "failed" | "done") {
            return Err(IndexError::InvalidJobState(state.to_string()));
        }
        if attempt_count < 0 {
            return Err(IndexError::NegativeAttemptCount(attempt_count));
        }
        let changed = self.conn.execute(
            "UPDATE index_jobs
             SET state = ?2, attempt_count = ?3, last_error = ?4
             WHERE id = ?1",
            params![job_id, state, attempt_count, last_error],
        )?;
        if changed == 0 {
            return Err(IndexError::JobNotFound(job_id));
        }
        Ok(())
    }

    pub fn delete_index_job(&self, job_id: i64) -> Result<()> {
        let changed = self
            .conn
            .execute("DELETE FROM index_jobs WHERE id = ?1", params![job_id])?;
        if changed == 0 {
            return Err(IndexError::JobNotFound(job_id));
        }
        Ok(())
    }

    /// Queue a source-level scan or rebuild, reusing an existing queued or
    /// running source scan. A rebuild upgrades a queued scan or waits for a
    /// non-forced scan that's already running.
    pub fn queue_source_scan(&self, source_id: i64, force: bool) -> Result<i64> {
        let kind = if force { "rebuild" } else { "scan" };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let exists: i64 = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sources WHERE id = ?1)",
            params![source_id],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(IndexError::SourceNotFound(source_id));
        }
        let existing: Option<(i64, String, String)> = tx
            .query_row(
                "SELECT id, state, kind FROM index_jobs
                 WHERE source_id = ?1
                   AND kind IN ('scan', 'rebuild')
                   AND state IN ('queued', 'running')
                 ORDER BY state, id LIMIT 1",
                params![source_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((job_id, state, existing_kind)) = existing {
            if force && state == "queued" {
                tx.execute(
                    "UPDATE index_jobs SET kind = 'rebuild' WHERE id = ?1 AND state = 'queued'",
                    params![job_id],
                )?;
            }
            if !force || state == "queued" || existing_kind == "rebuild" {
                tx.commit()?;
                return Ok(job_id);
            }
        }
        tx.execute(
            "INSERT INTO index_jobs(source_id, kind, state, requested_at)
             VALUES (?1, ?2, 'queued', ?3)",
            params![source_id, kind, now()],
        )?;
        let job_id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(job_id)
    }

    /// Claim one queued source scan/rebuild job. Sources that already have a
    /// running source scan are skipped, and the claim increments the attempt
    /// count atomically.
    pub fn claim_source_scan(&self) -> Result<Option<IndexJobRecord>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let candidate: Option<i64> = tx
            .query_row(
                "SELECT j.id FROM index_jobs j
                 WHERE j.state = 'queued'
                   AND j.kind IN ('scan', 'rebuild')
                   AND NOT EXISTS (
                     SELECT 1 FROM index_jobs r
                     WHERE r.source_id = j.source_id
                       AND r.state = 'running'
                       AND r.kind IN ('scan', 'rebuild')
                   )
                 ORDER BY j.id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(job_id) = candidate else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            "UPDATE index_jobs
             SET state = 'running', attempt_count = attempt_count + 1
             WHERE id = ?1 AND state = 'queued'",
            params![job_id],
        )?;
        let job = tx.query_row(
            "SELECT id, file_id, source_id, kind, state, attempt_count, requested_at, last_error
             FROM index_jobs WHERE id = ?1",
            params![job_id],
            row_to_index_job,
        )?;
        tx.commit()?;
        Ok(Some(job))
    }

    /// Crash recovery: requeue interrupted source scans while they still have
    /// attempts left, fail those at the limit, and clear a dangling `scanning`
    /// state. Index rows are preserved.
    pub fn recover_source_scans(&self) -> Result<usize> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let requeued = tx.execute(
            "UPDATE index_jobs SET state = 'queued'
             WHERE state = 'running'
               AND kind IN ('scan', 'rebuild')
               AND attempt_count < 3",
            [],
        )?;
        let failed = tx.execute(
            "UPDATE index_jobs
             SET state = 'failed',
                 last_error = COALESCE(last_error, 'interrupted after attempt limit')
             WHERE state = 'running'
               AND kind IN ('scan', 'rebuild')
               AND attempt_count >= 3",
            [],
        )?;
        tx.execute(
            "UPDATE sources SET online_state = 'unknown' WHERE online_state = 'scanning'",
            [],
        )?;
        tx.commit()?;
        Ok(requeued + failed)
    }

    pub fn upsert_file(&self, file: &NewFile) -> Result<i64> {
        Self::write_file(&self.conn, file)
    }

    pub(crate) fn write_file(conn: &Connection, file: &NewFile) -> Result<i64> {
        conn.execute(
            "INSERT INTO files(source_id, path_key, display_path, filename, ext, size_bytes, mtime_ns, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(source_id, path_key) DO UPDATE SET
               display_path = excluded.display_path,
               filename = excluded.filename,
               ext = excluded.ext,
               size_bytes = excluded.size_bytes,
               mtime_ns = excluded.mtime_ns,
               state = excluded.state",
            params![
                file.source_id,
                file.path_key,
                file.display_path,
                file.filename,
                file.ext,
                file.size_bytes,
                file.mtime_ns,
                file.state.as_str()
            ],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM files WHERE source_id = ?1 AND path_key = ?2",
            params![file.source_id, file.path_key],
            |row| row.get(0),
        )?)
    }

    pub fn find_file(&self, source_id: i64, path_key: &str) -> Result<Option<FileRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, source_id, path_key, display_path, filename, ext, size_bytes,
                        mtime_ns, state, indexed_at, error_message
                 FROM files WHERE source_id = ?1 AND path_key = ?2",
                params![source_id, path_key],
                row_to_file,
            )
            .optional()?)
    }

    pub fn file_by_id(&self, file_id: i64) -> Result<Option<FileRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, source_id, path_key, display_path, filename, ext, size_bytes,
                        mtime_ns, state, indexed_at, error_message
                 FROM files WHERE id = ?1",
                params![file_id],
                row_to_file,
            )
            .optional()?)
    }

    pub fn mark_file_state(
        &self,
        file_id: i64,
        state: FileState,
        error_message: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET state = ?2, error_message = ?3 WHERE id = ?1",
            params![file_id, state.as_str(), error_message],
        )?;
        Ok(())
    }

    pub fn mark_file_failed(&self, file_id: i64, error_message: &str) -> Result<()> {
        Self::write_file_failure(&self.conn, file_id, error_message)
    }

    pub(crate) fn write_file_failure(
        conn: &Connection,
        file_id: i64,
        error_message: &str,
    ) -> Result<()> {
        conn.execute(
            "UPDATE files SET
                state = CASE
                    WHEN state IN ('indexed', 'stale')
                         OR EXISTS(SELECT 1 FROM chunks WHERE file_id = ?1)
                    THEN 'stale' ELSE 'failed' END,
                error_message = ?2
             WHERE id = ?1",
            params![file_id, error_message],
        )?;
        Ok(())
    }

    /// Refresh size/mtime after a successful read so the next incremental scan
    /// compares against the version that was actually indexed.
    pub fn update_file_stat(
        &self,
        file_id: i64,
        size_bytes: i64,
        mtime_ns: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET size_bytes = ?2, mtime_ns = ?3 WHERE id = ?1",
            params![file_id, size_bytes, mtime_ns],
        )?;
        Ok(())
    }

    /// Atomically replace all chunks, FTS rows and short grams for one file.
    ///
    /// Either every new chunk becomes visible together or the previous index
    /// version stays intact.
    pub fn replace_file_content(&mut self, file_id: i64, doc: &ExtractedDocument) -> Result<()> {
        let tx = self.conn.transaction()?;
        let exists: i64 = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE id = ?1)",
            params![file_id],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(IndexError::FileNotFound(file_id));
        }
        Self::write_file_content(&tx, file_id, Some(doc))?;
        tx.commit()?;
        Ok(())
    }

    /// Publish metadata and content together. A missing document is metadata-only.
    pub fn store_file(&mut self, file: &NewFile, doc: Option<&ExtractedDocument>) -> Result<()> {
        let tx = self.conn.transaction()?;
        let id = Self::write_file(&tx, file)?;
        Self::write_file_content(&tx, id, doc)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn write_file_content(
        tx: &Transaction<'_>,
        file_id: i64,
        doc: Option<&ExtractedDocument>,
    ) -> Result<()> {
        tx.execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])?;

        {
            let mut insert_chunk = tx.prepare(
                "INSERT INTO chunks(file_id, ordinal, text, display_text, source_locator_json, origin)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let mut insert_gram =
                tx.prepare("INSERT OR IGNORE INTO short_grams(gram, chunk_id) VALUES (?1, ?2)")?;

            let sections = doc.map_or(&[][..], |doc| doc.sections.as_slice());
            for (ordinal, section) in sections.iter().enumerate() {
                let text = normalize_search_text(&section.display_text);
                let locator_json = serde_json::to_string(&section.locator)?;
                insert_chunk.execute(params![
                    file_id,
                    ordinal as i64,
                    text,
                    section.display_text,
                    locator_json,
                    section.origin.as_str()
                ])?;
                let chunk_id = tx.last_insert_rowid();
                for gram in short_grams_for_chunk(&section.text) {
                    insert_gram.execute(params![gram, chunk_id])?;
                }
                // Offsets in dictionary rows use display text; normalization
                // preserves byte length, so chunk.text offsets still apply.
                dictionary::index_chunk(tx, chunk_id, &section.display_text)?;
            }
        }

        tx.execute(
            "UPDATE files SET state = ?2, indexed_at = ?3, error_message = NULL WHERE id = ?1",
            params![
                file_id,
                if doc.is_some() {
                    FileState::Indexed
                } else {
                    FileState::SkippedSize
                }
                .as_str(),
                doc.map(|_| now())
            ],
        )?;
        Ok(())
    }

    pub fn chunks_for_file(&self, file_id: i64) -> Result<Vec<ChunkRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_id, ordinal, text, display_text, source_locator_json, origin
             FROM chunks WHERE file_id = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![file_id], row_to_chunk)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn chunk_count_for_file(&self, file_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM chunks WHERE file_id = ?1",
            params![file_id],
            |row| row.get(0),
        )?)
    }

    pub fn chunk_by_id(&self, chunk_id: i64) -> Result<Option<ChunkRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, file_id, ordinal, text, display_text, source_locator_json, origin
                 FROM chunks WHERE id = ?1",
                params![chunk_id],
                row_to_chunk,
            )
            .optional()?)
    }

    /// Dictionary candidates sharing at least `min_shared` grams with the
    /// supplied query features. The result is capped so a common gram cannot
    /// turn a fuzzy query into a full-table scan.
    pub fn candidate_lexemes(
        &self,
        grams: &[String],
        min_length: i64,
        max_length: i64,
        target_length: i64,
        min_shared: i64,
        limit: usize,
    ) -> Result<Vec<LexemeCandidate>> {
        let mut seen = HashSet::new();
        let mut unique = Vec::new();
        for gram in grams {
            if !gram.is_empty()
                && unique.len() < dictionary::MAX_GRAMS_PER_LEXEME
                && seen.insert(gram.as_str())
            {
                unique.push(gram.as_str());
            }
        }
        if unique.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, dictionary::MAX_CANDIDATE_LEXEMES) as i64;
        let placeholders = vec!["?"; unique.len()].join(",");
        let sql = format!(
            "SELECT l.id, l.text, l.length, l.kind,
                    count(DISTINCT g.gram) AS shared
             FROM lexeme_grams g
             JOIN lexemes l ON l.id = g.lexeme_id
             WHERE g.gram IN ({placeholders})
               AND l.length BETWEEN ? AND ?
             GROUP BY l.id, l.text, l.length, l.kind
             HAVING count(DISTINCT g.gram) >= ?
             ORDER BY shared DESC, abs(l.length - ?) ASC, l.id ASC
             LIMIT ?"
        );
        let mut values: Vec<rusqlite::types::Value> = unique
            .iter()
            .map(|gram| rusqlite::types::Value::Text((*gram).to_string()))
            .collect();
        values.push(rusqlite::types::Value::Integer(min_length));
        values.push(rusqlite::types::Value::Integer(max_length));
        values.push(rusqlite::types::Value::Integer(min_shared));
        values.push(rusqlite::types::Value::Integer(target_length));
        values.push(rusqlite::types::Value::Integer(limit));

        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            Ok(LexemeCandidate {
                id: row.get(0)?,
                text: row.get(1)?,
                length: row.get(2)?,
                kind: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Actual byte ranges of a dictionary lexeme inside committed chunks.
    /// Fuzzy hits use these ranges so the UI highlights the original text.
    pub fn lexeme_occurrences(
        &self,
        lexeme_id: i64,
        limit: usize,
    ) -> Result<Vec<LexemeOccurrence>> {
        let limit = limit.clamp(1, 1024) as i64;
        let mut statement = self.conn.prepare(
            "SELECT o.chunk_id, c.file_id, o.start, o.end
             FROM lexeme_occurrences o
             JOIN chunks c ON c.id = o.chunk_id
             WHERE o.lexeme_id = ?1
             ORDER BY o.chunk_id, o.start
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![lexeme_id, limit], |row| {
            let start: i64 = row.get(2)?;
            let end: i64 = row.get(3)?;
            Ok(LexemeOccurrence {
                chunk_id: row.get(0)?,
                file_id: row.get(1)?,
                start: start.max(0) as usize,
                end: end.max(0) as usize,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Chunk ids plausibly containing `term`: FTS5 trigram for >=3 non-space
    /// chars, the auxiliary index for shorter terms and boundary fallbacks.
    pub fn candidate_chunks(&self, term: &str) -> Result<Vec<i64>> {
        if term.contains('\0') {
            return Err(IndexError::NulTerm);
        }
        if term.is_empty() {
            return Ok(Vec::new());
        }
        let normalized = normalize_search_text(term);
        let chars: Vec<char> = normalized
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        let Some(first) = chars.first() else {
            // A whitespace-only phrase has no gram at all. Its opening
            // character is the only recall anchor; the caller's strict
            // verification decides whether a chunk actually contains it.
            let opening = normalized.chars().next().expect("checked non-empty");
            return self.chunks_containing_char(opening);
        };
        if chars.len() < 3 {
            // Every chunk holding the bigram also indexes its first single
            // character (whitespace is removed from both), so the first
            // character's postings are a strict superset. That superset also
            // recalls a 1+1 phrase split across two chunks, e.g. "内" | "核".
            return self.chunks_for_gram(&first.to_string());
        }

        let mut ids = std::collections::HashSet::new();
        let trigrams = trigrams_for_query(term);
        if !trigrams.is_empty() {
            let expression = trigrams
                .iter()
                .filter(|gram| !gram.contains('"'))
                .map(|gram| format!("\"{}\"", gram.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut stmt = self
                .conn
                .prepare("SELECT rowid FROM chunks_fts WHERE chunks_fts MATCH ?1")?;
            let rows = stmt.query_map(params![expression], |row| row.get(0))?;
            for row in rows {
                ids.insert(row?);
            }
        }
        // A phrase split 2+2 across a boundary has no trigram on either side;
        // bigram recall finds both candidate chunks and verification rejects
        // false positives.
        for gram in bigrams_for_query(term) {
            ids.extend(self.chunks_for_gram(&gram)?);
        }
        // Other files can have inline grams even when a boundary match has none.
        ids.extend(self.chunks_with_opening_near_edge(*first, normalized.len())?);
        let mut ids: Vec<i64> = ids.into_iter().collect();
        ids.sort_unstable();
        Ok(ids)
    }

    fn chunks_containing_char(&self, ch: char) -> Result<Vec<i64>> {
        let mut buffer = [0u8; 4];
        let needle: &str = ch.encode_utf8(&mut buffer);
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM chunks WHERE instr(text, ?1) > 0 ORDER BY id")?;
        let rows = stmt.query_map(params![needle], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Chunks where `opening` occurs within `span` bytes of either edge.
    /// The gram index narrows the scan to chunks already containing the
    /// character before `substr` checks the edge distance.
    fn chunks_with_opening_near_edge(&self, opening: char, span: usize) -> Result<Vec<i64>> {
        let mut buffer = [0u8; 4];
        let needle: &str = opening.encode_utf8(&mut buffer);
        let mut stmt = self.conn.prepare(
            "SELECT c.id
             FROM short_grams g JOIN chunks c ON c.id = g.chunk_id
             WHERE g.gram = ?1
               AND (instr(substr(CAST(c.text AS BLOB), 1, ?2), CAST(?1 AS BLOB)) > 0
                    OR instr(substr(CAST(c.text AS BLOB), -?2), CAST(?1 AS BLOB)) > 0)
             ORDER BY c.id",
        )?;
        let rows = stmt.query_map(params![needle, span as i64], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn chunks_for_gram(&self, gram: &str) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT chunk_id FROM short_grams WHERE gram = ?1 ORDER BY chunk_id")?;
        let rows = stmt.query_map(params![gram], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<i64>, _>>()?)
    }

    /// A candidate chunk with its previous and next chunks in the same file.
    /// Callers decide whether neighbors are a contiguous run by comparing
    /// locators; joining them lets a phrase that crosses one chunk boundary be
    /// verified from either side.
    pub fn chunk_context(&self, chunk_id: i64) -> Result<Option<ChunkContext>> {
        Ok(self
            .conn
            .query_row(
                "SELECT p.id, p.file_id, p.ordinal, p.text, p.display_text,
                        p.source_locator_json, p.origin,
                        c.id, c.file_id, c.ordinal, c.text, c.display_text,
                        c.source_locator_json, c.origin,
                        n.id, n.file_id, n.ordinal, n.text, n.display_text,
                        n.source_locator_json, n.origin
                 FROM chunks c
                 LEFT JOIN chunks p ON p.file_id = c.file_id AND p.ordinal = c.ordinal - 1
                 LEFT JOIN chunks n ON n.file_id = c.file_id AND n.ordinal = c.ordinal + 1
                 WHERE c.id = ?1",
                params![chunk_id],
                |row| {
                    let previous = read_optional_chunk(row, 0)?;
                    let current = read_chunk(row, 7)?;
                    let next = read_optional_chunk(row, 14)?;
                    Ok((previous, current, next))
                },
            )
            .optional()?)
    }

    pub fn all_indexed_file_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT file_id FROM chunks ORDER BY file_id")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<i64>, _>>()?)
    }

    pub fn all_file_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM files ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Filename substring candidates. `instr` keeps `%` and `_` literal.
    pub fn files_matching_filename(
        &self,
        needle: &str,
        case_sensitive: bool,
    ) -> Result<Vec<FileRecord>> {
        let sql = if case_sensitive {
            "SELECT id, source_id, path_key, display_path, filename, ext, size_bytes,
                    mtime_ns, state, indexed_at, error_message
             FROM files WHERE instr(filename, ?1) > 0 ORDER BY id"
        } else {
            "SELECT id, source_id, path_key, display_path, filename, ext, size_bytes,
                    mtime_ns, state, indexed_at, error_message
             FROM files WHERE instr(lower(filename), ?1) > 0 ORDER BY id"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![needle], row_to_file)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Remove files under a source that a *complete* scan did not see.
    /// Callers must only invoke this after a successful full enumeration.
    pub fn reconcile_source(
        &mut self,
        source_id: i64,
        seen_keys: &HashSet<String>,
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        prepare_seen_keys(&tx, seen_keys)?;
        let removed = remove_unseen_source_files(&tx, source_id)?;
        tx.execute("DROP TABLE seen_path_keys", [])?;
        tx.commit()?;
        Ok(removed)
    }

    pub fn stats(&self) -> Result<IndexStats> {
        let scalar =
            |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [], |row| row.get(0))?) };
        let db_page_size: i64 = scalar("PRAGMA page_size")?;
        let db_page_count: i64 = scalar("PRAGMA page_count")?;
        Ok(IndexStats {
            sources: scalar("SELECT count(*) FROM sources")?,
            files: scalar("SELECT count(*) FROM files")?,
            indexed_files: scalar("SELECT count(*) FROM files WHERE state = 'indexed'")?,
            skipped_files: scalar("SELECT count(*) FROM files WHERE state = 'skipped_size'")?,
            failed_files: scalar("SELECT count(*) FROM files WHERE state = 'failed'")?,
            stale_files: scalar("SELECT count(*) FROM files WHERE state = 'stale'")?,
            chunks: scalar("SELECT count(*) FROM chunks")?,
            short_grams: scalar("SELECT count(*) FROM short_grams")?,
            lexemes: scalar("SELECT count(*) FROM lexemes")?,
            lexeme_grams: scalar("SELECT count(*) FROM lexeme_grams")?,
            lexeme_occurrences: scalar("SELECT count(*) FROM lexeme_occurrences")?,
            db_size_bytes: db_page_size.saturating_mul(db_page_count),
            db_page_size,
            db_page_count,
        })
    }

    /// Run the FTS5 integrity check for the external-content table.
    pub fn verify_fts(&self) -> Result<()> {
        self.conn.execute_batch(
            "INSERT INTO chunks_fts(chunks_fts, rank) VALUES('integrity-check', 1);",
        )?;
        Ok(())
    }
}

/// Populate the per-connection temp table used by reconcile and finish.
fn prepare_seen_keys(tx: &Transaction<'_>, seen_keys: &HashSet<String>) -> Result<()> {
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS seen_path_keys(path_key TEXT PRIMARY KEY);
         DELETE FROM seen_path_keys;",
    )?;
    let mut insert = tx.prepare("INSERT OR IGNORE INTO seen_path_keys(path_key) VALUES (?1)")?;
    for key in seen_keys {
        insert.execute(params![key])?;
    }
    Ok(())
}

/// Delete files missing from `seen_path_keys` and the jobs bound to them.
/// Chunks, FTS entries and short grams cascade from the file delete.
fn remove_unseen_source_files(tx: &Transaction<'_>, source_id: i64) -> Result<usize> {
    // Jobs reference files without a cascading FK, so remove them first.
    tx.execute(
        "DELETE FROM index_jobs
         WHERE file_id IN (
           SELECT id FROM files
           WHERE source_id = ?1
             AND path_key NOT IN (SELECT path_key FROM seen_path_keys)
         )",
        params![source_id],
    )?;
    let removed = tx.execute(
        "DELETE FROM files
         WHERE source_id = ?1
           AND path_key NOT IN (SELECT path_key FROM seen_path_keys)",
        params![source_id],
    )?;
    // Chunk deletes cascade occurrences; the shared dictionary words that
    // only those chunks used are no longer reachable.
    dictionary::prune_orphan_lexemes(tx)?;
    Ok(removed)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn read_chunk(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<ChunkRecord> {
    Ok(ChunkRecord {
        id: row.get(base)?,
        file_id: row.get(base + 1)?,
        ordinal: row.get(base + 2)?,
        text: row.get(base + 3)?,
        display_text: row.get(base + 4)?,
        locator_json: row.get(base + 5)?,
        origin: row.get(base + 6)?,
    })
}

fn read_optional_chunk(
    row: &rusqlite::Row<'_>,
    base: usize,
) -> rusqlite::Result<Option<ChunkRecord>> {
    let id: Option<i64> = row.get(base)?;
    if id.is_none() {
        return Ok(None);
    }
    read_chunk(row, base).map(Some)
}

fn row_to_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id: row.get(0)?,
        source_id: row.get(1)?,
        path_key: row.get(2)?,
        display_path: row.get(3)?,
        filename: row.get(4)?,
        ext: row.get(5)?,
        size_bytes: row.get(6)?,
        mtime_ns: row.get(7)?,
        state: row.get(8)?,
        indexed_at: row.get(9)?,
        error_message: row.get(10)?,
    })
}

fn row_to_source(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceRecord> {
    let kind: String = row.get(1)?;
    Ok(SourceRecord {
        id: row.get(0)?,
        kind: parse_source_kind(&kind)?,
        root_path: row.get(2)?,
        enabled: row.get(3)?,
        online_state: row.get(4)?,
        scan_mode: row.get(5)?,
        last_success_scan_at: row.get(6)?,
        config_json: row.get(7)?,
        scan_generation: row.get(8)?,
    })
}

fn row_to_search_history(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchHistoryRecord> {
    let mode: String = row.get(2)?;
    Ok(SearchHistoryRecord {
        id: row.get(0)?,
        query_text: row.get(1)?,
        search_mode: parse_search_mode(&mode)?,
        filters_json: row.get(3)?,
        last_used_at: row.get(4)?,
    })
}

fn row_to_index_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexJobRecord> {
    Ok(IndexJobRecord {
        id: row.get(0)?,
        file_id: row.get(1)?,
        source_id: row.get(2)?,
        kind: row.get(3)?,
        state: row.get(4)?,
        attempt_count: row.get(5)?,
        requested_at: row.get(6)?,
        last_error: row.get(7)?,
    })
}

fn parse_source_kind(value: &str) -> rusqlite::Result<SourceKind> {
    match value {
        "local" => Ok(SourceKind::Local),
        "smb" => Ok(SourceKind::Smb),
        "wsl" => Ok(SourceKind::Wsl),
        "removable" => Ok(SourceKind::Removable),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown source kind {other}").into(),
        )),
    }
}

fn parse_search_mode(value: &str) -> rusqlite::Result<SearchMode> {
    SearchMode::parse(value).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown search mode {value}").into(),
        )
    })
}

fn row_to_chunk(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRecord> {
    Ok(ChunkRecord {
        id: row.get(0)?,
        file_id: row.get(1)?,
        ordinal: row.get(2)?,
        text: row.get(3)?,
        display_text: row.get(4)?,
        locator_json: row.get(5)?,
        origin: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexfox_core::{Origin, Section, SourceLocator};

    fn doc(texts: &[&str]) -> ExtractedDocument {
        ExtractedDocument {
            extractor_version: "test".into(),
            encoding: Some("utf-8".into()),
            sections: texts
                .iter()
                .enumerate()
                .map(|(index, text)| Section {
                    text: normalize_search_text(text),
                    display_text: (*text).to_string(),
                    locator: SourceLocator::Text {
                        start_line: index as u32 + 1,
                        end_line: index as u32 + 1,
                        start_offset: 0,
                        end_offset: text.len(),
                    },
                    origin: Origin::Native,
                })
                .collect(),
        }
    }

    fn seed(db: &mut IndexDb, path: &str, texts: &[&str]) -> i64 {
        let source = db.ensure_source(SourceKind::Local, "/tmp").unwrap();
        let id = db
            .upsert_file(&NewFile {
                source_id: source,
                path_key: path.into(),
                display_path: path.into(),
                filename: path.rsplit('/').next().unwrap().into(),
                ext: "txt".into(),
                size_bytes: 10,
                mtime_ns: Some(1),
                state: FileState::Pending,
            })
            .unwrap();
        db.replace_file_content(id, &doc(texts)).unwrap();
        id
    }

    #[test]
    fn replaces_chunks_atomically_and_clears_grams() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let id = seed(&mut db, "/tmp/a.txt", &["Linux 内核驱动"]);
        assert_eq!(db.chunk_count_for_file(id).unwrap(), 1);
        db.replace_file_content(id, &doc(&["Windows 驱动"]))
            .unwrap();
        let chunks = db.chunks_for_file(id).unwrap();
        assert!(chunks.iter().all(|chunk| !chunk.text.contains("linux")));
        assert_eq!(db.candidate_chunks("内核").unwrap().len(), 0);
        assert_eq!(db.candidate_chunks("windows").unwrap().len(), 1);
        db.verify_fts().unwrap();
    }

    #[test]
    fn dictionary_follows_atomic_replacement_and_clearing() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let id = seed(&mut db, "/tmp/a.txt", &["kernel dma_alloc_coherent"]);

        let grams = crate::dictionary::grams_of_len("kernal", 3);
        let candidates = db.candidate_lexemes(&grams, 4, 8, 6, 1, 100).unwrap();
        assert!(candidates
            .iter()
            .any(|candidate| candidate.text == "kernel"));
        let kernel = candidates
            .iter()
            .find(|candidate| candidate.text == "kernel")
            .unwrap();
        assert_eq!(db.lexeme_occurrences(kernel.id, 100).unwrap().len(), 1);

        db.replace_file_content(id, &doc(&["windows"])).unwrap();
        assert!(db
            .candidate_lexemes(&grams, 4, 8, 6, 1, 100)
            .unwrap()
            .iter()
            .all(|candidate| candidate.text != "kernel"));
        assert!(db
            .candidate_lexemes(
                &crate::dictionary::grams_of_len("windos", 3),
                4,
                8,
                6,
                1,
                100,
            )
            .unwrap()
            .iter()
            .any(|candidate| candidate.text == "windows"));

        db.replace_file_content(id, &doc(&[])).unwrap();
        let stats = db.stats().unwrap();
        assert_eq!(stats.lexemes, 0);
        assert_eq!(stats.lexeme_grams, 0);
        assert_eq!(stats.lexeme_occurrences, 0);
    }

    #[test]
    fn deleting_source_cascades_occurrences_and_prunes_dictionary_words() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let id = seed(&mut db, "/tmp/a.txt", &["kernel 内核驱动"]);
        let source_id = db.file_by_id(id).unwrap().unwrap().source_id;
        assert!(db.stats().unwrap().lexemes > 0);
        assert!(db.stats().unwrap().lexeme_occurrences > 0);

        db.delete_source(source_id).unwrap();

        let stats = db.stats().unwrap();
        assert_eq!(stats.lexemes, 0);
        assert_eq!(stats.lexeme_grams, 0);
        assert_eq!(stats.lexeme_occurrences, 0);
    }

    #[test]
    fn raw_chunk_delete_cascades_occurrences_and_prunes_dictionary() {
        let mut db = IndexDb::open_in_memory().unwrap();
        seed(&mut db, "/tmp/a.txt", &["kernel 内核驱动"]);
        assert!(db.stats().unwrap().lexemes > 0);

        // Mirrors the app's cache-clear path, which deletes chunks directly.
        db.conn().execute("DELETE FROM chunks", []).unwrap();

        let stats = db.stats().unwrap();
        assert_eq!(stats.chunks, 0);
        assert_eq!(stats.lexemes, 0);
        assert_eq!(stats.lexeme_grams, 0);
        assert_eq!(stats.lexeme_occurrences, 0);
    }

    #[test]
    fn candidate_lexeme_lookup_honours_length_and_cap() {
        let mut db = IndexDb::open_in_memory().unwrap();
        seed(
            &mut db,
            "/tmp/a.txt",
            &["kernel kernal kermel kernalize dma_alloc_coherent"],
        );
        let grams = crate::dictionary::grams_of_len("kernal", 2);
        let page = db.candidate_lexemes(&grams, 4, 9, 6, 1, 1).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].length, 6);
    }

    #[test]
    fn stats_report_logical_size_including_uncheckpointed_wal_pages() {
        use std::fs;
        use std::path::PathBuf;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("indexfox-stats-{}-{unique}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.db");
        let mut db = IndexDb::open(&path).unwrap();

        let initial = db.stats().unwrap();
        assert!(initial.db_page_size > 0);
        assert!(initial.db_page_count > 0);
        assert_eq!(
            initial.db_size_bytes,
            initial.db_page_size * initial.db_page_count
        );

        // Keep the transaction well below the 1000-page WAL auto-checkpoint
        // threshold so `stats()` has to read pages that only exist in the WAL.
        let big = "x".repeat(32 * 1024);
        seed(&mut db, "/tmp/large.txt", &[big.as_str(); 8]);

        let stats = db.stats().unwrap();
        assert!(stats.db_page_count > initial.db_page_count);
        assert_eq!(
            stats.db_size_bytes,
            stats.db_page_size * stats.db_page_count
        );

        let mut wal_name = path.as_os_str().to_os_string();
        wal_name.push("-wal");
        let wal_path = PathBuf::from(wal_name);
        let main_len = fs::metadata(&path).unwrap().len() as i64;
        assert!(fs::metadata(&wal_path).unwrap().len() > 0);
        assert!(
            stats.db_size_bytes > main_len,
            "logical size {} must include committed WAL pages beyond the {} byte main file",
            stats.db_size_bytes,
            main_len
        );

        db.conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let checkpointed = db.stats().unwrap();
        assert_eq!(checkpointed.db_page_count, stats.db_page_count);
        assert_eq!(checkpointed.db_size_bytes, stats.db_size_bytes);
        assert!(main_len < fs::metadata(&path).unwrap().len() as i64);

        drop(db);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_and_trigram_recall_work() {
        let mut db = IndexDb::open_in_memory().unwrap();
        seed(
            &mut db,
            "/tmp/a.txt",
            &["Linux 内核驱动 VIDIOC_STREAMON dma_alloc_coherent() #include 100% a_b"],
        );
        assert_eq!(db.candidate_chunks("内").unwrap().len(), 1);
        assert_eq!(db.candidate_chunks("内核").unwrap().len(), 1);
        assert_eq!(db.candidate_chunks("内核驱动").unwrap().len(), 1);
        assert_eq!(db.candidate_chunks("vidioc_streamon").unwrap().len(), 1);
        assert_eq!(db.candidate_chunks("100%").unwrap().len(), 1);
        assert_eq!(db.candidate_chunks("a_b").unwrap().len(), 1);
    }

    #[test]
    fn fts_check_detects_missing_index_entries() {
        let mut db = IndexDb::open_in_memory().unwrap();
        seed(&mut db, "/tmp/a.txt", &["IndexFox search"]);
        db.conn()
            .execute_batch("INSERT INTO chunks_fts(chunks_fts) VALUES('delete-all');")
            .unwrap();
        assert!(db.verify_fts().is_err());
    }

    #[test]
    fn two_char_recall_uses_first_character_superset_for_boundary_split() {
        let mut db = IndexDb::open_in_memory().unwrap();
        seed(&mut db, "/tmp/boundary.txt", &["前内", "核后"]);
        let chunks = db
            .chunks_for_file(db.all_indexed_file_ids().unwrap()[0])
            .unwrap();
        // "内核" has no bigram in either chunk, but the first character's
        // postings are a superset that recalls the left side. Verification
        // extends to the right neighbour.
        assert_eq!(db.candidate_chunks("内核").unwrap(), vec![chunks[0].id]);
        // Whitespace inside a two-character term must not change recall.
        assert_eq!(db.candidate_chunks("内 核").unwrap(), vec![chunks[0].id]);
    }

    #[test]
    fn whitespace_only_phrase_uses_opening_character_recall() {
        let mut db = IndexDb::open_in_memory().unwrap();
        seed(&mut db, "/tmp/space.txt", &["a   b"]);
        seed(&mut db, "/tmp/plain.txt", &["ab"]);
        let chunks = db
            .chunks_for_file(db.all_indexed_file_ids().unwrap()[0])
            .unwrap();
        assert_eq!(db.candidate_chunks("   ").unwrap(), vec![chunks[0].id]);
    }

    #[test]
    fn long_phrase_split_into_single_character_chunks_is_recalled() {
        let mut db = IndexDb::open_in_memory().unwrap();
        seed(&mut db, "/tmp/single.txt", &["a", "b", "c", "d"]);
        let chunks = db
            .chunks_for_file(db.all_indexed_file_ids().unwrap()[0])
            .unwrap();
        // No trigram or bigram exists in any chunk; only the edge-anchored
        // first character can recall the run.
        assert_eq!(db.candidate_chunks("abcd").unwrap(), vec![chunks[0].id]);
    }

    fn file_in(db: &mut IndexDb, source_id: i64, path: &str, texts: &[&str]) -> i64 {
        let id = db
            .upsert_file(&NewFile {
                source_id,
                path_key: path.into(),
                display_path: path.into(),
                filename: path.rsplit('/').next().unwrap().into(),
                ext: "txt".into(),
                size_bytes: 10,
                mtime_ns: Some(1),
                state: FileState::Pending,
            })
            .unwrap();
        db.replace_file_content(id, &doc(texts)).unwrap();
        id
    }

    #[test]
    fn search_history_dedupes_caps_and_clears() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let first = db
            .add_search_history("kernel", SearchMode::Exact, "{}")
            .unwrap();
        let duplicate = db
            .add_search_history("kernel", SearchMode::Exact, "{}")
            .unwrap();
        assert!(duplicate > first);
        assert_eq!(db.search_history().unwrap().len(), 1);

        // Mode and filters are part of the identity.
        db.add_search_history("kernel", SearchMode::Substring, "{}")
            .unwrap();
        db.add_search_history("kernel", SearchMode::Exact, r#"{"ext":["pdf"]}"#)
            .unwrap();
        assert_eq!(db.search_history().unwrap().len(), 3);

        for index in 0..105 {
            db.add_search_history(&format!("q{index}"), SearchMode::Exact, "{}")
                .unwrap();
        }
        let history = db.search_history().unwrap();
        assert_eq!(history.len(), 100);
        assert_eq!(history[0].query_text, "q104");
        assert_eq!(history[0].search_mode, SearchMode::Exact);
        assert!(!history.iter().any(|row| row.query_text == "q4"));
        assert!(history.iter().any(|row| row.query_text == "q5"));
        let previous_max = history.iter().map(|row| row.id).max().unwrap();

        // Refreshing the oldest retained row must move it to the front and
        // keep the cap at 100. Even if every row shares one timestamp, the
        // refreshed row's new id makes it sort first.
        let refreshed = db
            .add_search_history("q5", SearchMode::Exact, "{}")
            .unwrap();
        assert!(refreshed > previous_max);
        db.conn()
            .execute(
                "UPDATE search_history SET last_used_at = '2026-01-01T00:00:00Z'",
                [],
            )
            .unwrap();
        let history = db.search_history().unwrap();
        assert_eq!(history.len(), 100);
        assert_eq!(history[0].id, refreshed);
        assert_eq!(history[0].query_text, "q5");
        assert!(history.iter().any(|row| row.query_text == "q6"));
        assert!(!history.iter().any(|row| row.query_text == "q4"));

        assert_eq!(db.clear_search_history().unwrap(), 100);
        assert!(db.search_history().unwrap().is_empty());
    }

    #[test]
    fn deleting_one_history_item_persists_after_reopen() {
        let (dir, path) = temp_db("delete-history");
        let mut db = IndexDb::open(&path).unwrap();
        let keep = db
            .add_search_history("kernel", SearchMode::Exact, "{}")
            .unwrap();
        let remove = db
            .add_search_history("kernel", SearchMode::Exact, r#"{"ext":["pdf"]}"#)
            .unwrap();
        assert_eq!(db.delete_search_history_item(remove).unwrap(), 1);
        drop(db);

        let db = IndexDb::open(&path).unwrap();
        let items = db.search_history().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, keep);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn index_jobs_validate_ownership_and_update_fields() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source_a = db.ensure_source(SourceKind::Local, "/a").unwrap();
        let source_b = db.ensure_source(SourceKind::Local, "/b").unwrap();
        let file_a = file_in(&mut db, source_a, "a.txt", &["内核驱动"]);
        let file_b = file_in(&mut db, source_b, "b.txt", &["windows"]);

        let job_a = db
            .insert_index_job(source_a, Some(file_a), "index")
            .unwrap();
        db.insert_index_job(source_b, Some(file_b), "index")
            .unwrap();
        let scan_job = db.insert_index_job(source_a, None, "scan").unwrap();

        assert!(matches!(
            db.insert_index_job(source_a, Some(file_b), "index"),
            Err(IndexError::JobFileSourceMismatch {
                file_id,
                file_source_id,
                job_source_id,
            }) if file_id == file_b && file_source_id == source_b && job_source_id == source_a
        ));
        assert!(matches!(
            db.insert_index_job(source_a, Some(999), "index"),
            Err(IndexError::FileNotFound(999))
        ));

        db.update_index_job(job_a, "failed", 3, Some("boom"))
            .unwrap();
        let jobs = db.index_jobs_for_source(source_a).unwrap();
        assert_eq!(jobs.len(), 2);
        let updated = jobs.iter().find(|job| job.id == job_a).unwrap();
        assert_eq!(updated.state, "failed");
        assert_eq!(updated.attempt_count, 3);
        assert_eq!(updated.last_error.as_deref(), Some("boom"));

        assert!(matches!(
            db.update_index_job(job_a, "bogus", 1, None),
            Err(IndexError::InvalidJobState(state)) if state == "bogus"
        ));
        assert!(matches!(
            db.update_index_job(job_a, "running", -1, None),
            Err(IndexError::NegativeAttemptCount(-1))
        ));
        let unchanged = db
            .index_jobs_for_source(source_a)
            .unwrap()
            .into_iter()
            .find(|job| job.id == job_a)
            .unwrap();
        assert_eq!(unchanged.state, "failed");
        assert_eq!(unchanged.attempt_count, 3);
        assert_eq!(unchanged.last_error.as_deref(), Some("boom"));

        db.delete_index_job(scan_job).unwrap();
        assert_eq!(db.index_jobs_for_source(source_a).unwrap().len(), 1);
        assert!(matches!(
            db.delete_index_job(scan_job),
            Err(IndexError::JobNotFound(id)) if id == scan_job
        ));
        assert!(matches!(
            db.update_index_job(999, "done", 1, None),
            Err(IndexError::JobNotFound(999))
        ));
    }

    #[test]
    fn deleting_source_removes_its_files_jobs_fts_and_grams() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source_a = db.ensure_source(SourceKind::Local, "/a").unwrap();
        let source_b = db.ensure_source(SourceKind::Local, "/b").unwrap();
        let file_a = file_in(&mut db, source_a, "a.txt", &["内核驱动"]);
        let file_b = file_in(&mut db, source_b, "b.txt", &["windows"]);
        db.insert_index_job(source_a, Some(file_a), "index")
            .unwrap();
        db.insert_index_job(source_b, Some(file_b), "index")
            .unwrap();
        db.insert_index_job(source_a, None, "scan").unwrap();

        db.update_source_config(source_a, false, "manual", r#"{"recursive":false}"#)
            .unwrap();
        let sources = db.list_sources().unwrap();
        let updated = sources.iter().find(|source| source.id == source_a).unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.scan_mode, "manual");
        assert_eq!(updated.config_json, r#"{"recursive":false}"#);
        assert_eq!(updated.kind, SourceKind::Local);

        db.delete_source(source_a).unwrap();
        assert!(db.file_by_id(file_a).unwrap().is_none());
        assert!(db.file_by_id(file_b).unwrap().is_some());
        assert_eq!(db.chunk_count_for_file(file_a).unwrap(), 0);
        assert_eq!(db.chunk_count_for_file(file_b).unwrap(), 1);
        assert!(db.index_jobs_for_source(source_a).unwrap().is_empty());
        assert_eq!(db.index_jobs_for_source(source_b).unwrap().len(), 1);
        assert!(db.candidate_chunks("内核").unwrap().is_empty());
        assert_eq!(db.candidate_chunks("windows").unwrap().len(), 1);
        db.verify_fts().unwrap();
        let sources = db.list_sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, source_b);

        assert!(matches!(
            db.delete_source(source_a),
            Err(IndexError::SourceNotFound(id)) if id == source_a
        ));
        db.delete_source(source_b).unwrap();
        assert!(db.ensure_source(SourceKind::Local, "/new").unwrap() > source_b);
    }

    #[test]
    fn complete_scan_reconcile_deletes_unseen_files_jobs_and_index_rows() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/scan").unwrap();
        let keep = file_in(&mut db, source, "keep.txt", &["内核驱动"]);
        let drop = file_in(&mut db, source, "drop.txt", &["windows"]);
        db.insert_index_job(source, Some(keep), "index").unwrap();
        db.insert_index_job(source, Some(drop), "index").unwrap();
        let scan_job = db.insert_index_job(source, None, "scan").unwrap();

        let mut seen = HashSet::new();
        seen.insert("keep.txt".to_string());
        assert_eq!(db.reconcile_source(source, &seen).unwrap(), 1);
        assert!(db.file_by_id(drop).unwrap().is_none());
        assert!(db.file_by_id(keep).unwrap().is_some());
        let jobs = db.index_jobs_for_source(source).unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|job| job.id == scan_job));
        assert!(!jobs.iter().any(|job| job.file_id == Some(drop)));
        assert!(db.candidate_chunks("windows").unwrap().is_empty());
        assert_eq!(db.candidate_chunks("内核").unwrap().len(), 1);
        db.verify_fts().unwrap();

        assert_eq!(db.reconcile_source(source, &HashSet::new()).unwrap(), 1);
        assert!(db.file_by_id(keep).unwrap().is_none());
        assert!(db.candidate_chunks("内核").unwrap().is_empty());
        let stats = db.stats().unwrap();
        assert_eq!(stats.chunks, 0);
        assert_eq!(stats.short_grams, 0);
        assert_eq!(db.index_jobs_for_source(source).unwrap().len(), 1);
        assert_eq!(db.list_sources().unwrap().len(), 1);
        db.verify_fts().unwrap();
    }

    fn temp_db(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("indexfox-{label}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.db");
        (dir, path)
    }

    #[test]
    fn source_scan_lifecycle_preserves_index_until_current_generation_finishes() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/scan").unwrap();
        let keep = file_in(&mut db, source, "keep.txt", &["内核驱动"]);
        let drop = file_in(&mut db, source, "drop.txt", &["windows"]);
        db.insert_index_job(source, Some(keep), "index").unwrap();
        db.insert_index_job(source, Some(drop), "index").unwrap();
        let scan_job = db.queue_source_scan(source, false).unwrap();

        let first = db.begin_source_scan(source).unwrap();
        assert_eq!(first, 1);
        let source_row = db.source_by_id(source).unwrap().unwrap();
        assert_eq!(source_row.scan_generation, 1);
        assert_eq!(source_row.online_state, "scanning");

        // A newer generation invalidates the first without deleting anything.
        let second = db.begin_source_scan(source).unwrap();
        assert_eq!(second, 2);
        let mut seen = HashSet::new();
        seen.insert("keep.txt".to_string());
        assert_eq!(db.finish_source_scan(source, first, &seen).unwrap(), None);
        assert!(db.file_by_id(drop).unwrap().is_some());
        assert_eq!(
            db.source_by_id(source).unwrap().unwrap().online_state,
            "scanning"
        );

        // Aborting the current generation keeps every file and its index.
        db.abort_source_scan(source, second, "offline").unwrap();
        assert!(db.file_by_id(drop).unwrap().is_some());
        assert_eq!(
            db.source_by_id(source).unwrap().unwrap().online_state,
            "offline"
        );
        assert_eq!(db.finish_source_scan(source, second, &seen).unwrap(), None);

        let third = db.begin_source_scan(source).unwrap();
        assert_eq!(
            db.finish_source_scan(source, third, &seen).unwrap(),
            Some(1)
        );
        assert!(db.file_by_id(keep).unwrap().is_some());
        assert!(db.file_by_id(drop).unwrap().is_none());
        assert!(db.candidate_chunks("windows").unwrap().is_empty());
        assert_eq!(db.candidate_chunks("内核").unwrap().len(), 1);
        let jobs = db.index_jobs_for_source(source).unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|job| job.id == scan_job));
        assert!(jobs
            .iter()
            .any(|job| job.id != scan_job && job.file_id == Some(keep)));
        assert!(jobs.iter().all(|job| job.file_id != Some(drop)));

        let source_row = db.source_by_id(source).unwrap().unwrap();
        assert_eq!(source_row.online_state, "online");
        assert_eq!(source_row.scan_generation, third);
        assert!(source_row.last_success_scan_at.is_some());
        assert_eq!(db.find_file(source, "keep.txt").unwrap().unwrap().id, keep);
        assert_eq!(
            db.conn()
                .query_row(
                    "SELECT seen_generation FROM files WHERE id = ?1",
                    params![keep],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            third
        );
        // Committing the same generation again is a no-op.
        assert_eq!(db.finish_source_scan(source, third, &seen).unwrap(), None);
        db.verify_fts().unwrap();
    }

    #[test]
    fn unknown_sources_and_invalid_abort_states_are_rejected() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/known").unwrap();
        assert!(db.source_by_id(source).unwrap().is_some());
        assert!(db.source_by_id(999).unwrap().is_none());
        assert!(matches!(
            db.begin_source_scan(999),
            Err(IndexError::SourceNotFound(999))
        ));
        assert!(matches!(
            db.queue_source_scan(999, false),
            Err(IndexError::SourceNotFound(999))
        ));
        assert!(matches!(
            db.abort_source_scan(999, 1, "offline"),
            Err(IndexError::SourceNotFound(999))
        ));
        assert!(matches!(
            db.finish_source_scan(999, 1, &HashSet::new()),
            Err(IndexError::SourceNotFound(999))
        ));

        let generation = db.begin_source_scan(source).unwrap();
        assert!(matches!(
            db.abort_source_scan(source, generation, "bogus"),
            Err(IndexError::InvalidSourceState(state)) if state == "bogus"
        ));
        // A stale generation cannot change the current scanning state.
        db.abort_source_scan(source, generation - 1, "failed")
            .unwrap();
        assert_eq!(
            db.source_by_id(source).unwrap().unwrap().online_state,
            "scanning"
        );
        db.abort_source_scan(source, generation, "failed").unwrap();
        assert_eq!(
            db.source_by_id(source).unwrap().unwrap().online_state,
            "failed"
        );
    }

    #[test]
    fn queue_and_claim_source_scans_reuse_and_dedupe() {
        let db = IndexDb::open_in_memory().unwrap();
        let source_a = db.ensure_source(SourceKind::Local, "/a").unwrap();
        let source_b = db.ensure_source(SourceKind::Local, "/b").unwrap();

        let first = db.queue_source_scan(source_a, false).unwrap();
        assert_eq!(db.queue_source_scan(source_a, false).unwrap(), first);
        assert_eq!(db.queue_source_scan(source_a, true).unwrap(), first);
        let jobs = db.index_jobs_for_source(source_a).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, "rebuild");

        let second = db.queue_source_scan(source_b, false).unwrap();
        assert_ne!(second, first);

        let claimed = db.claim_source_scan().unwrap().unwrap();
        assert_eq!(claimed.id, first);
        assert_eq!(claimed.state, "running");
        assert_eq!(claimed.attempt_count, 1);

        // The same source still has a running scan, so nothing else is claimed
        // there; the other source remains eligible.
        let claimed = db.claim_source_scan().unwrap().unwrap();
        assert_eq!(claimed.id, second);
        assert_eq!(claimed.attempt_count, 1);
        let rebuild = db.queue_source_scan(source_b, true).unwrap();
        assert_ne!(rebuild, second);
        assert_eq!(db.queue_source_scan(source_b, true).unwrap(), rebuild);
        assert!(db.claim_source_scan().unwrap().is_none());

        db.update_index_job(first, "done", 1, None).unwrap();
        let requeued = db.queue_source_scan(source_a, false).unwrap();
        assert_ne!(requeued, first);
        assert_eq!(db.claim_source_scan().unwrap().unwrap().id, requeued);
    }

    #[test]
    fn recover_source_scans_requeues_or_fails_and_clears_scanning() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source_a = db.ensure_source(SourceKind::Local, "/a").unwrap();
        let source_b = db.ensure_source(SourceKind::Local, "/b").unwrap();
        let retryable = db.insert_index_job(source_a, None, "scan").unwrap();
        let exhausted = db.insert_index_job(source_b, None, "rebuild").unwrap();
        db.update_index_job(retryable, "running", 1, None).unwrap();
        db.update_index_job(exhausted, "running", 3, Some("boom"))
            .unwrap();
        db.begin_source_scan(source_a).unwrap();
        db.begin_source_scan(source_b).unwrap();

        assert_eq!(db.recover_source_scans().unwrap(), 2);
        let jobs = db.index_jobs_for_source(source_a).unwrap();
        assert_eq!(jobs[0].state, "queued");
        assert_eq!(jobs[0].attempt_count, 1);
        let jobs = db.index_jobs_for_source(source_b).unwrap();
        assert_eq!(jobs[0].state, "failed");
        assert_eq!(jobs[0].last_error.as_deref(), Some("boom"));
        assert_eq!(
            db.source_by_id(source_a).unwrap().unwrap().online_state,
            "unknown"
        );
        assert_eq!(
            db.source_by_id(source_b).unwrap().unwrap().online_state,
            "unknown"
        );
    }

    #[test]
    fn concurrent_begin_and_claim_are_serialized() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let (dir, path) = temp_db("concurrent");
        {
            let mut db = IndexDb::open(&path).unwrap();
            let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
            assert_eq!(source, 1);
            db.insert_index_job(source, None, "scan").unwrap();
            db.insert_index_job(source, None, "scan").unwrap();
        }

        let threads = 4;
        let barrier = Arc::new(Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let db = IndexDb::open(&path).unwrap();
                    let generation = db.begin_source_scan(1).unwrap();
                    barrier.wait();
                    (generation, db.claim_source_scan().unwrap())
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        let generations: HashSet<i64> = results.iter().map(|(generation, _)| *generation).collect();
        assert_eq!(generations.len(), threads);
        assert!(generations.iter().all(|generation| *generation > 0));
        let claims: Vec<_> = results.into_iter().filter_map(|(_, job)| job).collect();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].state, "running");
        assert_eq!(claims[0].attempt_count, 1);

        let db = IndexDb::open(&path).unwrap();
        assert_eq!(
            db.source_by_id(1).unwrap().unwrap().scan_generation,
            threads as i64
        );
        let jobs = db.index_jobs_for_source(1).unwrap();
        assert_eq!(jobs.iter().filter(|job| job.state == "running").count(), 1);
        assert_eq!(jobs.iter().filter(|job| job.state == "queued").count(), 1);
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }
}
