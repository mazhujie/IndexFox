use std::collections::HashSet;

use indexfox_core::normalize_search_text;
use rusqlite::{params, Transaction};

use crate::Result;

/// Bounded dictionary limits. They keep both per-chunk extraction and query
/// candidate work linear-ish on hostile input.
pub const MAX_LEXEME_CHARS: usize = 64;
pub const MAX_LEXEMES_PER_CHUNK: usize = 4096;
pub const MAX_GRAMS_PER_LEXEME: usize = 96;
pub const MAX_QUERY_TOKENS: usize = 16;
pub const MAX_CANDIDATE_LEXEMES: usize = 4096;
pub const CJK_WINDOW_MAX_CHARS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexemeEntry {
    pub text: String,
    pub length: usize,
    pub kind: &'static str,
    pub start: usize,
    pub end: usize,
}

fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// Han characters plus the common compatibility ranges. CJK extension planes
/// beyond the BMP are included even though they are rare in technical text.
fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2FA1F
    )
}

fn push_entry(
    entries: &mut Vec<LexemeEntry>,
    seen: &mut HashSet<(String, usize, usize)>,
    text: &str,
    start: usize,
    end: usize,
    kind: &'static str,
) {
    if entries.len() >= MAX_LEXEMES_PER_CHUNK || end <= start {
        return;
    }
    let length = text[start..end].chars().count();
    if length == 0 || length > MAX_LEXEME_CHARS {
        return;
    }
    let normalized = normalize_search_text(&text[start..end]);
    if seen.insert((normalized.clone(), start, end)) {
        entries.push(LexemeEntry {
            text: normalized,
            length,
            kind,
            start,
            end,
        });
    }
}

/// Identifier split points: `_`, camel-case, and letter/digit transitions.
/// The full token is indexed separately, so these are additional parts.
fn identifier_parts(word: &str) -> Vec<(usize, usize)> {
    let bytes = word.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    for index in 1..bytes.len() {
        let previous = bytes[index - 1] as char;
        let current = bytes[index] as char;
        let boundary = previous == '_'
            || current == '_'
            || (previous.is_ascii_lowercase() && current.is_ascii_uppercase())
            || (previous.is_ascii_alphabetic() && current.is_ascii_digit())
            || (previous.is_ascii_digit() && current.is_ascii_alphabetic());
        if boundary {
            if start < index {
                parts.push((start, index));
            }
            start = if current == '_' { index + 1 } else { index };
        }
    }
    if start < bytes.len() {
        parts.push((start, bytes.len()));
    }
    parts
}

fn push_word(
    entries: &mut Vec<LexemeEntry>,
    seen: &mut HashSet<(String, usize, usize)>,
    text: &str,
    start: usize,
    end: usize,
) {
    push_entry(entries, seen, text, start, end, "word");
    let word = &text[start..end];
    if word.chars().count() < 2 {
        return;
    }
    for (part_start, part_end) in identifier_parts(word) {
        if part_end - part_start < 2 {
            continue;
        }
        let part = &word[part_start..part_end];
        if !part.chars().any(|ch| ch.is_ascii_alphanumeric()) {
            continue;
        }
        push_entry(
            entries,
            seen,
            text,
            start + part_start,
            start + part_end,
            "word",
        );
    }
}

fn push_cjk(
    entries: &mut Vec<LexemeEntry>,
    seen: &mut HashSet<(String, usize, usize)>,
    text: &str,
    positions: &[(usize, char)],
    end: usize,
) {
    let start = positions[0].0;
    push_entry(entries, seen, text, start, end, "cjk");
    for window in 2..=CJK_WINDOW_MAX_CHARS.min(positions.len()) {
        for offset in 0..=positions.len() - window {
            let window_start = positions[offset].0;
            let window_end =
                positions[offset + window - 1].0 + positions[offset + window - 1].1.len_utf8();
            if window_start == start && window_end == end {
                continue;
            }
            push_entry(entries, seen, text, window_start, window_end, "cjk");
            if entries.len() >= MAX_LEXEMES_PER_CHUNK {
                return;
            }
        }
    }
}

/// Full English/code tokens, their identifier parts, and bounded Chinese
/// character windows for one chunk's display text. Offsets are byte offsets
/// into that text; normalization preserves byte offsets.
pub fn lexemes_for_text(text: &str) -> Vec<LexemeEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut chars = text.char_indices().peekable();

    while let Some((start, ch)) = chars.next() {
        if is_word_char(ch) {
            let mut end = start + ch.len_utf8();
            while let Some(&(index, next)) = chars.peek() {
                if !is_word_char(next) {
                    break;
                }
                end = index + next.len_utf8();
                chars.next();
            }
            push_word(&mut entries, &mut seen, text, start, end);
        } else if is_cjk(ch) {
            let mut positions = vec![(start, ch)];
            let mut end = start + ch.len_utf8();
            while let Some(&(index, next)) = chars.peek() {
                if !is_cjk(next) {
                    break;
                }
                positions.push((index, next));
                end = index + next.len_utf8();
                chars.next();
            }
            push_cjk(&mut entries, &mut seen, text, &positions, end);
        }
        if entries.len() >= MAX_LEXEMES_PER_CHUNK {
            break;
        }
    }
    entries
}

/// Tokens a fuzzy query literal may expand: whole ASCII/code runs containing
/// at least one letter, and CJK runs. Dictionary extraction still indexes
/// identifier parts, so a standalone part remains queryable; splitting query
/// parts here would fuzzy-expand numeric fragments of `name4606` into
/// unrelated numbers. Numeric and symbolic literals stay strict.
/// Quoted phrases and negated leaves never call this.
pub fn fuzzy_query_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut seen = HashSet::new();
    let mut chars = text.char_indices().peekable();

    while let Some((start, ch)) = chars.next() {
        if is_word_char(ch) {
            let mut end = start + ch.len_utf8();
            while let Some(&(index, next)) = chars.peek() {
                if !is_word_char(next) {
                    break;
                }
                end = index + next.len_utf8();
                chars.next();
            }
            let word = &text[start..end];
            if word.chars().any(|ch| ch.is_ascii_alphabetic()) {
                let token = normalize_search_text(word);
                if !token.is_empty() && seen.insert(token.clone()) {
                    tokens.push(token);
                }
            }
        } else if is_cjk(ch) {
            let mut end = start + ch.len_utf8();
            while let Some(&(index, next)) = chars.peek() {
                if !is_cjk(next) {
                    break;
                }
                end = index + next.len_utf8();
                chars.next();
            }
            let run = &text[start..end];
            let chars: Vec<char> = run.chars().collect();
            if chars.len() <= MAX_LEXEME_CHARS {
                let token = normalize_search_text(run);
                if seen.insert(token.clone()) {
                    tokens.push(token);
                }
            } else {
                for chunk in chars.chunks(MAX_LEXEME_CHARS) {
                    let token: String = chunk.iter().collect();
                    if seen.insert(token.clone()) {
                        tokens.push(token);
                    }
                }
            }
        }
        if tokens.len() >= MAX_QUERY_TOKENS {
            break;
        }
    }
    tokens.truncate(MAX_QUERY_TOKENS);
    tokens
}

/// Distinct character n-grams of a lexeme, lengths 1..=3, capped.
pub fn grams_for_lexeme(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut seen = HashSet::new();
    let mut grams = Vec::new();
    for length in 1..=3 {
        if chars.len() < length {
            break;
        }
        for window in chars.windows(length) {
            let gram: String = window.iter().collect();
            if seen.insert(gram.clone()) {
                grams.push(gram);
                if grams.len() >= MAX_GRAMS_PER_LEXEME {
                    return grams;
                }
            }
        }
    }
    grams
}

/// Distinct n-grams of exactly `length` characters, used as query tiers.
pub fn grams_of_len(text: &str, length: usize) -> Vec<String> {
    if length == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut seen = HashSet::new();
    let mut grams = Vec::new();
    if chars.len() < length {
        return grams;
    }
    for window in chars.windows(length) {
        let gram: String = window.iter().collect();
        if seen.insert(gram.clone()) {
            grams.push(gram);
            if grams.len() >= MAX_GRAMS_PER_LEXEME {
                break;
            }
        }
    }
    grams
}

/// Insert one chunk's dictionary rows. Must run in the same transaction as
/// the chunk insert/replace so a failed file write leaves no dictionary rows.
pub(crate) fn index_chunk(tx: &Transaction<'_>, chunk_id: i64, text: &str) -> Result<()> {
    let entries = lexemes_for_text(text);
    if entries.is_empty() {
        return Ok(());
    }
    let mut insert_lexeme =
        tx.prepare_cached("INSERT OR IGNORE INTO lexemes(text, length, kind) VALUES (?1, ?2, ?3)")?;
    let mut find_lexeme = tx.prepare_cached("SELECT id FROM lexemes WHERE text = ?1")?;
    let mut insert_gram =
        tx.prepare_cached("INSERT OR IGNORE INTO lexeme_grams(gram, lexeme_id) VALUES (?1, ?2)")?;
    let mut insert_occurrence = tx.prepare_cached(
        "INSERT OR IGNORE INTO lexeme_occurrences(lexeme_id, chunk_id, start, end)
         VALUES (?1, ?2, ?3, ?4)",
    )?;

    for entry in entries {
        let inserted =
            insert_lexeme.execute(params![entry.text, entry.length as i64, entry.kind])?;
        let lexeme_id = if inserted > 0 {
            let id = tx.last_insert_rowid();
            for gram in grams_for_lexeme(&entry.text) {
                insert_gram.execute(params![gram, id])?;
            }
            id
        } else {
            find_lexeme.query_row(params![entry.text], |row| row.get(0))?
        };
        insert_occurrence.execute(params![
            lexeme_id,
            chunk_id,
            entry.start as i64,
            entry.end as i64
        ])?;
    }
    Ok(())
}

/// Backfill the v3 dictionary from chunks committed by an older schema.
pub(crate) fn backfill(tx: &Transaction<'_>) -> Result<()> {
    let mut statement = tx.prepare("SELECT id, display_text FROM chunks ORDER BY id")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let text: String = row.get(1)?;
        index_chunk(tx, row.get(0)?, &text)?;
    }
    Ok(())
}

/// Delete dictionary words that are no longer referenced by any occurrence.
/// Shared lexemes referenced by other chunks/files stay.
pub(crate) fn prune_orphan_lexemes(tx: &Transaction<'_>) -> Result<()> {
    tx.execute(
        "DELETE FROM lexemes
         WHERE NOT EXISTS (
           SELECT 1 FROM lexeme_occurrences o WHERE o.lexeme_id = lexemes.id
         )",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(entries: &[LexemeEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.text.as_str()).collect()
    }

    #[test]
    fn indexes_full_tokens_and_identifier_parts() {
        let entries = lexemes_for_text("dma_alloc_coherent VIDIOC_STREAMON camelCase x1");
        let values = texts(&entries);
        assert!(values.contains(&"dma_alloc_coherent"));
        assert!(values.contains(&"coherent"));
        assert!(values.contains(&"vidioc_streamon"));
        assert!(values.contains(&"streamon"));
        assert!(values.contains(&"camelcase"));
        assert!(values.contains(&"camel"));
        assert!(values.contains(&"case"));
        assert!(values.contains(&"x1"));
    }

    #[test]
    fn indexes_bounded_chinese_windows() {
        let entries = lexemes_for_text("内核驱动");
        let values = texts(&entries);
        assert!(values.contains(&"内核驱动"));
        assert!(values.contains(&"内核"));
        assert!(values.contains(&"核驱动"));
        assert!(values.contains(&"驱动"));
        assert!(!values.contains(&"内"));
    }

    #[test]
    fn query_tokens_keep_whole_code_tokens_and_skip_number_parts() {
        let tokens = fuzzy_query_tokens("dma_alloc_cohernt() zqfuzzymesure4606 4606 100% 内核驱东");
        assert_eq!(
            tokens,
            vec!["dma_alloc_cohernt", "zqfuzzymesure4606", "内核驱东",]
        );
        assert_eq!(fuzzy_query_tokens("cohernt"), vec!["cohernt"]);
        assert!(fuzzy_query_tokens("4606 100%").is_empty());
    }

    #[test]
    fn gram_extraction_is_bounded() {
        let long = "a".repeat(1000);
        assert!(grams_for_lexeme(&long).len() <= MAX_GRAMS_PER_LEXEME);
        assert!(grams_of_len(&long, 3).len() <= MAX_GRAMS_PER_LEXEME);
    }
}
