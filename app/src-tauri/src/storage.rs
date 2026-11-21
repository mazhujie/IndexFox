//! M3.5 database location, online migration, cache clearing and shutdown
//! checkpoint.
//!
//! The root owns the database lifecycle. In particular:
//!
//! * `DatabaseLocation::migrate` must only be called while the root holds the
//!   exclusive database lease and background indexing is paused. This module
//!   never coordinates writers itself.
//! * Migration copies the live schema/data with `VACUUM INTO`; the old
//!   database file is never renamed, replaced or deleted.
//! * A new target must be absolute, must not exist, and must not have SQLite
//!   sidecars. The copied file is validated before it is published with
//!   `persist_noclobber`, and the settings file is updated only afterwards.
//! * Any failure before the settings update keeps `path()` on the old
//!   database. A settings failure removes the newly published target so the
//!   next start still opens the old database.
//!
//! This module intentionally uses only dependencies already used by the Tauri
//! app: `indexfox-index`, `serde_json` and `tempfile`.

#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use indexfox_index::IndexDb;
use serde_json::{json, Value};
use tempfile::{Builder, TempPath};

/// Current database path plus the JSON settings file that persists it.
#[derive(Debug, Clone)]
pub struct DatabaseLocation {
    current: PathBuf,
    settings_path: PathBuf,
}

impl DatabaseLocation {
    /// Load the persisted database path, falling back to `default_path`.
    ///
    /// A missing, unreadable, malformed or non-absolute `db_path` is ignored;
    /// starting the application must not fail because settings are damaged.
    pub fn load(default_path: PathBuf, settings_path: PathBuf) -> Self {
        let current = read_persisted_path(&settings_path).unwrap_or(default_path);
        Self {
            current,
            settings_path,
        }
    }

    /// Path of the database the application should open.
    pub fn path(&self) -> &Path {
        &self.current
    }

    /// Copy the current database to `target`, validate it, publish it and
    /// atomically update the settings file.
    ///
    /// The caller must hold the exclusive database lease and pause all
    /// background writers. On error the old database remains current and
    /// usable; any partially created target/temp file is removed.
    pub fn migrate(&mut self, target: &Path) -> Result<(), String> {
        validate_new_target(target)?;
        if same_logical_path(target, &self.settings_path) {
            return Err(format!(
                "migration target must not be the settings file: {}",
                target.display()
            ));
        }
        // A target at the current DB's sidecar path would silently corrupt the
        // retained old database as soon as it is opened again.
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let reserved = sidecar_path(&self.current, suffix);
            if same_logical_path(target, &reserved) {
                return Err(format!(
                    "migration target conflicts with the current database or its sidecars: {}",
                    target.display()
                ));
            }
        }

        let target_string = path_string(target)?;
        let parent = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| {
                format!(
                    "migration target has no parent directory: {}",
                    target.display()
                )
            })?;
        match fs::metadata(parent) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "migration target parent is not a directory: {}",
                    parent.display()
                ))
            }
            Err(error) => {
                return Err(format!(
                    "cannot access migration target parent {}: {error}",
                    parent.display()
                ))
            }
        }

        // The temp directory is in the target's own directory so the final
        // hard-link/rename cannot cross a filesystem boundary. VACUUM INTO
        // requires a path that does not exist yet, so the output lives one
        // level below a fresh private directory.
        let temp_dir = Builder::new()
            .prefix(".indexfox-migrate-")
            .tempdir_in(parent)
            .map_err(|error| {
                format!(
                    "cannot create a migration temp directory in {}: {error}",
                    parent.display()
                )
            })?;
        let staged = temp_dir.path().join("index.db");
        let staged_string = path_string(&staged)?;

        let source = self.current.as_path();
        let source_db = if source.try_exists().map_err(|error| {
            format!(
                "cannot inspect current database {}: {error}",
                source.display()
            )
        })? {
            IndexDb::open(source).map_err(|error| {
                format!("cannot open current database {}: {error}", source.display())
            })?
        } else {
            // First run with a custom location: publish a valid empty v1
            // database rather than failing merely because no old file exists.
            IndexDb::open_in_memory()
                .map_err(|error| format!("cannot create an empty migration source: {error}"))?
        };

        // Parameter binding keeps spaces, quotes and non-ASCII path
        // characters from being interpreted as SQL.
        source_db
            .conn()
            .execute("VACUUM INTO ?1", [&staged_string])
            .map_err(|error| format!("VACUUM INTO {} failed: {error}", staged.display()))?;
        drop(source_db);

        validate_staged_database(&staged)?;

        // Make the staged file durable before it can become the live path.
        let staged_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&staged)
            .map_err(|error| {
                format!(
                    "cannot reopen staged database {}: {error}",
                    staged.display()
                )
            })?;
        staged_file.sync_all().map_err(|error| {
            format!("cannot fsync staged database {}: {error}", staged.display())
        })?;
        drop(staged_file);

        // Re-check immediately before publishing. This closes the race where
        // another process creates the target or a sidecar while validation is
        // running.
        validate_new_target(target)?;
        ensure_no_sidecars(target, "target")?;

        let staged_path = TempPath::try_from_path(staged.clone()).map_err(|error| {
            format!("cannot track staged database {}: {error}", staged.display())
        })?;
        let published = staged_path.persist_noclobber(target);
        if let Err(error) = published {
            return Err(format!(
                "cannot publish migrated database at {} without overwriting: {}",
                target.display(),
                error.error
            ));
        }

        if let Err(error) = sync_dir(parent) {
            let rollback = remove_published_target(target);
            return Err(format!(
                "cannot fsync migration target directory {}: {error}{rollback}",
                parent.display()
            ));
        }
        if let Err(error) = ensure_no_sidecars(target, "published target") {
            let rollback = remove_published_target(target);
            return Err(format!("{error}{rollback}"));
        }

        // Settings are written last. Until this succeeds the application must
        // continue to use the old database.
        if let Err(error) = write_settings(&self.settings_path, &target_string) {
            let rollback = remove_published_target(target);
            return Err(format!(
                "settings update at {} failed: {error}{rollback}",
                self.settings_path.display()
            ));
        }

        self.current = target.to_path_buf();
        Ok(())
    }
}

/// Clear all cached document bodies and search structures in one transaction.
///
/// Sources, file metadata, search history and source files on disk are
/// preserved. Indexed/stale/failed files return to `pending` so a rebuild
/// re-extracts them; size-skipped, offline and already-pending rows keep their
/// state. Queued work is discarded because its content no longer exists.
///
/// The caller must pause background index writers and hold the database lease;
/// this function does not pause anything itself.
pub fn clear_cached_index(db: &mut IndexDb) -> Result<(), String> {
    let tx = db
        .conn()
        .unchecked_transaction()
        .map_err(|error| format!("cannot begin cache-clear transaction: {error}"))?;
    tx.execute("DELETE FROM chunks", [])
        .map_err(|error| format!("cannot clear chunks: {error}"))?;
    tx.execute("DELETE FROM ocr_page_cache", [])
        .map_err(|error| format!("cannot clear OCR pages: {error}"))?;
    // The chunks DELETE triggers already remove the corresponding external
    // content rows. `delete-all` also scrubs any pre-existing FTS divergence.
    tx.execute(
        "INSERT INTO chunks_fts(chunks_fts) VALUES('delete-all')",
        [],
    )
    .map_err(|error| format!("cannot clear FTS index: {error}"))?;
    tx.execute(
        "UPDATE files
         SET state = 'pending', indexed_at = NULL, error_message = NULL, ocr_profile = NULL
         WHERE state IN ('indexed', 'stale', 'failed')",
        [],
    )
    .map_err(|error| format!("cannot reset file index state: {error}"))?;
    tx.execute("DELETE FROM index_jobs", [])
        .map_err(|error| format!("cannot clear index jobs: {error}"))?;
    tx.commit()
        .map_err(|error| format!("cannot commit cache clear: {error}"))?;
    Ok(())
}

/// Checkpoint the WAL during application shutdown.
///
/// A missing database is treated as already closed. Opening the database may
/// apply the normal schema migration, matching what the next application start
/// would do anyway.
pub fn checkpoint(path: &Path) -> Result<(), String> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(format!(
                "database checkpoint path is not a file: {}",
                path.display()
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "cannot inspect database for checkpoint {}: {error}",
                path.display()
            ))
        }
    }

    let db = IndexDb::open(path).map_err(|error| {
        format!(
            "cannot open database for checkpoint {}: {error}",
            path.display()
        )
    })?;
    db.conn()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .map_err(|error| format!("WAL checkpoint failed for {}: {error}", path.display()))?;
    Ok(())
}

fn same_logical_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (logical_path(left), logical_path(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Canonicalize an existing path, or the canonical parent plus the final
/// component for a path that does not exist yet.
fn logical_path(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = fs::canonicalize(path) {
        return Some(canonical);
    }
    let parent = path.parent()?;
    let parent = fs::canonicalize(parent).ok()?;
    Some(parent.join(path.file_name()?))
}

fn read_persisted_path(settings_path: &Path) -> Option<PathBuf> {
    let bytes = fs::read(settings_path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let raw = value.get("db_path")?.as_str()?;
    let path = PathBuf::from(raw);
    path.is_absolute().then_some(path)
}

fn validate_new_target(target: &Path) -> Result<(), String> {
    if !target.is_absolute() {
        return Err(format!(
            "migration target must be an absolute path: {}",
            target.display()
        ));
    }
    if target.file_name().is_none() {
        return Err(format!(
            "migration target must name a database file: {}",
            target.display()
        ));
    }
    reject_existing(target, "database")?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = sidecar_path(target, suffix);
        reject_existing(&sidecar, "database sidecar")?;
    }
    Ok(())
}

fn reject_existing(path: &Path, label: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(format!(
            "refusing to overwrite existing {label}: {}",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "cannot inspect {label} path {}: {error}",
            path.display()
        )),
    }
}

fn ensure_no_sidecars(target: &Path, label: &str) -> Result<(), String> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = sidecar_path(target, suffix);
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => {
                return Err(format!(
                    "refusing to use {label} because a sidecar already exists: {}",
                    sidecar.display()
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot inspect sidecar path {}: {error}",
                    sidecar.display()
                ))
            }
        }
    }
    Ok(())
}

fn validate_staged_database(staged: &Path) -> Result<(), String> {
    let db = IndexDb::open(staged).map_err(|error| {
        format!(
            "cannot open staged database {} for validation: {error}",
            staged.display()
        )
    })?;

    let mut stmt = db
        .conn()
        .prepare("PRAGMA integrity_check")
        .map_err(|error| format!("cannot prepare integrity_check: {error}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("cannot run integrity_check: {error}"))?;
    let mut findings = Vec::new();
    for row in rows {
        findings.push(row.map_err(|error| format!("cannot read integrity_check row: {error}"))?);
    }
    drop(stmt);
    if findings.len() != 1 || findings[0] != "ok" {
        return Err(format!(
            "staged database failed integrity_check: {}",
            if findings.is_empty() {
                "no result".to_string()
            } else {
                findings.join("; ")
            }
        ));
    }

    let mut stmt = db
        .conn()
        .prepare("PRAGMA foreign_key_check")
        .map_err(|error| format!("cannot prepare foreign_key_check: {error}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|error| format!("cannot run foreign_key_check: {error}"))?;
    if let Some(row) = rows
        .next()
        .map_err(|error| format!("cannot read foreign_key_check row: {error}"))?
    {
        let table: String = row
            .get(0)
            .map_err(|error| format!("cannot read foreign_key_check table: {error}"))?;
        let rowid: Option<i64> = row
            .get(1)
            .map_err(|error| format!("cannot read foreign_key_check rowid: {error}"))?;
        let parent: String = row
            .get(2)
            .map_err(|error| format!("cannot read foreign_key_check parent: {error}"))?;
        let fk: i64 = row
            .get(3)
            .map_err(|error| format!("cannot read foreign_key_check id: {error}"))?;
        return Err(format!(
            "staged database failed foreign_key_check: table {table}, rowid {rowid:?}, parent {parent}, fk {fk}"
        ));
    }
    drop(rows);
    drop(stmt);

    db.verify_fts()
        .map_err(|error| format!("staged database failed FTS integrity check: {error}"))?;

    // Leave the staged file in rollback-journal mode with no outstanding WAL.
    // This makes the single file published below self-contained.
    db.conn()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode = DELETE;")
        .map_err(|error| format!("cannot finalize staged database WAL: {error}"))?;
    drop(db);

    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(staged, suffix);
        match fs::symlink_metadata(&sidecar) {
            Ok(metadata) if metadata.len() == 0 => {
                fs::remove_file(&sidecar).map_err(|error| {
                    format!(
                        "cannot remove staged sidecar {}: {error}",
                        sidecar.display()
                    )
                })?;
            }
            Ok(_) => {
                return Err(format!(
                    "staged database still has non-empty sidecar: {}",
                    sidecar.display()
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot inspect staged sidecar {}: {error}",
                    sidecar.display()
                ))
            }
        }
    }
    Ok(())
}

fn write_settings(settings_path: &Path, target: &str) -> Result<(), String> {
    let parent = parent_dir(settings_path);
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(&parent).map_err(|error| {
            format!(
                "cannot create settings directory {}: {error}",
                parent.display()
            )
        })?;
    }

    let mut root = match fs::read(settings_path) {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
                format!(
                    "existing settings file {} is not valid JSON: {error}",
                    settings_path.display()
                )
            })?;
            if !value.is_object() {
                return Err(format!(
                    "existing settings file {} is not a JSON object",
                    settings_path.display()
                ));
            }
            value
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({}),
        Err(error) => {
            return Err(format!(
                "cannot read existing settings file {}: {error}",
                settings_path.display()
            ))
        }
    };
    root["db_path"] = Value::String(target.to_string());

    let mut bytes = serde_json::to_vec_pretty(&root)
        .map_err(|error| format!("cannot serialize settings: {error}"))?;
    bytes.push(b'\n');

    let mut temp = Builder::new()
        .prefix(".indexfox-settings-")
        .suffix(".tmp")
        .tempfile_in(&parent)
        .map_err(|error| {
            format!(
                "cannot create settings temp file in {}: {error}",
                parent.display()
            )
        })?;
    temp.write_all(&bytes)
        .map_err(|error| format!("cannot write settings temp file: {error}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| format!("cannot fsync settings temp file: {error}"))?;
    temp.persist(settings_path).map_err(|error| {
        format!(
            "cannot atomically replace settings file {}: {}",
            settings_path.display(),
            error.error
        )
    })?;
    // The rename itself is already durable enough for a fallback settings
    // file; directory fsync is best effort and must not turn a successful
    // settings update into a failed migration.
    let _ = sync_dir(&parent);
    Ok(())
}

fn remove_published_target(target: &Path) -> String {
    match fs::remove_file(target) {
        Ok(()) => format!("; removed unpublished target {}", target.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            format!("; target {} was already absent", target.display())
        }
        Err(error) => format!(
            "; additionally failed to remove unpublished target {}: {error}",
            target.display()
        ),
    }
}

fn path_string(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))?;
    if text.contains('\0') {
        return Err(format!("path contains a NUL byte: {}", path.display()));
    }
    Ok(text.to_string())
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn parent_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexfox_core::{
        ExtractedDocument, FileState, Origin, SearchMode, Section, SourceKind, SourceLocator,
    };
    use indexfox_index::NewFile;
    use tempfile::TempDir;

    fn document(text: &str) -> ExtractedDocument {
        ExtractedDocument {
            extractor_version: "test".to_string(),
            encoding: None,
            sections: vec![Section {
                text: text.to_string(),
                display_text: text.to_string(),
                locator: SourceLocator::Text {
                    start_line: 1,
                    end_line: 1,
                    start_offset: 0,
                    end_offset: text.len(),
                },
                origin: Origin::Native,
            }],
        }
    }

    fn seed(path: &Path) -> i64 {
        let mut db = IndexDb::open(path).unwrap();
        let source = db
            .ensure_source(SourceKind::Local, "/tmp/storage-source")
            .unwrap();
        let file = db
            .upsert_file(&NewFile {
                source_id: source,
                path_key: "doc.txt".to_string(),
                display_path: "/tmp/storage-source/doc.txt".to_string(),
                filename: "doc.txt".to_string(),
                ext: "txt".to_string(),
                size_bytes: 12,
                mtime_ns: Some(1),
                state: FileState::Pending,
            })
            .unwrap();
        db.replace_file_content(file, &document("内核驱动 Linux kernel"))
            .unwrap();
        db.add_search_history("kernel", SearchMode::Exact, "{}")
            .unwrap();
        db.insert_index_job(source, Some(file), "index").unwrap();
        file
    }

    fn temp_root() -> TempDir {
        Builder::new()
            .prefix("indexfox-storage-")
            .tempdir()
            .unwrap()
    }

    #[test]
    fn migration_copies_v1_data_and_persists_settings() {
        let root = temp_root();
        let old = root.path().join("old.db");
        let settings = root.path().join("settings.json");
        let file_id = seed(&old);
        fs::write(&settings, br#"{"theme":"dark","history_enabled":true}"#).unwrap();

        let target_dir = root.path().join("new");
        fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("index.db");

        let mut location = DatabaseLocation::load(old.clone(), settings.clone());
        assert_eq!(location.path(), old.as_path());
        location.migrate(&target).unwrap();
        assert_eq!(location.path(), target.as_path());
        assert!(old.exists(), "migration must retain the old database");
        assert!(!sidecar_path(&target, "-wal").exists());
        assert!(!sidecar_path(&target, "-shm").exists());

        let copied = IndexDb::open(&target).unwrap();
        assert_eq!(copied.list_sources().unwrap().len(), 1);
        let file = copied.file_by_id(file_id).unwrap().unwrap();
        assert_eq!(file.state, "indexed");
        assert_eq!(copied.candidate_chunks("内核").unwrap().len(), 1);
        assert_eq!(copied.search_history().unwrap().len(), 1);
        copied.verify_fts().unwrap();
        drop(copied);

        let original = IndexDb::open(&old).unwrap();
        assert_eq!(original.candidate_chunks("内核").unwrap().len(), 1);
        drop(original);

        let persisted: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(persisted["db_path"], target.to_str().unwrap());
        assert_eq!(persisted["theme"], "dark");

        let reloaded = DatabaseLocation::load(root.path().join("ignored.db"), settings);
        assert_eq!(reloaded.path(), target.as_path());
    }

    #[test]
    fn migration_refuses_existing_target_sidecars_and_relative_paths() {
        let root = temp_root();
        let old = root.path().join("old.db");
        let settings = root.path().join("settings.json");
        seed(&old);
        let mut location = DatabaseLocation::load(old.clone(), settings.clone());

        let existing = root.path().join("existing.db");
        fs::write(&existing, b"do not touch").unwrap();
        let error = location.migrate(&existing).unwrap_err();
        assert!(error.contains("refusing to overwrite"), "{error}");
        assert_eq!(fs::read(&existing).unwrap(), b"do not touch");

        let target_dir = root.path().join("new");
        fs::create_dir(&target_dir).unwrap();
        let with_sidecar = target_dir.join("sidecar.db");
        fs::write(sidecar_path(&with_sidecar, "-wal"), b"wal").unwrap();
        let error = location.migrate(&with_sidecar).unwrap_err();
        assert!(error.contains("-wal"), "{error}");
        assert!(!with_sidecar.exists());
        assert_eq!(location.path(), old.as_path());

        let error = location.migrate(Path::new("relative.db")).unwrap_err();
        assert!(error.contains("absolute"), "{error}");
        assert_eq!(location.path(), old.as_path());

        // Never create a new database where the retained database's sidecar
        // would be expected: opening the old database would then read it as a
        // WAL/journal.
        let reserved = sidecar_path(&old, "-journal");
        let error = location.migrate(&reserved).unwrap_err();
        assert!(error.contains("conflicts"), "{error}");
        assert!(!reserved.exists());
        assert_eq!(location.path(), old.as_path());
    }

    #[test]
    fn settings_failure_keeps_old_database_and_removes_target() {
        let root = temp_root();
        let old = root.path().join("old.db");
        let settings = root.path().join("settings.json");
        let file_id = seed(&old);
        let mut location = DatabaseLocation::load(old.clone(), settings.clone());

        // A directory cannot be atomically replaced by the settings rename.
        fs::create_dir(&settings).unwrap();
        let target_dir = root.path().join("new");
        fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("index.db");

        let error = location.migrate(&target).unwrap_err();
        assert!(error.contains("settings"), "{error}");
        assert!(
            !target.exists(),
            "failed settings update must roll back target"
        );
        assert_eq!(location.path(), old.as_path());

        let original = IndexDb::open(&old).unwrap();
        assert_eq!(
            original.file_by_id(file_id).unwrap().unwrap().state,
            "indexed"
        );
        assert_eq!(original.candidate_chunks("内核").unwrap().len(), 1);
    }

    #[test]
    fn clear_cached_index_keeps_metadata_history_and_source_files() {
        let root = temp_root();
        let db_path = root.path().join("index.db");
        let source_file = root.path().join("doc.txt");
        fs::write(&source_file, "original file").unwrap();
        let file_id = seed(&db_path);
        let mut db = IndexDb::open(&db_path).unwrap();
        assert_eq!(db.candidate_chunks("内核").unwrap().len(), 1);
        db.conn()
            .execute(
                "INSERT INTO ocr_page_cache(file_id, cache_key, text) VALUES (?1, 'page1', 'cached OCR')",
                [file_id],
            )
            .unwrap();

        clear_cached_index(&mut db).unwrap();

        let stats = db.stats().unwrap();
        assert_eq!(stats.chunks, 0);
        assert_eq!(stats.short_grams, 0);
        assert!(db.cached_ocr_pages(file_id).unwrap().is_empty());
        assert!(db.candidate_chunks("内核").unwrap().is_empty());
        assert_eq!(db.list_sources().unwrap().len(), 1);
        let file = db.file_by_id(file_id).unwrap().unwrap();
        assert_eq!(file.state, "pending");
        assert!(file.indexed_at.is_none());
        assert_eq!(db.search_history().unwrap().len(), 1);
        assert!(db.index_jobs_for_source(1).unwrap().is_empty());
        db.verify_fts().unwrap();
        assert!(
            source_file.exists(),
            "cache clearing must not delete sources"
        );
    }

    #[test]
    fn checkpoint_handles_existing_and_missing_databases() {
        let root = temp_root();
        let existing = root.path().join("old.db");
        seed(&existing);
        checkpoint(&existing).unwrap();
        assert!(existing.exists());

        let missing = root.path().join("missing.db");
        checkpoint(&missing).unwrap();
        assert!(!missing.exists());
    }
}
