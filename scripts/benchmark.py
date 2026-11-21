#!/usr/bin/env python3
"""M4.6 synthetic 10k/100k IndexFox benchmark harness.

Generates deterministic synthetic files with an explicit truth set, indexes
them through the real CLI, and measures extraction/index/db/query/RSS numbers.
No user data is read. Everything lives in a temporary directory by default.

Example (sanity):
  python3 scripts/benchmark.py --files 300 --bin target/debug/indexfox-cli \
      --repeats 2 --warmup 0 --json-out /tmp/bench-300.json

Full run after the release build:
  python3 scripts/benchmark.py --files 10000,100000 \
      --bin target/release/indexfox-cli --json-out /tmp/bench-10k-100k.json
"""
from __future__ import annotations

import argparse
import codecs
import hashlib
import io
import json
import math
import os
import platform
import random
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import zipfile
from collections import defaultdict
from pathlib import Path
from xml.sax.saxutils import escape as xml_escape

BENCH_SCHEMA = 1
BASE_MTIME = 1_700_000_000
FORMAT_ORDER = ["txt", "code", "md", "gbk", "utf16", "utf8bom", "docx", "pdf"]
FORMAT_WEIGHTS = {
    "txt": 68,
    "code": 12,
    "md": 10,
    "gbk": 3,
    "utf16": 3,
    "utf8bom": 2,
    "docx": 1,
    "pdf": 1,
}
FILLER_WORDS = (
    "kernel driver buffer search index cache network test parser stream page "
    "memory request source result schedule chunk query local portable document "
    "adapter service monitor storage token offset matcher verify"
).split()
CHINESE_FILLER = "内核 驱动 缓存 检索 网络 文档 测试 调度 存储 查询".split()

TEXT_FORMATS = {"txt", "code", "md", "gbk", "utf16", "utf8bom"}


class BenchError(RuntimeError):
    pass


def parse_sizes(value: str) -> list[int]:
    sizes: list[int] = []
    for part in value.split(","):
        part = part.strip().lower()
        if not part:
            continue
        multiplier = 1
        if part.endswith("k"):
            multiplier, part = 1_000, part[:-1]
        elif part.endswith("m"):
            multiplier, part = 1_000_000, part[:-1]
        try:
            size = int(float(part) * multiplier)
        except ValueError as error:
            raise argparse.ArgumentTypeError(f"bad --files value: {value}") from error
        if size <= 0:
            raise argparse.ArgumentTypeError("--files must be positive")
        sizes.append(size)
    if not sizes:
        raise argparse.ArgumentTypeError("--files is empty")
    return sizes


def format_for(index: int) -> str:
    bucket = index % 100
    start = 0
    for fmt in FORMAT_ORDER:
        end = start + FORMAT_WEIGHTS[fmt]
        if start <= bucket < end:
            return fmt
        start = end
    raise AssertionError(bucket)


def code_ext(index: int) -> str:
    return ("c", "rs", "py")[index % 3]


def relative_path(index: int, fmt: str, filename_index: int, filename_token: str) -> str:
    shard = f"d{index // 1000:04d}"
    if index == filename_index:
        return f"{shard}/filename_only_{filename_token}.txt"
    stem = f"f{index:06d}"
    if fmt == "txt":
        name = f"{stem}.txt"
    elif fmt == "code":
        name = f"{stem}.{code_ext(index)}"
    elif fmt == "md":
        name = f"{stem}.md"
    elif fmt == "gbk":
        name = f"{stem}_gbk.txt"
    elif fmt == "utf16":
        name = f"{stem}_utf16.txt"
    elif fmt == "utf8bom":
        name = f"{stem}_utf8bom.txt"
    elif fmt == "docx":
        name = f"{stem}.docx"
    elif fmt == "pdf":
        name = f"{stem}.pdf"
    else:
        raise AssertionError(fmt)
    return f"{shard}/{name}"


class Allocator:
    """Deterministic per-format index allocation without touching the same file twice."""

    def __init__(self, by_format: dict[str, list[int]]):
        self.by_format = by_format
        self.cursor = {fmt: 0 for fmt in by_format}

    def take(self, fmt: str, count: int) -> list[int]:
        pool = self.by_format.get(fmt, [])
        if not pool or count <= 0:
            return []
        start = self.cursor[fmt]
        picks = [pool[(start + offset) % len(pool)] for offset in range(count)]
        self.cursor[fmt] = start + count
        return picks

    def take_mixed(self, order: list[str], count: int) -> list[int]:
        picks: list[int] = []
        for fmt in order:
            if len(picks) >= count:
                break
            picks.extend(self.take(fmt, 1))
        if len(picks) < count:
            # Small corpora may not have every format; fill from any pool.
            all_indices = [index for pool in self.by_format.values() for index in pool]
            for index in all_indices:
                if len(picks) >= count:
                    break
                if index not in picks:
                    picks.append(index)
        return dedupe(picks)[:count]


def dedupe(values: list[int]) -> list[int]:
    seen: set[int] = set()
    result: list[int] = []
    for value in values:
        if value not in seen:
            seen.add(value)
            result.append(value)
    return result


def text_body(index: int, fmt: str, tokens: set[str], phrases: list[str], rng: random.Random, phrase_words: set[str]) -> str:
    lines: list[str] = []
    if fmt == "md":
        lines.append(f"# Benchmark document {index}")
        lines.append("")
    elif fmt == "code":
        lines.append(f"// synthetic benchmark source {index}")
    else:
        lines.append(f"benchmark document {index}")
    filler = FILLER_WORDS if fmt != "gbk" else FILLER_WORDS
    for _ in range(10):
        words = [rng.choice(filler) for _ in range(8)]
        if fmt != "gbk" and index % 4 == 0:
            words.append(rng.choice(CHINESE_FILLER))
        lines.append(" ".join(words))
    for phrase in phrases:
        lines.append(phrase)
    body_tokens = sorted(token for token in tokens if token not in phrase_words)
    if body_tokens:
        lines.append("markers " + " ".join(body_tokens))
    return "\n".join(lines) + "\n"


def encode_text(text: str, fmt: str) -> bytes:
    if fmt == "gbk":
        return text.encode("gbk")
    if fmt == "utf16":
        return b"\xff\xfe" + text.encode("utf-16-le")
    if fmt == "utf8bom":
        return codecs.BOM_UTF8 + text.encode("utf-8")
    return text.encode("utf-8")


def docx_bytes(tokens: set[str], index: int, group_token: str) -> bytes:
    paragraph_tokens = sorted(tokens)
    table_token = ""
    if index % 2 == 1 and group_token in tokens:
        paragraph_tokens = [token for token in paragraph_tokens if token != group_token]
        table_token = group_token
    paragraph = " ".join(paragraph_tokens) or f"benchmark docx {index}"
    table_cell = table_token or f"table cell benchmark {index}"
    document = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>{xml_escape(paragraph)}</w:t></w:r></w:p>
    <w:tbl>
      <w:tr>
        <w:tc><w:p><w:r><w:t>register DMA_ADDR</w:t></w:r></w:p></w:tc>
        <w:tc><w:p><w:r><w:t>{xml_escape(table_cell)}</w:t></w:r></w:p></w:tc>
      </w:tr>
    </w:tbl>
    <w:p><w:r><w:t>tail paragraph {index}</w:t></w:r></w:p>
  </w:body>
</w:document>
"""
    content_types = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>
"""
    rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>
"""
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_STORED) as archive:
        for name, data in (
            ("[Content_Types].xml", content_types),
            ("_rels/.rels", rels),
            ("word/document.xml", document),
        ):
            info = zipfile.ZipInfo(name, date_time=(2024, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_STORED
            archive.writestr(info, data.encode("utf-8"))
    return buffer.getvalue()


def pdf_bytes(tokens: set[str], index: int) -> bytes:
    marker = " ".join(sorted(tokens)) or f"benchmark pdf {index}"
    pages = [
        f"IndexFox benchmark PDF {index} page one",
        "page two markers " + marker,
    ]
    objects: list[str] = []

    def add(body: str) -> int:
        objects.append(body)
        return len(objects)

    add("<< /Type /Catalog /Pages 2 0 R >>")
    add("")
    content_ids: list[int] = []
    for text in pages:
        escaped = text.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
        stream = f"BT /F1 20 Tf 72 720 Td ({escaped}) Tj ET"
        content_ids.append(add(f"<< /Length {len(stream)} >>\nstream\n{stream}\nendstream"))
    font = add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
    page_ids: list[int] = []
    for content_id in content_ids:
        page_ids.append(
            add(
                f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
                f"/Resources << /Font << /F1 {font} 0 R >> >> /Contents {content_id} 0 R >>"
            )
        )
    objects[1] = (
        "<< /Type /Pages /Kids ["
        + " ".join(f"{page_id} 0 R" for page_id in page_ids)
        + f"] /Count {len(page_ids)} >>"
    )

    buffer = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = [0]
    for number, body in enumerate(objects, 1):
        offsets.append(len(buffer))
        buffer.extend(f"{number} 0 obj\n{body}\nendobj\n".encode("latin-1"))
    xref = len(buffer)
    buffer.extend(f"xref\n0 {len(objects) + 1}\n".encode())
    buffer.extend(b"0000000000 65535 f \n")
    for offset in offsets[1:]:
        buffer.extend(f"{offset:010d} 00000 n \n".encode())
    buffer.extend(
        f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    )
    return bytes(buffer)


def generate_corpus(workdir: Path, files: int, seed: int) -> tuple[dict, dict[str, str]]:
    corpus = workdir / "corpus"
    corpus.mkdir(parents=True, exist_ok=True)

    by_format: dict[str, list[int]] = {fmt: [] for fmt in FORMAT_ORDER}
    for index in range(files):
        by_format[format_for(index)].append(index)

    tokens = {
        "alpha": f"zqalpha{seed}",
        "beta": f"zqbeta{seed}",
        "gamma": f"zqgamma{seed}",
        "phrase_one": f"zqphraseone{seed}",
        "phrase_two": f"zqphrasetwo{seed}",
        "chinese": f"量子检索狐{seed}",
        "code": f"zq_code_marker_{seed}",
        "filename": f"zqfilename{seed}",
        "docx": f"zqdocxmarker{seed}",
        "pdf": f"zqpdfmarker{seed}",
        "fuzzy": f"zqfuzzymeasure{seed}",
        "edited": f"zqedited{seed}",
        "absent": f"zqabsent{seed}",
    }
    fuzzy_typo = f"zqfuzzymesure{seed}"
    phrase_words = {tokens["phrase_one"], tokens["phrase_two"]}

    allocator = Allocator(by_format)
    alpha = allocator.take_mixed(FORMAT_ORDER, 8)
    beta = dedupe(alpha[:2] + allocator.take_mixed(FORMAT_ORDER, 2))
    gamma = allocator.take_mixed(FORMAT_ORDER, 4)
    phrase = dedupe(allocator.take("txt", 3) + allocator.take("md", 2))
    code = allocator.take("code", 5)
    chinese = dedupe(
        allocator.take("gbk", 2)
        + allocator.take("utf16", 2)
        + allocator.take("utf8bom", 2)
        + allocator.take("txt", 3)
        + allocator.take("md", 2)
        + allocator.take("docx", 2)
    )
    fuzzy = dedupe(allocator.take_mixed(["txt", "code", "md", "docx"], 4))
    filename_index = by_format["txt"][-1] if by_format["txt"] else 0
    edited_indices = [index for index in by_format["txt"][:10] if index != filename_index][:10]
    if not edited_indices:
        edited_indices = [index for index in by_format["txt"][:10]]

    per_index_tokens: dict[int, set[str]] = defaultdict(set)
    per_index_phrases: dict[int, list[str]] = defaultdict(list)
    for index in alpha:
        per_index_tokens[index].add(tokens["alpha"])
    for index in beta:
        per_index_tokens[index].add(tokens["beta"])
    for index in gamma:
        per_index_tokens[index].add(tokens["gamma"])
    for index in phrase:
        per_index_tokens[index].update(phrase_words)
        per_index_phrases[index].append(f"{tokens['phrase_one']} {tokens['phrase_two']}")
    for index in code:
        per_index_tokens[index].add(tokens["code"])
    for index in chinese:
        per_index_tokens[index].add(tokens["chinese"])
    for index in fuzzy:
        per_index_tokens[index].add(tokens["fuzzy"])
    for index in by_format["docx"]:
        per_index_tokens[index].add(tokens["docx"])
    for index in by_format["pdf"]:
        per_index_tokens[index].add(tokens["pdf"])

    digest = hashlib.sha256()
    format_stats = {fmt: {"files": 0, "bytes": 0} for fmt in FORMAT_ORDER}
    path_formats: dict[str, str] = {}
    manifest_files: list[dict] = []
    total_bytes = 0

    for index in range(files):
        fmt = format_for(index)
        rel = relative_path(index, fmt, filename_index, tokens["filename"])
        path = corpus / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        file_tokens = per_index_tokens[index]
        if fmt in TEXT_FORMATS:
            rng = random.Random(seed * 1_000_003 + index)
            text = text_body(
                index,
                fmt,
                file_tokens,
                per_index_phrases[index],
                rng,
                phrase_words,
            )
            data = encode_text(text, fmt)
        elif fmt == "docx":
            data = docx_bytes(file_tokens, index, tokens["docx"])
        elif fmt == "pdf":
            data = pdf_bytes(file_tokens, index)
        else:
            raise AssertionError(fmt)

        path.write_bytes(data)
        mtime = BASE_MTIME + index
        os.utime(path, (mtime, mtime))
        total_bytes += len(data)
        format_stats[fmt]["files"] += 1
        format_stats[fmt]["bytes"] += len(data)
        path_formats[rel] = fmt
        manifest_files.append({"path": rel, "format": fmt, "bytes": len(data)})
        digest.update(rel.encode("utf-8"))
        digest.update(b"\0")
        digest.update(len(data).to_bytes(8, "little"))
        digest.update(hashlib.sha256(data).digest())

    def paths(indices: list[int]) -> list[str]:
        return sorted(
            relative_path(index, format_for(index), filename_index, tokens["filename"])
            for index in indices
        )

    alpha_paths = paths(alpha)
    beta_paths = paths(beta)
    gamma_paths = paths(gamma)
    phrase_paths = paths(phrase)
    chinese_paths = paths(chinese)
    code_paths = paths(code)
    docx_paths = paths(by_format["docx"])
    pdf_paths = paths(by_format["pdf"])
    fuzzy_paths = paths(fuzzy)
    edited_paths = paths(edited_indices)
    filename_path = relative_path(filename_index, format_for(filename_index), filename_index, tokens["filename"])

    queries = [
        {
            "id": "alpha",
            "query": tokens["alpha"],
            "mode": "exact",
            "category": "strict",
            "expected": alpha_paths,
        },
        {
            "id": "beta",
            "query": tokens["beta"],
            "mode": "exact",
            "category": "strict",
            "expected": beta_paths,
        },
        {
            "id": "boolean_alpha_not_beta",
            "query": f"{tokens['alpha']} -{tokens['beta']}",
            "mode": "exact",
            "category": "strict",
            "expected": sorted(set(alpha_paths) - set(beta_paths)),
        },
        {
            "id": "or_alpha_gamma",
            "query": f"{tokens['alpha']} OR {tokens['gamma']}",
            "mode": "exact",
            "category": "strict",
            "expected": sorted(set(alpha_paths) | set(gamma_paths)),
        },
        {
            "id": "phrase",
            "query": f'"{tokens["phrase_one"]} {tokens["phrase_two"]}"',
            "mode": "exact",
            "category": "strict",
            "expected": phrase_paths,
        },
        {
            "id": "chinese_encodings",
            "query": tokens["chinese"],
            "mode": "exact",
            "category": "strict",
            "expected": chinese_paths,
        },
        {
            "id": "code_identifier",
            "query": tokens["code"],
            "mode": "exact",
            "category": "strict",
            "expected": code_paths,
        },
        {
            "id": "filename_only",
            "query": tokens["filename"],
            "mode": "exact",
            "category": "strict",
            "expected": [filename_path],
        },
        {
            "id": "docx_body",
            "query": tokens["docx"],
            "mode": "exact",
            "category": "strict",
            "expected": docx_paths,
        },
        {
            "id": "pdf_page_two",
            "query": tokens["pdf"],
            "mode": "exact",
            "category": "strict",
            "expected": pdf_paths,
            "expected_pages": {path: 2 for path in pdf_paths},
        },
        {
            "id": "absent",
            "query": tokens["absent"],
            "mode": "exact",
            "category": "strict_negative",
            "expected": [],
        },
        {
            "id": "fuzzy_typo_as_strict",
            "query": fuzzy_typo,
            "mode": "exact",
            "category": "strict_negative",
            "expected": [],
        },
        {
            "id": "fuzzy_typo",
            "query": fuzzy_typo,
            "mode": "fuzzy",
            "category": "fuzzy",
            "expected": fuzzy_paths,
        },
    ]

    manifest = {
        "schema": BENCH_SCHEMA,
        "generator": "scripts/benchmark.py",
        "seed": seed,
        "files": files,
        "corpus": {
            "path": str(corpus),
            "files": files,
            "bytes": total_bytes,
            "digest": digest.hexdigest(),
            "formats": format_stats,
        },
        "tokens": tokens,
        "fuzzy_typo": fuzzy_typo,
        "filename_index": filename_index,
        "edited_files": edited_paths,
        "edited_token": tokens["edited"],
        "queries": queries,
        "files_list": manifest_files,
        "path_formats": path_formats,
    }

    manifest_path = workdir / "manifest.json"
    dump_json(manifest_path, {key: value for key, value in manifest.items() if key != "path_formats"}, pretty=files <= 5000)
    return manifest, path_formats


def dump_json(path: Path, value, pretty: bool = True) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        if pretty:
            json.dump(value, handle, ensure_ascii=False, indent=2)
        else:
            json.dump(value, handle, ensure_ascii=False, separators=(",", ":"))
        handle.write("\n")


def gnu_time() -> str | None:
    for candidate in ("/usr/bin/time", "/bin/time"):
        if not Path(candidate).exists():
            continue
        try:
            result = subprocess.run(
                [candidate, "--version"], capture_output=True, text=True, timeout=5
            )
        except (OSError, subprocess.SubprocessError):
            continue
        if "GNU" in result.stdout:
            return candidate
    return None


def run_measured(cmd: list[str], env: dict[str, str]) -> dict:
    timer = gnu_time()
    started = time.perf_counter()
    if timer:
        handle, name = tempfile.mkstemp(prefix="indexfox-bench-time-")
        os.close(handle)
        try:
            result = subprocess.run(
                [timer, "-f", "%e %M", "-o", name, "--"] + cmd,
                capture_output=True,
                text=True,
                env=env,
            )
            wall_s = time.perf_counter() - started
            rss_kb = None
            try:
                lines = Path(name).read_text(encoding="utf-8").strip().splitlines()
                if lines:
                    parts = lines[-1].split()
                    if len(parts) >= 2:
                        rss_kb = int(parts[-1])
            except (OSError, ValueError):
                pass
            return {
                "wall_s": wall_s,
                "rss_kb": rss_kb,
                "returncode": result.returncode,
                "stdout": result.stdout,
                "stderr": result.stderr,
            }
        finally:
            try:
                os.unlink(name)
            except OSError:
                pass

    process = subprocess.Popen(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env
    )
    peak_kb = 0
    while process.poll() is None:
        try:
            status = Path(f"/proc/{process.pid}/status").read_text(encoding="utf-8")
            for line in status.splitlines():
                if line.startswith(("VmHWM:", "VmRSS:")):
                    value = int(line.split()[1])
                    peak_kb = max(peak_kb, value)
        except (OSError, ValueError):
            pass
        time.sleep(0.02)
    stdout, stderr = process.communicate()
    return {
        "wall_s": time.perf_counter() - started,
        "rss_kb": peak_kb or None,
        "returncode": process.returncode,
        "stdout": stdout,
        "stderr": stderr,
    }


INDEX_FIELDS = {
    "scanned": "scanned",
    "indexed": "indexed",
    "unchanged": "unchanged",
    "skipped (size)": "skipped_size",
    "failed": "failed",
    "removed": "removed",
    "incomplete scan": "incomplete_scan",
    "elapsed": "elapsed_ms",
}


def parse_index_report(stdout: str) -> dict:
    report: dict = {"issues": []}
    for line in stdout.splitlines():
        if line.startswith("issue:"):
            report["issues"].append(line[len("issue:"):].strip())
            continue
        match = re.match(r"^(scanned|indexed|unchanged|skipped \(size\)|failed|removed|incomplete scan|elapsed)\s+(.+)$", line)
        if not match:
            continue
        key = INDEX_FIELDS[match.group(1)]
        value = match.group(2).strip()
        if key == "incomplete_scan":
            report[key] = value.lower() == "true"
        else:
            try:
                report[key] = int(value)
            except ValueError:
                match_ms = re.match(r"^(\d+)\s*ms$", value)
                report[key] = int(match_ms.group(1)) if match_ms else value
    return report


def db_metrics(db: Path, corpus: Path, path_formats: dict[str, str]) -> dict:
    connection = sqlite3.connect(str(db))
    try:
        connection.execute("PRAGMA query_only = ON")
        scalar = lambda sql: connection.execute(sql).fetchone()[0]
        page_size = int(scalar("PRAGMA page_size"))
        page_count = int(scalar("PRAGMA page_count"))
        chunks = int(scalar("SELECT count(*) FROM chunks"))
        short_grams = int(scalar("SELECT count(*) FROM short_grams"))
        text_bytes = int(scalar("SELECT COALESCE(SUM(LENGTH(CAST(text AS BLOB))), 0) FROM chunks"))
        files = int(scalar("SELECT count(*) FROM files"))
        states = {
            state: int(count)
            for state, count in connection.execute(
                "SELECT state, count(*) FROM files GROUP BY state"
            )
        }
        file_rows = list(
            connection.execute("SELECT id, display_path, state, size_bytes FROM files")
        )
        text_by_file = {
            file_id: int(value or 0)
            for file_id, value in connection.execute(
                "SELECT file_id, COALESCE(SUM(LENGTH(CAST(text AS BLOB))), 0) "
                "FROM chunks GROUP BY file_id"
            )
        }
        ext_rows = [
            {"ext": ext, "files": int(count), "bytes": int(total or 0)}
            for ext, count, total in connection.execute(
                "SELECT ext, count(*), COALESCE(SUM(size_bytes), 0) FROM files GROUP BY ext ORDER BY ext"
            )
        ]
    finally:
        connection.close()

    formats: dict[str, dict] = {}
    for fmt in FORMAT_ORDER:
        formats[fmt] = {
            "files": 0,
            "bytes": 0,
            "indexed": 0,
            "failed": 0,
            "stale": 0,
            "pending": 0,
            "skipped_size": 0,
            "text_bytes": 0,
        }
    for file_id, display_path, state, size_bytes in file_rows:
        relative = Path(os.path.relpath(display_path, corpus)).as_posix()
        fmt = path_formats.get(relative)
        if fmt is None:
            continue
        entry = formats[fmt]
        entry["files"] += 1
        entry["bytes"] += int(size_bytes)
        if state in entry:
            entry[state] += 1
        entry["text_bytes"] += text_by_file.get(file_id, 0)

    db_disk_bytes = 0
    sidecars = {}
    for suffix in ("", "-wal", "-shm"):
        candidate = Path(str(db) + suffix)
        size = candidate.stat().st_size if candidate.exists() else 0
        sidecars[suffix or "main"] = size
        db_disk_bytes += size

    return {
        "main_bytes": sidecars["main"],
        "wal_bytes": sidecars["-wal"],
        "shm_bytes": sidecars["-shm"],
        "disk_bytes": db_disk_bytes,
        "logical_bytes": page_size * page_count,
        "page_size": page_size,
        "page_count": page_count,
        "files": files,
        "states": states,
        "chunks": chunks,
        "text_bytes": text_bytes,
        "short_grams": short_grams,
        "ext": ext_rows,
        "formats": formats,
    }


def result_paths(response: dict, corpus: Path) -> list[str]:
    paths = []
    for result in response.get("results", []):
        relative = Path(os.path.relpath(result["path"], corpus)).as_posix()
        paths.append(relative)
    return sorted(set(paths))


def percentile(values: list[float], fraction: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, math.ceil(fraction * len(ordered)) - 1))
    return ordered[index]


def round_ms(value: float | None) -> float | None:
    return None if value is None else round(value, 3)


def latency_summary(samples: list[dict]) -> dict:
    engine = [sample["engine_ms"] for sample in samples if sample.get("engine_ms") is not None]
    wall = [sample["wall_ms"] for sample in samples if sample.get("wall_ms") is not None]
    return {
        "samples": len(samples),
        "p50_engine_ms": round_ms(percentile(engine, 0.50)),
        "p95_engine_ms": round_ms(percentile(engine, 0.95)),
        "mean_engine_ms": round_ms(sum(engine) / len(engine)) if engine else None,
        "p50_wall_ms": round_ms(percentile(wall, 0.50)),
        "p95_wall_ms": round_ms(percentile(wall, 0.95)),
        "mean_wall_ms": round_ms(sum(wall) / len(wall)) if wall else None,
    }


def score_query(record: dict) -> dict:
    expected = set(record["expected"])
    returned = set(record.get("returned_paths", []))
    true_positive = len(expected & returned)
    precision = true_positive / len(returned) if returned else (1.0 if not expected else 0.0)
    recall = true_positive / len(expected) if expected else (1.0 if not returned else 0.0)
    record["matched"] = true_positive
    record["precision"] = round(precision, 6)
    record["recall"] = round(recall, 6)
    f1_denominator = precision + recall
    record["f1"] = round(2 * precision * recall / f1_denominator, 6) if f1_denominator else 0.0
    record["missing_paths"] = sorted(expected - returned)
    record["unexpected_paths"] = sorted(returned - expected)

    expected_pages = record.get("expected_pages")
    if expected_pages:
        page_ok = 0
        for result in record.get("raw_results", []):
            relative = record["_rel"](result["path"])
            wanted = expected_pages.get(relative)
            if wanted is None:
                continue
            locators = [hit.get("locator") or {} for hit in result.get("hits", [])]
            if any(
                locator.get("kind") == "pdf_page" and locator.get("page") == wanted
                for locator in locators
            ):
                page_ok += 1
        record["page_matched"] = page_ok
        record["page_accuracy"] = round(page_ok / len(expected_pages), 6)
    return record


def aggregate_queries(records: list[dict]) -> dict | None:
    if not records:
        return None
    true_positive = sum(record["matched"] for record in records)
    returned = sum(len(record.get("returned_paths", [])) for record in records)
    expected = sum(len(record["expected"]) for record in records)
    precision = true_positive / returned if returned else (1.0 if expected == 0 else 0.0)
    recall = true_positive / expected if expected else (1.0 if returned == 0 else 0.0)
    f1_denominator = precision + recall
    return {
        "queries": len(records),
        "true_positive": true_positive,
        "returned": returned,
        "expected": expected,
        "micro_precision": round(precision, 6),
        "micro_recall": round(recall, 6),
        "micro_f1": round(2 * precision * recall / f1_denominator, 6) if f1_denominator else 0.0,
        "mean_precision": round(sum(r["precision"] for r in records) / len(records), 6),
        "mean_recall": round(sum(r["recall"] for r in records) / len(records), 6),
    }


def search_once(
    binary: str,
    db: Path,
    query: str,
    mode: str,
    limit: int,
    env: dict[str, str],
) -> dict:
    started = time.perf_counter()
    result = subprocess.run(
        [
            binary,
            "--db",
            str(db),
            "search",
            query,
            "--mode",
            mode,
            "--limit",
            str(limit),
            "--json",
        ],
        capture_output=True,
        text=True,
        env=env,
    )
    wall_ms = (time.perf_counter() - started) * 1000
    if result.returncode != 0:
        combined = (result.stderr + result.stdout).lower()
        unsupported = mode == "fuzzy" and "fuzzy" in combined and (
            "not implemented" in combined or "planned" in combined or "unsupported" in combined
        )
        return {
            "status": "unsupported" if unsupported else "error",
            "wall_ms": wall_ms,
            "engine_ms": None,
            "error": (result.stderr or result.stdout).strip()[-1000:],
        }
    try:
        response = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        return {
            "status": "error",
            "wall_ms": wall_ms,
            "engine_ms": None,
            "error": f"invalid search JSON: {error}: {result.stdout[:300]}",
        }
    return {
        "status": "ok",
        "wall_ms": wall_ms,
        "engine_ms": response.get("elapsed_ms"),
        "response": response,
    }


def run_queries(
    binary: str,
    db: Path,
    corpus: Path,
    queries: list[dict],
    repeats: int,
    warmup: int,
    limit: int,
    env: dict[str, str],
) -> list[dict]:
    records: list[dict] = []
    for query in queries:
        record = dict(query)
        record["samples"] = []
        record["returned_paths"] = []
        record["raw_results"] = []
        record["status"] = "error"
        record["_rel"] = lambda path: Path(os.path.relpath(path, corpus)).as_posix()
        for _ in range(max(0, warmup)):
            search_once(binary, db, query["query"], query["mode"], limit, env)
        last_ok = None
        for _ in range(max(1, repeats)):
            outcome = search_once(binary, db, query["query"], query["mode"], limit, env)
            record["status"] = outcome["status"]
            record["error"] = outcome.get("error")
            if outcome["status"] == "ok":
                last_ok = outcome
                record["samples"].append(
                    {"engine_ms": outcome["engine_ms"], "wall_ms": round(outcome["wall_ms"], 3)}
                )
            if outcome["status"] == "unsupported":
                break
        if last_ok is not None:
            response = last_ok["response"]
            record["returned_paths"] = result_paths(response, corpus)
            record["raw_results"] = response.get("results", [])
            record["total_files"] = response.get("total_files")
            record["total_hits"] = response.get("total_hits")
            record["query_plan"] = response.get("query_plan")
        record["latency"] = latency_summary(record["samples"])
        score_query(record)
        record.pop("raw_results", None)
        record.pop("_rel", None)
        record.pop("expected_pages", None)
        records.append(record)
    return records


def accuracy_report(queries: list[dict]) -> dict:
    strict_positive = [
        query for query in queries if query["category"] == "strict" and query["status"] == "ok"
    ]
    strict_negative = [
        query
        for query in queries
        if query["category"] == "strict_negative" and query["status"] == "ok"
    ]
    fuzzy = [query for query in queries if query["category"] == "fuzzy"]
    incremental = [
        query for query in queries if query["category"] == "incremental" and query["status"] == "ok"
    ]

    pdf_queries = [
        query for query in queries if query["category"] == "strict" and "page_accuracy" in query
    ]
    page_matched = sum(query.get("page_matched", 0) for query in pdf_queries)
    page_expected = sum(len(query.get("expected", [])) for query in pdf_queries)

    fuzzy_ok = [query for query in fuzzy if query["status"] == "ok"]
    fuzzy_status = "unsupported" if fuzzy and all(query["status"] == "unsupported" for query in fuzzy) else (
        "measured" if fuzzy_ok else "unavailable"
    )
    report = {
        "strict": aggregate_queries(strict_positive),
        "strict_negative": {
            "queries": len(strict_negative),
            "clean": sum(1 for query in strict_negative if not query["returned_paths"]),
            "false_positives": sum(len(query["returned_paths"]) for query in strict_negative),
        },
        "fuzzy": {
            "status": fuzzy_status,
            "aggregate": aggregate_queries(fuzzy_ok),
            "queries": len(fuzzy),
            "unsupported": sum(1 for query in fuzzy if query["status"] == "unsupported"),
        },
        "incremental": aggregate_queries(incremental),
        "pdf_pages": {
            "expected": page_expected,
            "matched": page_matched,
            "accuracy": round(page_matched / page_expected, 6) if page_expected else None,
        },
    }
    return report


def hardware_info(binary: str, workdir: Path) -> dict:
    uname = platform.uname()
    cpu = ""
    try:
        for line in Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="replace").splitlines():
            if line.lower().startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    memory_kb = None
    try:
        for line in Path("/proc/meminfo").read_text(encoding="utf-8").splitlines():
            if line.startswith("MemTotal:"):
                memory_kb = int(line.split()[1])
                break
    except (OSError, ValueError):
        pass
    try:
        disk = shutil.disk_usage(workdir)
        disk_info = {"total_bytes": disk.total, "free_bytes": disk.free}
    except OSError:
        disk_info = {}
    try:
        version = subprocess.run(
            [binary, "--version"], capture_output=True, text=True, timeout=10
        ).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        version = "unknown"
    digest = hashlib.sha256()
    try:
        with open(binary, "rb") as handle:
            for block in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(block)
        binary_sha256 = digest.hexdigest()
    except OSError:
        binary_sha256 = None
    return {
        "hostname": uname.node,
        "platform": platform.platform(),
        "kernel": uname.release,
        "machine": uname.machine,
        "wsl": "microsoft" in uname.release.lower(),
        "cpu": cpu or uname.processor,
        "logical_cpus": os.cpu_count(),
        "memory_kb": memory_kb,
        "load_average": list(os.getloadavg()) if hasattr(os, "getloadavg") else None,
        "python": sys.version.split()[0],
        "disk": disk_info,
        "binary": {
            "path": str(Path(binary).resolve()),
            "version": version,
            "sha256": binary_sha256,
            "profile": "release" if "/release/" in str(Path(binary).resolve()) else "debug",
        },
        "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }


def detect_pdfium() -> str | None:
    configured = os.environ.get("PDFIUM_LIB_PATH")
    if configured:
        return configured
    candidate = Path(__file__).resolve().parent.parent / ".deps/pdfium/lib/libpdfium.so"
    return str(candidate) if candidate.exists() else None


def tesseract_languages(executable: str) -> list[str]:
    try:
        result = subprocess.run(
            [executable, "--list-langs"], capture_output=True, text=True, timeout=15
        )
    except (OSError, subprocess.SubprocessError):
        return []
    languages = []
    for line in result.stdout.splitlines():
        line = line.strip()
        if line and not line.lower().startswith("list of available"):
            languages.append(line)
    return languages


def pdf_page_count(path: Path) -> int:
    data = path.read_bytes()
    return len(re.findall(rb"/Type\s*/Page\b", data))


def run_ocr_sample(
    args, workdir: Path, binary: str, env: dict[str, str], pdfium: str | None
) -> dict:
    if args.skip_ocr:
        return {"status": "skipped", "reason": "--skip-ocr"}
    sample = Path(args.ocr_sample)
    if not sample.exists():
        return {"status": "unavailable", "reason": f"sample not found: {sample}"}
    tesseract = os.environ.get("TESSERACT_PATH") or shutil.which("tesseract")
    if not tesseract or not Path(tesseract).exists():
        return {"status": "unavailable", "reason": "tesseract executable not found"}
    if not pdfium:
        return {"status": "unavailable", "reason": "PDFium library not found"}
    languages = tesseract_languages(tesseract)
    requested = args.ocr_lang
    if "chi_sim" in requested and "chi_sim" not in languages:
        return {
            "status": "unavailable",
            "reason": f"chi_sim model missing; available={languages}",
            "languages": languages,
        }
    pages = pdf_page_count(sample)
    if pages <= 0:
        return {"status": "unavailable", "reason": f"cannot determine PDF pages: {sample}"}

    root = workdir / "ocr" / "corpus"
    root.mkdir(parents=True, exist_ok=True)
    copied = root / sample.name
    shutil.copyfile(sample, copied)

    runs = []
    errors = []
    last_db = None
    for repeat in range(max(1, args.ocr_repeats)):
        db = workdir / "ocr" / f"index-{repeat}.db"
        for suffix in ("", "-wal", "-shm"):
            try:
                Path(str(db) + suffix).unlink()
            except OSError:
                pass
        command = [
            binary,
            "--db",
            str(db),
            "index",
            str(root),
            "--max-size-mb",
            str(args.max_size_mb),
            "--ocr",
            "--tesseract",
            str(tesseract),
            "--lang",
            requested,
        ]
        measured = run_measured(command, env)
        if measured["returncode"] != 0:
            errors.append(measured["stderr"].strip()[-1000:])
            continue
        report = parse_index_report(measured["stdout"])
        total_ms = measured["wall_s"] * 1000
        runs.append(
            {
                "wall_ms": round(total_ms, 3),
                "per_page_ms": round(total_ms / pages, 3),
                "rss_kb": measured["rss_kb"],
                "report": report,
            }
        )
        last_db = db

    result = {
        "status": "measured" if runs else "failed",
        "sample": str(sample),
        "sample_bytes": sample.stat().st_size,
        "pages": pages,
        "languages_available": languages,
        "lang": requested,
        "tesseract": str(tesseract),
        "pdfium": pdfium,
        "repeats": len(runs),
        "runs": runs,
        "errors": errors,
    }
    if runs:
        per_page = [run["per_page_ms"] for run in runs]
        result["per_page_p50_ms"] = round_ms(percentile(per_page, 0.50))
        result["per_page_p95_ms"] = round_ms(percentile(per_page, 0.95))
    if last_db is not None:
        try:
            metrics = db_metrics(last_db, root, {copied.name: "pdf"})
            result["db"] = {
                key: metrics[key]
                for key in ("logical_bytes", "disk_bytes", "text_bytes", "chunks", "states")
            }
            checks = []
            for query in ("内核驱动", "VIDIOC_STREAMON"):
                outcome = search_once(binary, last_db, query, "substring", 20, env)
                if outcome["status"] != "ok":
                    checks.append({"query": query, "status": outcome["status"], "error": outcome.get("error")})
                    continue
                response = outcome["response"]
                ocr_hits = sum(
                    1
                    for file in response.get("results", [])
                    for hit in file.get("hits", [])
                    if hit.get("origin") == "ocr"
                )
                checks.append(
                    {
                        "query": query,
                        "status": "ok",
                        "files": response.get("total_files"),
                        "ocr_hits": ocr_hits,
                    }
                )
            result["checks"] = checks
        except (sqlite3.Error, OSError) as error:
            result["metrics_error"] = str(error)
    return result


def ensure_workdir(workdir: Path) -> None:
    workdir.mkdir(parents=True, exist_ok=True)
    for name in ("corpus", "index.db", "manifest.json", "ocr"):
        candidate = workdir / name
        if candidate.exists():
            if candidate.is_dir() and not any(candidate.iterdir()):
                continue
            raise BenchError(
                f"workdir is not empty ({candidate}); use a new directory or omit --workdir"
            )


def select_binary(requested: str | None) -> str:
    if requested:
        path = Path(requested)
        if not path.exists():
            raise BenchError(f"CLI binary not found: {path}")
        return str(path.resolve())
    env = os.environ.get("INDEXFOX_CLI")
    if env:
        if not Path(env).exists():
            raise BenchError(f"INDEXFOX_CLI not found: {env}")
        return str(Path(env).resolve())
    root = Path(__file__).resolve().parent.parent
    release = root / "target/release/indexfox-cli"
    debug = root / "target/debug/indexfox-cli"
    if release.exists():
        return str(release)
    if debug.exists():
        print(f"warning: release CLI missing; using {debug}", file=sys.stderr)
        return str(debug)
    raise BenchError("no CLI binary; build target/release/indexfox-cli or pass --bin")


def run_one(args, files: int, base_workdir: Path | None) -> dict:
    if base_workdir is None:
        workdir = Path(tempfile.mkdtemp(prefix=f"indexfox-bench-{files}-"))
        cleanup = True
    else:
        workdir = base_workdir
        cleanup = False
    ensure_workdir(workdir)
    try:
        print(f"[{files}] generating deterministic corpus in {workdir}", flush=True)
        manifest, path_formats = generate_corpus(workdir, files, args.seed)
        corpus = workdir / "corpus"
        corpus_info = manifest["corpus"]

        run: dict = {
            "schema": BENCH_SCHEMA,
            "scale": files,
            "workdir": str(workdir),
            "manifest": str(workdir / "manifest.json") if (not cleanup or args.keep) else None,
            "corpus": corpus_info,
            "queries_truth": manifest["queries"],
            "edited_files": manifest["edited_files"],
            "edited_token": manifest["edited_token"],
        }

        if args.generate_only:
            run["generated_only"] = True
            run["hardware"] = hardware_info(args.binary, workdir)
            print(
                f"[{files}] generated {files} files, {corpus_info['bytes']} bytes, "
                f"digest={corpus_info['digest']}",
                flush=True,
            )
            return run

        binary = args.binary
        pdfium = detect_pdfium()
        env = os.environ.copy()
        env["LC_ALL"] = "C"
        if pdfium and "PDFIUM_LIB_PATH" not in env:
            env["PDFIUM_LIB_PATH"] = pdfium

        run["hardware"] = hardware_info(binary, workdir)
        run["pdfium"] = pdfium
        db = workdir / "index.db"

        base_command = [
            binary,
            "--db",
            str(db),
            "index",
            str(corpus),
            "--max-size-mb",
            str(args.max_size_mb),
        ]
        print(f"[{files}] full index", flush=True)
        full = run_measured(base_command, env)
        if full["returncode"] != 0:
            raise BenchError(f"full index failed: {full['stderr'].strip()[-2000:]}")
        full_report = parse_index_report(full["stdout"])
        run["index"] = {
            "full": {
                "wall_s": round(full["wall_s"], 3),
                "rss_kb": full["rss_kb"],
                **full_report,
            }
        }

        print(f"[{files}] full index done in {full['wall_s']:.2f}s", flush=True)
        run["index"]["db_after_full"] = db_metrics(db, corpus, path_formats)

        print(f"[{files}] queries (repeats={args.repeats}, warmup={args.warmup})", flush=True)
        query_records = run_queries(
            binary,
            db,
            corpus,
            manifest["queries"],
            args.repeats,
            args.warmup,
            args.limit,
            env,
        )

        print(f"[{files}] no-op index", flush=True)
        noop = run_measured(base_command, env)
        if noop["returncode"] != 0:
            raise BenchError(f"no-op index failed: {noop['stderr'].strip()[-2000:]}")
        run["index"]["noop"] = {
            "wall_s": round(noop["wall_s"], 3),
            "rss_kb": noop["rss_kb"],
            **parse_index_report(noop["stdout"]),
        }

        print(f"[{files}] editing {len(manifest['edited_files'])} files", flush=True)
        edited_paths = manifest["edited_files"]
        edit_line = f"\n{manifest['edited_token']}\n"
        for relative in edited_paths:
            rel_path = corpus / relative
            fmt = path_formats[relative]
            with rel_path.open("ab") as handle:
                handle.write(encode_text(edit_line, fmt))
        edit_query = {
            "id": "incremental_edit",
            "query": manifest["edited_token"],
            "mode": "exact",
            "category": "incremental",
            "expected": edited_paths,
        }
        incremental = run_measured(base_command, env)
        if incremental["returncode"] != 0:
            raise BenchError(
                f"incremental index failed: {incremental['stderr'].strip()[-2000:]}"
            )
        run["index"]["incremental"] = {
            "wall_s": round(incremental["wall_s"], 3),
            "rss_kb": incremental["rss_kb"],
            "edited_files": len(edited_paths),
            **parse_index_report(incremental["stdout"]),
        }
        run["index"]["db_after_incremental"] = db_metrics(db, corpus, path_formats)

        edit_records = run_queries(
            binary, db, corpus, [edit_query], max(1, args.repeats), args.warmup, args.limit, env
        )
        query_records.extend(edit_records)

        run["queries"] = query_records
        run["accuracy"] = accuracy_report(query_records)
        run["latency"] = {
            "strict": latency_summary(
                [sample for query in query_records if query["category"].startswith("strict") for sample in query["samples"]]
            ),
            "fuzzy": latency_summary(
                [sample for query in query_records if query["category"] == "fuzzy" for sample in query["samples"]]
            ),
            "incremental": latency_summary(
                [sample for query in query_records if query["category"] == "incremental" for sample in query["samples"]]
            ),
        }
        run["formats"] = run["index"]["db_after_incremental"]["formats"]

        print(f"[{files}] OCR sample", flush=True)
        run["ocr"] = run_ocr_sample(args, workdir, binary, env, pdfium)
        return run
    finally:
        if cleanup and not args.keep:
            shutil.rmtree(workdir, ignore_errors=True)
        else:
            print(f"[{files}] kept workdir {workdir}", flush=True)


def print_run(run: dict) -> None:
    if run.get("generated_only"):
        corpus = run["corpus"]
        print(f"generated files={run['scale']} bytes={corpus['bytes']} digest={corpus['digest']}")
        return
    corpus = run["corpus"]
    index = run["index"]
    db = index["db_after_incremental"]
    accuracy = run["accuracy"]
    strict = accuracy["strict"] or {}
    fuzzy = accuracy["fuzzy"]
    negatives = accuracy["strict_negative"]
    print("")
    print(f"scale              {run['scale']} files")
    print(f"corpus bytes       {corpus['bytes']}")
    print(f"corpus digest      {corpus['digest']}")
    print(
        f"full index         {index['full']['wall_s']} s, "
        f"peak RSS {index['full']['rss_kb']} kB, indexed={index['full'].get('indexed')}, "
        f"failed={index['full'].get('failed')}"
    )
    print(
        f"no-op index        {index['noop']['wall_s']} s, "
        f"unchanged={index['noop'].get('unchanged')}, failed={index['noop'].get('failed')}"
    )
    print(
        f"incremental        {index['incremental']['wall_s']} s for "
        f"{index['incremental']['edited_files']} edited, "
        f"indexed={index['incremental'].get('indexed')}, removed={index['incremental'].get('removed')}"
    )
    print(
        f"db                 logical={db['logical_bytes']} B disk={db['disk_bytes']} B "
        f"text={db['text_bytes']} B chunks={db['chunks']} grams={db['short_grams']}"
    )
    if strict:
        print(
            f"strict P/R/F1      {strict['micro_precision']:.4f} / "
            f"{strict['micro_recall']:.4f} / {strict['micro_f1']:.4f} "
            f"({strict['queries']} queries)"
        )
    print(
        f"strict negatives   {negatives['clean']}/{negatives['queries']} clean, "
        f"{negatives['false_positives']} false positives"
    )
    if fuzzy["status"] == "measured":
        agg = fuzzy["aggregate"]
        print(
            f"fuzzy P/R/F1       {agg['micro_precision']:.4f} / {agg['micro_recall']:.4f} / "
            f"{agg['micro_f1']:.4f}"
        )
    else:
        print(f"fuzzy              {fuzzy['status']} ({fuzzy['unsupported']} unsupported)")
    if accuracy["incremental"]:
        inc = accuracy["incremental"]
        print(
            f"incremental P/R    {inc['micro_precision']:.4f} / {inc['micro_recall']:.4f}"
        )
    if accuracy["pdf_pages"]["expected"]:
        print(
            f"pdf page accuracy  {accuracy['pdf_pages']['matched']}/"
            f"{accuracy['pdf_pages']['expected']}"
        )
    for category in ("strict", "fuzzy", "incremental"):
        latency = run["latency"][category]
        if latency["samples"]:
            print(
                f"{category:<18} p50/p95 engine {latency['p50_engine_ms']}/{latency['p95_engine_ms']} ms, "
                f"wall {latency['p50_wall_ms']}/{latency['p95_wall_ms']} ms"
            )
    print(f"ocr                {run['ocr']['status']}"
          + (f": {run['ocr'].get('reason')}" if run['ocr'].get("reason") else ""))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--files", default="10000", type=parse_sizes, help="file counts, comma separated: 10000,100000 (default 10000)")
    parser.add_argument("--seed", default=4606, type=int, help="deterministic corpus seed (default 4606)")
    parser.add_argument("--bin", default=None, help="indexfox-cli path (default release, then debug)")
    parser.add_argument("--workdir", default=None, help="empty work directory; default isolated temp dir")
    parser.add_argument("--keep", action="store_true", help="keep the temporary work directory")
    parser.add_argument("--generate-only", action="store_true", help="generate corpus/manifest and stop")
    parser.add_argument("--repeats", default=5, type=int, help="measured search runs per query (default 5)")
    parser.add_argument("--warmup", default=1, type=int, help="unmeasured search runs per query (default 1)")
    parser.add_argument("--limit", default=1_000_000, type=int, help="search result limit (default 1000000)")
    parser.add_argument("--max-size-mb", default=100, type=int, help="index size limit (default 100)")
    parser.add_argument("--skip-ocr", action="store_true", help="skip the separate OCR sample")
    parser.add_argument("--ocr-sample", default=str(Path(__file__).resolve().parent.parent / "fixtures/pdf/scanned_zh.pdf"), help="synthetic scanned PDF")
    parser.add_argument("--ocr-lang", default=os.environ.get("INDEXFOX_OCR_LANG", "chi_sim+eng"), help="Tesseract languages")
    parser.add_argument("--ocr-repeats", default=3, type=int, help="separate OCR index runs (default 3)")
    parser.add_argument("--json-out", default=None, help="write full machine-readable results here")
    args = parser.parse_args()

    try:
        args.binary = select_binary(args.bin)
        base_workdir = Path(args.workdir).resolve() if args.workdir else None
        if base_workdir is not None and len(args.files) > 1:
            runs = []
            for files in args.files:
                runs.append(run_one(args, files, base_workdir / f"n{files}"))
        else:
            runs = [run_one(args, files, base_workdir) for files in args.files]
    except (BenchError, OSError, sqlite3.Error, subprocess.SubprocessError) as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        return 1

    document = {
        "schema": BENCH_SCHEMA,
        "generator": "scripts/benchmark.py",
        "argv": sys.argv,
        "seed": args.seed,
        "runs": runs,
    }
    if args.json_out:
        dump_json(Path(args.json_out), document, pretty=True)
        print(f"wrote {args.json_out}")
    for run in runs:
        print_run(run)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
