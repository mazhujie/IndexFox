//! Format adapters: TXT/MD/source, DOCX and (feature-gated) PDF via PDFium.
//!
//! Every adapter produces [`indexfox_core::ExtractedDocument`] with display
//! text plus a locator (line range, PDF page, DOCX paragraph/cell). Nothing
//! here writes to the source file.

mod docx;
pub mod ocr;
mod text;

#[cfg(feature = "pdfium")]
pub mod pdf;

pub use ocr::{
    classify_page_text, file_fingerprint, ocr_image, ocr_image_with_engine, ocr_page_cache_key,
    OcrEngineKind, OcrError, OcrExtraction, OcrOptions, OcrPageFailure, OcrSettings, PageOcrCache,
    PageOcrReport, TextQuality, DEFAULT_MAX_OCR_CONCURRENCY, DEFAULT_MAX_RENDER_PIXELS,
    MAX_OCR_CONCURRENCY,
};

pub const EXTRACTOR_VERSION: &str = "m0-1";

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported extension: {0}")]
    UnsupportedExtension(String),
    #[error("file exceeds the configured size limit")]
    TooLarge,
    #[error("docx zip error: {0}")]
    DocxZip(#[from] zip::result::ZipError),
    #[error("docx xml error: {0}")]
    DocxXml(#[from] quick_xml::Error),
    #[error("docx is missing word/document.xml")]
    MissingDocumentXml,
    #[error("pdf support is not compiled in this build")]
    PdfUnsupported,
    #[error("pdfium: {0}")]
    Pdfium(String),
    #[error("ocr: {0}")]
    Ocr(String),
    #[error("image: {0}")]
    Image(String),
}

pub type Result<T, E = ExtractError> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy)]
pub struct ChunkOptions {
    pub target_bytes: usize,
    pub max_bytes: usize,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            target_bytes: 8 * 1024,
            max_bytes: 16 * 1024,
        }
    }
}

pub fn extract_path(
    path: &std::path::Path,
    options: ChunkOptions,
) -> Result<indexfox_core::ExtractedDocument> {
    extract_path_with_ocr(path, options, None)
}

pub fn extract_path_with_ocr(
    path: &std::path::Path,
    options: ChunkOptions,
    ocr: Option<&OcrOptions>,
) -> Result<indexfox_core::ExtractedDocument> {
    let settings = ocr.map(OcrSettings::from_options);
    extract_path_with_ocr_settings(path, options, settings.as_ref(), None)
        .map(|extraction| extraction.document)
}

/// New M4 entry point: same format dispatch as [`extract_path_with_ocr`] but
/// exposes page-level OCR reports, retryable failures and the optional
/// persistent/page cache. Old entry points remain source-compatible.
pub fn extract_path_with_ocr_settings(
    path: &std::path::Path,
    options: ChunkOptions,
    ocr: Option<&OcrSettings>,
    cache: Option<&dyn PageOcrCache>,
) -> Result<OcrExtraction> {
    let _ = cache;
    let declared = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let extension = if declared.is_empty() {
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .trim_start_matches('.')
            .to_ascii_lowercase()
    } else {
        declared
    };

    match extension.as_str() {
        "pdf" => {
            #[cfg(feature = "pdfium")]
            {
                let engine = pdf::PdfEngine::bind(None)?;
                match ocr {
                    Some(ocr) => engine.extract_document_ocr(path, options, ocr, cache),
                    None => engine.extract_document_report(path, options),
                }
            }
            #[cfg(not(feature = "pdfium"))]
            {
                let _ = (options, ocr);
                Err(ExtractError::PdfUnsupported)
            }
        }
        "docx" => {
            let bytes = std::fs::read(path)?;
            docx::extract_docx(&bytes, options).map(OcrExtraction::from_document)
        }
        "txt" | "md" | "markdown" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "rs"
        | "py" | "pyi" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "java" | "kt" | "kts"
        | "go" | "rb" | "php" | "cs" | "swift" | "m" | "mm" | "scala" | "lua" | "pl" | "r"
        | "dart" | "vue" | "svelte" | "sql" | "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat"
        | "cmd" | "json" | "jsonc" | "jsonl" | "toml" | "yaml" | "yml" | "ini" | "conf" | "cfg"
        | "properties" | "xml" | "html" | "htm" | "css" | "scss" | "less" | "gradle" | "cmake"
        | "mk" | "makefile" | "dockerfile" | "gitignore" | "editorconfig" | "log" | "csv"
        | "tsv" => {
            let bytes = std::fs::read(path)?;
            Ok(OcrExtraction::from_document(text::extract_text_bytes(
                bytes, options,
            )))
        }
        _ => Err(ExtractError::UnsupportedExtension(extension)),
    }
}
