use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{DEFAULT_DATA_DIR_NAME, DEFAULT_DB_FILE_NAME, DEFAULT_MAX_FILE_SIZE_BYTES};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceMode {
    Saver,
    #[default]
    Balanced,
    Fast,
}

impl ResourceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Saver => "saver",
            Self::Balanced => "balanced",
            Self::Fast => "fast",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SourceScanConfig {
    pub recursive: bool,
    pub extensions: Vec<String>,
    pub excluded_dirs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub max_file_size_bytes: u64,
    pub poll_interval_secs: u64,
}

impl SourceScanConfig {
    pub fn from_json(value: &str) -> Self {
        serde_json::from_str(value).unwrap_or_default()
    }
}

impl Default for SourceScanConfig {
    fn default() -> Self {
        Self {
            recursive: true,
            extensions: default_text_extensions(),
            excluded_dirs: default_excluded_dirs(),
            exclude_globs: Vec::new(),
            max_file_size_bytes: DEFAULT_MAX_FILE_SIZE_BYTES,
            poll_interval_secs: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OcrConfig {
    pub enabled: bool,
    pub languages: String,
    pub max_concurrency: u8,
    pub dpi: u16,
    pub page_timeout_secs: u64,
    pub tesseract_path: Option<PathBuf>,
    /// Explicit tessdata directory; when absent the extractor looks next to the
    /// executable, then honours `TESSDATA_PREFIX`.
    pub tessdata_path: Option<PathBuf>,
    /// Hard per-page raster allocation cap (width × height pixels).
    pub max_render_pixels: u64,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            // Requirement 03 / design §6: per-page OCR fallback is on by default.
            enabled: true,
            languages: "chi_sim+eng".to_string(),
            max_concurrency: 2,
            dpi: 250,
            page_timeout_secs: 60,
            tesseract_path: None,
            tessdata_path: None,
            max_render_pixels: 40_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub max_file_size_bytes: u64,
    pub text_extensions: Vec<String>,
    pub excluded_dirs: Vec<String>,
    pub resource_mode: ResourceMode,
    pub ocr: OcrConfig,
    pub close_to_tray: bool,
    pub launch_on_startup: bool,
    pub history_limit: usize,
    pub history_enabled: bool,
}

impl AppConfig {
    pub fn with_data_dir(data_dir: PathBuf) -> Self {
        let db_path = data_dir.join("data").join(DEFAULT_DB_FILE_NAME);
        Self {
            data_dir,
            db_path,
            max_file_size_bytes: DEFAULT_MAX_FILE_SIZE_BYTES,
            text_extensions: default_text_extensions(),
            excluded_dirs: default_excluded_dirs(),
            resource_mode: ResourceMode::Balanced,
            ocr: OcrConfig::default(),
            close_to_tray: false,
            launch_on_startup: false,
            history_limit: 100,
            history_enabled: true,
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::with_data_dir(default_data_dir())
    }
}

/// `%LOCALAPPDATA%\IndexFox` on Windows; XDG data dir (lowercase app name) elsewhere.
pub fn default_data_dir() -> PathBuf {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local).join(DEFAULT_DATA_DIR_NAME);
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("indexfox");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("indexfox");
    }
    PathBuf::from(".").join(DEFAULT_DATA_DIR_NAME)
}

pub fn default_text_extensions() -> Vec<String> {
    [
        // documents
        "txt",
        "md",
        "markdown",
        "pdf",
        "docx", // source code
        "c",
        "h",
        "cc",
        "cpp",
        "cxx",
        "hpp",
        "hh",
        "rs",
        "py",
        "pyi",
        "js",
        "mjs",
        "cjs",
        "ts",
        "tsx",
        "jsx",
        "java",
        "kt",
        "kts",
        "go",
        "rb",
        "php",
        "cs",
        "swift",
        "m",
        "mm",
        "scala",
        "lua",
        "pl",
        "r",
        "dart",
        "vue",
        "svelte",
        "sql",
        "sh",
        "bash",
        "zsh",
        "fish",
        "ps1",
        "bat",
        "cmd", // config / data / logs
        "json",
        "jsonc",
        "jsonl",
        "toml",
        "yaml",
        "yml",
        "ini",
        "conf",
        "cfg",
        "properties",
        "xml",
        "html",
        "htm",
        "css",
        "scss",
        "less",
        "gradle",
        "cmake",
        "mk",
        "makefile",
        "dockerfile",
        "gitignore",
        "editorconfig",
        "log",
        "csv",
        "tsv",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn default_excluded_dirs() -> Vec<String> {
    [
        ".git",
        ".hg",
        ".svn",
        "node_modules",
        "target",
        ".venv",
        "venv",
        "__pycache__",
        ".idea",
        ".vscode",
        ".next",
        "dist",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
