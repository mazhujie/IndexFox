use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use indexfox_cli::pipeline::register_source;
use indexfox_core::{ResourceMode, SearchMode, SortKey, SourceLocator, SourceScanConfig};
use indexfox_index::IndexDb;
use indexfox_query::{search_cancellable, SearchOptions, SearchResponse};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::scheduler::{Database, RuntimeSnapshot};
use crate::state::AppState;
use crate::storage;

/// Rendered page PNGs larger than this are rejected before crossing IPC.
const MAX_RENDER_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub request_id: u64,
    pub query: String,
    pub mode: String,
    pub case_sensitive: bool,
    pub sort: SortKey,
    pub limit: usize,
    pub offset: usize,
    pub source_id: Option<i64>,
    pub extensions: Vec<String>,
    pub record_history: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryFilters<'a> {
    case_sensitive: bool,
    sort: SortKey,
    source_id: Option<i64>,
    extensions: &'a [String],
}

#[derive(Debug, Serialize)]
pub struct SourceSummary {
    id: i64,
    root_path: String,
    online_state: String,
    enabled: bool,
    scan_generation: i64,
    last_success_scan_at: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSettings {
    enabled: bool,
    scan_mode: String,
    #[serde(flatten)]
    scan: SourceScanConfig,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrSettings {
    enabled: bool,
    languages: String,
    max_concurrency: u8,
    dpi: u16,
    page_timeout_secs: u64,
}

#[derive(Debug, Serialize)]
pub struct PreviewText {
    text: String,
    locator: Option<SourceLocator>,
    origin: String,
}

#[derive(Debug, Serialize)]
pub struct HistoryItem {
    id: i64,
    query_text: String,
    search_mode: String,
    filters_json: String,
    last_used_at: String,
}

#[derive(Debug, Serialize)]
pub struct HistorySettings {
    enabled: bool,
    items: Vec<HistoryItem>,
}

/// Run a blocking database operation on Tauri's blocking pool.
async fn with_db<T, F>(database: Database, operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&mut IndexDb) -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || {
        let location = database.read().map_err(|error| error.to_string())?;
        let mut db = IndexDb::open(location.path()).map_err(|error| {
            format!(
                "cannot open index database {}: {error}",
                location.path().display()
            )
        })?;
        operation(&mut db)
    })
    .await
    .map_err(|error| format!("background database task failed: {error}"))?
}

fn history_enabled(db: &IndexDb, default_enabled: bool) -> Result<bool, String> {
    let sql = if default_enabled {
        "SELECT COALESCE((SELECT value FROM app_meta WHERE key = 'history_enabled'), 'true')"
    } else {
        "SELECT COALESCE((SELECT value FROM app_meta WHERE key = 'history_enabled'), 'false')"
    };
    let value: String = db
        .conn()
        .query_row(sql, [], |row| row.get(0))
        .map_err(|error| format!("cannot read history setting: {error}"))?;
    Ok(matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    ))
}

#[tauri::command]
pub async fn search_documents(
    request: SearchRequest,
    state: State<'_, AppState>,
) -> Result<SearchResponse, String> {
    if !(1..=100).contains(&request.limit) {
        return Err(format!(
            "search limit {} is out of range; expected 1..=100",
            request.limit
        ));
    }
    if request.query.len() > 4096 {
        return Err(format!(
            "search query is {} UTF-8 bytes, above the 4096 byte limit",
            request.query.len()
        ));
    }
    let mode = SearchMode::parse(&request.mode)
        .ok_or_else(|| format!("unknown search mode {:?}", request.mode))?;
    let request_id = request.request_id;
    let guard = state.begin_search(request_id)?;
    let cancel = std::sync::Arc::clone(guard.token());
    let database = state.database.clone();
    let default_history_enabled = state.config.history_enabled;

    let SearchRequest {
        query,
        case_sensitive,
        sort,
        limit,
        offset,
        source_id,
        extensions,
        record_history,
        ..
    } = request;

    let response = with_db(database, move |db| {
        let options = SearchOptions {
            mode,
            case_sensitive,
            sort,
            limit,
            offset,
            source_id,
            extensions: extensions.clone(),
            ..SearchOptions::default()
        };
        let response = search_cancellable(db, &query, &options, cancel.as_ref())
            .map_err(|error| format!("search {request_id} failed: {error}"))?;

        // History records only completed, non-cancelled first pages.
        let should_record = record_history
            && offset == 0
            && !cancel.load(Ordering::Relaxed)
            && history_enabled(db, default_history_enabled)?;
        if should_record {
            let filters = HistoryFilters {
                case_sensitive,
                sort,
                source_id,
                extensions: &extensions,
            };
            let filters_json = serde_json::to_string(&filters)
                .map_err(|error| format!("cannot encode history filters: {error}"))?;
            if let Err(error) = db.add_search_history(&query, mode, &filters_json) {
                // Losing a history row must not turn a successful search into
                // a visible failure.
                eprintln!("indexfox: cannot record search history: {error}");
            }
        }
        Ok(response)
    })
    .await?;

    drop(guard);
    Ok(response)
}

#[tauri::command]
pub fn cancel_search(request_id: u64, state: State<'_, AppState>) -> Result<(), String> {
    state.cancel(request_id);
    Ok(())
}

#[tauri::command]
pub async fn list_sources(state: State<'_, AppState>) -> Result<Vec<SourceSummary>, String> {
    let database = state.database.clone();
    with_db(database, |db| {
        let sources = db
            .list_sources()
            .map_err(|error| format!("cannot list sources: {error}"))?;
        Ok(sources
            .into_iter()
            .map(|source| SourceSummary {
                id: source.id,
                root_path: source.root_path,
                online_state: source.online_state,
                enabled: source.enabled,
                scan_generation: source.scan_generation,
                last_success_scan_at: source.last_success_scan_at,
            })
            .collect())
    })
    .await
}

#[tauri::command]
pub async fn index_folder(path: String, state: State<'_, AppState>) -> Result<i64, String> {
    let path = PathBuf::from(path.trim());
    if !path.is_absolute() {
        return Err("请输入目录绝对路径。".into());
    }
    let job_id = with_db(state.database.clone(), move |db| {
        let source_id = register_source(db, &path).map_err(|error| format!("{error:#}"))?;
        db.queue_source_scan(source_id, false)
            .map_err(|error| error.to_string())
    })
    .await?;
    state.scheduler.wake();
    Ok(job_id)
}

#[tauri::command]
pub fn get_index_runtime(state: State<'_, AppState>) -> RuntimeSnapshot {
    state.scheduler.snapshot()
}

#[tauri::command]
pub fn set_index_paused(paused: bool, state: State<'_, AppState>) {
    state.scheduler.set_paused(paused);
}

#[tauri::command]
pub async fn set_resource_mode(
    mode: ResourceMode,
    state: State<'_, AppState>,
) -> Result<(), String> {
    with_db(state.database.clone(), move |db| {
        db.conn()
            .execute(
                "INSERT INTO app_meta(key, value) VALUES ('resource_mode', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [mode.as_str()],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    })
    .await?;
    state.scheduler.set_mode(mode);
    Ok(())
}

#[tauri::command]
pub async fn get_ocr_settings(state: State<'_, AppState>) -> Result<OcrSettings, String> {
    with_db(state.database.clone(), |db| {
        let config = db.ocr_config().map_err(|error| error.to_string())?;
        Ok(OcrSettings {
            enabled: config.enabled,
            languages: config.languages,
            max_concurrency: config.max_concurrency,
            dpi: config.dpi,
            page_timeout_secs: config.page_timeout_secs,
        })
    })
    .await
}

#[tauri::command]
pub async fn set_ocr_settings(
    settings: OcrSettings,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if !matches!(
        settings.languages.as_str(),
        "chi_sim+eng" | "chi_sim" | "eng"
    ) || !(1..=4).contains(&settings.max_concurrency)
        || !(100..=400).contains(&settings.dpi)
        || !(1..=180).contains(&settings.page_timeout_secs)
    {
        return Err("OCR 语言、并发、分辨率或超时超出支持范围。".into());
    }
    with_db(state.database.clone(), move |db| {
        let mut config = db.ocr_config().map_err(|error| error.to_string())?;
        config.enabled = settings.enabled;
        config.languages = settings.languages;
        config.max_concurrency = settings.max_concurrency;
        config.dpi = settings.dpi;
        config.page_timeout_secs = settings.page_timeout_secs;
        db.set_ocr_config(&config)
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
pub async fn cancel_index_job(job_id: i64, app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || app.state::<AppState>().scheduler.cancel(job_id))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn get_source_config(
    source_id: i64,
    state: State<'_, AppState>,
) -> Result<SourceSettings, String> {
    with_db(state.database.clone(), move |db| {
        let source = db
            .source_by_id(source_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("找不到索引源 {source_id}"))?;
        Ok(SourceSettings {
            enabled: source.enabled,
            scan_mode: source.scan_mode,
            scan: serde_json::from_str(&source.config_json).map_err(|error| error.to_string())?,
        })
    })
    .await
}

#[tauri::command]
pub async fn set_source_config(
    source_id: i64,
    mut config: SourceSettings,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if !matches!(config.scan_mode.as_str(), "auto" | "poll" | "manual") {
        return Err("未知的扫描策略。".into());
    }
    if !(5..=86_400).contains(&config.scan.poll_interval_secs)
        || config.scan.max_file_size_bytes > i64::MAX as u64
    {
        return Err("轮询周期须为 5–86400 秒，文件大小上限超出范围。".into());
    }
    for extension in &mut config.scan.extensions {
        *extension = extension
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase();
    }
    config
        .scan
        .extensions
        .retain(|extension| !extension.is_empty());
    let settings = serde_json::to_string(&config.scan).map_err(|error| error.to_string())?;
    with_db(state.database.clone(), move |db| {
        db.update_source_config(source_id, config.enabled, &config.scan_mode, &settings)
            .map_err(|error| error.to_string())
    })
    .await?;
    state.scheduler.wake();
    Ok(())
}

#[tauri::command]
pub async fn scan_source(
    source_id: i64,
    force: bool,
    state: State<'_, AppState>,
) -> Result<i64, String> {
    let job_id = with_db(state.database.clone(), move |db| {
        let source = db
            .source_by_id(source_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("找不到索引源 {source_id}"))?;
        if !source.enabled {
            return Err("索引源已停用。".into());
        }
        db.queue_source_scan(source_id, force)
            .map_err(|error| error.to_string())
    })
    .await?;
    state.scheduler.wake();
    Ok(job_id)
}

#[tauri::command]
pub async fn rebuild_indexes(state: State<'_, AppState>) -> Result<usize, String> {
    let queued = with_db(state.database.clone(), |db| {
        let sources = db.list_sources().map_err(|error| error.to_string())?;
        let mut queued = 0;
        for source in sources.into_iter().filter(|source| source.enabled) {
            db.queue_source_scan(source.id, true)
                .map_err(|error| error.to_string())?;
            queued += 1;
        }
        Ok(queued)
    })
    .await?;
    state.scheduler.wake();
    Ok(queued)
}

#[tauri::command]
pub fn get_database_path(state: State<'_, AppState>) -> Result<String, String> {
    let location = state.database.try_read().map_err(|_| "数据库正在维护。")?;
    Ok(location.path().display().to_string())
}

#[tauri::command]
pub async fn migrate_database(path: String, state: State<'_, AppState>) -> Result<(), String> {
    let database = state.database.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut location = database
            .try_write()
            .map_err(|_| "数据库正忙，请等待当前索引或搜索结束。")?;
        location.migrate(Path::new(path.trim()))
    })
    .await
    .map_err(|error| error.to_string())??;
    state.scheduler.wake();
    Ok(())
}

#[tauri::command]
pub async fn clear_index_cache(state: State<'_, AppState>) -> Result<(), String> {
    let database = state.database.clone();
    let was_paused = state.scheduler.snapshot().paused;
    state.scheduler.set_paused(true);
    let result = tauri::async_runtime::spawn_blocking(move || {
        let location = database
            .try_write()
            .map_err(|_| "数据库正忙，请取消当前索引或等待搜索结束。")?;
        let mut db = IndexDb::open(location.path()).map_err(|error| error.to_string())?;
        storage::clear_cached_index(&mut db)
    })
    .await
    .map_err(|error| error.to_string())
    .and_then(|result| result);
    if result.is_err() {
        state.scheduler.set_paused(was_paused);
    }
    state.scheduler.wake();
    result
}

#[tauri::command]
pub async fn preview_text(
    file_id: i64,
    chunk_id: Option<i64>,
    state: State<'_, AppState>,
) -> Result<PreviewText, String> {
    let database = state.database.clone();
    with_db(database, move |db| {
        if db
            .file_by_id(file_id)
            .map_err(|error| format!("cannot read file {file_id}: {error}"))?
            .is_none()
        {
            return Err(format!("file id {file_id} not found"));
        }

        let chunk = match chunk_id {
            Some(chunk_id) => {
                let (_, chunk, _) = db
                    .chunk_context(chunk_id)
                    .map_err(|error| format!("cannot read chunk {chunk_id}: {error}"))?
                    .ok_or_else(|| format!("chunk id {chunk_id} not found"))?;
                if chunk.file_id != file_id {
                    return Err(format!(
                        "chunk id {chunk_id} belongs to file {}, not {file_id}",
                        chunk.file_id
                    ));
                }
                Some((
                    chunk.id,
                    chunk.display_text,
                    chunk.locator_json,
                    chunk.origin,
                ))
            }
            None => {
                // Read only the first cached chunk; loading every chunk of a
                // large file just to keep the head wastes memory and IPC time.
                let mut stmt = db
                    .conn()
                    .prepare(
                        "SELECT id, display_text, source_locator_json, origin
                         FROM chunks WHERE file_id = ?1 ORDER BY ordinal LIMIT 1",
                    )
                    .map_err(|error| {
                        format!("cannot prepare cached chunk lookup for file {file_id}: {error}")
                    })?;
                let mut rows = stmt.query([file_id]).map_err(|error| {
                    format!("cannot read cached chunks for file {file_id}: {error}")
                })?;
                match rows.next().map_err(|error| {
                    format!("cannot read cached chunks for file {file_id}: {error}")
                })? {
                    Some(row) => Some((
                        row.get(0).map_err(|error| {
                            format!("cannot read cached chunk for file {file_id}: {error}")
                        })?,
                        row.get(1).map_err(|error| {
                            format!("cannot read cached chunk for file {file_id}: {error}")
                        })?,
                        row.get(2).map_err(|error| {
                            format!("cannot read cached chunk for file {file_id}: {error}")
                        })?,
                        row.get(3).map_err(|error| {
                            format!("cannot read cached chunk for file {file_id}: {error}")
                        })?,
                    )),
                    None => None,
                }
            }
        };

        let Some((chunk_id, display_text, locator_json, origin)) = chunk else {
            return Ok(PreviewText {
                text: String::new(),
                locator: None,
                origin: "native".to_string(),
            });
        };
        let locator: SourceLocator = serde_json::from_str(&locator_json).map_err(|error| {
            format!("file {file_id} chunk {chunk_id} has an invalid locator: {error}")
        })?;
        Ok(PreviewText {
            text: display_text,
            locator: Some(locator),
            origin,
        })
    })
    .await
}

fn pdf_preview_state_error(file_id: i64, state: &str) -> String {
    format!("file id {file_id} is {state}; PDF preview requires an indexed file")
}

/// Match the pipeline's indexed timestamp semantics exactly.
fn file_mtime_ns(metadata: &std::fs::Metadata) -> Option<i64> {
    use std::time::UNIX_EPOCH;
    metadata.modified().ok().and_then(|time| {
        time.duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
    })
}

#[tauri::command]
pub async fn render_pdf_page(
    file_id: i64,
    page: u32,
    state: State<'_, AppState>,
) -> Result<tauri::ipc::Response, String> {
    if page == 0 {
        return Err("PDF page must be 1 or greater".to_string());
    }
    let database = state.database.clone();
    let max_file_size_bytes = state.config.max_file_size_bytes;
    let bytes = with_db(database, move |db| {
        let file = db
            .file_by_id(file_id)
            .map_err(|error| format!("cannot read file {file_id}: {error}"))?
            .ok_or_else(|| format!("file id {file_id} not found"))?;

        if !file.ext.eq_ignore_ascii_case("pdf") {
            return Err(format!(
                "file id {file_id} has extension {:?}, not pdf",
                file.ext
            ));
        }
        if file.state != "indexed" {
            return Err(pdf_preview_state_error(file_id, &file.state));
        }

        // The source path comes only from the cached file row; no caller path
        // is accepted. The file is opened read-only.
        let path = PathBuf::from(&file.display_path);
        if !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
        {
            return Err(format!(
                "cached path for file id {file_id} is not a PDF: {}",
                path.display()
            ));
        }
        let metadata = std::fs::metadata(&path).map_err(|error| {
            format!(
                "cannot read PDF for file id {file_id} at {}: {error}",
                path.display()
            )
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "cached path for file id {file_id} is not a regular file: {}",
                path.display()
            ));
        }
        let actual_size = metadata.len();
        let actual_mtime_ns = file_mtime_ns(&metadata);
        let indexed_size = u64::try_from(file.size_bytes).ok();
        if indexed_size != Some(actual_size) || file.mtime_ns != actual_mtime_ns {
            return Err(format!(
                "PDF for file id {file_id} changed on disk since it was indexed \
                 (indexed {} bytes, mtime {:?}; now {} bytes, mtime {:?}); \
                 refusing to render, use the cached summary or text",
                file.size_bytes, file.mtime_ns, actual_size, actual_mtime_ns
            ));
        }
        if metadata.len() > max_file_size_bytes {
            return Err(format!(
                "PDF for file id {file_id} is {} bytes, above the {} byte limit",
                metadata.len(),
                max_file_size_bytes
            ));
        }

        let engine = indexfox_extract::pdf::PdfEngine::bind(None)
            .map_err(|error| format!("PDFium is unavailable: {error}"))?;
        let page_count = engine
            .page_count(&path)
            .map_err(|error| format!("cannot read PDF page count for file id {file_id}: {error}"))?;
        if page > u32::from(page_count) {
            return Err(format!(
                "page {page} is out of range; file id {file_id} at {} has {page_count} pages",
                path.display()
            ));
        }

        let output = tempfile::Builder::new()
            .prefix("indexfox-pdf-preview-")
            .suffix(".png")
            .tempfile()
            .map_err(|error| format!("cannot create temporary PDF render file: {error}"))?;
        engine
            .render_page_png(&path, page as u16, 1600, output.path())
            .map_err(|error| {
                format!(
                    "cannot render page {page} of file id {file_id} at {}: {error}",
                    path.display()
                )
            })?;

        let rendered_len = output
            .as_file()
            .metadata()
            .map_err(|error| format!("cannot inspect rendered PDF page: {error}"))?
            .len();
        if rendered_len > MAX_RENDER_BYTES {
            return Err(format!(
                "rendered page {page} of file id {file_id} is {rendered_len} bytes, above the {MAX_RENDER_BYTES} byte IPC limit"
            ));
        }
        let png = std::fs::read(output.path())
            .map_err(|error| format!("cannot read rendered PDF page: {error}"))?;
        drop(output); // Removes the temporary PNG on every path.
        Ok(png)
    })
    .await?;

    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
pub async fn history_settings(state: State<'_, AppState>) -> Result<HistorySettings, String> {
    let database = state.database.clone();
    let default_enabled = state.config.history_enabled;
    with_db(database, move |db| {
        let enabled = history_enabled(db, default_enabled)?;
        let items = db
            .search_history()
            .map_err(|error| format!("cannot list search history: {error}"))?
            .into_iter()
            .map(|record| HistoryItem {
                id: record.id,
                query_text: record.query_text,
                search_mode: record.search_mode.as_str().to_string(),
                filters_json: record.filters_json,
                last_used_at: record.last_used_at,
            })
            .collect();
        Ok(HistorySettings { enabled, items })
    })
    .await
}

#[tauri::command]
pub async fn set_history_enabled(enabled: bool, state: State<'_, AppState>) -> Result<(), String> {
    let database = state.database.clone();
    with_db(database, move |db| {
        let sql = if enabled {
            "INSERT INTO app_meta(key, value) VALUES ('history_enabled', 'true')
             ON CONFLICT(key) DO UPDATE SET value = 'true'"
        } else {
            "INSERT INTO app_meta(key, value) VALUES ('history_enabled', 'false')
             ON CONFLICT(key) DO UPDATE SET value = 'false'"
        };
        db.conn()
            .execute(sql, [])
            .map_err(|error| format!("cannot save history setting: {error}"))?;
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn clear_history(state: State<'_, AppState>) -> Result<(), String> {
    let database = state.database.clone();
    with_db(database, |db| {
        db.clear_search_history()
            .map_err(|error| format!("cannot clear search history: {error}"))?;
        Ok(())
    })
    .await
}
