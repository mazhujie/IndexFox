use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use indexfox_core::{FileState, SourceKind, SourceScanConfig};
use indexfox_extract::{
    extract_path_with_ocr_settings, ocr_page_cache_key, ChunkOptions, OcrExtraction, OcrOptions,
    OcrSettings, PageOcrCache,
};
use indexfox_index::{IndexDb, IndexError, NewFile, SourceRecord};
use indexfox_source::{
    absolute_path, canonical_path_key, paths_overlap, scan_file, scan_root_controlled, source_kind,
    ScanError, ScanOptions, ScannedFile,
};

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub max_file_size_bytes: u64,
    pub force: bool,
    pub chunk: ChunkOptions,
    pub ocr: Option<OcrOptions>,
    pub ocr_settings: Option<OcrSettings>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        let config = indexfox_core::AppConfig::default();
        Self {
            max_file_size_bytes: config.max_file_size_bytes,
            force: false,
            chunk: ChunkOptions::default(),
            ocr: None,
            ocr_settings: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct IndexReport {
    pub scanned: usize,
    pub indexed: usize,
    pub unchanged: usize,
    pub skipped_size: usize,
    pub failed: usize,
    pub removed: usize,
    pub incomplete_scan: bool,
    pub issues: Vec<String>,
    pub elapsed_ms: u64,
}

pub fn index_directory(
    db: &mut IndexDb,
    root: &Path,
    options: &IndexOptions,
) -> Result<IndexReport> {
    let scan_options = ScanOptions {
        max_file_size_bytes: options.max_file_size_bytes,
        ..ScanOptions::default()
    };
    let source_id = register_source(db, root)?;
    let source = db
        .source_by_id(source_id)?
        .ok_or(IndexError::SourceNotFound(source_id))?;
    run_source(db, &source, options, &scan_options, &|| false, &|_| {})
}

pub fn register_source(db: &IndexDb, root: &Path) -> Result<i64> {
    let root = absolute_path(root)?;
    let kind = source_kind(&root);
    let real_root = local_root(&root);
    let key = canonical_path_key(&root);
    for existing in db.list_sources()? {
        let existing_root = Path::new(&existing.root_path);
        if key == canonical_path_key(existing_root) {
            return Ok(existing.id);
        }
        if paths_overlap(&root, existing_root) {
            bail!("目录与已索引目录重叠：{}", existing.root_path);
        }
        if let (Some(real_root), Some(real_existing)) = (&real_root, local_root(existing_root)) {
            if canonical_path_key(real_root) == canonical_path_key(&real_existing) {
                return Ok(existing.id);
            }
            if paths_overlap(real_root, &real_existing) {
                bail!("目录与已索引目录重叠：{}", existing.root_path);
            }
        }
    }
    readable_root(&root)?;
    Ok(db.ensure_source(kind, &root.display().to_string())?)
}

fn local_root(path: &Path) -> Option<std::path::PathBuf> {
    match source_kind(path) {
        SourceKind::Local | SourceKind::Removable => path.canonicalize().ok(),
        _ => None,
    }
}

pub fn index_registered_source(
    db: &mut IndexDb,
    source_id: i64,
    force: bool,
    should_stop: &dyn Fn() -> bool,
    progress: &dyn Fn(&IndexReport),
) -> Result<IndexReport> {
    let source = db
        .source_by_id(source_id)?
        .ok_or(IndexError::SourceNotFound(source_id))?;
    if !source.enabled {
        bail!("索引源已停用：{}", source.root_path);
    }
    let config: SourceScanConfig = serde_json::from_str(&source.config_json)?;
    let ocr = db.ocr_config()?;
    let options = IndexOptions {
        max_file_size_bytes: config.max_file_size_bytes,
        force,
        ocr_settings: ocr.enabled.then(|| OcrSettings::from_config(&ocr)),
        ..IndexOptions::default()
    };
    let scan_options = ScanOptions {
        max_file_size_bytes: config.max_file_size_bytes,
        max_depth: (!config.recursive).then_some(1),
        extensions: config.extensions,
        excluded_dirs: config.excluded_dirs,
        exclude_globs: config.exclude_globs,
    };
    run_source(db, &source, &options, &scan_options, should_stop, progress)
}

fn run_source(
    db: &mut IndexDb,
    source: &SourceRecord,
    options: &IndexOptions,
    scan_options: &ScanOptions,
    should_stop: &dyn Fn() -> bool,
    progress: &dyn Fn(&IndexReport),
) -> Result<IndexReport> {
    check_stop(should_stop)?;
    let generation = db.begin_source_scan(source.id)?;
    let result = run_generation(
        db,
        source,
        generation,
        options,
        scan_options,
        should_stop,
        progress,
    );
    if let Err(error) = &result {
        let status = match error.downcast_ref::<ScanError>() {
            Some(ScanError::Unavailable(_)) => "offline",
            Some(ScanError::Cancelled) if source.online_state != "scanning" => {
                source.online_state.as_str()
            }
            Some(ScanError::Cancelled) => "unknown",
            _ => "failed",
        };
        if let Err(update_error) = db.abort_source_scan(source.id, generation, status) {
            return Err(update_error).context(error.to_string());
        }
    }
    result
}

fn run_generation(
    db: &mut IndexDb,
    source: &SourceRecord,
    generation: i64,
    options: &IndexOptions,
    scan_options: &ScanOptions,
    should_stop: &dyn Fn() -> bool,
    progress: &dyn Fn(&IndexReport),
) -> Result<IndexReport> {
    let started = Instant::now();
    let source_id = source.id;
    let root = Path::new(&source.root_path);
    let root_before = readable_root(root)?;
    progress(&IndexReport::default());
    let outcome = scan_root_controlled(root, scan_options, should_stop)?;

    let mut report = IndexReport {
        scanned: outcome.files.len(),
        incomplete_scan: !outcome.complete,
        issues: outcome
            .issues
            .iter()
            .map(|issue| format!("{}: {}", issue.path, issue.message))
            .collect(),
        ..IndexReport::default()
    };
    progress(&report);
    let mut seen = HashSet::with_capacity(outcome.files.len());

    for file in &outcome.files {
        check_stop(should_stop)?;
        if !seen.insert(file.path_key.clone()) {
            report.unchanged += 1;
            continue;
        }
        match index_one(db, source_id, file, options, should_stop)? {
            FileOutcome::Indexed => report.indexed += 1,
            FileOutcome::Unchanged => report.unchanged += 1,
            FileOutcome::SkippedSize => report.skipped_size += 1,
            FileOutcome::Failed(issue) => {
                report.failed += 1;
                report.issues.push(issue);
            }
        }
        progress(&report);
    }

    check_stop(should_stop)?;
    let root_after = readable_root(root)?;
    if root_before != root_after {
        report.incomplete_scan = true;
        report
            .issues
            .push(format!("扫描期间目录发生变化：{}", root.display()));
    }
    check_stop(should_stop)?;
    if report.incomplete_scan {
        db.abort_source_scan(source_id, generation, "failed")?;
    } else {
        // shortcut: rename reconciles as removal plus insert until stable file IDs are available.
        report.removed = db
            .finish_source_scan(source_id, generation, &seen)?
            .ok_or(ScanError::Cancelled)?;
    }

    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

fn readable_root(root: &Path) -> Result<(Option<SystemTime>, Option<SystemTime>), ScanError> {
    let unavailable =
        |error: std::io::Error| ScanError::Unavailable(format!("{}: {error}", root.display()));
    let mut entries = std::fs::read_dir(root).map_err(unavailable)?;
    entries.next().transpose().map_err(unavailable)?;
    let metadata = std::fs::metadata(root).map_err(unavailable)?;
    Ok((metadata.created().ok(), metadata.modified().ok()))
}

fn check_stop(should_stop: &dyn Fn() -> bool) -> Result<(), ScanError> {
    if should_stop() {
        return Err(ScanError::Cancelled);
    }
    Ok(())
}

/// Index exactly one file below `root` without walking or reconciling the
/// directory. Other files' records (including stale/removed ones) are untouched.
pub fn index_file(
    db: &mut IndexDb,
    root: &Path,
    relative: &Path,
    options: &IndexOptions,
) -> Result<IndexReport> {
    let started = Instant::now();
    let scan_options = ScanOptions {
        max_file_size_bytes: options.max_file_size_bytes,
        ..ScanOptions::default()
    };
    let root = absolute_path(root)?;
    let file = scan_file(&root, relative, &scan_options)?;
    let source_id = register_source(db, &root)?;

    let mut report = IndexReport {
        scanned: 1,
        ..IndexReport::default()
    };
    match index_one(db, source_id, &file, options, &|| false)? {
        FileOutcome::Indexed => report.indexed = 1,
        FileOutcome::Unchanged => report.unchanged = 1,
        FileOutcome::SkippedSize => report.skipped_size = 1,
        FileOutcome::Failed(issue) => {
            report.failed = 1;
            report.issues.push(issue);
        }
    }
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

enum FileOutcome {
    Indexed,
    Unchanged,
    SkippedSize,
    Failed(String),
}

struct CachedOcrPages<'a> {
    available: &'a BTreeMap<String, String>,
    used: RefCell<BTreeMap<String, String>>,
}

impl PageOcrCache for CachedOcrPages<'_> {
    fn get(&self, key: &str) -> Option<String> {
        let text = self.available.get(key)?;
        self.put(key, text);
        Some(text.clone())
    }

    fn put(&self, key: &str, text: &str) {
        self.used
            .borrow_mut()
            .insert(key.to_owned(), text.to_owned());
    }
}

#[derive(Default)]
struct StableExtraction {
    result: Option<OcrExtraction>,
    pages: BTreeMap<String, String>,
    size: u64,
    mtime: Option<i64>,
}

fn index_one(
    db: &mut IndexDb,
    source_id: i64,
    file: &ScannedFile,
    options: &IndexOptions,
    should_stop: &dyn Fn() -> bool,
) -> Result<FileOutcome> {
    let existing = db.find_file(source_id, &file.path_key)?;
    let is_pdf = file.ext.eq_ignore_ascii_case("pdf");
    let legacy_ocr = if is_pdf && options.ocr_settings.is_none() {
        options.ocr.as_ref().map(OcrSettings::from_options)
    } else {
        None
    };
    let ocr = options.ocr_settings.as_ref().or(legacy_ocr.as_ref());
    let profile = is_pdf.then(|| {
        ocr.map(|settings| ocr_page_cache_key("", 0, settings))
            .unwrap_or_else(|| "native".to_owned())
    });
    let unchanged = !file.oversize
        && !options.force
        && file.mtime_ns.is_some()
        && existing.as_ref().is_some_and(|record| {
            record.state == FileState::Indexed.as_str()
                && record.size_bytes == file.size_bytes as i64
                && record.mtime_ns == file.mtime_ns
        });
    if unchanged {
        let profile_matches = match (&existing, &profile) {
            (Some(record), Some(profile)) => db.pdf_profile_matches(record.id, profile)?,
            _ => true,
        };
        if profile_matches {
            return Ok(FileOutcome::Unchanged);
        }
    }

    let mut record = NewFile {
        source_id,
        path_key: file.path_key.clone(),
        display_path: file.display_path.clone(),
        filename: file.filename.clone(),
        ext: file.ext.clone(),
        size_bytes: file.size_bytes as i64,
        mtime_ns: file.mtime_ns,
        state: FileState::Failed,
    };

    let extracted = if file.oversize {
        Ok(StableExtraction {
            size: file.size_bytes,
            mtime: file.mtime_ns,
            ..StableExtraction::default()
        })
    } else {
        let cached = match (&existing, is_pdf && ocr.is_some()) {
            (Some(record), true) => db.cached_ocr_pages(record.id)?,
            _ => BTreeMap::new(),
        };
        extract_stable(&file.path, options, ocr, &cached)
    };
    check_stop(should_stop)?;

    match extracted {
        Ok(extracted) => {
            record.size_bytes = extracted.size as i64;
            record.mtime_ns = extracted.mtime;
            let failure = extracted.result.as_ref().and_then(|result| {
                result.failed_pages.first().map(|page| {
                    format!(
                        "OCR 第 {} 页失败（共 {} 页）：{}",
                        page.page,
                        result.failed_pages.len(),
                        page.error
                    )
                })
            });
            let document = extracted.result.as_ref().map(|result| &result.document);
            let store = match &profile {
                Some(profile) => db.store_pdf_index(
                    &record,
                    document,
                    profile,
                    &extracted.pages,
                    failure.as_deref(),
                ),
                None => db.store_file(&record, document),
            };
            if let Err(error) = store {
                if let Some(existing) = &existing {
                    db.mark_file_failed(existing.id, &error.to_string())
                        .context(error.to_string())?;
                }
                return Err(error.into());
            }
            if let Some(message) = failure {
                return Ok(FileOutcome::Failed(format!(
                    "{}: {message}",
                    file.display_path
                )));
            }
            Ok(if extracted.result.is_some() {
                FileOutcome::Indexed
            } else {
                FileOutcome::SkippedSize
            })
        }
        Err(message) => {
            let file_id = match existing {
                Some(record) => record.id,
                None => db.upsert_file(&record)?,
            };
            db.mark_file_failed(file_id, &message)?;
            Ok(FileOutcome::Failed(format!(
                "{}: {message}",
                file.display_path
            )))
        }
    }
}

/// Extract, then confirm size/mtime did not change during the read. One retry
/// covers an editor writing the file mid-index; a second change is queued for
/// the next scan rather than indexing a torn version. The limit is rechecked
/// from fresh metadata before each read, so growth after the scan cannot bypass
/// the metadata-only rule. `None` means oversize (metadata-only).
fn extract_stable(
    path: &Path,
    options: &IndexOptions,
    ocr: Option<&OcrSettings>,
    cached: &BTreeMap<String, String>,
) -> std::result::Result<StableExtraction, String> {
    for _ in 0..2 {
        let before = metadata(path).map_err(|error| error.to_string())?;
        if before.0 > options.max_file_size_bytes {
            return Ok(StableExtraction {
                size: before.0,
                mtime: before.1,
                ..StableExtraction::default()
            });
        }
        let cache = CachedOcrPages {
            available: cached,
            used: RefCell::new(BTreeMap::new()),
        };
        let result = extract_path_with_ocr_settings(path, options.chunk, ocr, Some(&cache));
        let after = metadata(path).map_err(|error| error.to_string())?;
        if before.0 == after.0 && before.1 == after.1 {
            return result
                .map(|result| StableExtraction {
                    result: Some(result),
                    pages: cache.used.into_inner(),
                    size: after.0,
                    mtime: after.1,
                })
                .map_err(|error| error.to_string());
        }
    }
    Err("file changed while it was being indexed".to_string())
}

fn metadata(path: &Path) -> std::io::Result<(u64, Option<i64>)> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file is no longer a regular file",
        ));
    }
    let mtime = metadata.modified().ok().and_then(|time| {
        time.duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
    });
    Ok((metadata.len(), mtime))
}
