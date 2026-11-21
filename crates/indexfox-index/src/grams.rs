use std::collections::HashSet;

use indexfox_core::normalize_search_text;

/// Distinct 1- and 2-character grams of a chunk, used to serve short queries
/// that FTS5 trigram cannot answer.
pub fn short_grams_for_chunk(text: &str) -> Vec<String> {
    let normalized = normalize_search_text(text);
    let chars: Vec<char> = normalized.chars().filter(|c| !c.is_whitespace()).collect();
    let mut grams = HashSet::new();
    for (index, ch) in chars.iter().enumerate() {
        grams.insert(ch.to_string());
        if let Some(next) = chars.get(index + 1) {
            let mut gram = String::with_capacity(ch.len_utf8() + next.len_utf8());
            gram.push(*ch);
            gram.push(*next);
            grams.insert(gram);
        }
    }
    let mut grams: Vec<String> = grams.into_iter().collect();
    grams.sort();
    grams
}

/// Query grams with whitespace removed. A 1- or 2-character term yields one
/// gram; longer terms are not served from this index.
pub fn grams_for_query(term: &str) -> Vec<String> {
    let normalized = normalize_search_text(term);
    let chars: Vec<char> = normalized.chars().filter(|c| !c.is_whitespace()).collect();
    match chars.as_slice() {
        [ch] => vec![ch.to_string()],
        [a, b] => {
            let mut gram = String::new();
            gram.push(*a);
            gram.push(*b);
            vec![gram]
        }
        _ => Vec::new(),
    }
}

/// Adjacent pairs after whitespace removal, matching how `short_grams` were
/// built. Used as a recall fallback for phrases that straddle a chunk boundary
/// where neither side contains a complete trigram.
pub fn bigrams_for_query(term: &str) -> Vec<String> {
    let normalized = normalize_search_text(term);
    let chars: Vec<char> = normalized.chars().filter(|c| !c.is_whitespace()).collect();
    let mut grams: Vec<String> = chars.windows(2).map(|pair| pair.iter().collect()).collect();
    grams.sort();
    grams.dedup();
    grams
}

/// Literal character trigrams (spaces kept) for FTS5 candidate recall.
pub fn trigrams_for_query(term: &str) -> Vec<String> {
    let normalized = normalize_search_text(term);
    let chars: Vec<char> = normalized.chars().collect();
    let mut trigrams: Vec<String> = chars
        .windows(3)
        .map(|triple| triple.iter().collect())
        .collect();
    trigrams.sort();
    trigrams.dedup();
    trigrams
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grams_cover_chinese_and_punctuation() {
        let grams = short_grams_for_chunk("内核 C++ a_b");
        assert!(grams.contains(&"内".to_string()));
        assert!(grams.contains(&"内核".to_string()));
        assert!(grams.contains(&"c+".to_string()));
        assert!(!grams.contains(&" ".to_string()));
    }

    #[test]
    fn query_grams_are_short_only() {
        assert_eq!(grams_for_query("内"), vec!["内"]);
        assert_eq!(grams_for_query("内核"), vec!["内核"]);
        assert!(grams_for_query("内核驱动").is_empty());
    }
}
