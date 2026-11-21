# IndexFox M4.6 benchmark: 10k / 100k files

Measured with the release CLI on 2026-11-21 (Linux/WSL2). Every number below
comes from the saved run:

- Local raw JSON: `target/IndexFox-M4-benchmark.json`
- Console log: `/tmp/indexfox-m4-benchmark-final.log`
- Harness: [`scripts/benchmark.py`](../scripts/benchmark.py)

The corpus is synthetic and deterministic. Accuracy is measured only against
that corpus's explicit manifest/oracle, not against production files. **No
Windows run was made; this document makes no Windows performance or accuracy
claim.**

## 1. Host and build

| Item | Value |
|---|---|
| Host | `MZJ-DESKTOP` |
| OS / kernel | Linux 6.18.40.1-microsoft-standard-WSL2, x86_64, WSL2, glibc 2.43 |
| CPU | AMD Ryzen 9 9900X 12-Core Processor |
| Logical CPUs | 24 |
| RAM | 15,914,688 kB |
| Disk | 1,081,101,176,832 B total; 875,510,939,648 B free at 10k start, 875,007,287,296 B free at 100k start |
| Load average (1/5/15 min) | 10k: 4.58 / 4.43 / 2.81; 100k: 13.51 / 8.58 / 4.72 |
| Run timestamps (UTC) | 10k: 2026-11-21T02:00:09Z; 100k: 2026-11-21T02:03:19Z |
| CLI | `target/release/indexfox-cli`, `indexfox 0.1.0`, release profile |
| CLI SHA-256 | `f17b9668ab8d1a9cf1e4b63892267e80a47dba582aa3952868682b326fd47a8e` |
| CLI SHA-256 for fuzzy retest | `4632ba35aeec0f720cdba5b49c7b0a911213784c6a504f9dc4bf62083bfc473b` |
| PDFium | `.deps/pdfium/lib/libpdfium.so` |
| Harness Python | 3.14.4 |
| OCR Tesseract | `/tmp/tess/root/usr/bin/tesseract`; languages `chi_sim`, `eng` |

The 100k pass started under a substantially higher load average; treat the
10k/100k latency ratio as measured on this host, not as a controlled scaling
curve.

## 2. Measured 10k / 100k results

Source bytes are generated source-file bytes. Extracted text is the normalized
text stored in `chunks`. Database values are after the full index unless the
row says otherwise.

| Metric | 10,000 files | 100,000 files |
|---|---:|---:|
| Source files | 10,000 | 100,000 |
| Source bytes | 6,415,573 B | 64,251,096 B |
| Corpus SHA-256 | `f9fb2c62ec098a4fd3d86007f6a55bde710697752eae19082f7ae708506603c7` | `6554b3c8f77a7021e689bb20a0a24553bd2d1500f1503fb87173803319d6749a` |
| Full index: scanned / indexed / failed / skipped | 10,000 / 10,000 / 0 / 0 | 100,000 / 100,000 / 0 / 0 |
| Full index wall (CLI `elapsed_ms`) | 158.153 s (158.120 s) | 1,422.542 s (1,422.456 s) |
| No-op index: indexed / unchanged / failed / skipped | 0 / 10,000 / 0 / 0 | 0 / 100,000 / 0 / 0 |
| No-op wall (CLI `elapsed_ms`) | 0.243 s (0.197 s) | 1.485 s (1.440 s) |
| 10-edit incremental: indexed / unchanged / failed / skipped | 10 / 9,990 / 0 / 0 | 10 / 99,990 / 0 / 0 |
| 10-edit incremental wall (CLI `elapsed_ms`) | 1.218 s (1.201 s) | 6.925 s (6.887 s) |
| Peak RSS: full / no-op / 10 edits | 21,764 / 18,592 / 18,856 kB | 83,228 / 81,804 / 82,088 kB |
| DB after full: logical / disk / main / WAL / SHM | 78,655,488 / 78,655,488 / 78,655,488 / 0 / 0 B | 821,170,176 / 821,170,176 / 821,170,176 / 0 / 0 B |
| DB after full: page size × page count | 4,096 × 19,203 | 4,096 × 200,481 |
| DB after 10 edits: logical / disk | 79,036,416 / 79,036,416 B | 821,264,384 / 821,264,384 B |
| Extracted text: after full / after 10 edits | 6,003,179 / 6,003,319 B | 60,123,406 / 60,123,546 B |
| Chunks: after full / after 10 edits | 10,400 / 10,400 | 104,000 / 104,000 |
| `short_grams`: after full / after 10 edits | 1,726,397 / 1,726,441 | 17,422,245 / 17,422,289 |
| Exact query micro P / R / F1 | 1.0000 / 1.0000 / 1.0000 | 1.0000 / 1.0000 / 1.0000 |
| Exact expected / returned / TP | 254 / 254 / 254 | 2,054 / 2,054 / 2,054 |
| Strict negatives: clean / total, false positives | 2 / 2, 0 | 2 / 2, 0 |
| Fuzzy query micro P / R / F1 | 1.0000 / 1.0000 / 1.0000 | 1.0000 / 1.0000 / 1.0000 |
| Fuzzy expected / returned / TP | 4 / 4 / 4 | 4 / 4 / 4 |
| Incremental query P / R / F1 | 1.0000 / 1.0000 / 1.0000 | 1.0000 / 1.0000 / 1.0000 |
| PDF page locator accuracy | 100 / 100 (1.0000) | 1,000 / 1,000 (1.0000) |
| Exact query P50 / P95: engine | 151 / 302 ms | 1,382 / 2,700 ms |
| Exact query P50 / P95: wall | 154.109 / 321.614 ms | 1,387.079 / 2,703.083 ms |
| Fuzzy query P50 / P95: engine | 141 / 148 ms | 1,482 / 2,212 ms |
| Fuzzy query P50 / P95: wall | 144.021 / 150.355 ms | 1,486.435 / 2,232.581 ms |
| Incremental query P50 / P95: engine | 230 / 251 ms | 1,371 / 1,377 ms |
| Incremental query P50 / P95: wall | 233.886 / 254.432 ms | 1,373.767 / 1,380.233 ms |
| PDF page query P50 / P95: engine | 144 / 147 ms | 1,378 / 1,399 ms |
| PDF page query P50 / P95: wall | 146.878 / 149.911 ms | 1,382.409 / 1,403.765 ms |
| OCR per-page P50 / P95 | 279.278 / 305.376 ms | 191.115 / 202.380 ms |

All index stages reported `removed=0`, `incomplete_scan=false`, and no issues.
Exact micro and mean precision and recall are 1.0 on this corpus. Each scale
has 60 strict and strict-negative timing samples, five fuzzy samples and five
incremental query samples. Wall time includes CLI startup.

## 3. Per-format index state

All formats had `failed=0`, `skipped=0`, `stale=0`, and `pending=0` at both
scales.

### 10,000 files

| Format | Files | Source bytes | Indexed | Failed | Skipped | Extracted text bytes |
|---|---:|---:|---:|---:|---:|---:|
| TXT | 6,800 | 4,146,139 | 6,800 | 0 | 0 | 4,146,139 |
| code (`.c`/`.rs`/`.py`) | 1,200 | 744,688 | 1,200 | 0 | 0 | 744,688 |
| Markdown | 1,000 | 616,220 | 1,000 | 0 | 0 | 616,220 |
| GBK TXT | 300 | 177,996 | 300 | 0 | 0 | 178,006 |
| UTF-16 TXT | 300 | 356,048 | 300 | 0 | 0 | 177,744 |
| UTF-8 BOM TXT | 200 | 126,232 | 200 | 0 | 0 | 125,632 |
| DOCX | 100 | 158,049 | 100 | 0 | 0 | 7,949 |
| PDF | 100 | 90,201 | 100 | 0 | 0 | 6,801 |
| **Total** | **10,000** | **6,415,573** | **10,000** | **0** | **0** | **6,003,179** |

### 100,000 files

| Format | Files | Source bytes | Indexed | Failed | Skipped | Extracted text bytes |
|---|---:|---:|---:|---:|---:|---:|
| TXT | 68,000 | 41,525,805 | 68,000 | 0 | 0 | 41,525,805 |
| code (`.c`/`.rs`/`.py`) | 12,000 | 7,459,897 | 12,000 | 0 | 0 | 7,459,897 |
| Markdown | 10,000 | 6,171,194 | 10,000 | 0 | 0 | 6,171,194 |
| GBK TXT | 3,000 | 1,779,141 | 3,000 | 0 | 0 | 1,779,151 |
| UTF-16 TXT | 3,000 | 3,567,440 | 3,000 | 0 | 0 | 1,780,740 |
| UTF-8 BOM TXT | 2,000 | 1,262,869 | 2,000 | 0 | 0 | 1,256,869 |
| DOCX | 1,000 | 1,581,849 | 1,000 | 0 | 0 | 80,849 |
| PDF | 1,000 | 902,901 | 1,000 | 0 | 0 | 68,901 |
| **Total** | **100,000** | **64,251,096** | **100,000** | **0** | **0** | **60,123,406** |

## 4. Accuracy and oracle detail

The oracle is the harness manifest. Expected sets are exact file sets; a
missing file is recall loss, any extra file is precision loss. The search limit
is 1,000,000 so every truth match is collectable.

| Query id | Oracle | Mode | 10k expected / returned | 100k expected / returned |
|---|---|---|---:|---:|
| `alpha` | 8 files, one per format/encoding | exact | 8 / 8 | 8 / 8 |
| `beta` | 4 files, 2 shared with alpha | exact | 4 / 4 | 4 / 4 |
| `boolean_alpha_not_beta` | alpha NOT beta | exact | 6 / 6 | 6 / 6 |
| `or_alpha_gamma` | alpha OR gamma | exact | 12 / 12 | 12 / 12 |
| `phrase` | contiguous two-token phrase | exact | 5 / 5 | 5 / 5 |
| `chinese_encodings` | UTF-8, Markdown, GBK, UTF-16, BOM, DOCX | exact | 13 / 13 | 13 / 13 |
| `code_identifier` | code files | exact | 5 / 5 | 5 / 5 |
| `filename_only` | filename match, body absent | exact | 1 / 1 | 1 / 1 |
| `docx_body` | all DOCX files | exact | 100 / 100 | 1,000 / 1,000 |
| `pdf_page_two` | all PDF files; locator must be page 2 | exact | 100 / 100 | 1,000 / 1,000 |
| `absent` | absent token | exact | 0 / 0 | 0 / 0 |
| `fuzzy_typo_as_strict` | one-deletion typo in exact mode | exact | 0 / 0 | 0 / 0 |
| `fuzzy_typo` | same typo in fuzzy mode | fuzzy | 4 / 4 | 4 / 4 |
| `incremental_edit` | 10 appended TXT files | exact | 10 / 10 | 10 / 10 |

All 14 queries returned status `ok`. The PDF oracle checks `pdf_page` and
`page == 2`, with 100/100 correct at 10k and 1,000/1,000 at 100k.

The first pass found 4 expected files and 16 false positives at each scale.
The query had expanded a numeric suffix separately from its identifier.
After making identifier expansion whole-token, both scales returned the
four expected files. The retest reused the databases after the 10 unrelated
appends because the fix changes query tokenization alone. The JSON retains the
first-pass counts with the retest SHA-256 and timestamps. Fuzzy accuracy here
describes one synthetic typo.

## 5. OCR sample

The OCR measurement uses a separate corpus and a fresh database per repeat.
The sample is `fixtures/pdf/scanned_zh.pdf`: 26,298 B, one scanned page. A
one-page sample means per-page latency equals the whole CLI index run.

| Item | 10k run | 100k run |
|---|---:|---:|
| Repeats | 3 | 3 |
| Indexed / failed | 1 / 0 | 1 / 0 |
| Per-page P50 / P95 | 279.278 / 305.376 ms | 191.115 / 202.380 ms |
| Three-run wall range | 276.264–305.376 ms | 185.039–202.380 ms |
| Peak RSS range | 79,920–80,252 kB | 79,592–80,048 kB |
| DB logical / disk | 118,784 / 118,784 B | 118,784 / 118,784 B |
| OCR text / chunks | 35 B / 1 | 35 B / 1 |
| `内核驱动` check | 1 file, 1 OCR-origin hit | 1 file, 1 OCR-origin hit |
| `VIDIOC_STREAMON` check | 1 file, 1 OCR-origin hit | 1 file, 1 OCR-origin hit |

Tesseract ran with `chi_sim+eng`; both `chi_sim` and `eng` were available.
The OCR sample is independent of corpus size: the 10k and 100k values are
repeated measurements under different host load, not a scale comparison.

## 6. Method and boundaries

- **Corpus:** synthetic, deterministic, seed `4606`; 68% short TXT, 12% code,
  10% Markdown, 3% GBK, 3% UTF-16, 2% UTF-8 BOM, 1% DOCX, 1% PDF. Files are
  sharded 1,000 per directory and use fixed mtimes. No user files are read.
- **Format handling:** DOCX and PDF are generated with fixed shapes; PDFs are
  two pages with the marker on page 2. The corpus digest is recorded in each
  run's manifest and JSON.
- **Accuracy:** file-level set comparison against the manifest. Micro
  precision/recall sums true positives over summed returned/expected sets;
  mean values are per query. Negative queries are reported separately. The
  fuzzy truth set is four files containing the correctly spelled token.
- **Incremental:** after the full and no-op indexes, a new token is appended to
  10 TXT files, then the same corpus is indexed again. The expected set is
  exactly those 10 files.
- **Index time:** Python `perf_counter` around the CLI process, including
  startup; the CLI's own `elapsed_ms` is shown in parentheses. No-op and
  incremental runs use the same database and corpus.
- **Query latency:** engine latency is the CLI query `elapsed_ms` (parse,
  evaluate, search); wall latency is process end-to-end. One warmup plus five
  measured runs per query, except the OCR checks. P50/P95 are nearest-rank over
  measured samples.
- **Database bytes:** logical is `page_count × page_size`; disk is the
  `.db`/`-wal`/`-shm` file total. The measured WAL and SHM files were 0 B.
- **Extracted text bytes:** `SUM(LENGTH(CAST(chunks.text AS BLOB)))` over
  indexed chunks, i.e. normalized stored text, not source bytes.
- **Peak RSS:** harness-recorded maximum resident set size for the index
  process (GNU time when available, otherwise `/proc` sampling).
- **OCR:** separate fresh DB per repeat, one page, Tesseract
  `chi_sim+eng`; per-page latency is total wall time divided by page count.
- **Scope:** Linux/WSL2 release build only. The results do not establish
  Windows performance or accuracy, and the synthetic corpus is not a claim
  about arbitrary real-world file collections.

## 7. Reproduce

Build the release CLI, then run with an empty work directory. The harness
creates `n10000/` and `n100000/` below it; it refuses a non-empty workdir.

```bash
cargo build --release -p indexfox-cli

# PDFium is auto-detected at .deps/pdfium/lib/libpdfium.so.
# Tesseract with chi_sim+eng must be on PATH, or set TESSERACT_PATH.
TESSERACT_PATH=/tmp/tess/root/usr/bin/tesseract \
python3 scripts/benchmark.py --files 10000,100000 \
  --bin target/release/indexfox-cli \
  --workdir /tmp/indexfox-m4-bench \
  --json-out /tmp/indexfox-m4.json
```

Run options were the harness defaults plus the two scales: seed `4606`,
`--repeats 5`, `--warmup 1`, `--limit 1000000`, `--max-size-mb 100`,
`--ocr-repeats 3`, `--ocr-lang chi_sim+eng`. The measured run's JSON `argv`
recorded the kept workdir `target/m4-benchmark-final.rjcAUx` and the output
`target/IndexFox-M4-benchmark.json`; use a fresh empty workdir for a new run.
Keep the result JSON with this report.
