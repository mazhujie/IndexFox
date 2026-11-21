use std::path::{Path, PathBuf};

use indexfox_cli::pipeline::{index_registered_source, register_source};
use indexfox_core::{Origin, SourceLocator};
use indexfox_index::IndexDb;
use indexfox_query::{search, SearchOptions, SearchResponse};

fn fixture(root: &Path) -> PathBuf {
    let path = root.join("mixed.pdf");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/pdf/mixed.pdf"),
        &path,
    )
    .unwrap();
    path
}

fn query(db: &IndexDb, term: &str) -> SearchResponse {
    search(db, term, &SearchOptions::default()).unwrap()
}

#[test]
fn q15_mixed_pdf_keeps_native_and_ocr_pages_separate() {
    if std::env::var_os("PDFIUM_LIB_PATH").is_none() || std::env::var_os("TESSERACT_PATH").is_none()
    {
        eprintln!("skipping mixed PDF test: configure PDFIUM_LIB_PATH and TESSERACT_PATH");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let path = fixture(root.path());
    let original = std::fs::read(&path).unwrap();
    let mut db = IndexDb::open_in_memory().unwrap();
    let source = register_source(&db, root.path()).unwrap();
    let report = index_registered_source(&mut db, source, false, &|| false, &|_| {}).unwrap();
    assert_eq!((report.indexed, report.failed), (1, 0), "{report:?}");
    let native = query(&db, "page two");
    assert!(native.results[0].hits.iter().any(|hit| {
        hit.origin == Origin::Native
            && matches!(hit.locator, Some(SourceLocator::PdfPage { page: 2, .. }))
    }));
    let scan = query(&db, "内核驱动");
    let mut ocr_pages: Vec<_> = scan.results[0]
        .hits
        .iter()
        .map(|hit| {
            assert_eq!(hit.origin, Origin::Ocr);
            match hit.locator {
                Some(SourceLocator::PdfPage { page, .. }) => page,
                _ => panic!("missing OCR page locator: {:?}", hit.locator),
            }
        })
        .collect();
    ocr_pages.sort_unstable();
    assert_eq!(ocr_pages, [4, 5]);
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn only_the_failed_ocr_page_runs_again_after_reopening_the_index() {
    use std::os::unix::fs::PermissionsExt;

    if std::env::var_os("PDFIUM_LIB_PATH").is_none() {
        eprintln!("skipping page retry test: configure PDFIUM_LIB_PATH");
        return;
    }
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("documents");
    std::fs::create_dir(&root).unwrap();
    fixture(&root);
    let executable = sandbox.path().join("ocr");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo 'test OCR 1'; exit; fi
countfile="$0.count"
n=0
if [ -f "$countfile" ]; then read -r n < "$countfile"; fi
n=$((n + 1))
echo "$n" > "$countfile"
case "$n" in
  1) printf 'OCRFIRSTPAGEMARKER' > "$2.txt" ;;
  2) exit 1 ;;
  3) printf 'OCRSECONDPAGEMARKER' > "$2.txt" ;;
  *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = sandbox.path().join("index.db");
    let mut db = IndexDb::open(&path).unwrap();
    let mut config = db.ocr_config().unwrap();
    config.tesseract_path = Some(executable);
    config.max_concurrency = 1;
    db.set_ocr_config(&config).unwrap();
    let source = register_source(&db, &root).unwrap();
    let first = index_registered_source(&mut db, source, false, &|| false, &|_| {}).unwrap();
    assert_eq!(first.failed, 1, "{first:?}");
    assert_eq!(query(&db, "page two").total_files, 1);
    assert_eq!(query(&db, "ocrfirstpagemarker").total_files, 1);
    assert_eq!(query(&db, "ocrsecondpagemarker").total_files, 0);
    drop(db);

    let mut db = IndexDb::open(&path).unwrap();
    let second = index_registered_source(&mut db, source, false, &|| false, &|_| {}).unwrap();
    assert_eq!((second.indexed, second.failed), (1, 0), "{second:?}");
    assert_eq!(
        std::fs::read_to_string(sandbox.path().join("ocr.count")).unwrap(),
        "3\n"
    );
    assert_eq!(query(&db, "ocrfirstpagemarker").total_files, 1);
    assert_eq!(query(&db, "ocrsecondpagemarker").total_files, 1);
    db.verify_fts().unwrap();
}
