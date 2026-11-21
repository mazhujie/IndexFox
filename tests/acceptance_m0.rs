//! M0 acceptance subset: Q01-Q09, Q11-Q13, Q16-Q19, Q23 and FTS integrity.
//!
//! These tests exercise the same pipeline the CLI uses: scanner -> extractor
//! -> atomic SQLite write -> AST query -> verified hits.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexfox_cli::pipeline::{index_directory, index_file, IndexOptions};
use indexfox_core::{ExtractedDocument, FileState, SearchMode, SourceLocator};
use indexfox_extract::{extract_path, ChunkOptions, OcrOptions};
use indexfox_index::{IndexDb, NewFile};
use indexfox_query::{search, FileResult, SearchOptions, SearchResponse};
use tempfile::TempDir;

fn fixture(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(relative)
}

fn copy_fixture(relative: &str, directory: &Path) {
    let name = Path::new(relative).file_name().expect("fixture file name");
    fs::copy(fixture(relative), directory.join(name)).expect("copy fixture");
}

fn index(db: &mut IndexDb, directory: &Path, options: IndexOptions) -> usize {
    index_directory(db, directory, &options)
        .expect("index directory")
        .indexed
}

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

fn result<'a>(response: &'a SearchResponse, filename: &str) -> &'a FileResult {
    response
        .results
        .iter()
        .find(|result| result.filename == filename)
        .unwrap_or_else(|| panic!("{filename} not in {:?}", names(response)))
}

#[test]
fn q01_to_q06_short_terms_identifiers_and_literals() {
    let dir = TempDir::new().unwrap();
    copy_fixture("text/kernel.md", dir.path());
    copy_fixture("text/code.c", dir.path());

    let mut db = IndexDb::open_in_memory().unwrap();
    assert_eq!(index(&mut db, dir.path(), IndexOptions::default()), 2);

    // Q01/Q02: 1- and 2-character terms are served by short_grams.
    let single = query(&db, "内");
    assert_eq!(names(&single), vec!["kernel.md"]);
    let double = query(&db, "内核");
    assert_eq!(names(&double), vec!["kernel.md"]);

    // Q03: contiguous order matters; reversed phrase must not match.
    let phrase = query(&db, "内核驱动");
    assert_eq!(names(&phrase), vec!["kernel.md"]);
    assert!(query(&db, "驱动内核").results.is_empty());

    // Q04: identifier exact hit in source code.
    let identifier = query(&db, "VIDIOC_STREAMON");
    assert!(names(&identifier).contains(&"code.c"));

    // Q05: parentheses and underscores are literal characters.
    let function = query(&db, "dma_alloc_coherent()");
    assert!(names(&function).contains(&"code.c"));
    assert!(names(&function).contains(&"kernel.md"));

    // Q06: leading '#' is content, not an operator.
    let include = query(&db, "#include");
    assert!(names(&include).contains(&"code.c"));

    assert_eq!(query(&db, "100%").total_files, 2);
    assert_eq!(query(&db, "a_b").total_files, 2);
}

#[test]
fn q07_q08_file_level_boolean_expressions() {
    let dir = TempDir::new().unwrap();
    copy_fixture("text/kernel.md", dir.path());
    fs::write(
        dir.path().join("windows.txt"),
        "Linux Windows 驱动 双系统说明",
    )
    .unwrap();

    let mut db = IndexDb::open_in_memory().unwrap();
    index(&mut db, dir.path(), IndexOptions::default());

    let excluded = query(&db, "Linux -Windows");
    assert!(names(&excluded).contains(&"kernel.md"));
    assert!(!names(&excluded).contains(&"windows.txt"));

    let grouped = query(&db, "(Linux OR Windows) 驱动");
    assert!(names(&grouped).contains(&"kernel.md"));
    assert!(names(&grouped).contains(&"windows.txt"));

    let or_query = query(&db, "Windows OR 驱动");
    assert!(names(&or_query).contains(&"windows.txt"));
}

#[test]
fn q09_quoted_phrase_with_space() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("phrase.txt"),
        "A Linux kernel driver\nLinux driver with kernel word",
    )
    .unwrap();
    let mut db = IndexDb::open_in_memory().unwrap();
    index(&mut db, dir.path(), IndexOptions::default());

    assert_eq!(names(&query(&db, "\"Linux kernel\"")), vec!["phrase.txt"]);
    assert!(query(&db, "\"kernel Linux\"").results.is_empty());
}

#[test]
fn q11_percent_and_underscore_are_not_sql_wildcards() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("wild.txt"), "进度 100% 名称 a_b").unwrap();
    fs::write(dir.path().join("other.txt"), "进度 1000 名称 aXb").unwrap();

    let mut db = IndexDb::open_in_memory().unwrap();
    index(&mut db, dir.path(), IndexOptions::default());

    assert_eq!(names(&query(&db, "100%")), vec!["wild.txt"]);
    assert_eq!(names(&query(&db, "a_b")), vec!["wild.txt"]);
    assert_eq!(names(&query(&db, "%")), vec!["wild.txt"]);
    assert_eq!(names(&query(&db, "_")), vec!["wild.txt"]);
}

#[test]
fn q12_phrase_across_chunk_boundary_is_found() {
    let dir = TempDir::new().unwrap();
    copy_fixture("text/cross_chunk.txt", dir.path());
    let mut db = IndexDb::open_in_memory().unwrap();
    index(&mut db, dir.path(), IndexOptions::default());

    let response = query(&db, "内核驱动");
    assert_eq!(names(&response), vec!["cross_chunk.txt"]);
    let hit = &response.results[0].hits[0];
    assert!(hit.crosses_chunk_end, "expected a boundary-spanning hit");
    assert!(query(&db, "驱动内核").results.is_empty());
}

#[test]
fn q13_pdf_page_locator() {
    if std::env::var_os("PDFIUM_LIB_PATH").is_none() {
        eprintln!("skipping PDF test: PDFIUM_LIB_PATH is not set");
        return;
    }
    let dir = TempDir::new().unwrap();
    copy_fixture("pdf/sample.pdf", dir.path());
    let mut db = IndexDb::open_in_memory().unwrap();
    index(&mut db, dir.path(), IndexOptions::default());

    let response = query(&db, "VIDIOC_STREAMON");
    let file = result(&response, "sample.pdf");
    assert!(file
        .hits
        .iter()
        .any(|hit| { matches!(hit.locator, Some(SourceLocator::PdfPage { page: 2, .. })) }));
    let third = query(&db, "kernel driver");
    assert!(third.results.iter().any(|file| {
        file.hits
            .iter()
            .any(|hit| matches!(hit.locator, Some(SourceLocator::PdfPage { page: 3, .. })))
    }));
}

#[test]
fn q14_scanned_pdf_ocr_when_available() {
    let (Ok(tesseract), Some(_)) = (
        std::env::var("TESSERACT_PATH"),
        std::env::var_os("PDFIUM_LIB_PATH"),
    ) else {
        eprintln!("skipping OCR test: TESSERACT_PATH or PDFIUM_LIB_PATH is not set");
        return;
    };
    let dir = TempDir::new().unwrap();
    copy_fixture("pdf/scanned_zh.pdf", dir.path());
    let mut db = IndexDb::open_in_memory().unwrap();
    let report = index_directory(
        &mut db,
        dir.path(),
        &IndexOptions {
            ocr: Some(OcrOptions {
                executable: PathBuf::from(tesseract),
                languages: std::env::var("INDEXFOX_OCR_LANG")
                    .unwrap_or_else(|_| "chi_sim+eng".to_string()),
                dpi: 250,
                timeout: Duration::from_secs(60),
            }),
            ..IndexOptions::default()
        },
    )
    .unwrap();
    assert_eq!(report.indexed, 1, "{report:?}");

    let response = query(&db, "内核驱动");
    let file = result(&response, "scanned_zh.pdf");
    let hit = &file.hits[0];
    assert_eq!(hit.origin, indexfox_core::Origin::Ocr);
    assert!(matches!(
        hit.locator,
        Some(SourceLocator::PdfPage { page: 1, .. })
    ));
    assert!(names(&query(&db, "VIDIOC_STREAMON")).contains(&"scanned_zh.pdf"));
}

#[test]
fn q16_q17_docx_cross_run_and_table_cells() {
    let dir = TempDir::new().unwrap();
    copy_fixture("docx/cross_run_table.docx", dir.path());
    let mut db = IndexDb::open_in_memory().unwrap();
    index(&mut db, dir.path(), IndexOptions::default());

    // "VIDIOC_STREAM" + "ON" live in different runs of one paragraph.
    let response = query(&db, "VIDIOC_STREAMON");
    let file = result(&response, "cross_run_table.docx");
    assert!(file
        .hits
        .iter()
        .any(|hit| matches!(hit.locator, Some(SourceLocator::DocxParagraph { .. }))));

    let table = query(&db, "寄存器 DMA_ADDR 0x1F");
    let file = result(&table, "cross_run_table.docx");
    assert!(file.hits.iter().any(|hit| matches!(
        &hit.locator,
        Some(SourceLocator::DocxTableCell {
            table: 1,
            row: 1,
            column: 1,
            ..
        })
    )));
}

#[test]
fn q18_gbk_and_utf16_decoding() {
    let gbk = extract_path(&fixture("text/gbk.txt"), ChunkOptions::default()).unwrap();
    assert_eq!(gbk.encoding.as_deref(), Some("gb18030"));
    assert_eq!(gbk.sections.len(), 1);
    assert_eq!(gbk.sections[0].display_text, "Linux 内核驱动 GBK 中文测试");
    // 27 GBK bytes decode to 35 UTF-8 bytes; locators use decoded offsets.
    assert_eq!(
        gbk.sections[0].locator,
        SourceLocator::Text {
            start_line: 1,
            end_line: 1,
            start_offset: 0,
            end_offset: 35,
        }
    );

    let utf16 = extract_path(&fixture("text/utf16.txt"), ChunkOptions::default()).unwrap();
    assert_eq!(utf16.encoding.as_deref(), Some("utf-16le-bom"));
    assert_eq!(utf16.sections.len(), 1);
    assert_eq!(utf16.sections[0].display_text, "Linux 内核驱动 UTF16 测试");
    // 40 UTF-16 bytes (BOM included) decode to 31 UTF-8 bytes.
    assert_eq!(
        utf16.sections[0].locator,
        SourceLocator::Text {
            start_line: 1,
            end_line: 1,
            start_offset: 0,
            end_offset: 31,
        }
    );

    let dir = TempDir::new().unwrap();
    copy_fixture("text/gbk.txt", dir.path());
    copy_fixture("text/utf16.txt", dir.path());
    let mut db = IndexDb::open_in_memory().unwrap();
    index(&mut db, dir.path(), IndexOptions::default());
    let response = query(&db, "内核驱动");
    assert!(names(&response).contains(&"gbk.txt"));
    assert!(names(&response).contains(&"utf16.txt"));
    assert!(names(&query(&db, "GBK")).contains(&"gbk.txt"));
    assert!(names(&query(&db, "UTF16")).contains(&"utf16.txt"));
}

#[test]
fn text_locators_report_line_and_offset_truth() {
    let txt = extract_path(&fixture("text/kernel.md"), ChunkOptions::default()).unwrap();
    assert_eq!(txt.sections.len(), 1);
    let section = &txt.sections[0];
    assert_eq!(section.display_text.matches('\n').count(), 5);
    assert_eq!(section.display_text.len(), 157);
    // Offsets are UTF-8 byte offsets into the decoded text.
    assert_eq!(
        section.locator,
        SourceLocator::Text {
            start_line: 1,
            end_line: 5,
            start_offset: 0,
            end_offset: 157,
        }
    );
}

#[test]
fn legacy_text_locator_trailing_lf_end_line_is_repaired() {
    let mut db = IndexDb::open_in_memory().unwrap();
    let source = db
        .ensure_source(indexfox_core::SourceKind::Local, "/legacy")
        .unwrap();
    let file = db
        .upsert_file(&NewFile {
            source_id: source,
            path_key: "/legacy/notes.txt".into(),
            display_path: "/legacy/notes.txt".into(),
            filename: "notes.txt".into(),
            ext: "txt".into(),
            size_bytes: 50,
            mtime_ns: Some(0),
            state: FileState::Pending,
        })
        .unwrap();

    let section =
        |text: &str, start_line: u32, end_line: u32, start_offset: usize| indexfox_core::Section {
            text: indexfox_core::normalize_search_text(text),
            display_text: text.into(),
            locator: SourceLocator::Text {
                start_line,
                end_line,
                start_offset,
                end_offset: start_offset + text.len(),
            },
            origin: indexfox_core::Origin::Native,
        };
    let document = ExtractedDocument {
        extractor_version: "v1".into(),
        encoding: Some("utf-8".into()),
        sections: vec![
            // Old V1 locator: trailing LF pushed end_line one past the body.
            section("alpha\nbeta\n", 1, 3, 0),
            // New extractor output for the same trailing-LF shape.
            section("one\ntwo\n", 4, 5, 20),
            // No trailing LF: V1 already stored the correct inclusive line.
            section("gamma\ndelta", 7, 8, 30),
        ],
    };
    db.replace_file_content(file, &document).unwrap();

    let end_line = |term: &str| {
        let response = query(&db, term);
        let hit = &result(&response, "notes.txt").hits[0];
        match hit.locator.as_ref().unwrap() {
            SourceLocator::Text {
                start_line,
                end_line,
                ..
            } => (*start_line, *end_line),
            other => panic!("unexpected locator {other:?}"),
        }
    };

    // Legacy trailing LF is recomputed from the cached body.
    assert_eq!(end_line("beta"), (1, 2));
    // Already-correct trailing LF stays unchanged.
    assert_eq!(end_line("two"), (4, 5));
    // Non-trailing LF keeps the stored inclusive line.
    assert_eq!(end_line("delta"), (7, 8));
}

#[test]
fn docx_missing_document_xml_fails_safely() {
    use std::io::{Cursor, Write as _};

    let dir = TempDir::new().unwrap();
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            "docProps/core.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    writer.write_all(b"<core/>").unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    let path = dir.path().join("no_document.docx");
    fs::write(&path, bytes).unwrap();

    let error = extract_path(&path, ChunkOptions::default()).unwrap_err();
    assert!(
        matches!(error, indexfox_extract::ExtractError::MissingDocumentXml),
        "unexpected error: {error:?}"
    );

    let mut db = IndexDb::open_in_memory().unwrap();
    let report = index_directory(&mut db, dir.path(), &IndexOptions::default()).unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(db.stats().unwrap().failed_files, 1);
}

#[test]
fn q19_oversize_is_metadata_only_but_filename_searchable() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("oversize_doc.txt"),
        "hidden_token ".repeat(100),
    )
    .unwrap();
    let mut db = IndexDb::open_in_memory().unwrap();
    assert_eq!(index(&mut db, dir.path(), IndexOptions::default()), 1);
    assert_eq!(query(&db, "hidden_token").total_files, 1);

    let options = IndexOptions {
        max_file_size_bytes: 32,
        ..IndexOptions::default()
    };
    let report = index_directory(&mut db, dir.path(), &options).unwrap();
    assert_eq!(report.skipped_size, 1);
    assert_eq!(report.indexed, 0);

    assert!(query(&db, "hidden_token").results.is_empty());
    let by_name = query(&db, "oversize_doc");
    assert_eq!(names(&by_name), vec!["oversize_doc.txt"]);
    assert_eq!(by_name.results[0].state, FileState::SkippedSize.as_str());
}

#[test]
fn q23_complete_reconcile_removes_deleted_files_only() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("alpha.txt"), "alpha unique").unwrap();
    fs::write(dir.path().join("beta.txt"), "beta unique").unwrap();

    let mut db = IndexDb::open_in_memory().unwrap();
    index(&mut db, dir.path(), IndexOptions::default());

    fs::remove_file(dir.path().join("alpha.txt")).unwrap();
    let report = index_directory(&mut db, dir.path(), &IndexOptions::default()).unwrap();
    assert_eq!(report.removed, 1);
    assert!(query(&db, "alpha").results.is_empty());
    assert_eq!(names(&query(&db, "beta")), vec!["beta.txt"]);
}

#[test]
fn incremental_index_skips_unchanged_and_force_reindexes() {
    let dir = TempDir::new().unwrap();
    copy_fixture("text/kernel.md", dir.path());
    let mut db = IndexDb::open_in_memory().unwrap();

    let first = index_directory(&mut db, dir.path(), &IndexOptions::default()).unwrap();
    assert_eq!(first.indexed, 1);
    let second = index_directory(&mut db, dir.path(), &IndexOptions::default()).unwrap();
    assert_eq!(second.indexed, 0);
    assert_eq!(second.unchanged, 1);
    let forced = index_directory(
        &mut db,
        dir.path(),
        &IndexOptions {
            force: true,
            ..IndexOptions::default()
        },
    )
    .unwrap();
    assert_eq!(forced.indexed, 1);
    db.verify_fts().unwrap();
}

#[test]
fn failed_reindex_keeps_the_last_successful_version_searchable() {
    let dir = TempDir::new().unwrap();
    copy_fixture("docx/cross_run_table.docx", dir.path());
    let path = dir.path().join("cross_run_table.docx");
    let mut db = IndexDb::open_in_memory().unwrap();
    let options = IndexOptions::default();
    assert_eq!(index(&mut db, dir.path(), options.clone()), 1);
    let response = query(&db, "VIDIOC_STREAMON");
    let before = result(&response, "cross_run_table.docx");

    fs::write(&path, b"invalid OOXML archive").unwrap();
    fs::write(dir.path().join("new.docx"), b"invalid OOXML archive").unwrap();
    let report = index_directory(&mut db, dir.path(), &options).unwrap();
    assert_eq!(report.failed, 2);

    let response = query(&db, "VIDIOC_STREAMON");
    let stale = result(&response, "cross_run_table.docx");
    assert_eq!(stale.state, FileState::Stale.as_str());
    assert_eq!(stale.size_bytes, before.size_bytes);
    assert_eq!(stale.mtime_ns, before.mtime_ns);
    assert_eq!(db.stats().unwrap().stale_files, 1);
    assert_eq!(db.stats().unwrap().failed_files, 1);
    db.verify_fts().unwrap();

    copy_fixture("docx/cross_run_table.docx", dir.path());
    assert_eq!(index(&mut db, dir.path(), options), 1);
    let response = query(&db, "VIDIOC_STREAMON");
    assert_eq!(result(&response, "cross_run_table.docx").state, "indexed");
    assert_eq!(db.stats().unwrap().stale_files, 0);
}

#[test]
fn database_write_failure_rolls_back_the_file_version() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("source.txt");
    fs::write(&path, "内核驱动").unwrap();
    let mut db = IndexDb::open_in_memory().unwrap();
    let options = IndexOptions::default();
    assert_eq!(index(&mut db, dir.path(), options.clone()), 1);
    db.conn()
        .execute_batch(
            "CREATE TEMP TRIGGER fail_chunk_write BEFORE INSERT ON chunks
             BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
        )
        .unwrap();

    fs::write(&path, "new content").unwrap();
    let error = index_directory(&mut db, dir.path(), &options).unwrap_err();
    assert!(error.to_string().contains("injected write failure"));
    let response = query(&db, "内核驱动");
    let stale = result(&response, "source.txt");
    assert_eq!(stale.state, FileState::Stale.as_str());
    assert_eq!(stale.size_bytes, "内核驱动".len() as i64);
    assert_eq!(query(&db, "内").total_files, 1);
    assert_eq!(query(&db, "new content").total_files, 0);
    db.verify_fts().unwrap();
}

#[test]
fn exact_and_substring_modes_share_strict_verification() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("case.txt"), "Linux Kernel 内核").unwrap();
    let mut db = IndexDb::open_in_memory().unwrap();
    index(&mut db, dir.path(), IndexOptions::default());

    let insensitive = search(
        &db,
        "\"linux kernel\"",
        &SearchOptions {
            mode: SearchMode::Exact,
            case_sensitive: false,
            ..SearchOptions::default()
        },
    )
    .unwrap();
    assert_eq!(names(&insensitive), vec!["case.txt"]);

    let sensitive = search(
        &db,
        "\"linux kernel\"",
        &SearchOptions {
            mode: SearchMode::Exact,
            case_sensitive: true,
            ..SearchOptions::default()
        },
    )
    .unwrap();
    assert!(sensitive.results.is_empty());
}

#[test]
fn fts_and_short_grams_stay_in_sync_after_replace() {
    let mut db = IndexDb::open_in_memory().unwrap();
    let source = db
        .ensure_source(indexfox_core::SourceKind::Local, "/synthetic")
        .unwrap();
    let file = db
        .upsert_file(&NewFile {
            source_id: source,
            path_key: "/synthetic/a.txt".into(),
            display_path: "/synthetic/a.txt".into(),
            filename: "a.txt".into(),
            ext: "txt".into(),
            size_bytes: 1,
            mtime_ns: Some(0),
            state: FileState::Pending,
        })
        .unwrap();

    let document = |text: &str| ExtractedDocument {
        extractor_version: "test".into(),
        encoding: Some("utf-8".into()),
        sections: vec![indexfox_core::Section {
            text: indexfox_core::normalize_search_text(text),
            display_text: text.into(),
            locator: SourceLocator::Text {
                start_line: 1,
                end_line: 1,
                start_offset: 0,
                end_offset: text.len(),
            },
            origin: indexfox_core::Origin::Native,
        }],
    };
    db.replace_file_content(file, &document("内核驱动"))
        .unwrap();
    assert_eq!(db.candidate_chunks("内").unwrap().len(), 1);
    db.replace_file_content(file, &document("Windows driver"))
        .unwrap();
    assert!(db.candidate_chunks("内").unwrap().is_empty());
    assert_eq!(db.candidate_chunks("windows").unwrap().len(), 1);
    db.verify_fts().unwrap();
}

#[test]
fn q19_single_file_limit_reveals_content_without_touching_other_records() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("keep.txt"), "keep_token original").unwrap();
    fs::write(dir.path().join("big.txt"), "big_token ".repeat(100)).unwrap();
    let mut db = IndexDb::open_in_memory().unwrap();

    let low = IndexOptions {
        max_file_size_bytes: 64,
        ..IndexOptions::default()
    };
    let first = index_directory(&mut db, dir.path(), &low).unwrap();
    assert_eq!(first.indexed, 1);
    assert_eq!(first.skipped_size, 1);
    assert!(query(&db, "big_token").results.is_empty());
    let keep_before = result(&query(&db, "keep_token"), "keep.txt").clone();

    // Change the other file on disk. A targeted index must not notice it.
    fs::write(dir.path().join("keep.txt"), "keep_token changed new_token").unwrap();

    let high = IndexOptions {
        max_file_size_bytes: 1024 * 1024,
        force: true,
        ..IndexOptions::default()
    };
    let report = index_file(&mut db, dir.path(), Path::new("big.txt"), &high).unwrap();
    assert_eq!(report.scanned, 1);
    assert_eq!(report.indexed, 1);
    assert_eq!(report.removed, 0);
    assert_eq!(query(&db, "big_token").total_files, 1);

    let keep_response = query(&db, "keep_token");
    let keep_after = result(&keep_response, "keep.txt");
    assert_eq!(keep_after.file_id, keep_before.file_id);
    assert_eq!(keep_after.size_bytes, keep_before.size_bytes);
    assert_eq!(keep_after.mtime_ns, keep_before.mtime_ns);
    assert_eq!(keep_after.state, keep_before.state);
    assert_eq!(query(&db, "new_token").total_files, 0);

    // The explicit limit still applies to a single file and stays metadata-only.
    let report = index_file(&mut db, dir.path(), Path::new("big.txt"), &low).unwrap();
    assert_eq!(report.skipped_size, 1);
    assert_eq!(report.indexed, 0);
    assert!(query(&db, "big_token").results.is_empty());
    let by_name = query(&db, "big.txt");
    assert_eq!(names(&by_name), vec!["big.txt"]);
    assert_eq!(by_name.results[0].state, FileState::SkippedSize.as_str());
    assert_eq!(
        result(&query(&db, "keep_token"), "keep.txt").file_id,
        keep_before.file_id
    );
    db.verify_fts().unwrap();
}

#[test]
fn single_file_index_rejects_invalid_paths_and_ignored_items() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("notes.bin"), "not whitelisted").unwrap();
    fs::create_dir_all(dir.path().join("node_modules")).unwrap();
    fs::write(dir.path().join("node_modules/pkg.txt"), "ignored").unwrap();
    fs::create_dir_all(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/real.txt"), "hello").unwrap();

    let mut db = IndexDb::open_in_memory().unwrap();
    let options = IndexOptions::default();
    let cases = [
        (
            PathBuf::from("../escape.txt"),
            "relative to the source root",
        ),
        (
            PathBuf::from("sub/../sub/real.txt"),
            "relative to the source root",
        ),
        (
            dir.path().join("sub/real.txt"),
            "relative to the source root",
        ),
        (PathBuf::from("node_modules/pkg.txt"), "ignore rules"),
        (PathBuf::from("notes.bin"), "whitelist"),
        (PathBuf::from("sub"), "regular file"),
        (PathBuf::from("missing.txt"), "cannot inspect"),
    ];
    for (relative, expected) in cases {
        let error = index_file(&mut db, dir.path(), &relative, &options)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(expected),
            "{relative:?}: expected {expected:?}, got {error:?}"
        );
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path().join("sub/real.txt"), dir.path().join("link.txt"))
            .unwrap();
        let error = index_file(&mut db, dir.path(), Path::new("link.txt"), &options)
            .unwrap_err()
            .to_string();
        assert!(error.contains("regular file"), "symlink: {error}");
    }

    // Rejections must not create file records.
    assert_eq!(db.stats().unwrap().files, 0);
}
