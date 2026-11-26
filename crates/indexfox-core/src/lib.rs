//! Shared models, naming constants and configuration for IndexFox.

pub mod config;
pub mod model;
pub mod text;

pub const APP_NAME_EN: &str = "IndexFox";
pub const APP_NAME_ZH: &str = "索引狐";
pub const WINDOW_TITLE: &str = "IndexFox · 索引狐";
pub const DEFAULT_DATA_DIR_NAME: &str = "IndexFox";
pub const DEFAULT_DB_FILE_NAME: &str = "index.db";
pub const DEFAULT_MAX_FILE_SIZE_BYTES: u64 = 300 * 1024 * 1024;
pub const PORTABLE_ZIP_NAME: &str = "IndexFox-windows-x64-portable.zip";
pub const TAURI_IDENTIFIER: &str = "com.indexfox.desktop";

pub use config::{AppConfig, OcrConfig, ResourceMode, SourceScanConfig};
pub use model::{
    ExtractedDocument, FileState, Origin, SearchMode, Section, SortKey, SourceKind, SourceLocator,
};
pub use text::{normalize_search_text, snippet};
