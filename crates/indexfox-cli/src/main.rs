use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use indexfox_cli::pipeline::{index_directory, index_file, IndexOptions, IndexReport};
use indexfox_core::{AppConfig, ExtractedDocument, FileState, SearchMode, SortKey, SourceKind};
use indexfox_extract::{extract_path, ocr_image, ChunkOptions, OcrOptions, OcrSettings};
use indexfox_index::{IndexDb, NewFile};
use indexfox_query::{search, SearchOptions, SearchResponse};

#[derive(Debug, Parser)]
#[command(
    name = "indexfox",
    version,
    about = "IndexFox · 索引狐 local search CLI"
)]
struct Cli {
    /// SQLite index path (defaults to %LOCALAPPDATA%\IndexFox\data\index.db).
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Override the data directory; the database lives in <data-dir>/data/index.db.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Index a directory, or one file below it with --file RELATIVE_PATH
    /// (format whitelist, ignore rules, 300 MB default single-file limit).
    Index {
        path: PathBuf,
        /// Index only this path, relative to PATH, without directory reconcile.
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        max_size_mb: Option<u64>,
        #[arg(long)]
        force: bool,
        /// Enable automatic PDF OCR, overriding the saved setting.
        #[arg(long, conflicts_with = "no_ocr")]
        ocr: bool,
        /// Disable automatic PDF OCR for this index run.
        #[arg(long)]
        no_ocr: bool,
        #[arg(long)]
        tesseract: Option<PathBuf>,
        #[arg(long)]
        lang: Option<String>,
    },
    /// Run a committed query (no live search).
    Search {
        query: String,
        #[arg(long, default_value = "substring")]
        mode: String,
        #[arg(long)]
        case_sensitive: bool,
        #[arg(long, default_value = "relevance")]
        sort: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show index counters.
    Stats,
    /// Extract one file and print its sections (debug / M0 validation).
    Extract { file: PathBuf },
    /// Run Tesseract on one image.
    Ocr {
        image: PathBuf,
        #[arg(long)]
        tesseract: Option<PathBuf>,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Render one PDF page to a PNG (requires PDFium).
    PdfRender {
        file: PathBuf,
        #[arg(long, default_value_t = 1)]
        page: u16,
        #[arg(long, default_value = "page.png")]
        out: PathBuf,
        #[arg(long, default_value_t = 1600)]
        width: i32,
    },
    /// Demonstrate FTS5 trigram vs the 1/2-character gram index.
    #[command(name = "fts-demo")]
    FtsDemo,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db_path = cli
        .db
        .clone()
        .or_else(|| {
            cli.data_dir
                .as_ref()
                .map(|dir| dir.join("data").join("index.db"))
        })
        .unwrap_or_else(|| AppConfig::default().db_path);

    match cli.command {
        Command::Index {
            path,
            file,
            max_size_mb,
            force,
            ocr,
            no_ocr,
            tesseract,
            lang,
        } => {
            let mut db = IndexDb::open(&db_path)
                .with_context(|| format!("opening {}", db_path.display()))?;
            let defaults = AppConfig::default();
            let mut ocr_config = db.ocr_config()?;
            if let Some(path) = tesseract {
                ocr_config.tesseract_path = Some(path);
            }
            if let Some(lang) = lang {
                ocr_config.languages = lang;
            }
            let options = IndexOptions {
                max_file_size_bytes: max_size_mb
                    .map(|mb| mb.saturating_mul(1024 * 1024))
                    .unwrap_or(defaults.max_file_size_bytes),
                force,
                ocr_settings: (!no_ocr && (ocr || ocr_config.enabled))
                    .then(|| OcrSettings::from_config(&ocr_config)),
                ..IndexOptions::default()
            };
            let report = match file {
                Some(file) => index_file(&mut db, &path, &file, &options)?,
                None => index_directory(&mut db, &path, &options)?,
            };
            print_index_report(&report);
        }
        Command::Search {
            query,
            mode,
            case_sensitive,
            sort,
            limit,
            offset,
            json,
        } => {
            let mode =
                SearchMode::parse(&mode).with_context(|| format!("unknown search mode: {mode}"))?;
            let sort = parse_sort(&sort)?;
            let db = IndexDb::open(&db_path)
                .with_context(|| format!("opening {}", db_path.display()))?;
            let options = SearchOptions {
                mode,
                case_sensitive,
                sort,
                limit,
                offset,
                ..SearchOptions::default()
            };
            let response = search(&db, &query, &options)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                print_response(&response);
            }
        }
        Command::Stats => {
            let db = IndexDb::open(&db_path)?;
            let stats = db.stats()?;
            println!("db               {}", db_path.display());
            println!("sources          {}", stats.sources);
            println!("files            {}", stats.files);
            println!("indexed          {}", stats.indexed_files);
            println!("skipped (size)   {}", stats.skipped_files);
            println!("failed           {}", stats.failed_files);
            println!("stale            {}", stats.stale_files);
            println!("chunks           {}", stats.chunks);
            println!("short grams      {}", stats.short_grams);
            println!(
                "logical db size  {} B ({} pages x {} B)",
                stats.db_size_bytes, stats.db_page_count, stats.db_page_size
            );
        }
        Command::Extract { file } => {
            let document = extract_path(&file, ChunkOptions::default())
                .with_context(|| format!("extracting {}", file.display()))?;
            println!("extractor  {}", document.extractor_version);
            println!(
                "encoding   {}",
                document.encoding.as_deref().unwrap_or("unknown")
            );
            println!("sections   {}", document.sections.len());
            for (index, section) in document.sections.iter().take(20).enumerate() {
                let preview: String = section
                    .display_text
                    .chars()
                    .take(90)
                    .collect::<String>()
                    .replace(['\r', '\n'], " ");
                println!(
                    "{:>3} {:<24} {}",
                    index + 1,
                    locator_label(Some(&section.locator)),
                    preview
                );
            }
        }
        Command::Ocr {
            image,
            tesseract,
            lang,
            timeout,
        } => {
            let mut options = OcrOptions::default();
            if let Some(path) = tesseract {
                options.executable = path;
            }
            if let Some(lang) = lang {
                options.languages = lang;
            }
            options.timeout = Duration::from_secs(timeout.max(1));
            let text = ocr_image(&image, &options)?;
            println!("{text}");
        }
        Command::PdfRender {
            file,
            page,
            out,
            width,
        } => {
            render_pdf(&file, page, &out, width)?;
            println!("wrote {}", out.display());
        }
        Command::FtsDemo => run_fts_demo(&db_path)?,
    }

    Ok(())
}

fn print_index_report(report: &IndexReport) {
    println!("scanned          {}", report.scanned);
    println!("indexed          {}", report.indexed);
    println!("unchanged        {}", report.unchanged);
    println!("skipped (size)   {}", report.skipped_size);
    println!("failed           {}", report.failed);
    println!("removed          {}", report.removed);
    println!("incomplete scan  {}", report.incomplete_scan);
    println!("elapsed          {} ms", report.elapsed_ms);
    for issue in report.issues.iter().take(20) {
        println!("issue: {issue}");
    }
}

fn parse_sort(value: &str) -> Result<SortKey> {
    Ok(match value.to_ascii_lowercase().as_str() {
        "relevance" => SortKey::Relevance,
        "mtime" | "modified" => SortKey::ModifiedTime,
        "size" => SortKey::Size,
        "path" => SortKey::Path,
        "filename" => SortKey::Filename,
        "hits" | "hit_count" => SortKey::HitCount,
        _ => bail!("unknown sort key: {value}"),
    })
}

fn print_response(response: &SearchResponse) {
    println!("plan       {}", response.query_plan);
    println!(
        "found      {} files, {} hits, {} ms",
        response.total_files, response.total_hits, response.elapsed_ms
    );
    println!("returned   {}", response.returned_files);
    for (index, file) in response.results.iter().enumerate() {
        println!(
            "{:>3}. {} [{}] score={}",
            index + 1,
            file.path,
            file.state,
            file.score
        );
        for hit in &file.hits {
            println!(
                "     {:<18} {}",
                locator_label(hit.locator.as_ref()),
                hit.snippet
            );
        }
    }
}

fn locator_label(locator: Option<&indexfox_core::SourceLocator>) -> String {
    use indexfox_core::SourceLocator::*;
    match locator {
        Some(PdfPage { page, .. }) => format!("PDF p.{page}"),
        Some(Text { start_line, .. }) => format!("line {start_line}"),
        Some(DocxParagraph { paragraph }) => format!("paragraph {paragraph}"),
        Some(DocxTableCell {
            table, row, column, ..
        }) => format!("table {table} r{row} c{column}"),
        None => "filename".to_string(),
    }
}

#[cfg(feature = "pdfium")]
fn render_pdf(file: &std::path::Path, page: u16, out: &std::path::Path, width: i32) -> Result<()> {
    use indexfox_extract::pdf::PdfEngine;
    let engine = PdfEngine::bind(None)?;
    let size = engine.render_page_png(file, page, width, out)?;
    println!("rendered {}x{}", size.0, size.1);
    Ok(())
}

#[cfg(not(feature = "pdfium"))]
fn render_pdf(_: &std::path::Path, _: u16, _: &std::path::Path, _: i32) -> Result<()> {
    bail!("this build was compiled without the `pdfium` feature")
}

fn run_fts_demo(db_path: &std::path::Path) -> Result<()> {
    let mut db = IndexDb::open_in_memory()?;
    let source = db.ensure_source(SourceKind::Local, "/demo")?;
    let file_id = db.upsert_file(&NewFile {
        source_id: source,
        path_key: "/demo/camera_driver.c".into(),
        display_path: "/demo/camera_driver.c".into(),
        filename: "camera_driver.c".into(),
        ext: "c".into(),
        size_bytes: 512,
        mtime_ns: Some(0),
        state: FileState::Pending,
    })?;
    let text = "Linux 内核驱动 VIDIOC_STREAMON\n\
                dma_alloc_coherent() 采集卡寄存器\n\
                #include <linux/videodev2.h>\n\
                进度 100% 名称 a_b 内核\n";
    let document = ExtractedDocument {
        extractor_version: "demo".into(),
        encoding: Some("utf-8".into()),
        sections: vec![indexfox_core::Section {
            text: indexfox_core::normalize_search_text(text),
            display_text: text.to_string(),
            locator: indexfox_core::SourceLocator::Text {
                start_line: 1,
                end_line: 4,
                start_offset: 0,
                end_offset: text.len(),
            },
            origin: indexfox_core::Origin::Native,
        }],
    };
    db.replace_file_content(file_id, &document)?;

    println!("db: {} (in-memory, schema demo)", db_path.display());
    println!(
        "{:<22} {:>8} {:>10} {:>8} {:>8}",
        "query", "fts5", "shortgram", "files", "hits"
    );
    for query in [
        "内",
        "内核",
        "内核驱动",
        "VIDIOC_STREAMON",
        "dma_alloc_coherent()",
        "#include",
        "100%",
        "a_b",
        "linux",
        "Linux",
        "Linux kernel",
    ] {
        let escaped = query.replace('"', "\"\"");
        let fts: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
                [format!("\"{escaped}\"")],
                |row| row.get(0),
            )
            .unwrap_or(-1);
        let grams = db.candidate_chunks(query)?.len();
        let response = search(
            &db,
            &format!("\"{}\"", query.replace('"', "\\\"")),
            &SearchOptions::default(),
        )?;
        println!(
            "{:<22} {:>8} {:>10} {:>8} {:>8}",
            query, fts, grams, response.total_files, response.total_hits
        );
    }
    println!("note: FTS5 trigram returns 0 for 1-2 character terms; short_grams supplies them.");
    Ok(())
}
