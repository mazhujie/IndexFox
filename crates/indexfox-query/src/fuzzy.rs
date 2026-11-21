use indexfox_index::MAX_CANDIDATE_LEXEMES;

/// Fuzzy only expands tokens long enough that a single typo is evidence.
pub(crate) fn max_distance(char_len: usize) -> usize {
    match char_len {
        0..=2 => 0,
        3..=5 => 1,
        _ => 2,
    }
}

/// Bounded optimal-string-alignment Damerau-Levenshtein distance. Returns
/// `None` as soon as the best path in a row exceeds `max`, so callers can use
/// the same function for cheap verification without unbounded DP.
pub(crate) fn bounded_damerau_levenshtein(left: &str, right: &str, max: usize) -> Option<usize> {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.len().abs_diff(right.len()) > max {
        return None;
    }
    if left == right {
        return Some(0);
    }
    if max == 0 {
        return None;
    }

    let width = right.len() + 1;
    let mut previous_previous = vec![0usize; width];
    let mut previous: Vec<usize> = (0..width).collect();
    let mut current = vec![0usize; width];

    for i in 1..=left.len() {
        current[0] = i;
        let mut row_min = current[0];
        for j in 1..=right.len() {
            let cost = usize::from(left[i - 1] != right[j - 1]);
            let mut value = (previous[j] + 1)
                .min(current[j - 1] + 1)
                .min(previous[j - 1] + cost);
            if i > 1 && j > 1 && left[i - 1] == right[j - 2] && left[i - 2] == right[j - 1] {
                value = value.min(previous_previous[j - 2] + 1);
            }
            current[j] = value;
            row_min = row_min.min(value);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut previous_previous, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }
    let distance = previous[right.len()];
    (distance <= max).then_some(distance)
}

/// Gram tiers for candidate dictionary lookup: longer grams first, then
/// shorter fallbacks. Every tier is capped by the index layer.
pub(crate) fn candidate_tiers(token: &str) -> Vec<Vec<String>> {
    let char_len = token.chars().count();
    let lengths: &[usize] = match char_len {
        0..=2 => &[],
        3 => &[2, 1],
        4..=5 => &[3, 2, 1],
        _ => &[3, 2],
    };
    let mut tiers = Vec::new();
    for &length in lengths {
        let grams = indexfox_index::grams_of_len(token, length);
        if !grams.is_empty() {
            tiers.push(grams);
        }
    }
    tiers
}

pub(crate) fn candidate_limit() -> usize {
    MAX_CANDIDATE_LEXEMES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_handles_substitution_insertion_and_transposition() {
        assert_eq!(
            bounded_damerau_levenshtein("dma_alloc_cohernt", "dma_alloc_coherent", 2),
            Some(1)
        );
        assert_eq!(bounded_damerau_levenshtein("kernal", "kernel", 1), Some(1));
        assert_eq!(bounded_damerau_levenshtein("krenel", "kernel", 1), Some(1));
        assert_eq!(bounded_damerau_levenshtein("cat", "cot", 1), Some(1));
        assert_eq!(bounded_damerau_levenshtein("cat", "dog", 1), None);
        assert_eq!(
            bounded_damerau_levenshtein("内核驱东", "内核驱动", 1),
            Some(1)
        );
    }

    #[test]
    fn thresholds_are_bounded() {
        assert_eq!(max_distance(2), 0);
        assert_eq!(max_distance(3), 1);
        assert_eq!(max_distance(5), 1);
        assert_eq!(max_distance(6), 2);
        assert_eq!(max_distance(200), 2);
    }
}
