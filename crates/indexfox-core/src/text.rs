/// Normalize text for matching.
///
/// Only ASCII letters are case-folded. The transformation is byte-length
/// preserving, so offsets found in normalized text are valid in display text.
/// Non-ASCII case folding is intentionally out of scope for V1 (documented).
pub fn normalize_search_text(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

/// Byte offset → 1-based line number, counting `\n` before `offset`.
pub fn line_at_offset(text: &str, offset: usize) -> u32 {
    let end = offset.min(text.len());
    1 + text.as_bytes()[..end]
        .iter()
        .filter(|&&b| b == b'\n')
        .count() as u32
}

/// Produce a one-line snippet around `[start, end)` in `text`.
pub fn snippet(text: &str, start: usize, end: usize, radius: usize) -> String {
    let mut lo = start.saturating_sub(radius);
    let mut hi = (end + radius).min(text.len());
    while lo > 0 && !text.is_char_boundary(lo) {
        lo -= 1;
    }
    while hi < text.len() && !text.is_char_boundary(hi) {
        hi += 1;
    }
    let prefix = if lo > 0 { "…" } else { "" };
    let suffix = if hi < text.len() { "…" } else { "" };
    let body = text[lo..hi].replace(['\r', '\n'], " ");
    format!("{prefix}{body}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_fold_preserves_byte_offsets() {
        let input = "Linux 内核 VIDIOC";
        let normalized = normalize_search_text(input);
        assert_eq!(normalized, "linux 内核 vidioc");
        assert_eq!(normalized.len(), input.len());
    }

    #[test]
    fn line_numbers_are_one_based() {
        let text = "a\nb\nc";
        assert_eq!(line_at_offset(text, 0), 1);
        assert_eq!(line_at_offset(text, 2), 2);
        assert_eq!(line_at_offset(text, text.len()), 3);
    }
}
