use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use indexfox_core::OcrConfig;

/// Default cap on simultaneous Tesseract subprocesses.
pub const DEFAULT_MAX_OCR_CONCURRENCY: usize = 2;
/// Upper bound so a bad config cannot spawn an unbounded process fan-out.
pub const MAX_OCR_CONCURRENCY: usize = 16;

/// Process-wide OCR cap. The app indexes with several worker threads, so a
/// per-document pool alone would not keep the machine at two Tesseract
/// processes. Every OCR subprocess acquires one slot.
struct OcrSlotGate {
    state: std::sync::Mutex<OcrSlotState>,
    released: std::sync::Condvar,
}

struct OcrSlotState {
    active: usize,
    limit: usize,
}

static OCR_SLOTS: std::sync::LazyLock<OcrSlotGate> = std::sync::LazyLock::new(|| OcrSlotGate {
    state: std::sync::Mutex::new(OcrSlotState {
        active: 0,
        limit: DEFAULT_MAX_OCR_CONCURRENCY,
    }),
    released: std::sync::Condvar::new(),
});

struct OcrSlotGuard {
    gate: &'static OcrSlotGate,
}

impl Drop for OcrSlotGuard {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state.active.saturating_sub(1);
        self.gate.released.notify_one();
    }
}

fn acquire_ocr_slot(limit: usize) -> OcrSlotGuard {
    let gate = &*OCR_SLOTS;
    let mut state = gate
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.limit = limit.clamp(1, MAX_OCR_CONCURRENCY);
    while state.active >= state.limit {
        state = gate
            .released
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    state.active += 1;
    OcrSlotGuard { gate }
}
/// Default per-page raster allocation cap (width × height pixels).
pub const DEFAULT_MAX_RENDER_PIXELS: u64 = 40_000_000;
/// Bump when OCR invocation, raster policy or cache key layout changes.
pub const OCR_CACHE_SCHEMA: &str = "indexfox-ocr-v2";

#[derive(Debug, Clone)]
pub struct OcrOptions {
    pub executable: PathBuf,
    pub languages: String,
    pub dpi: u16,
    pub timeout: Duration,
}

impl Default for OcrOptions {
    fn default() -> Self {
        Self {
            executable: std::env::var_os("TESSERACT_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("tesseract")),
            languages: "chi_sim+eng".to_string(),
            dpi: 250,
            timeout: Duration::from_secs(60),
        }
    }
}

/// OCR engine selected for a run. PaddleOCR is a reserved adapter and is
/// always explicitly unavailable in this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrEngineKind {
    Tesseract,
    PaddleOcr,
}

impl OcrEngineKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Tesseract => "tesseract",
            Self::PaddleOcr => "paddleocr",
        }
    }

    /// Included in cache keys so a future Paddle implementation cannot reuse
    /// Tesseract page text (and vice versa).
    pub fn adapter_version(self) -> &'static str {
        match self {
            Self::Tesseract => "tesseract-cli-v1",
            Self::PaddleOcr => "paddleocr-not-implemented",
        }
    }

    pub fn availability(self) -> Result<(), OcrError> {
        match self {
            Self::Tesseract => Ok(()),
            Self::PaddleOcr => Err(OcrError::EngineUnavailable(
                "PaddleOCR adapter is reserved and not available in this build".to_string(),
            )),
        }
    }
}

/// Extended OCR settings used by the cache/failure-aware entry points.
///
/// Unlike [`OcrOptions`] this can be built directly from
/// [`indexfox_core::OcrConfig`] and carries the concurrency and raster caps.
#[derive(Debug, Clone)]
pub struct OcrSettings {
    pub engine: OcrEngineKind,
    pub executable: Option<PathBuf>,
    pub languages: String,
    pub dpi: u16,
    pub timeout: Duration,
    pub tessdata_dir: Option<PathBuf>,
    pub max_concurrency: usize,
    pub max_render_pixels: u64,
}

impl Default for OcrSettings {
    fn default() -> Self {
        Self {
            engine: OcrEngineKind::Tesseract,
            executable: None,
            languages: "chi_sim+eng".to_string(),
            dpi: 250,
            timeout: Duration::from_secs(60),
            tessdata_dir: None,
            max_concurrency: DEFAULT_MAX_OCR_CONCURRENCY,
            max_render_pixels: DEFAULT_MAX_RENDER_PIXELS,
        }
    }
}

impl OcrSettings {
    pub fn from_options(options: &OcrOptions) -> Self {
        Self {
            engine: OcrEngineKind::Tesseract,
            executable: Some(options.executable.clone()),
            languages: options.languages.clone(),
            dpi: options.dpi,
            timeout: options.timeout,
            ..Self::default()
        }
    }

    pub fn from_config(config: &OcrConfig) -> Self {
        Self {
            engine: OcrEngineKind::Tesseract,
            executable: config.tesseract_path.clone(),
            languages: config.languages.clone(),
            dpi: config.dpi,
            timeout: Duration::from_secs(config.page_timeout_secs),
            tessdata_dir: config.tessdata_path.clone(),
            max_concurrency: usize::from(config.max_concurrency),
            max_render_pixels: config.max_render_pixels,
        }
    }

    /// `0` means "use the default", not "serialise everything".
    pub fn concurrency(&self) -> usize {
        if self.max_concurrency == 0 {
            DEFAULT_MAX_OCR_CONCURRENCY
        } else {
            self.max_concurrency.clamp(1, MAX_OCR_CONCURRENCY)
        }
    }

    pub fn dpi(&self) -> u16 {
        if self.dpi == 0 {
            250
        } else {
            self.dpi
        }
    }

    pub fn max_pixels(&self) -> u64 {
        if self.max_render_pixels == 0 {
            DEFAULT_MAX_RENDER_PIXELS
        } else {
            self.max_render_pixels
        }
    }

    pub fn resolved_executable(&self) -> PathBuf {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let env_override = std::env::var_os("TESSERACT_PATH").map(PathBuf::from);
        resolve_tesseract_in(
            exe_dir.as_deref(),
            self.executable.as_deref(),
            env_override.as_deref(),
        )
    }

    pub fn resolved_tessdata_dir(&self) -> Option<PathBuf> {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let env_override = std::env::var_os("TESSDATA_PREFIX").map(PathBuf::from);
        resolve_tessdata_in(
            exe_dir.as_deref(),
            self.tessdata_dir.as_deref(),
            env_override.as_deref(),
        )
    }
}

/// Persistent per-page OCR cache. Implementations must invalidate by key;
/// the extractor never reuses text under a different file/page/settings key.
pub trait PageOcrCache {
    fn get(&self, key: &str) -> Option<String>;
    /// Best effort: a failed cache write must not fail extraction.
    fn put(&self, key: &str, text: &str);
}

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    #[error("tesseract executable not found: {0}")]
    NotAvailable(String),
    #[error("OCR engine unavailable: {0}")]
    EngineUnavailable(String),
    #[error("failed to start tesseract: {0}")]
    Spawn(String),
    #[error("tesseract timed out after {0:?}")]
    Timeout(Duration),
    #[error("tesseract exited with status {0}")]
    Failed(String),
    #[error("failed to read OCR output: {0}")]
    Output(String),
}

/// Native PDF text assessment. Doc comment included in `pdf.rs` policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextQuality {
    Good,
    Empty,
    Sparse,
    Garbled,
}

impl TextQuality {
    pub fn needs_ocr(self) -> bool {
        !matches!(self, Self::Good)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Empty => "empty",
            Self::Sparse => "sparse",
            Self::Garbled => "garbled",
        }
    }
}

/// One page report, in document order. `origin` is `None` when the page has
/// no retained text (empty page or failed OCR with no native fallback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageOcrReport {
    pub page: u32,
    pub quality: TextQuality,
    pub origin: Option<indexfox_core::Origin>,
    pub ocr_attempted: bool,
    pub from_cache: bool,
    pub cache_key: Option<String>,
    pub chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrPageFailure {
    pub page: u32,
    pub cache_key: Option<String>,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OcrExtraction {
    pub document: indexfox_core::ExtractedDocument,
    pub pages: Vec<PageOcrReport>,
    pub failed_pages: Vec<OcrPageFailure>,
}

impl OcrExtraction {
    /// Wrap a non-PDF / no-OCR extraction that has no page-level OCR metadata.
    pub fn from_document(document: indexfox_core::ExtractedDocument) -> Self {
        Self {
            document,
            pages: Vec::new(),
            failed_pages: Vec::new(),
        }
    }
}

/// Run Tesseract on one image and return its text.
///
/// The child process is killed on timeout, so a stuck OCR page cannot block the
/// indexing pipeline. Arguments are passed directly (never through a shell).
pub fn ocr_image(image: &Path, options: &OcrOptions) -> Result<String, OcrError> {
    let settings = OcrSettings::from_options(options);
    ocr_image_with_engine(settings.engine, image, &settings)
}

/// Run the selected engine. PaddleOCR is reserved and returns an explicit
/// unavailable error without spawning anything.
pub fn ocr_image_with_engine(
    engine: OcrEngineKind,
    image: &Path,
    settings: &OcrSettings,
) -> Result<String, OcrError> {
    match engine {
        OcrEngineKind::Tesseract => {
            // Global slot, not just per-batch: several index workers may OCR
            // different PDFs at once.
            let _slot = acquire_ocr_slot(settings.concurrency());
            ocr_tesseract(image, settings)
        }
        OcrEngineKind::PaddleOcr => Err(OcrError::EngineUnavailable(
            "PaddleOCR adapter is reserved and not available in this build".to_string(),
        )),
    }
}

fn ocr_tesseract(image: &Path, settings: &OcrSettings) -> Result<String, OcrError> {
    let executable = settings.resolved_executable();
    let workdir = tempfile::Builder::new()
        .prefix("indexfox-ocr-")
        .tempdir()
        .map_err(|error| OcrError::Output(error.to_string()))?;
    let output_base = workdir.path().join("page");
    let output_txt = output_base.with_extension("txt");

    let mut command = Command::new(&executable);
    command
        .arg(image)
        .arg(&output_base)
        .arg("-l")
        .arg(&settings.languages)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Passing the model directory explicitly avoids TESSDATA_PREFIX/global
    // lookup semantics when the portable layout is used.
    if let Some(tessdata) = settings.resolved_tessdata_dir() {
        command.arg("--tessdata-dir").arg(tessdata);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            OcrError::NotAvailable(executable.display().to_string())
        } else {
            OcrError::Spawn(error.to_string())
        }
    })?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err(OcrError::Failed(format!("{status}")));
                }
                break;
            }
            Ok(None) => {
                if started.elapsed() >= settings.timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(OcrError::Timeout(settings.timeout));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(OcrError::Spawn(error.to_string()));
            }
        }
    }

    let text = std::fs::read_to_string(&output_txt)
        .map_err(|error| OcrError::Output(error.to_string()))?;
    Ok(text.trim_end_matches('\u{0c}').to_string())
}

/// OCR up to `max_concurrency` images at once, preserving job order.
/// Each group starts at most `settings.concurrency()` subprocesses.
#[cfg(any(test, feature = "pdfium"))]
pub(crate) fn ocr_batch(
    jobs: &[(u32, PathBuf)],
    settings: &OcrSettings,
) -> Vec<(u32, Result<String, OcrError>)> {
    let limit = settings.concurrency();
    let mut results = Vec::with_capacity(jobs.len());
    for chunk in jobs.chunks(limit) {
        let chunk_results = std::thread::scope(|scope| {
            let handles: Vec<(u32, _)> = chunk
                .iter()
                .map(|(page, image)| {
                    let page = *page;
                    let image = image.clone();
                    let settings = settings.clone();
                    let handle = scope
                        .spawn(move || ocr_image_with_engine(settings.engine, &image, &settings));
                    (page, handle)
                })
                .collect();
            handles
                .into_iter()
                .map(|(page, handle)| {
                    (
                        page,
                        handle.join().unwrap_or_else(|_| {
                            Err(OcrError::Spawn("OCR worker panicked".to_string()))
                        }),
                    )
                })
                .collect::<Vec<_>>()
        });
        results.extend(chunk_results);
    }
    results
}

/// Stable local cache key: file fingerprint + page + engine/version + language
/// + DPI + raster cap. Not a security boundary.
pub fn ocr_page_cache_key(fingerprint: &str, page: u32, settings: &OcrSettings) -> String {
    let executable = settings.resolved_executable();
    let deployed_executable = resolved_command_file(&executable);
    let engine_revision = file_revision(&deployed_executable);
    let models = settings.resolved_tessdata_dir().map(|dir| {
        settings
            .languages
            .split('+')
            .map(|language| file_revision(&dir.join(format!("{language}.traineddata"))))
            .collect::<Vec<_>>()
    });
    format!(
        "{}:{}:{}:{}:{}:{}:{}:{}:{engine_revision:?}:{models:?}",
        OCR_CACHE_SCHEMA,
        fingerprint,
        page,
        settings.engine.id(),
        settings.engine.adapter_version(),
        settings.languages,
        settings.dpi(),
        settings.max_pixels()
    )
}

fn resolved_command_file(executable: &Path) -> PathBuf {
    if executable.components().count() == 1 && !executable.is_file() {
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let candidate = dir.join(executable);
                if candidate.is_file() {
                    return candidate;
                }
                #[cfg(windows)]
                if candidate.extension().is_none() && candidate.with_extension("exe").is_file() {
                    return candidate.with_extension("exe");
                }
            }
        }
    }
    executable.to_path_buf()
}

fn file_revision(path: &Path) -> (PathBuf, Option<(u64, std::time::SystemTime)>) {
    let stat = std::fs::metadata(path).ok().and_then(|metadata| {
        metadata
            .modified()
            .ok()
            .map(|mtime| (metadata.len(), mtime))
    });
    (path.to_path_buf(), stat)
}

/// Non-cryptographic content fingerprint (FNV-1a 128 + byte length), used only
/// for local OCR cache invalidation.
pub fn file_fingerprint(path: &Path) -> std::io::Result<String> {
    const FNV_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const FNV_PRIME: u128 = 0x0000000001000000000000000000013b;

    let mut file = File::open(path)?;
    let mut hash = FNV_OFFSET;
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        for byte in &buffer[..read] {
            hash ^= u128::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    Ok(format!("{hash:032x}-{size:x}"))
}

/// Documented native-page text-quality policy (M4.1).
///
/// - `Empty`: zero non-whitespace characters.
/// - `Garbled`: >2% U+FFFD replacement characters, >2% non-whitespace control
///   characters, or at least 32 visible characters with under 40%
///   alphanumeric/CJK characters (symbol soup).
/// - `Sparse`: fewer than `max(8, page_area_pt2 / 50_000)` alphanumeric/CJK
///   characters. A normal A4 page therefore needs at least ~10 meaningful
///   characters to stay native.
/// - `Good`: everything else; the clean native text is preserved and never
///   replaced by OCR output.
pub fn classify_page_text(text: &str, width_points: f32, height_points: f32) -> TextQuality {
    let mut visible = 0usize;
    let mut meaningful = 0usize;
    let mut replacement = 0usize;
    let mut control = 0usize;
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        visible += 1;
        if ch.is_alphanumeric() {
            meaningful += 1;
        }
        if ch == '\u{fffd}' {
            replacement += 1;
        } else if ch.is_control() {
            control += 1;
        }
    }
    if visible == 0 {
        return TextQuality::Empty;
    }
    if replacement * 50 >= visible || control * 50 >= visible {
        return TextQuality::Garbled;
    }
    if visible >= 32 && meaningful * 5 < visible * 2 {
        return TextQuality::Garbled;
    }
    let area = if width_points.is_finite()
        && height_points.is_finite()
        && width_points > 0.0
        && height_points > 0.0
    {
        f64::from(width_points) * f64::from(height_points)
    } else {
        0.0
    };
    let required = 8usize.max((area / 50_000.0).round() as usize);
    if meaningful < required {
        TextQuality::Sparse
    } else {
        TextQuality::Good
    }
}

fn resolve_tesseract_in(
    exe_dir: Option<&Path>,
    explicit: Option<&Path>,
    env_override: Option<&Path>,
) -> PathBuf {
    if let Some(path) = explicit {
        // A bare "tesseract" is the default; treat it as "use portable lookup".
        let bare_default = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "tesseract" || name == "tesseract.exe");
        if path.is_absolute() || path.components().count() > 1 || !bare_default {
            return path.to_path_buf();
        }
    }
    if let Some(path) = env_override {
        return path.to_path_buf();
    }
    if let Some(dir) = exe_dir {
        for relative in [
            "ocr/tesseract.exe",
            "ocr/tesseract",
            "tesseract",
            "tesseract.exe",
            "bin/tesseract",
            "bin/tesseract.exe",
            "tesseract/tesseract",
            "tesseract/tesseract.exe",
        ] {
            let candidate = dir.join(relative);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("tesseract"))
}

fn resolve_tessdata_in(
    exe_dir: Option<&Path>,
    explicit: Option<&Path>,
    env_override: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Some(path) = env_override {
        return Some(path.to_path_buf());
    }
    let dir = exe_dir?;
    for relative in ["ocr/tessdata", "tessdata", "share/tessdata"] {
        let candidate = dir.join(relative);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// Serialises "write executable script, then fork/exec it" across tests.
    ///
    /// A forked child can inherit the write handle on a script another test is
    /// still creating; exec then fails with `ETXTBSY` ("Text file busy"), and
    /// Unix cannot revoke that inherited handle. Holding this lock for the
    /// create-and-spawn window keeps the two harnesses from overlapping.
    static SCRIPT_EXEC_LOCK: Mutex<()> = Mutex::new(());

    fn script(body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-tesseract");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        (dir, path)
    }

    #[test]
    fn reads_text_from_configured_executable() {
        let _guard = SCRIPT_EXEC_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_dir, executable) = script("printf 'OCR 内核驱动' > \"$2.txt\"");
        let options = OcrOptions {
            executable,
            languages: "chi_sim+eng".into(),
            dpi: 250,
            timeout: Duration::from_secs(2),
        };
        let text = ocr_image(Path::new("/tmp/ignored.png"), &options).unwrap();
        assert_eq!(text, "OCR 内核驱动");
    }

    #[test]
    fn kills_a_timed_out_ocr_child() {
        let _guard = SCRIPT_EXEC_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_dir, executable) = script("sleep 5");
        let options = OcrOptions {
            executable,
            languages: "eng".into(),
            dpi: 250,
            timeout: Duration::from_millis(150),
        };
        let started = Instant::now();
        let error = ocr_image(Path::new("/tmp/ignored.png"), &options).unwrap_err();
        assert!(matches!(error, OcrError::Timeout(_)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn paddle_adapter_is_explicitly_unavailable() {
        let settings = OcrSettings::default();
        assert!(settings.engine.availability().is_ok());
        let error = ocr_image_with_engine(
            OcrEngineKind::PaddleOcr,
            Path::new("/tmp/ignored.png"),
            &settings,
        )
        .unwrap_err();
        assert!(matches!(error, OcrError::EngineUnavailable(_)));
        assert!(OcrEngineKind::PaddleOcr.availability().is_err());
    }

    #[test]
    fn config_maps_concurrency_dpi_models_and_raster_cap() {
        let config = indexfox_core::OcrConfig {
            max_concurrency: 3,
            dpi: 300,
            page_timeout_secs: 7,
            tessdata_path: Some(PathBuf::from("/models/tessdata")),
            max_render_pixels: 123_456,
            ..indexfox_core::OcrConfig::default()
        };
        let settings = OcrSettings::from_config(&config);
        assert_eq!(settings.concurrency(), 3);
        assert_eq!(settings.dpi(), 300);
        assert_eq!(settings.timeout, Duration::from_secs(7));
        assert_eq!(
            settings.tessdata_dir,
            Some(PathBuf::from("/models/tessdata"))
        );
        assert_eq!(settings.max_pixels(), 123_456);
    }

    #[test]
    fn settings_default_to_two_ocr_in_flight_and_zero_means_default() {
        let mut settings = OcrSettings::default();
        assert_eq!(settings.concurrency(), 2);
        settings.max_concurrency = 0;
        assert_eq!(settings.concurrency(), 2);
        settings.max_concurrency = 3;
        assert_eq!(settings.concurrency(), 3);
        settings.max_concurrency = 1000;
        assert_eq!(settings.concurrency(), MAX_OCR_CONCURRENCY);
    }

    #[test]
    fn concurrency_is_bounded_and_job_order_is_preserved() {
        let _guard = SCRIPT_EXEC_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("events.log");
        let executable = dir.path().join("fake-tesseract");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf 'S\\n' >> '{}'\nsleep 0.15\nprintf 'E\\n' >> '{}'\nprintf 'page' > \"$2.txt\"\n",
                log.display(),
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let settings = OcrSettings {
            executable: Some(executable),
            max_concurrency: 2,
            ..OcrSettings::default()
        };
        let jobs: Vec<(u32, PathBuf)> = (1..=5)
            .map(|page| (page, dir.path().join(format!("{page}.png"))))
            .collect();
        let started = Instant::now();
        let results = ocr_batch(&jobs, &settings);
        assert_eq!(
            results.iter().map(|(page, _)| *page).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert!(results.iter().all(|(_, result)| result.is_ok()));
        assert!(
            started.elapsed() >= Duration::from_millis(400),
            "5 jobs at 2 in flight took {:?}",
            started.elapsed()
        );

        let events = std::fs::read_to_string(&log).unwrap();
        let mut live = 0usize;
        let mut peak = 0usize;
        for event in events.lines() {
            match event {
                "S" => {
                    live += 1;
                    peak = peak.max(live);
                }
                "E" => live = live.saturating_sub(1),
                _ => {}
            }
        }
        assert!(peak <= 2, "peak OCR concurrency was {peak}");
        assert!(peak >= 2, "OCR jobs did not overlap: peak {peak}");
    }

    #[test]
    fn portable_tesseract_and_models_prefer_executable_directory() {
        let dir = tempfile::tempdir().unwrap();
        let portable = dir.path().join("ocr/tesseract.exe");
        let env_path = dir.path().join("env-tesseract");
        let explicit = dir.path().join("explicit/tesseract");
        let tessdata = dir.path().join("ocr/tessdata");
        std::fs::create_dir_all(&tessdata).unwrap();
        std::fs::write(&portable, b"fake").unwrap();

        assert_eq!(resolve_tesseract_in(Some(dir.path()), None, None), portable);
        assert_eq!(
            resolve_tesseract_in(Some(dir.path()), None, Some(&env_path)),
            env_path
        );
        assert_eq!(
            resolve_tesseract_in(Some(dir.path()), Some(&explicit), Some(&env_path)),
            explicit
        );
        assert_eq!(
            resolve_tessdata_in(Some(dir.path()), None, None),
            Some(tessdata)
        );
    }

    #[test]
    fn page_quality_policy_classifies_empty_sparse_garbled_and_good() {
        let a4 = (595.0, 842.0);
        assert_eq!(classify_page_text("   \n", a4.0, a4.1), TextQuality::Empty);
        assert_eq!(classify_page_text("x", a4.0, a4.1), TextQuality::Sparse);
        assert_eq!(
            classify_page_text(
                "This is a normal page of native PDF text with enough words.",
                a4.0,
                a4.1
            ),
            TextQuality::Good
        );
        assert_eq!(
            classify_page_text("\u{fffd}\u{fffd}abc", a4.0, a4.1),
            TextQuality::Garbled
        );
        let symbols = "!@#$%^&*()_+-=[]{};:'\",.<>/?\\|`~".repeat(2);
        assert_eq!(
            classify_page_text(&symbols, a4.0, a4.1),
            TextQuality::Garbled
        );
    }

    #[test]
    fn cache_key_changes_with_page_engine_and_raster_settings() {
        let mut settings = OcrSettings::default();
        let base = ocr_page_cache_key("fp", 1, &settings);
        assert_eq!(base, ocr_page_cache_key("fp", 1, &settings));
        assert_ne!(base, ocr_page_cache_key("fp", 2, &settings));
        settings.dpi = 300;
        assert_ne!(base, ocr_page_cache_key("fp", 1, &settings));
        settings.dpi = 250;
        settings.engine = OcrEngineKind::PaddleOcr;
        assert_ne!(base, ocr_page_cache_key("fp", 1, &settings));

        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("tesseract.exe");
        std::fs::write(&binary, b"first").unwrap();
        settings.executable = Some(binary.clone());
        let first = ocr_page_cache_key("fp", 1, &settings);
        std::fs::write(&binary, b"replacement").unwrap();
        assert_ne!(first, ocr_page_cache_key("fp", 1, &settings));
    }
}
