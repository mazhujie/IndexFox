use indexfox_core::{normalize_search_text, ExtractedDocument, Origin, Section, SourceLocator};

use crate::{ChunkOptions, EXTRACTOR_VERSION};

/// Decode source bytes using BOM, strict UTF-8, UTF-16 heuristic and GB18030
/// fallback (a superset of GBK). Returns the text, a label and whether the
/// decoder had to substitute characters.
pub fn decode_bytes(bytes: &[u8]) -> (String, String, bool) {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return (
            String::from_utf8_lossy(rest).into_owned(),
            "utf-8-bom".into(),
            false,
        );
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let (text, _, errors) = encoding_rs::UTF_16LE.decode(rest);
        return (text.into_owned(), "utf-16le-bom".into(), errors);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let (text, _, errors) = encoding_rs::UTF_16BE.decode(rest);
        return (text.into_owned(), "utf-16be-bom".into(), errors);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        let (bom_free, had_bom) = match text.strip_prefix('\u{FEFF}') {
            Some(stripped) => (stripped, true),
            None => (text, false),
        };
        let label = if had_bom { "utf-8-bom" } else { "utf-8" };
        return (bom_free.to_string(), label.to_string(), false);
    }

    // BOM-less UTF-16: NULs concentrated in one parity position.
    if bytes.len() >= 4 && bytes.len().is_multiple_of(2) {
        let even_nuls = bytes.iter().step_by(2).filter(|&&b| b == 0).count();
        let odd_nuls = bytes.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
        let half = bytes.len() / 2;
        if odd_nuls * 10 > half * 3 && even_nuls * 10 < half {
            let (text, _, errors) = encoding_rs::UTF_16LE.decode(bytes);
            return (text.into_owned(), "utf-16le".into(), errors);
        }
        if even_nuls * 10 > half * 3 && odd_nuls * 10 < half {
            let (text, _, errors) = encoding_rs::UTF_16BE.decode(bytes);
            return (text.into_owned(), "utf-16be".into(), errors);
        }
    }

    let (text, _, errors) = encoding_rs::GB18030.decode(bytes);
    let label = if errors { "gb18030-lossy" } else { "gb18030" };
    (text.into_owned(), label.to_string(), errors)
}

/// One chunk boundary with 1-based inclusive line numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSpan {
    pub start: usize,
    pub end: usize,
    pub start_line: u32,
    pub end_line: u32,
}

/// Split text at line boundaries where possible. There is deliberately no
/// overlap: cross-chunk matching concatenates adjacent chunks instead of
/// duplicating text and creating duplicate hits.
pub fn chunk_spans(text: &str, options: ChunkOptions) -> Vec<ChunkSpan> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut line = 1u32;

    while start < bytes.len() {
        let hard_end = (start + options.max_bytes).min(bytes.len());
        let mut end = hard_end;
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // A single character can exceed max_bytes only if max_bytes is tiny.
            end = text[start..]
                .char_indices()
                .nth(1)
                .map(|(offset, _)| start + offset)
                .unwrap_or(bytes.len());
        }

        if end < bytes.len() {
            let earliest_break = start + options.target_bytes.min(options.max_bytes) / 2;
            if let Some(relative) = text[start..end].rfind('\n') {
                let break_at = start + relative + 1;
                if break_at > earliest_break && text.is_char_boundary(break_at) {
                    end = break_at;
                }
            }
        }

        let slice = &text[start..end];
        let newlines = slice.as_bytes().iter().filter(|&&b| b == b'\n').count() as u32;
        // A trailing newline terminates the last content line instead of
        // opening another one, so it must not be counted in the inclusive
        // end line. The next chunk still starts on the following line.
        let end_line = if slice.ends_with('\n') {
            line + newlines - 1
        } else {
            line + newlines
        };
        spans.push(ChunkSpan {
            start,
            end,
            start_line: line,
            end_line,
        });
        line += newlines;
        start = end;
    }

    spans
}

pub fn extract_text_bytes(bytes: Vec<u8>, options: ChunkOptions) -> ExtractedDocument {
    let (text, encoding, _lossy) = decode_bytes(&bytes);
    let sections = chunk_spans(&text, options)
        .into_iter()
        .map(|span| {
            let display = text[span.start..span.end].to_string();
            Section {
                text: normalize_search_text(&display),
                display_text: display,
                locator: SourceLocator::Text {
                    start_line: span.start_line,
                    end_line: span.end_line,
                    start_offset: span.start,
                    end_offset: span.end,
                },
                origin: Origin::Native,
            }
        })
        .collect();
    ExtractedDocument {
        extractor_version: EXTRACTOR_VERSION.to_string(),
        encoding: Some(encoding),
        sections,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_bom_utf16_and_gb18030() {
        let (utf8, label, _) = decode_bytes("内核".as_bytes());
        assert_eq!((utf8.as_str(), label.as_str()), ("内核", "utf-8"));

        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "内核".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        let (decoded, label, _) = decode_bytes(&utf16);
        assert_eq!((decoded.as_str(), label.as_str()), ("内核", "utf-16le-bom"));

        let (gbk, _, _) = encoding_rs::GB18030.encode("Linux 内核驱动");
        let (decoded, label, _) = decode_bytes(&gbk);
        assert_eq!(decoded, "Linux 内核驱动");
        assert_eq!(label, "gb18030");
    }

    #[test]
    fn chunks_respect_max_bytes_and_line_numbers() {
        let mut text = String::new();
        for i in 0..2000 {
            text.push_str(&format!("line {i} 内核驱动 VIDIOC\n"));
        }
        let spans = chunk_spans(
            &text,
            ChunkOptions {
                target_bytes: 1024,
                max_bytes: 2048,
            },
        );
        assert!(spans.len() > 10);
        assert_eq!(spans[0].start_line, 1);
        assert_eq!(spans[0].start, 0);
        for pair in spans.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        let last = spans.last().unwrap();
        assert_eq!(last.end, text.len());
        for span in &spans {
            assert!(span.end - span.start <= 2048);
        }
    }

    #[test]
    fn no_chunks_for_empty_text() {
        assert!(chunk_spans("", ChunkOptions::default()).is_empty());
    }

    #[test]
    fn trailing_newline_does_not_add_an_empty_end_line() {
        let text = "one\ntwo\nthree\n";
        let spans = chunk_spans(text, ChunkOptions::default());
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start_line, spans[0].end_line), (1, 3));
        assert_eq!(spans[0].end, text.len());
    }

    #[test]
    fn next_chunk_starts_on_the_line_after_a_trailing_newline() {
        let text = "aa\nbb\ncc\n";
        let options = ChunkOptions {
            target_bytes: 4,
            max_bytes: 4,
        };
        let spans = chunk_spans(text, options);
        assert_eq!(
            spans
                .iter()
                .map(|span| (span.start_line, span.end_line))
                .collect::<Vec<_>>(),
            vec![(1, 1), (2, 2), (3, 3)]
        );
        for pair in spans.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
    }

    #[test]
    fn chunks_within_one_line_keep_the_same_line_number() {
        let text = "abcdefghij";
        let options = ChunkOptions {
            target_bytes: 4,
            max_bytes: 4,
        };
        let spans = chunk_spans(text, options);
        assert_eq!(spans.len(), 3);
        for span in &spans {
            assert_eq!((span.start_line, span.end_line), (1, 1));
        }
        for pair in spans.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        assert_eq!(spans.last().unwrap().end, text.len());
    }
}
