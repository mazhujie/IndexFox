//! M3 focused failure acceptance: offline roots, cancellation, directory
//! mutation during a scan, overlapping roots, and SQLite `SQLITE_FULL`.
//!
//! Every test drives the production entry points used by the app:
//! `register_source` + `index_registered_source`, the generation-aware
//! `begin/finish/abort_source_scan` path, and verified search results.
//!
//! Simulation boundaries:
//! - Offline/root mutation use a temporary directory rename and extra file
//!   creation; no network or removable media is involved.
//! - Disk exhaustion is simulated with SQLite's `max_page_count`, not by
//!   filling the host filesystem.

use std::cell::Cell;
use std::fs;
use std::path::Path;

use indexfox_cli::pipeline::{index_registered_source, register_source, IndexReport};
use indexfox_core::FileState;
use indexfox_index::IndexDb;
use indexfox_query::{search, SearchOptions, SearchResponse};
use tempfile::TempDir;

fn query(db: &IndexDb, text: &str) -> SearchResponse {
    search(db, text, &SearchOptions::default()).expect("search")
}

fn names(response: &SearchResponse) -> Vec<&str> {
    response
        .results
        .iter()
        .map(|result| result.filename.as_str())
        .collect()
}

fn index_registered(db: &mut IndexDb, source_id: i64) -> IndexReport {
    index_registered_source(db, source_id, false, &|| false, &|_| {}).expect("scan source")
}

fn online_state(db: &IndexDb, source_id: i64) -> String {
    db.source_by_id(source_id).unwrap().unwrap().online_state
}

fn assert_preserved(db: &IndexDb, token: &str, filename: &str) {
    let response = query(db, token);
    assert_eq!(
        names(&response),
        vec![filename],
        "old full text must survive"
    );
    assert_eq!(response.results[0].state, FileState::Indexed.as_str());
}

#[test]
fn offline_root_scan_keeps_old_index_and_only_a_complete_scan_deletes() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("alpha.txt"), "offline_alpha_token").unwrap();

    let mut db = IndexDb::open_in_memory().unwrap();
    let source = register_source(&db, &root).unwrap();
    assert_eq!(index_registered(&mut db, source).indexed, 1);
    assert_eq!(online_state(&db, source), "online");

    // The root disappears between scans (removable drive unplugged, share
    // unmounted). The source root path stays registered.
    let hidden = temp.path().join("root.offline");
    fs::rename(&root, &hidden).unwrap();

    let error = index_registered_source(&mut db, source, false, &|| false, &|_| {}).unwrap_err();
    assert!(
        format!("{error:#}").contains("not an accessible directory"),
        "{error:#}"
    );
    assert_eq!(online_state(&db, source), "offline");
    assert_preserved(&db, "offline_alpha_token", "alpha.txt");

    // A second offline attempt must be equally non-destructive.
    assert!(index_registered_source(&mut db, source, false, &|| false, &|_| {}).is_err());
    assert_preserved(&db, "offline_alpha_token", "alpha.txt");

    // Recovery alone deletes nothing. Only a complete scan that truly misses
    // the file reconciles it away.
    fs::rename(&hidden, &root).unwrap();
    fs::remove_file(root.join("alpha.txt")).unwrap();

    let report = index_registered(&mut db, source);
    assert!(!report.incomplete_scan, "{report:?}");
    assert_eq!(report.removed, 1, "{report:?}");
    assert_eq!(online_state(&db, source), "online");
    assert!(query(&db, "offline_alpha_token").results.is_empty());
}

#[test]
fn progress_cancellation_keeps_old_version_until_a_clean_scan() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    let alpha = root.join("alpha.txt");
    fs::write(&alpha, "cancel_old_token").unwrap();

    let mut db = IndexDb::open_in_memory().unwrap();
    let source = register_source(&db, &root).unwrap();
    assert_eq!(index_registered(&mut db, source).indexed, 1);

    // Change the file on disk, then cancel from the progress callback after
    // enumeration but before the first file is consumed.
    fs::write(&alpha, "cancel_new_token").unwrap();
    let cancel = Cell::new(false);
    let progress = |report: &IndexReport| {
        if report.scanned > 0 {
            cancel.set(true);
        }
    };
    let error =
        index_registered_source(&mut db, source, false, &|| cancel.get(), &progress).unwrap_err();
    assert!(format!("{error:#}").contains("scan cancelled"), "{error:#}");

    assert_preserved(&db, "cancel_old_token", "alpha.txt");
    assert!(query(&db, "cancel_new_token").results.is_empty());
    assert_eq!(online_state(&db, source), "online");

    // No cancellation afterwards: the same generated scan publishes the new
    // version atomically.
    let report = index_registered(&mut db, source);
    assert_eq!(report.indexed, 1, "{report:?}");
    assert!(query(&db, "cancel_old_token").results.is_empty());
    assert_eq!(names(&query(&db, "cancel_new_token")), vec!["alpha.txt"]);
}

#[test]
fn directory_change_during_scan_aborts_reconcile_and_keeps_old_index() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    let alpha = root.join("alpha.txt");
    fs::write(&alpha, "stable_old_token").unwrap();

    let mut db = IndexDb::open_in_memory().unwrap();
    let source = register_source(&db, &root).unwrap();
    assert_eq!(index_registered(&mut db, source).indexed, 1);

    // Mutate the root after the enumeration snapshot, changing its mtime. The
    // generation must abort instead of reconciling against the stale listing.
    let changed = Cell::new(false);
    let root_for_change = root.clone();
    let progress = move |report: &IndexReport| {
        if report.scanned > 0 && !changed.replace(true) {
            fs::write(root_for_change.join("delta.txt"), "delta_new_token").unwrap();
        }
    };
    let report = index_registered_source(&mut db, source, false, &|| false, &progress).unwrap();
    assert!(report.incomplete_scan, "{report:?}");
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("目录发生变化")),
        "{report:?}"
    );
    assert_eq!(report.removed, 0, "{report:?}");
    assert_eq!(online_state(&db, source), "failed");
    assert_preserved(&db, "stable_old_token", "alpha.txt");
    assert!(query(&db, "delta_new_token").results.is_empty());

    // A later stable scan is complete and publishes the edited file.
    fs::remove_file(root.join("delta.txt")).unwrap();
    fs::write(&alpha, "stable_new_token").unwrap();
    let report = index_registered(&mut db, source);
    assert!(!report.incomplete_scan, "{report:?}");
    assert_eq!(report.indexed, 1, "{report:?}");
    assert!(query(&db, "stable_old_token").results.is_empty());
    assert_eq!(names(&query(&db, "stable_new_token")), vec!["alpha.txt"]);
}

#[test]
fn registered_source_honors_camel_case_config_and_rejects_overlapping_roots() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("root");
    fs::create_dir_all(root.join("skip")).unwrap();
    fs::write(root.join("keep.txt"), "keep_unique_token").unwrap();
    fs::write(root.join("skip/hidden.txt"), "hidden_unique_token").unwrap();

    let mut db = IndexDb::open_in_memory().unwrap();
    let source = register_source(&db, &root).unwrap();
    db.update_source_config(
        source,
        true,
        "auto",
        r#"{"extensions":["txt"],"excludedDirs":["skip"],"maxFileSizeBytes":1048576}"#,
    )
    .unwrap();

    let report = index_registered(&mut db, source);
    assert_eq!(report.indexed, 1, "{report:?}");
    assert_eq!(names(&query(&db, "keep_unique_token")), vec!["keep.txt"]);
    assert!(query(&db, "hidden_unique_token").results.is_empty());
    assert_eq!(db.stats().unwrap().files, 1);

    // Re-registering the same root reuses the source and the same rows.
    let same = register_source(&db, &root).unwrap();
    assert_eq!(same, source);
    assert_eq!(index_registered(&mut db, source).unchanged, 1);
    assert_eq!(db.list_sources().unwrap().len(), 1);
    assert_eq!(db.stats().unwrap().files, 1);

    // A nested root must be rejected before it can create a second metadata
    // owner for the same tree.
    let error = register_source(&db, &root.join("skip"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("重叠"), "{error}");
    assert_eq!(db.list_sources().unwrap().len(), 1);

    // Symmetric: parent after child is also rejected.
    let reverse = IndexDb::open_in_memory().unwrap();
    register_source(&reverse, &root.join("skip")).unwrap();
    let error = register_source(&reverse, &root).unwrap_err().to_string();
    assert!(error.contains("重叠"), "{error}");
    assert_eq!(reverse.list_sources().unwrap().len(), 1);
}

#[test]
fn sqlite_full_rolls_back_new_version_and_keeps_old_index() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    let alpha = root.join("alpha.txt");
    fs::write(&alpha, "disk_old_token").unwrap();

    let db_path = temp.path().join("index.sqlite");
    let mut db = IndexDb::open(&db_path).unwrap();
    let source = register_source(&db, &root).unwrap();
    assert_eq!(index_registered(&mut db, source).indexed, 1);
    let before = query(&db, "disk_old_token").results[0].clone();

    // Leave a few pages of headroom so the scan can start and only the new
    // content insert hits the limit; 1.2 MiB of text needs far more.
    let pages: i64 = db
        .conn()
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    db.conn()
        .execute_batch(&format!("PRAGMA max_page_count = {}", pages + 8))
        .unwrap();

    let new_text = format!(
        "disk_new_token\n{}",
        "replacement_filler_token ".repeat(50_000)
    );
    fs::write(&alpha, new_text).unwrap();

    let error = index_registered_source(&mut db, source, false, &|| false, &|_| {}).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.to_ascii_lowercase().contains("full"),
        "expected SQLITE_FULL, got {message}"
    );

    let response = query(&db, "disk_old_token");
    let after = &response.results[0];
    assert_eq!(after.file_id, before.file_id);
    assert_eq!(after.state, FileState::Stale.as_str());
    assert_eq!(after.size_bytes, before.size_bytes);
    assert_eq!(after.mtime_ns, before.mtime_ns);
    assert!(query(&db, "disk_new_token").results.is_empty());
    db.verify_fts().unwrap();

    // Keep the path referenced so the test documents the real on-disk DB.
    assert!(Path::new(&db_path).is_file());
}
