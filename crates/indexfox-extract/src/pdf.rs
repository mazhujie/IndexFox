use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::Duration;

use indexfox_core::{normalize_search_text, ExtractedDocument, Origin, Section, SourceLocator};
use pdfium_render::prelude::*;

use crate::ocr::{
    classify_page_text, file_fingerprint, ocr_batch, ocr_page_cache_key, OcrError, OcrExtraction,
    OcrPageFailure, OcrSettings, PageOcrCache, PageOcrReport, TextQuality,
};
use crate::text::chunk_spans;
use crate::{ChunkOptions, ExtractError, Result, EXTRACTOR_VERSION};

/// Largest pixel width this crate will ask PDFium to render.
const MAX_RENDER_WIDTH: i32 = 10_000;
/// Largest pixel height this crate will ask PDFium to render. Without this an
/// extreme portrait page can request gigabytes before PDFium allocates.
const MAX_RENDER_HEIGHT: i32 = 16_384;
/// Default total RGBA pixels per rendered page, bounding the bitmap allocation.
const MAX_RENDER_PIXELS: u64 = crate::ocr::DEFAULT_MAX_RENDER_PIXELS;

/// PDFium's `FPDF_InitLibrary`/`FPDF_DestroyLibrary` operate on process-global
/// state, while pdfium-render destroys the library when each `Pdfium` value is
/// dropped. Two live `PdfEngine`s would therefore let one Drop tear the library
/// down underneath the other. Hold one engine lifecycle at a time; the guard is
/// released only after `Pdfium`'s Drop finishes. The rest of this module never
/// binds a nested engine, so the non-reentrant lock cannot self-deadlock.
static PDFIUM_LIFECYCLE: Mutex<()> = Mutex::new(());

/// Thin owner of the dynamically loaded PDFium library.
///
/// The library is loaded at runtime (`PDFIUM_LIB_PATH`, an explicit path, or
/// the system loader) so the Rust crate builds without PDFium present.
pub struct PdfEngine {
    pdfium: Pdfium,
    /// Field order matters: the library must be destroyed before the next
    /// `bind` can initialize PDFium.
    _lifecycle: MutexGuard<'static, ()>,
}

impl PdfEngine {
    pub fn bind(library: Option<&Path>) -> Result<Self> {
        // Recover from a poisoned lock: a panic while an engine was alive leaves
        // the mutex poisoned but the engine had already been dropped by unwind.
        let lifecycle = PDFIUM_LIFECYCLE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::bind_with_lifecycle(library, lifecycle)
    }

    pub(crate) fn bind_controlled(cancelled: &dyn Fn() -> bool) -> Result<Self> {
        let lifecycle = loop {
            crate::check_cancelled(cancelled)?;
            match PDFIUM_LIFECYCLE.try_lock() {
                Ok(lifecycle) => break lifecycle,
                Err(TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(25)),
            }
        };
        Self::bind_with_lifecycle(None, lifecycle)
    }

    fn bind_with_lifecycle(
        library: Option<&Path>,
        lifecycle: MutexGuard<'static, ()>,
    ) -> Result<Self> {
        let candidate = library
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("PDFIUM_LIB_PATH").map(PathBuf::from));
        let bindings = match candidate {
            Some(path) => {
                let path = if path.is_dir() {
                    Pdfium::pdfium_platform_library_name_at_path(&path)
                } else {
                    path
                };
                Pdfium::bind_to_library(&path)
            }
            None => Pdfium::bind_to_system_library(),
        }
        .map_err(|error| ExtractError::Pdfium(error.to_string()))?;
        Ok(Self {
            pdfium: Pdfium::new(bindings),
            _lifecycle: lifecycle,
        })
    }

    pub fn extract_document(
        &self,
        path: &Path,
        options: ChunkOptions,
    ) -> Result<ExtractedDocument> {
        self.extract_document_controlled(path, options, None, None, &|| false)
            .map(|extraction| extraction.document)
    }

    /// Extract text pages; pages whose native text is empty, sparse or
    /// garbled are rendered and OCR'd when `ocr` is provided. Mixed PDFs only
    /// OCR the pages that need it and never merge native + OCR text.
    pub fn extract_document_with_ocr(
        &self,
        path: &Path,
        options: ChunkOptions,
        ocr: &crate::ocr::OcrOptions,
    ) -> Result<ExtractedDocument> {
        let settings = OcrSettings::from_options(ocr);
        self.extract_document_controlled(path, options, Some(&settings), None, &|| false)
            .map(|extraction| extraction.document)
    }

    /// Page-report extraction without OCR. Keeps one report per PDF page so
    /// callers can inspect native quality even when OCR is disabled.
    pub fn extract_document_report(
        &self,
        path: &Path,
        options: ChunkOptions,
    ) -> Result<OcrExtraction> {
        self.extract_document_controlled(path, options, None, None, &|| false)
    }

    /// Cache/failure-aware OCR entry point. Returns one report per page plus
    /// retryable OCR failures; every retained section keeps its page locator.
    pub fn extract_document_ocr(
        &self,
        path: &Path,
        options: ChunkOptions,
        ocr: &OcrSettings,
        cache: Option<&dyn PageOcrCache>,
    ) -> Result<OcrExtraction> {
        self.extract_document_controlled(path, options, Some(ocr), cache, &|| false)
    }

    pub(crate) fn extract_document_controlled(
        &self,
        path: &Path,
        options: ChunkOptions,
        ocr: Option<&OcrSettings>,
        cache: Option<&dyn PageOcrCache>,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<OcrExtraction> {
        crate::check_cancelled(cancelled)?;
        if let Some(settings) = ocr {
            settings
                .engine
                .availability()
                .map_err(|error| ExtractError::Ocr(error.to_string()))?;
        }
        let document = self
            .pdfium
            .load_pdf_from_file(path, None)
            .map_err(pdf_error)?;
        let pages = document.pages();
        let concurrency = ocr.map(OcrSettings::concurrency).unwrap_or(1).max(1);
        let mut states: Vec<PageState> = Vec::with_capacity(usize::from(pages.len()));
        let mut failing_pages = Vec::new();
        let mut fingerprint: Option<String> = None;
        let mut jobs: Vec<PendingJob> = Vec::new();

        for (index, page) in pages.iter().enumerate() {
            crate::check_cancelled(cancelled)?;
            let page_number = index as u16 + 1;
            let native = page
                .text()
                .map(|page_text| page_text.all())
                .unwrap_or_default();
            let quality = classify_page_text(&native, page.width().value, page.height().value);
            let state_index = states.len();
            states.push(PageState {
                page: u32::from(page_number),
                native: native.clone(),
                quality,
                text: None,
                origin: None,
                ocr_attempted: false,
                from_cache: false,
                cache_key: None,
            });

            let Some(settings) = ocr else {
                retain_native(&mut states[state_index]);
                continue;
            };
            if !quality.needs_ocr() {
                retain_native(&mut states[state_index]);
                continue;
            }

            let fingerprint = match &fingerprint {
                Some(value) => value.clone(),
                None => {
                    let value = file_fingerprint(path)?;
                    fingerprint = Some(value.clone());
                    value
                }
            };
            let cache_key = ocr_page_cache_key(&fingerprint, u32::from(page_number), settings);
            states[state_index].ocr_attempted = true;
            states[state_index].cache_key = Some(cache_key.clone());

            if let Some(cache) = cache {
                if let Some(cached) = cache.get(&cache_key) {
                    let state = &mut states[state_index];
                    retain_ocr_text(state, cached);
                    state.from_cache = true;
                    continue;
                }
            }

            let target_width = render_target_width(page.width().value, settings.dpi());
            let render_width = render_width_within_limits(
                page.width().value,
                page.height().value,
                target_width,
                settings.max_pixels(),
            );
            let Some(render_width) = render_width else {
                record_failure(
                    &mut states[state_index],
                    &mut failing_pages,
                    format!(
                        "page is too tall to render safely ({} x {} points)",
                        page.width().value,
                        page.height().value
                    ),
                );
                continue;
            };

            let png = tempfile::Builder::new()
                .prefix("indexfox-page-")
                .suffix(".png")
                .tempfile()
                .map_err(ExtractError::Io)?;
            match self.render_page_to_png(
                &page,
                page_number,
                render_width,
                settings.max_pixels(),
                png.path(),
                None,
            ) {
                Ok(_) => jobs.push(PendingJob {
                    state_index,
                    page: u32::from(page_number),
                    cache_key,
                    image: png.into_temp_path(),
                }),
                Err(error) => record_failure(
                    &mut states[state_index],
                    &mut failing_pages,
                    error.to_string(),
                ),
            }
            if jobs.len() >= concurrency {
                flush_jobs(
                    &mut jobs,
                    settings,
                    cache,
                    &mut states,
                    &mut failing_pages,
                    cancelled,
                )?;
            }
        }
        if let Some(settings) = ocr {
            flush_jobs(
                &mut jobs,
                settings,
                cache,
                &mut states,
                &mut failing_pages,
                cancelled,
            )?;
        }

        let mut sections = Vec::new();
        let mut used_ocr = false;
        for state in &states {
            crate::check_cancelled(cancelled)?;
            let Some(text) = &state.text else {
                continue;
            };
            let origin = state.origin.unwrap_or(Origin::Native);
            if origin == Origin::Ocr {
                used_ocr = true;
            }
            for span in chunk_spans(text, options) {
                let display = text[span.start..span.end].to_string();
                sections.push(Section {
                    text: normalize_search_text(&display),
                    display_text: display,
                    locator: SourceLocator::PdfPage {
                        page: state.page,
                        bbox: None,
                    },
                    origin,
                });
            }
        }

        let reports = states
            .into_iter()
            .map(|state| PageOcrReport {
                page: state.page,
                quality: state.quality,
                origin: state.origin,
                ocr_attempted: state.ocr_attempted,
                from_cache: state.from_cache,
                cache_key: state.cache_key,
                chars: state
                    .text
                    .as_ref()
                    .map(|text| text.chars().count())
                    .unwrap_or(0),
            })
            .collect();

        Ok(OcrExtraction {
            document: ExtractedDocument {
                extractor_version: EXTRACTOR_VERSION.to_string(),
                encoding: Some(if used_ocr {
                    "pdfium+ocr".to_string()
                } else {
                    "pdfium".to_string()
                }),
                sections,
            },
            pages: reports,
            failed_pages: failing_pages,
        })
    }

    pub fn page_count(&self, path: &Path) -> Result<u16> {
        let document = self
            .pdfium
            .load_pdf_from_file(path, None)
            .map_err(pdf_error)?;
        Ok(document.pages().len())
    }

    pub fn page_sizes(&self, path: &Path) -> Result<Vec<Option<[f32; 2]>>> {
        let document = self
            .pdfium
            .load_pdf_from_file(path, None)
            .map_err(pdf_error)?;
        let pages = document.pages();
        Ok(pages
            .as_range()
            .map(|index| {
                let rect = pages.page_size(index).ok()?;
                let width = rect.width().value;
                let height = rect.height().value;
                (width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0)
                    .then_some([width, height])
            })
            .collect())
    }

    /// Render one 1-based page to a PNG. Returns the rendered pixel size.
    pub fn render_page_png(
        &self,
        path: &Path,
        page_number: u16,
        target_width: i32,
        output: &Path,
    ) -> Result<(u32, u32)> {
        self.render_page_png_with_highlight(path, page_number, target_width, output, None)
    }

    /// Render a page with the literal native-text matches highlighted in the PNG.
    pub fn render_page_png_with_highlight(
        &self,
        path: &Path,
        page_number: u16,
        target_width: i32,
        output: &Path,
        highlight: Option<&str>,
    ) -> Result<(u32, u32)> {
        let document = self
            .pdfium
            .load_pdf_from_file(path, None)
            .map_err(pdf_error)?;
        let pages = document.pages();
        let index = page_number.saturating_sub(1);
        if index >= pages.len() {
            return Err(ExtractError::Pdfium(format!(
                "page {page_number} is out of range (document has {} pages)",
                pages.len()
            )));
        }
        let page = pages.get(index).map_err(pdf_error)?;
        self.render_page_to_png(
            &page,
            page_number,
            target_width,
            MAX_RENDER_PIXELS,
            output,
            highlight,
        )
    }

    fn render_page_to_png(
        &self,
        page: &PdfPage<'_>,
        page_number: u16,
        target_width: i32,
        max_pixels: u64,
        output: &Path,
        highlight: Option<&str>,
    ) -> Result<(u32, u32)> {
        // Bound the pixel size before PDFium allocates the bitmap. A portrait
        // page with an extreme height/width ratio is scaled down as a whole so
        // its aspect ratio is preserved; ordinary pages keep their requested
        // width exactly.
        let requested_width = target_width.clamp(50, MAX_RENDER_WIDTH);
        let render_width = render_width_within_limits(
            page.width().value,
            page.height().value,
            requested_width,
            max_pixels,
        )
        .ok_or_else(|| {
            ExtractError::Pdfium(format!(
                "page {page_number} is too tall to render safely ({} x {} points)",
                page.width().value,
                page.height().value
            ))
        })?;
        let config = PdfRenderConfig::new()
            .set_target_width(render_width)
            .set_maximum_width(MAX_RENDER_WIDTH)
            .set_maximum_height(MAX_RENDER_HEIGHT);
        let bitmap = page.render_with_config(&config).map_err(pdf_error)?;
        let size = (bitmap.width() as u32, bitmap.height() as u32);
        let mut image = bitmap.as_image();
        if let (Some(highlight), Some(pixels)) = (highlight, image.as_mut_rgba8()) {
            draw_text_highlights(page, &config, pixels, highlight);
        }
        image
            .save(output)
            .map_err(|error| ExtractError::Image(error.to_string()))?;
        Ok(size)
    }
}

fn draw_text_highlights(
    page: &PdfPage<'_>,
    config: &PdfRenderConfig,
    image: &mut image::RgbaImage,
    highlight: &str,
) {
    let Ok(text) = page.text() else { return };
    let Ok(matches) = text.search(highlight, &PdfSearchOptions::new().match_case(true)) else {
        return;
    };
    for segments in matches.iter(PdfSearchDirection::SearchForward) {
        for rect in merge_highlight_boxes(segments.iter().map(|segment| segment.bounds())) {
            let padding = rect.height() * 0.08;
            let (Ok((x1, y1)), Ok((x2, y2))) = (
                page.points_to_pixels(rect.left() - padding, rect.top() + padding, config),
                page.points_to_pixels(rect.right() + padding, rect.bottom() - padding, config),
            ) else {
                continue;
            };
            let x = x1.min(x2).max(0) as u32..(x1.max(x2).max(0) as u32).min(image.width());
            let y = y1.min(y2).max(0) as u32..(y1.max(y2).max(0) as u32).min(image.height());
            for row in y {
                for col in x.clone() {
                    let pixel = image.get_pixel_mut(col, row);
                    for (channel, tint) in pixel.0.iter_mut().zip([255_u16, 210, 30]) {
                        *channel = ((u16::from(*channel) * 2 + tint) / 3) as u8;
                    }
                }
            }
        }
    }
}

// Merge in page coordinates so the same line remains a single block when
// PDFium maps it into a rotated preview. Each call handles one search hit.
fn merge_highlight_boxes(boxes: impl Iterator<Item = PdfRect>) -> Vec<PdfRect> {
    let mut lines: Vec<PdfRect> = Vec::new();
    for bounds in boxes.filter(|bounds| bounds.width().value > 0.0 && bounds.height().value > 0.0) {
        let Some(previous) = lines.last_mut() else {
            lines.push(bounds);
            continue;
        };
        let overlap = previous.top().min(bounds.top()) - previous.bottom().max(bounds.bottom());
        let gap = previous.left().max(bounds.left()) - previous.right().min(bounds.right());
        if overlap >= previous.height().min(bounds.height()) * 0.5
            && gap <= previous.height().max(bounds.height())
        {
            *previous = PdfRect::new(
                previous.bottom().min(bounds.bottom()),
                previous.left().min(bounds.left()),
                previous.top().max(bounds.top()),
                previous.right().max(bounds.right()),
            );
        } else {
            lines.push(bounds);
        }
    }
    lines
}

struct PageState {
    page: u32,
    native: String,
    quality: TextQuality,
    text: Option<String>,
    origin: Option<Origin>,
    ocr_attempted: bool,
    from_cache: bool,
    cache_key: Option<String>,
}

struct PendingJob {
    state_index: usize,
    page: u32,
    cache_key: String,
    image: tempfile::TempPath,
}

fn retain_native(state: &mut PageState) {
    if !state.native.trim().is_empty() {
        state.text = Some(state.native.clone());
        state.origin = Some(Origin::Native);
    }
}

fn record_failure(state: &mut PageState, failures: &mut Vec<OcrPageFailure>, error: String) {
    failures.push(OcrPageFailure {
        page: state.page,
        cache_key: state.cache_key.clone(),
        error,
    });
    retain_native(state);
}

fn flush_jobs(
    jobs: &mut Vec<PendingJob>,
    settings: &OcrSettings,
    cache: Option<&dyn PageOcrCache>,
    states: &mut [PageState],
    failures: &mut Vec<OcrPageFailure>,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    if jobs.is_empty() {
        return Ok(());
    }
    let batch: Vec<(u32, PathBuf)> = jobs
        .iter()
        .map(|job| (job.page, job.image.to_path_buf()))
        .collect();
    let results = ocr_batch(&batch, settings, cancelled);
    crate::check_cancelled(cancelled)?;
    for (job, (page, result)) in jobs.drain(..).zip(results) {
        debug_assert_eq!(job.page, page);
        let state = &mut states[job.state_index];
        match result {
            Err(OcrError::Cancelled) => return Err(ExtractError::Cancelled),
            Ok(text) => {
                if let Some(cache) = cache {
                    cache.put(&job.cache_key, &text);
                }
                retain_ocr_text(state, text);
            }
            Err(error) => {
                failures.push(OcrPageFailure {
                    page: job.page,
                    cache_key: Some(job.cache_key),
                    error: error.to_string(),
                });
                retain_native(state);
            }
        }
    }
    Ok(())
}

fn retain_ocr_text(state: &mut PageState, text: String) {
    if text.trim().is_empty() {
        retain_native(state);
    } else {
        state.text = Some(text);
        state.origin = Some(Origin::Ocr);
    }
}

fn render_target_width(width_points: f32, dpi: u16) -> i32 {
    if !width_points.is_finite() || width_points <= 0.0 {
        return 200;
    }
    ((f64::from(width_points) * f64::from(dpi) / 72.0).round() as i64)
        .clamp(50, i64::from(MAX_RENDER_WIDTH)) as i32
}

fn render_width_within_limits(
    width_points: f32,
    height_points: f32,
    requested: i32,
    max_pixels: u64,
) -> Option<i32> {
    if !width_points.is_finite() || !height_points.is_finite() {
        return Some(requested);
    }
    if width_points <= 0.0 || height_points <= 0.0 {
        return Some(requested);
    }
    let aspect = f64::from(height_points) / f64::from(width_points);
    if !aspect.is_finite() || aspect <= 0.0 {
        return Some(requested);
    }
    let width = f64::from(requested)
        .min(f64::from(MAX_RENDER_WIDTH))
        .min(f64::from(MAX_RENDER_HEIGHT) / aspect)
        .min((max_pixels.max(1) as f64 / aspect).sqrt())
        .floor();
    if width < 1.0 {
        None
    } else {
        Some(width as i32)
    }
}

fn pdf_error(error: PdfiumError) -> ExtractError {
    ExtractError::Pdfium(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_adjacent_glyph_boxes_without_joining_lines_or_columns() {
        let first = PdfRect::new_from_values(20.0, 10.0, 30.0, 15.0);
        let second = PdfRect::new_from_values(21.0, 17.0, 32.0, 22.0);
        let third = PdfRect::new_from_values(20.0, 24.0, 30.0, 28.0);
        let other_column = PdfRect::new_from_values(20.0, 60.0, 30.0, 69.0);
        let next_line = PdfRect::new_from_values(7.0, 10.0, 17.0, 28.0);

        assert_eq!(
            merge_highlight_boxes([first, second, third, other_column, next_line].into_iter()),
            vec![
                PdfRect::new_from_values(20.0, 10.0, 32.0, 28.0),
                other_column,
                next_line,
            ]
        );
    }
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Mutex;

    fn integration_pdf() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/pdf/sample.pdf")
    }

    fn pdfium_available() -> bool {
        std::env::var_os("PDFIUM_LIB_PATH").is_some()
    }

    fn single_page_pdf(media_box: &str) -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [{media_box}] /Resources << >> /Contents 4 0 R >>"
            ),
            "<< /Length 0 >>\nstream\n\nendstream".to_string(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn multi_page_pdf(contents: &[&str]) -> Vec<u8> {
        let page_count = contents.len();
        let font_id = 3 + 2 * page_count;
        let mut objects = Vec::with_capacity(2 + page_count * 2 + 1);
        objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
        let kids = (0..page_count)
            .map(|index| format!("{} 0 R", 3 + 2 * index))
            .collect::<Vec<_>>()
            .join(" ");
        objects.push(format!(
            "<< /Type /Pages /Kids [{kids}] /Count {page_count} >>"
        ));
        for (index, content) in contents.iter().enumerate() {
            let page_id = 3 + 2 * index;
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font_id} 0 R >> >> /Contents {} 0 R >>",
                page_id + 1
            ));
            let body = if content.is_empty() {
                String::new()
            } else {
                format!(
                    "BT /F1 12 Tf 72 720 Td ({}) Tj ET",
                    content
                        .replace('\\', "\\\\")
                        .replace('(', "\\(")
                        .replace(')', "\\)")
                )
            };
            objects.push(format!(
                "<< /Length {} >>\nstream\n{body}\nendstream",
                body.len()
            ));
        }
        objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());

        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[derive(Default)]
    struct MemoryCache {
        entries: Mutex<HashMap<String, String>>,
    }

    impl PageOcrCache for MemoryCache {
        fn get(&self, key: &str) -> Option<String> {
            self.entries
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(key)
                .cloned()
        }

        fn put(&self, key: &str, text: &str) {
            self.entries
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(key.to_string(), text.to_string());
        }
    }

    #[test]
    fn ordinary_page_keeps_requested_width() {
        assert_eq!(
            render_width_within_limits(595.0, 842.0, 1600, MAX_RENDER_PIXELS),
            Some(1600)
        );
        assert_eq!(
            render_width_within_limits(595.0, 842.0, 4000, MAX_RENDER_PIXELS),
            Some(4000)
        );
    }

    #[test]
    fn dpi_scales_render_target_width() {
        // 612 pt at 250 DPI -> 2125 px, still under the hard width cap.
        assert_eq!(render_target_width(612.0, 250), 2125);
        assert_eq!(render_target_width(612.0, 300), 2550);
        assert_eq!(render_target_width(f32::NAN, 250), 200);
    }

    #[test]
    fn extreme_page_width_is_bounded_without_changing_aspect_ratio() {
        let width = render_width_within_limits(100.0, 100_000.0, 1600, MAX_RENDER_PIXELS).unwrap();
        assert!(width < 1600);
        let height = (f64::from(width) * 1000.0).round() as u32;
        assert!(height <= MAX_RENDER_HEIGHT as u32);
        assert!(u64::from(width as u32) * u64::from(height) <= MAX_RENDER_PIXELS);
        assert_eq!(
            render_width_within_limits(100.0, 10_000_000.0, 1600, MAX_RENDER_PIXELS),
            None
        );
        // A smaller configured pixel cap is honoured.
        let small = render_width_within_limits(100.0, 100.0, 1600, 250_000).unwrap();
        assert_eq!(small, 500);
    }

    #[test]
    fn render_page_bounds_extreme_portrait_page() {
        if !pdfium_available() {
            eprintln!("skipping PDF test: PDFIUM_LIB_PATH is not set");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("tall.pdf");
        fs::write(&pdf, single_page_pdf("0 0 100 50000")).unwrap();
        let png = dir.path().join("tall.png");
        let engine = PdfEngine::bind(None).unwrap();
        let (width, height) = engine.render_page_png(&pdf, 1, 1600, &png).unwrap();
        assert!(width < 1600, "{width}x{height}");
        assert!(height <= MAX_RENDER_HEIGHT as u32, "{width}x{height}");
        assert!(
            u64::from(width) * u64::from(height) <= MAX_RENDER_PIXELS,
            "{width}x{height}"
        );
        assert!((height as f64 / width as f64 - 500.0).abs() < 1.0);
        assert!(fs::metadata(&png).unwrap().len() > 0);
    }

    #[test]
    fn concurrent_engines_serialize_pdfium_lifecycle() {
        if !pdfium_available() {
            eprintln!("skipping PDF test: PDFIUM_LIB_PATH is not set");
            return;
        }
        let pdf = integration_pdf();
        let mut threads = Vec::new();
        for _ in 0..4 {
            let pdf = pdf.clone();
            threads.push(std::thread::spawn(move || {
                for _ in 0..3 {
                    let engine = PdfEngine::bind(None).unwrap();
                    assert!(engine.page_count(&pdf).unwrap() > 0);
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_successful_blank_ocr_page_is_cached_without_a_retry() {
        use std::os::unix::fs::PermissionsExt;

        if !pdfium_available() {
            eprintln!("skipping PDF test: PDFIUM_LIB_PATH is not set");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("blank.pdf");
        fs::write(&pdf, multi_page_pdf(&[""])).unwrap();
        let executable = dir.path().join("ocr");
        fs::write(&executable, "#!/bin/sh\n: > \"$2.txt\"\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let settings = OcrSettings {
            executable: Some(executable),
            ..OcrSettings::default()
        };
        let cache = MemoryCache::default();
        let engine = PdfEngine::bind(None).unwrap();
        for cached in [false, true] {
            let result = engine
                .extract_document_ocr(&pdf, ChunkOptions::default(), &settings, Some(&cache))
                .unwrap();
            assert!(result.failed_pages.is_empty());
            assert!(result.document.sections.is_empty());
            assert_eq!(result.pages[0].from_cache, cached);
        }
    }

    #[cfg(unix)]
    #[test]
    fn mixed_quality_pdf_keeps_native_pages_and_uses_one_origin_per_page() {
        use std::os::unix::fs::PermissionsExt;

        if !pdfium_available() {
            eprintln!("skipping PDF test: PDFIUM_LIB_PATH is not set");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("mixed.pdf");
        fs::write(
            &pdf,
            multi_page_pdf(&[
                "Native page one has clean extractable text with enough words.",
                "",
                "x",
            ]),
        )
        .unwrap();
        let marker = dir.path().join("ocr-runs.log");
        let executable = dir.path().join("fake-tesseract");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf 'run\\n' >> '{}'\nprintf 'OCR 内核驱动' > \"$2.txt\"\n",
                marker.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();

        let settings = OcrSettings {
            executable: Some(executable),
            languages: "chi_sim+eng".into(),
            timeout: std::time::Duration::from_secs(2),
            ..OcrSettings::default()
        };
        let cache = MemoryCache::default();
        let engine = PdfEngine::bind(None).unwrap();
        let extraction = engine
            .extract_document_ocr(&pdf, ChunkOptions::default(), &settings, Some(&cache))
            .unwrap();

        assert!(
            extraction.failed_pages.is_empty(),
            "{:?}",
            extraction.failed_pages
        );
        assert_eq!(extraction.pages.len(), 3);
        assert_eq!(extraction.pages[0].quality, TextQuality::Good);
        assert_eq!(extraction.pages[0].origin, Some(Origin::Native));
        assert!(!extraction.pages[0].ocr_attempted);
        assert_eq!(extraction.pages[1].quality, TextQuality::Empty);
        assert_eq!(extraction.pages[1].origin, Some(Origin::Ocr));
        assert!(!extraction.pages[1].from_cache);
        assert_eq!(extraction.pages[2].quality, TextQuality::Sparse);
        assert_eq!(extraction.pages[2].origin, Some(Origin::Ocr));

        let mut pages_seen = HashMap::<u32, Vec<Origin>>::new();
        for section in &extraction.document.sections {
            let page = section.locator.page().expect("retained page locator");
            assert!((1..=3).contains(&page));
            pages_seen.entry(page).or_default().push(section.origin);
        }
        for page in 1..=3 {
            let origins = pages_seen.get(&page).unwrap();
            assert_eq!(origins.len(), 1, "page {page}: {origins:?}");
        }
        assert_eq!(pages_seen[&1], vec![Origin::Native]);
        assert!(!extraction
            .document
            .sections
            .iter()
            .any(|section| section.locator.page() == Some(1)
                && section.display_text.contains("内核驱动")));
        assert_eq!(pages_seen[&2], vec![Origin::Ocr]);
        assert_eq!(pages_seen[&3], vec![Origin::Ocr]);

        let first_runs = fs::read_to_string(&marker).unwrap().lines().count();
        assert_eq!(first_runs, 2, "only empty/sparse pages should be OCR'd");

        // The same cache keys must serve a second extraction without spawning.
        let second = engine
            .extract_document_ocr(&pdf, ChunkOptions::default(), &settings, Some(&cache))
            .unwrap();
        assert!(second.failed_pages.is_empty());
        assert!(second.pages[1].from_cache);
        assert!(second.pages[2].from_cache);
        assert_eq!(
            fs::read_to_string(&marker).unwrap().lines().count(),
            first_runs
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_ocr_page_is_reported_and_native_page_survives() {
        use std::os::unix::fs::PermissionsExt;

        if !pdfium_available() {
            eprintln!("skipping PDF test: PDFIUM_LIB_PATH is not set");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("failure.pdf");
        fs::write(
            &pdf,
            multi_page_pdf(&[
                "Native page remains searchable after another page fails.",
                "",
            ]),
        )
        .unwrap();
        let executable = dir.path().join("fake-tesseract");
        fs::write(&executable, "#!/bin/sh\nexit 7\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();

        let settings = OcrSettings {
            executable: Some(executable),
            timeout: std::time::Duration::from_secs(2),
            ..OcrSettings::default()
        };
        let engine = PdfEngine::bind(None).unwrap();
        let extraction = engine
            .extract_document_ocr(&pdf, ChunkOptions::default(), &settings, None)
            .unwrap();

        assert_eq!(extraction.failed_pages.len(), 1);
        assert_eq!(extraction.failed_pages[0].page, 2);
        assert!(extraction.failed_pages[0].cache_key.is_some());
        assert_eq!(extraction.pages[1].origin, None);
        assert!(extraction.pages[1].ocr_attempted);
        let native: Vec<_> = extraction
            .document
            .sections
            .iter()
            .filter(|section| section.locator.page() == Some(1))
            .collect();
        assert!(!native.is_empty());
        assert!(native
            .iter()
            .all(|section| section.origin == Origin::Native));
        assert!(!extraction
            .document
            .sections
            .iter()
            .any(|section| section.locator.page() == Some(2)));
    }
}
