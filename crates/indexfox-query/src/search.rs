use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use indexfox_core::text::line_at_offset;
use indexfox_core::{normalize_search_text, snippet, Origin, SearchMode, SortKey, SourceLocator};
use indexfox_index::{
    ChunkRecord, FileRecord, IndexDb, IndexError, LexemeCandidate, LexemeOccurrence,
};
use serde::Serialize;

use crate::fuzzy::{bounded_damerau_levenshtein, candidate_limit, candidate_tiers, max_distance};
use crate::{parse, Expr, ParseError};

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("query parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("index error: {0}")]
    Index(#[from] IndexError),
    #[error("empty search term")]
    EmptyTerm,
    #[error("stored locator is invalid: {0}")]
    Locator(String),
    #[error("search cancelled")]
    Cancelled,
}

pub type Result<T, E = QueryError> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub mode: SearchMode,
    pub case_sensitive: bool,
    pub sort: SortKey,
    pub limit: usize,
    pub offset: usize,
    pub max_hits_per_file: usize,
    /// Restrict results to files from this source. `None` means all sources.
    pub source_id: Option<i64>,
    /// Restrict results to these file extensions. Leading dots and ASCII case
    /// are ignored. An empty list (or only empty entries) means no filter.
    pub extensions: Vec<String>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            mode: SearchMode::Substring,
            case_sensitive: false,
            sort: SortKey::Relevance,
            limit: 50,
            offset: 0,
            max_hits_per_file: 5,
            source_id: None,
            extensions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    Filename,
    Body,
    OcrBody,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchHit {
    pub chunk_id: i64,
    pub ordinal: i64,
    pub locator: Option<SourceLocator>,
    pub origin: Origin,
    pub start: usize,
    pub end: usize,
    pub matched_text: String,
    pub snippet: String,
    pub kind: MatchKind,
    pub crosses_chunk_end: bool,
    /// Edit-distance fallback hit; false for every strict match.
    #[serde(default)]
    pub fuzzy: bool,
    /// Actual distance for `fuzzy` hits, `None` for strict hits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_distance: Option<usize>,
    /// Actual indexed lexeme that was found, e.g. `dma_alloc_coherent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_lexeme: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileResult {
    pub file_id: i64,
    pub path: String,
    pub filename: String,
    pub ext: String,
    pub size_bytes: i64,
    pub mtime_ns: Option<i64>,
    pub state: String,
    pub score: i32,
    /// True iff the whole boolean expression matched strictly in this file.
    /// Fuzzy fallback files always sort after strict files.
    #[serde(default)]
    pub strict: bool,
    /// Distinct hit positions, including those beyond the display cap.
    pub hit_count: usize,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub query_plan: String,
    pub mode: SearchMode,
    pub total_files: usize,
    pub returned_files: usize,
    pub total_hits: usize,
    pub elapsed_ms: u64,
    pub results: Vec<FileResult>,
}

pub fn search(db: &IndexDb, query: &str, options: &SearchOptions) -> Result<SearchResponse> {
    let expression = parse(query)?;
    search_expr(db, &expression, options)
}

pub fn search_expr(
    db: &IndexDb,
    expression: &Expr,
    options: &SearchOptions,
) -> Result<SearchResponse> {
    search_expr_inner(db, expression, options, None)
}

/// Same as [`search`], but returns [`QueryError::Cancelled`] as soon as
/// `cancel` is observed. Checks run before and after SQLite calls and inside
/// the candidate, verification, merge and result-materialisation loops.
///
/// A small watchdog additionally calls SQLite's `interrupt` handle, so a long
/// recall statement cannot outlive cancellation. It is scoped to this call and
/// uses only rusqlite already linked by `indexfox-index`.
pub fn search_cancellable(
    db: &IndexDb,
    query: &str,
    options: &SearchOptions,
    cancel: &AtomicBool,
) -> Result<SearchResponse> {
    let expression = parse(query)?;
    check_cancelled(Some(cancel))?;
    let interrupt = db.conn().get_interrupt_handle();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let watchdog = scope.spawn(|| loop {
            if done.load(Ordering::Relaxed) {
                return;
            }
            if cancel.load(Ordering::Relaxed) {
                interrupt.interrupt();
                return;
            }
            std::thread::park_timeout(Duration::from_millis(2));
        });
        let result = search_expr_inner(db, &expression, options, Some(cancel));
        done.store(true, Ordering::Relaxed);
        watchdog.thread().unpark();
        match result {
            Err(_) if cancel.load(Ordering::Relaxed) => Err(QueryError::Cancelled),
            result => result,
        }
    })
}

fn search_expr_inner(
    db: &IndexDb,
    expression: &Expr,
    options: &SearchOptions,
    cancel: Option<&AtomicBool>,
) -> Result<SearchResponse> {
    check_cancelled(cancel)?;
    let started = Instant::now();
    let engine = Engine {
        db,
        options,
        cancel,
        strict_candidates: RefCell::new(HashMap::new()),
        fuzzy_tokens: RefCell::new(HashMap::new()),
        lexeme_occurrences: RefCell::new(HashMap::new()),
        literal_cache: RefCell::new(HashMap::new()),
    };
    let evidence = engine.evaluate(expression, false)?;

    let mut results = Vec::with_capacity(evidence.len());
    let mut total_hits = 0usize;
    for (file_id, item) in evidence {
        check_cancelled(cancel)?;
        let Some(file) = db.file_by_id(file_id)? else {
            continue;
        };
        if !file_matches_filters(&file, options) {
            continue;
        }
        let hit_count = item.seen.len();
        total_hits += hit_count;
        results.push(FileResult {
            file_id,
            path: file.display_path,
            filename: file.filename,
            ext: file.ext,
            size_bytes: file.size_bytes,
            mtime_ns: file.mtime_ns,
            state: file.state,
            score: item.score,
            strict: item.strict,
            hit_count,
            hits: item.hits,
        });
    }

    check_cancelled(cancel)?;
    sort_results(&mut results, options.sort);
    check_cancelled(cancel)?;
    let total_files = results.len();
    let results: Vec<FileResult> = results
        .into_iter()
        .skip(options.offset)
        .take(options.limit)
        .collect();

    Ok(SearchResponse {
        query_plan: expression.describe(),
        mode: options.mode,
        total_files,
        returned_files: results.len(),
        total_hits,
        elapsed_ms: started.elapsed().as_millis() as u64,
        results,
    })
}

fn check_cancelled(cancel: Option<&AtomicBool>) -> Result<()> {
    match cancel {
        Some(cancel) if cancel.load(Ordering::Relaxed) => Err(QueryError::Cancelled),
        _ => Ok(()),
    }
}

fn file_matches_filters(file: &FileRecord, options: &SearchOptions) -> bool {
    if let Some(source_id) = options.source_id {
        if file.source_id != source_id {
            return false;
        }
    }
    if options.extensions.is_empty() {
        return true;
    }
    let ext = file.ext.trim_start_matches('.').to_ascii_lowercase();
    let mut has_filter = false;
    for filter in &options.extensions {
        let filter = filter.trim().trim_start_matches('.');
        if filter.is_empty() {
            continue;
        }
        has_filter = true;
        if filter.eq_ignore_ascii_case(&ext) {
            return true;
        }
    }
    !has_filter
}

/// Compact identity of one hit: a position inside one chunk plus match kind.
/// Cross-chunk discoveries of the same text and duplicate boolean branches
/// therefore collapse to a single actual hit without storing every match.
type HitKey = (i64, usize, usize, MatchKind);

fn hit_key(hit: &SearchHit) -> HitKey {
    (hit.chunk_id, hit.start, hit.end, hit.kind)
}

#[derive(Debug, Default, Clone)]
struct Evidence {
    hits: Vec<SearchHit>,
    /// All distinct hit keys for this file, including hits past the display
    /// cap. Default `HashSet` does not allocate until a hit is admitted.
    seen: HashSet<HitKey>,
    score: i32,
    /// Whole sub-expression matched strictly for this file.
    strict: bool,
}

impl Evidence {
    /// Admit a hit key before the caller builds the full hit. Returns `false`
    /// when this exact position was already counted.
    fn admit(&mut self, key: HitKey) -> bool {
        self.seen.insert(key)
    }

    /// Record an already-admitted hit. The hit list stays capped for display;
    /// `seen` keeps the actual count.
    fn record(&mut self, hit: SearchHit, score: i32, cap: usize) {
        if self.hits.len() >= cap {
            return;
        }
        self.score += score;
        self.hits.push(hit);
    }

    fn push(&mut self, hit: SearchHit, score: i32, cap: usize) {
        if !self.admit(hit_key(&hit)) {
            return;
        }
        self.record(hit, score, cap);
    }

    fn merge(&mut self, other: Evidence, cap: usize, cancel: Option<&AtomicBool>) -> Result<()> {
        self.score += other.score;
        for hit in other.hits {
            check_cancelled(cancel)?;
            if self.seen.insert(hit_key(&hit)) && self.hits.len() < cap {
                self.hits.push(hit);
            }
        }
        // Hits past the display cap only carry their compact key.
        for key in other.seen {
            check_cancelled(cancel)?;
            self.seen.insert(key);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct FuzzyLexeme {
    lexeme: String,
    distance: usize,
    occurrences: Rc<Vec<LexemeOccurrence>>,
}

struct Engine<'a> {
    db: &'a IndexDb,
    options: &'a SearchOptions,
    cancel: Option<&'a AtomicBool>,
    /// Shared candidate lists for repeated literal leaves.
    strict_candidates: RefCell<HashMap<String, Rc<Vec<i64>>>>,
    fuzzy_tokens: RefCell<HashMap<String, Rc<Vec<FuzzyLexeme>>>>,
    lexeme_occurrences: RefCell<HashMap<i64, Rc<Vec<LexemeOccurrence>>>>,
    /// Exact leaf result including whether each file's leaf matched strictly.
    literal_cache: RefCell<HashMap<(String, bool), HashMap<i64, Evidence>>>,
}

impl Engine<'_> {
    fn check_cancelled(&self) -> Result<()> {
        check_cancelled(self.cancel)
    }

    /// `strict_only` is set under `Not`, so negated leaves never expand to
    /// edit-distance matches. Quoted phrases are strict at their call site.
    fn evaluate(&self, expression: &Expr, strict_only: bool) -> Result<HashMap<i64, Evidence>> {
        self.check_cancelled()?;
        match expression {
            Expr::Term(value) => self.match_literal(value, !strict_only),
            Expr::Phrase(value) => self.match_literal(value, false),
            Expr::And(left, right) => {
                let left = self.evaluate(left, strict_only)?;
                let mut right = self.evaluate(right, strict_only)?;
                let mut merged = HashMap::new();
                for (file_id, mut evidence) in left {
                    self.check_cancelled()?;
                    if let Some(other) = right.remove(&file_id) {
                        let strict = evidence.strict && other.strict;
                        evidence.merge(other, self.options.max_hits_per_file, self.cancel)?;
                        evidence.strict = strict;
                        merged.insert(file_id, evidence);
                    }
                }
                Ok(merged)
            }
            Expr::Or(left, right) => {
                let mut merged = self.evaluate(left, strict_only)?;
                for (file_id, incoming) in self.evaluate(right, strict_only)? {
                    self.check_cancelled()?;
                    let incoming_strict = incoming.strict;
                    let entry = merged.entry(file_id).or_default();
                    let previous_strict = entry.strict;
                    entry.merge(incoming, self.options.max_hits_per_file, self.cancel)?;
                    entry.strict = previous_strict || incoming_strict;
                }
                Ok(merged)
            }
            Expr::Not(inner) => {
                let excluded: HashSet<i64> = self.evaluate(inner, true)?.into_keys().collect();
                self.check_cancelled()?;
                let all = self.db.all_file_ids()?;
                self.check_cancelled()?;
                let mut result = HashMap::with_capacity(all.len());
                for file_id in all {
                    self.check_cancelled()?;
                    if !excluded.contains(&file_id) {
                        result.insert(
                            file_id,
                            Evidence {
                                strict: true,
                                ..Evidence::default()
                            },
                        );
                    }
                }
                Ok(result)
            }
        }
    }

    fn match_literal(&self, literal: &str, allow_fuzzy: bool) -> Result<HashMap<i64, Evidence>> {
        let key = (literal.to_string(), allow_fuzzy);
        if let Some(cached) = self.literal_cache.borrow().get(&key) {
            return Ok(cached.clone());
        }
        let result = self.match_literal_uncached(literal, allow_fuzzy)?;
        self.literal_cache.borrow_mut().insert(key, result.clone());
        Ok(result)
    }

    fn candidate_chunks(&self, needle: &str) -> Result<Rc<Vec<i64>>> {
        if let Some(cached) = self.strict_candidates.borrow().get(needle) {
            return Ok(Rc::clone(cached));
        }
        let ids = Rc::new(self.db.candidate_chunks(needle)?);
        self.strict_candidates
            .borrow_mut()
            .insert(needle.to_string(), Rc::clone(&ids));
        Ok(ids)
    }

    fn match_literal_uncached(
        &self,
        literal: &str,
        allow_fuzzy: bool,
    ) -> Result<HashMap<i64, Evidence>> {
        if literal.is_empty() {
            return Err(QueryError::EmptyTerm);
        }
        let needle = if self.options.case_sensitive {
            literal.to_string()
        } else {
            normalize_search_text(literal)
        };
        let mut by_file: HashMap<i64, Evidence> = HashMap::new();

        self.check_cancelled()?;
        let candidates = self.candidate_chunks(&needle)?;
        self.check_cancelled()?;
        for chunk_id in candidates.iter().copied() {
            self.check_cancelled()?;
            let Some((mut previous, chunk, mut next)) = self.db.chunk_context(chunk_id)? else {
                continue;
            };
            let locator = chunk_locator(&chunk)?;
            let mut segments = vec![(chunk, locator)];

            // The old fixed prev+current+next window misses a phrase longer
            // than those three chunks. Extend each side until it holds at least
            // `needle` bytes or the locator run breaks; for ordinary chunks and
            // queries this stops after the first neighbour, preserving cost.
            let mut left_len = 0usize;
            while left_len < needle.len() {
                let Some(candidate) = previous.take() else {
                    break;
                };
                let candidate_locator = chunk_locator(&candidate)?;
                if !continues(&candidate_locator, &segments[0].1) {
                    break;
                }
                left_len += candidate.text.len();
                let candidate_id = candidate.id;
                segments.insert(0, (candidate, candidate_locator));
                self.check_cancelled()?;
                if left_len < needle.len() {
                    previous = self
                        .db
                        .chunk_context(candidate_id)?
                        .and_then(|context| context.0);
                }
            }
            let mut right_len = 0usize;
            while right_len < needle.len() {
                let Some(candidate) = next.take() else {
                    break;
                };
                let candidate_locator = chunk_locator(&candidate)?;
                if !continues(
                    &segments.last().expect("current chunk").1,
                    &candidate_locator,
                ) {
                    break;
                }
                right_len += candidate.text.len();
                let candidate_id = candidate.id;
                segments.push((candidate, candidate_locator));
                self.check_cancelled()?;
                if right_len < needle.len() {
                    next = self
                        .db
                        .chunk_context(candidate_id)?
                        .and_then(|context| context.2);
                }
            }

            let total_len = segments
                .iter()
                .map(|(chunk, _)| chunk.text.len())
                .sum::<usize>();
            let mut hay_text = String::with_capacity(total_len);
            let mut hay_display = String::with_capacity(total_len);
            let mut ranges = Vec::with_capacity(segments.len());
            for (chunk, _) in &segments {
                let start = hay_text.len();
                hay_text.push_str(&chunk.text);
                hay_display.push_str(&chunk.display_text);
                ranges.push((start, hay_text.len()));
            }

            let hay = if self.options.case_sensitive {
                &hay_display
            } else {
                &hay_text
            };
            let mut from = 0usize;
            while from < hay.len() {
                self.check_cancelled()?;
                let Some(relative) = hay[from..].find(&needle) else {
                    break;
                };
                let start = from + relative;
                let end = start + needle.len();
                let display_end = end.min(hay_display.len());
                if !hay_display.is_char_boundary(start)
                    || !hay_display.is_char_boundary(display_end)
                {
                    from = start + needle.len().max(1);
                    continue;
                }

                let segment_index = ranges.partition_point(|(_, seg_end)| *seg_end <= start);
                let (owner, owner_locator) = &segments[segment_index];
                let (owner_start, owner_end) = ranges[segment_index];

                let local_start = start - owner_start;
                let local_end = end - owner_start;
                let origin = Origin::parse(&owner.origin);
                let kind = if origin == Origin::Ocr {
                    MatchKind::OcrBody
                } else {
                    MatchKind::Body
                };
                let entry = by_file.entry(owner.file_id).or_default();
                entry.strict = true;
                if !entry.admit((owner.id, local_start, local_end, kind)) {
                    from = start + needle.len().max(1);
                    continue;
                }
                let hit = SearchHit {
                    chunk_id: owner.id,
                    ordinal: owner.ordinal,
                    locator: Some(owner_locator.clone()),
                    origin,
                    start: local_start,
                    end: local_end,
                    matched_text: hay_display[start..display_end].to_string(),
                    snippet: snippet(&hay_display, start, display_end, 60),
                    kind,
                    crosses_chunk_end: end > owner_end,
                    fuzzy: false,
                    edit_distance: None,
                    matched_lexeme: None,
                };
                // In fuzzy mode a strict body hit is still a strict result and
                // must outrank edit-distance fallbacks.
                let score = if matches!(self.options.mode, SearchMode::Exact | SearchMode::Fuzzy) {
                    12
                } else {
                    10
                };
                entry.record(hit, score, self.options.max_hits_per_file);
                from = start + needle.len().max(1);
            }
        }

        self.check_cancelled()?;
        let filename_matches = self
            .db
            .files_matching_filename(&needle, self.options.case_sensitive)?;
        self.check_cancelled()?;
        for file in filename_matches {
            self.check_cancelled()?;
            let exact_name = if self.options.case_sensitive {
                file.filename == needle
            } else {
                normalize_search_text(&file.filename) == needle
            };
            let hit = SearchHit {
                chunk_id: 0,
                ordinal: -1,
                locator: None,
                origin: Origin::Native,
                start: 0,
                end: needle.len(),
                matched_text: file.filename.clone(),
                snippet: file.filename.clone(),
                kind: MatchKind::Filename,
                crosses_chunk_end: false,
                fuzzy: false,
                edit_distance: None,
                matched_lexeme: None,
            };
            let score = if exact_name { 500 } else { 100 };
            let entry = by_file.entry(file.id).or_default();
            entry.strict = true;
            entry.push(hit, score, self.options.max_hits_per_file);
        }

        if allow_fuzzy && self.options.mode == SearchMode::Fuzzy {
            self.add_fuzzy_matches(literal, &mut by_file)?;
        }

        Ok(by_file)
    }

    fn add_fuzzy_matches(&self, literal: &str, by_file: &mut HashMap<i64, Evidence>) -> Result<()> {
        let mut pending: Vec<(i64, SearchHit)> = Vec::new();
        'tokens: for token in indexfox_index::fuzzy_query_tokens(literal) {
            self.check_cancelled()?;
            let matches = self.fuzzy_token_matches(&token)?;
            for matched in matches.iter() {
                for occurrence in matched.occurrences.iter() {
                    if pending.len() >= 4096 {
                        break 'tokens;
                    }
                    if by_file
                        .get(&occurrence.file_id)
                        .is_some_and(|evidence| evidence.strict)
                    {
                        continue;
                    }
                    self.check_cancelled()?;
                    let Some(chunk) = self.db.chunk_by_id(occurrence.chunk_id)? else {
                        continue;
                    };
                    if chunk.file_id != occurrence.file_id
                        || occurrence.start >= occurrence.end
                        || occurrence.end > chunk.display_text.len()
                        || !chunk.display_text.is_char_boundary(occurrence.start)
                        || !chunk.display_text.is_char_boundary(occurrence.end)
                    {
                        continue;
                    }
                    let origin = Origin::parse(&chunk.origin);
                    let kind = if origin == Origin::Ocr {
                        MatchKind::OcrBody
                    } else {
                        MatchKind::Body
                    };
                    let hit = SearchHit {
                        chunk_id: chunk.id,
                        ordinal: chunk.ordinal,
                        locator: Some(chunk_locator(&chunk)?),
                        origin,
                        start: occurrence.start,
                        end: occurrence.end,
                        matched_text: chunk.display_text[occurrence.start..occurrence.end]
                            .to_string(),
                        snippet: snippet(&chunk.display_text, occurrence.start, occurrence.end, 60),
                        kind,
                        crosses_chunk_end: false,
                        fuzzy: true,
                        edit_distance: Some(matched.distance),
                        matched_lexeme: Some(matched.lexeme.clone()),
                    };
                    pending.push((occurrence.file_id, hit));
                }
            }
        }

        // A shorter identifier part can point inside the full token that the
        // same query already matched. Keep the longest actual span.
        pending.sort_by(|left, right| {
            left.1
                .chunk_id
                .cmp(&right.1.chunk_id)
                .then_with(|| left.1.start.cmp(&right.1.start))
                .then_with(|| right.1.end.cmp(&left.1.end))
        });
        let mut accepted: Vec<(i64, SearchHit)> = Vec::new();
        for (file_id, hit) in pending {
            let contained = accepted.iter().any(|(other_file, other)| {
                *other_file == file_id
                    && other.chunk_id == hit.chunk_id
                    && other.start <= hit.start
                    && other.end >= hit.end
                    && (other.start, other.end) != (hit.start, hit.end)
            });
            if !contained {
                accepted.push((file_id, hit));
            }
        }

        for (file_id, hit) in accepted {
            self.check_cancelled()?;
            let entry = by_file.entry(file_id).or_default();
            if entry.strict {
                continue;
            }
            entry.push(hit, 4, self.options.max_hits_per_file);
        }
        Ok(())
    }

    fn fuzzy_token_matches(&self, token: &str) -> Result<Rc<Vec<FuzzyLexeme>>> {
        let normalized = normalize_search_text(token);
        if let Some(cached) = self.fuzzy_tokens.borrow().get(&normalized) {
            return Ok(Rc::clone(cached));
        }
        let matches = Rc::new(self.compute_fuzzy_token(&normalized)?);
        self.fuzzy_tokens
            .borrow_mut()
            .insert(normalized, Rc::clone(&matches));
        Ok(matches)
    }

    fn compute_fuzzy_token(&self, token: &str) -> Result<Vec<FuzzyLexeme>> {
        let char_len = token.chars().count();
        let max_distance = max_distance(char_len);
        if max_distance == 0 {
            return Ok(Vec::new());
        }
        let min_length = char_len.saturating_sub(max_distance).max(2) as i64;
        let max_length = (char_len + max_distance).min(indexfox_index::MAX_LEXEME_CHARS) as i64;

        for grams in candidate_tiers(token) {
            self.check_cancelled()?;
            let candidates = self.db.candidate_lexemes(
                &grams,
                min_length,
                max_length,
                char_len as i64,
                1,
                candidate_limit(),
            )?;
            let mut tier: Vec<(usize, LexemeCandidate)> = Vec::new();
            for candidate in candidates {
                if candidate.length != candidate.text.chars().count() as i64 {
                    continue;
                }
                let Some(distance) =
                    bounded_damerau_levenshtein(token, &candidate.text, max_distance)
                else {
                    continue;
                };
                // Exact lexemes are already covered by the strict pass for
                // their own files; other files still need their fuzzy matches.
                if distance > 0 {
                    tier.push((distance, candidate));
                }
            }
            let Some(best) = tier.iter().map(|(distance, _)| *distance).min() else {
                continue;
            };
            tier.retain(|(distance, _)| *distance == best);
            tier.sort_by(|left, right| {
                left.1
                    .text
                    .cmp(&right.1.text)
                    .then_with(|| left.1.id.cmp(&right.1.id))
            });
            tier.truncate(16);
            let mut matches = Vec::new();
            for (_, candidate) in tier {
                let occurrences = self.occurrences_for(candidate.id)?;
                if occurrences.is_empty() {
                    continue;
                }
                matches.push(FuzzyLexeme {
                    lexeme: candidate.text,
                    distance: best,
                    occurrences,
                });
            }
            return Ok(matches);
        }
        Ok(Vec::new())
    }

    fn occurrences_for(&self, lexeme_id: i64) -> Result<Rc<Vec<LexemeOccurrence>>> {
        if let Some(cached) = self.lexeme_occurrences.borrow().get(&lexeme_id) {
            return Ok(Rc::clone(cached));
        }
        let occurrences = Rc::new(self.db.lexeme_occurrences(lexeme_id, 1024)?);
        self.lexeme_occurrences
            .borrow_mut()
            .insert(lexeme_id, Rc::clone(&occurrences));
        Ok(occurrences)
    }
}

fn chunk_locator(chunk: &ChunkRecord) -> Result<SourceLocator> {
    let mut locator = serde_json::from_str(&chunk.locator_json)
        .map_err(|error| QueryError::Locator(error.to_string()))?;
    // Early indexes counted a trailing line break as an extra content line.
    if let SourceLocator::Text {
        start_line,
        end_line,
        ..
    } = &mut locator
    {
        if let Some(text) = chunk.display_text.strip_suffix('\n') {
            *end_line = start_line.saturating_add(line_at_offset(text, text.len()) - 1);
        }
    }
    Ok(locator)
}

/// A phrase may only span a chunk boundary when the two chunks are a
/// contiguous run of the same text, PDF page, paragraph or table cell.
fn continues(left: &SourceLocator, right: &SourceLocator) -> bool {
    use SourceLocator::*;
    match (left, right) {
        (Text { end_offset, .. }, Text { start_offset, .. }) => end_offset == start_offset,
        (PdfPage { page: left, .. }, PdfPage { page: right, .. }) => left == right,
        (DocxParagraph { paragraph: left }, DocxParagraph { paragraph: right }) => left == right,
        (
            DocxTableCell {
                table: t1,
                row: r1,
                column: c1,
                paragraph: p1,
            },
            DocxTableCell {
                table: t2,
                row: r2,
                column: c2,
                paragraph: p2,
            },
        ) => t1 == t2 && r1 == r2 && c1 == c2 && p1 == p2,
        _ => false,
    }
}

fn sort_results(results: &mut [FileResult], sort: SortKey) {
    results.sort_by(|a, b| {
        // Strict full-expression matches always precede fuzzy fallbacks, even
        // when the requested sort key would order the fuzzy file first.
        b.strict
            .cmp(&a.strict)
            .then_with(|| compare_results(a, b, sort))
    });
}

fn compare_results(a: &FileResult, b: &FileResult, sort: SortKey) -> std::cmp::Ordering {
    // Every comparator ends with the same path + id tie-break, so equal sort
    // keys cannot inherit HashMap iteration order across paginated requests.
    match sort {
        SortKey::Relevance => b
            .score
            .cmp(&a.score)
            .then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase()))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.file_id.cmp(&b.file_id)),
        SortKey::Filename => a
            .filename
            .to_lowercase()
            .cmp(&b.filename.to_lowercase())
            .then_with(|| a.filename.cmp(&b.filename))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.file_id.cmp(&b.file_id)),
        SortKey::Path => a.path.cmp(&b.path).then_with(|| a.file_id.cmp(&b.file_id)),
        SortKey::Size => b
            .size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.file_id.cmp(&b.file_id)),
        SortKey::ModifiedTime => b
            .mtime_ns
            .unwrap_or(0)
            .cmp(&a.mtime_ns.unwrap_or(0))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.file_id.cmp(&b.file_id)),
        SortKey::HitCount => b
            .hit_count
            .cmp(&a.hit_count)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.file_id.cmp(&b.file_id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexfox_core::{ExtractedDocument, FileState, Origin, Section, SourceKind, SourceLocator};
    use indexfox_index::NewFile;

    fn seed(
        db: &mut IndexDb,
        source_id: i64,
        path: &str,
        ext: &str,
        text: &str,
        size_bytes: i64,
        mtime_ns: Option<i64>,
    ) -> i64 {
        let filename = path.rsplit('/').next().expect("filename").to_string();
        let file_id = db
            .upsert_file(&NewFile {
                source_id,
                path_key: path.into(),
                display_path: path.into(),
                filename,
                ext: ext.into(),
                size_bytes,
                mtime_ns,
                state: FileState::Pending,
            })
            .unwrap();
        let document = ExtractedDocument {
            extractor_version: "test".into(),
            encoding: Some("utf-8".into()),
            sections: vec![Section {
                text: normalize_search_text(text),
                display_text: text.into(),
                locator: SourceLocator::Text {
                    start_line: 1,
                    end_line: 1,
                    start_offset: 0,
                    end_offset: text.len(),
                },
                origin: Origin::Native,
            }],
        };
        db.replace_file_content(file_id, &document).unwrap();
        file_id
    }

    fn ids(response: &SearchResponse) -> Vec<i64> {
        response
            .results
            .iter()
            .map(|result| result.file_id)
            .collect()
    }

    #[test]
    fn cancellable_search_honours_the_flag() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        seed(
            &mut db,
            source,
            "/a/alpha.txt",
            "txt",
            "alpha body",
            10,
            Some(1),
        );
        let options = SearchOptions::default();

        let cancel = AtomicBool::new(true);
        assert!(matches!(
            search_cancellable(&db, "alpha", &options, &cancel),
            Err(QueryError::Cancelled)
        ));

        cancel.store(false, Ordering::Relaxed);
        let response = search_cancellable(&db, "alpha", &options, &cancel).unwrap();
        assert_eq!(response.total_files, 1);
        assert_eq!(response.total_hits, 2); // body + filename
    }

    #[test]
    fn filters_apply_to_files_totals_and_pages() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source_a = db.ensure_source(SourceKind::Local, "/a").unwrap();
        let source_b = db.ensure_source(SourceKind::Local, "/b").unwrap();
        seed(
            &mut db,
            source_a,
            "/a/one.txt",
            "txt",
            "bodyword",
            10,
            Some(1),
        );
        seed(
            &mut db,
            source_a,
            "/a/two.md",
            "md",
            "bodyword",
            10,
            Some(1),
        );
        seed(
            &mut db,
            source_b,
            "/b/three.txt",
            "txt",
            "bodyword",
            10,
            Some(1),
        );

        let base = SearchOptions {
            sort: SortKey::Path,
            limit: 1,
            ..SearchOptions::default()
        };

        let first = search(
            &db,
            "bodyword",
            &SearchOptions {
                source_id: Some(source_a),
                ..base.clone()
            },
        )
        .unwrap();
        assert_eq!(first.total_files, 2);
        assert_eq!(first.returned_files, 1);
        assert_eq!(first.total_hits, 2);

        let second = search(
            &db,
            "bodyword",
            &SearchOptions {
                source_id: Some(source_a),
                offset: 1,
                ..base.clone()
            },
        )
        .unwrap();
        assert_eq!(second.total_files, 2);
        assert_eq!(second.returned_files, 1);
        assert_ne!(ids(&first), ids(&second));

        let other_source = search(
            &db,
            "bodyword",
            &SearchOptions {
                source_id: Some(source_b),
                ..base.clone()
            },
        )
        .unwrap();
        assert_eq!(other_source.total_files, 1);
        assert_eq!(other_source.total_hits, 1);

        let md_only = search(
            &db,
            "bodyword",
            &SearchOptions {
                extensions: vec![".MD".into()],
                ..base.clone()
            },
        )
        .unwrap();
        assert_eq!(md_only.total_files, 1);
        assert_eq!(md_only.results[0].ext, "md");

        let txt_only = search(
            &db,
            "bodyword",
            &SearchOptions {
                extensions: vec!["TXT".into()],
                ..base.clone()
            },
        )
        .unwrap();
        assert_eq!(txt_only.total_files, 2);

        let all_extensions = search(
            &db,
            "bodyword",
            &SearchOptions {
                extensions: vec!["txt".into(), "md".into()],
                ..base.clone()
            },
        )
        .unwrap();
        assert_eq!(all_extensions.total_files, 3);

        let empty_filter = search(
            &db,
            "bodyword",
            &SearchOptions {
                extensions: vec![String::new()],
                ..base.clone()
            },
        )
        .unwrap();
        assert_eq!(empty_filter.total_files, 3);
    }

    fn seed_sections(db: &mut IndexDb, source_id: i64, path: &str, texts: &[&str]) -> i64 {
        let filename = path.rsplit('/').next().expect("filename").to_string();
        let file_id = db
            .upsert_file(&NewFile {
                source_id,
                path_key: path.into(),
                display_path: path.into(),
                filename,
                ext: "txt".into(),
                size_bytes: texts.iter().map(|text| text.len() as i64).sum(),
                mtime_ns: Some(1),
                state: FileState::Pending,
            })
            .unwrap();
        let mut offset = 0usize;
        let sections = texts
            .iter()
            .map(|text| {
                let start_offset = offset;
                offset += text.len();
                Section {
                    text: normalize_search_text(text),
                    display_text: (*text).into(),
                    locator: SourceLocator::Text {
                        start_line: 1,
                        end_line: 1,
                        start_offset,
                        end_offset: offset,
                    },
                    origin: Origin::Native,
                }
            })
            .collect();
        db.replace_file_content(
            file_id,
            &ExtractedDocument {
                extractor_version: "test".into(),
                encoding: Some("utf-8".into()),
                sections,
            },
        )
        .unwrap();
        file_id
    }

    #[test]
    fn phrase_spanning_more_than_three_chunks_is_found() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        seed_sections(&mut db, source, "/a/long.txt", &["ab", "cd", "ef", "gh"]);

        let response = search(&db, "\"abcdefgh\"", &SearchOptions::default()).unwrap();
        assert_eq!(response.total_files, 1);
        assert_eq!(response.total_hits, 1);
        let hit = &response.results[0].hits[0];
        assert_eq!(hit.matched_text, "abcdefgh");
        assert!(hit.crosses_chunk_end);
    }

    #[test]
    fn two_character_phrase_split_one_plus_one_is_found() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        seed_sections(&mut db, source, "/a/split.txt", &["内", "核"]);

        let response = search(&db, "\"内核\"", &SearchOptions::default()).unwrap();
        assert_eq!(response.total_files, 1);
        assert_eq!(response.total_hits, 1);
        let hit = &response.results[0].hits[0];
        assert_eq!(hit.matched_text, "内核");
        assert!(hit.crosses_chunk_end);
    }

    #[test]
    fn phrase_split_into_single_character_chunks_is_found() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/b").unwrap();
        seed_sections(&mut db, source, "/b/split.txt", &["a", "b", "c", "d"]);
        seed_sections(&mut db, source, "/b/inline.txt", &["abcd"]);

        let response = search(&db, "\"abcd\"", &SearchOptions::default()).unwrap();
        assert_eq!(response.total_files, 2);
        assert_eq!(response.total_hits, 2);
        let split = response
            .results
            .iter()
            .find(|file| file.filename == "split.txt")
            .unwrap();
        let hit = &split.hits[0];
        assert_eq!(hit.matched_text, "abcd");
        assert!(hit.crosses_chunk_end);
    }

    #[test]
    fn whitespace_only_phrase_is_recalled_and_verified() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/c").unwrap();
        seed(&mut db, source, "/c/wide.txt", "txt", "a   b", 5, Some(1));
        seed(&mut db, source, "/c/narrow.txt", "txt", "a b", 3, Some(1));

        let response = search(&db, "\"   \"", &SearchOptions::default()).unwrap();
        assert_eq!(response.total_files, 1);
        assert_eq!(response.total_hits, 1);
        assert_eq!(response.results[0].path, "/c/wide.txt");
        assert_eq!(response.results[0].hits[0].matched_text, "   ");
    }

    #[test]
    fn every_sort_key_paginates_stably_with_ties() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        for name in ["f", "e", "d", "c", "b", "a"] {
            seed(
                &mut db,
                source,
                &format!("/a/{name}.txt"),
                "txt",
                "bodyword",
                10,
                Some(5),
            );
        }

        for sort in [
            SortKey::Relevance,
            SortKey::ModifiedTime,
            SortKey::Size,
            SortKey::Path,
            SortKey::Filename,
            SortKey::HitCount,
        ] {
            let options = SearchOptions {
                sort,
                limit: 100,
                ..SearchOptions::default()
            };
            let full = ids(&search(&db, "bodyword", &options).unwrap());
            let repeated = ids(&search(&db, "bodyword", &options).unwrap());
            assert_eq!(full, repeated, "unstable full result for {sort:?}");

            let mut paged = Vec::new();
            for offset in 0..full.len() {
                let page = search(
                    &db,
                    "bodyword",
                    &SearchOptions {
                        offset,
                        limit: 1,
                        ..options.clone()
                    },
                )
                .unwrap();
                assert_eq!(page.total_files, full.len());
                paged.extend(ids(&page));
            }
            assert_eq!(full, paged, "unstable pagination for {sort:?}");
        }
    }

    #[test]
    fn hit_count_orders_by_actual_hits_beyond_the_display_cap() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        let dense = seed(
            &mut db,
            source,
            "/a/dense.txt",
            "txt",
            &"needle ".repeat(30),
            210,
            Some(1),
        );
        let sparse = seed(
            &mut db,
            source,
            "/a/sparse.txt",
            "txt",
            &"needle ".repeat(6),
            42,
            Some(1),
        );

        let response = search(
            &db,
            "needle",
            &SearchOptions {
                sort: SortKey::HitCount,
                limit: 10,
                ..SearchOptions::default()
            },
        )
        .unwrap();

        assert_eq!(response.total_hits, 36);
        assert_eq!(ids(&response), vec![dense, sparse]);
        assert_eq!(response.results[0].hit_count, 30);
        assert_eq!(response.results[0].hits.len(), 5);
        assert_eq!(response.results[1].hit_count, 6);
        assert_eq!(response.results[1].hits.len(), 5);
    }

    #[test]
    fn duplicate_or_branches_do_not_double_hits() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        seed(
            &mut db,
            source,
            "/a/one.txt",
            "txt",
            "needle body",
            11,
            Some(1),
        );

        let response = search(&db, "needle OR needle", &SearchOptions::default()).unwrap();

        assert_eq!(response.total_files, 1);
        assert_eq!(response.total_hits, 1);
        assert_eq!(response.results[0].hit_count, 1);
        assert_eq!(response.results[0].hits.len(), 1);
    }

    fn fuzzy_options(sort: SortKey) -> SearchOptions {
        SearchOptions {
            mode: SearchMode::Fuzzy,
            sort,
            limit: 50,
            ..SearchOptions::default()
        }
    }

    #[test]
    fn fuzzy_code_query_does_not_expand_numeric_suffixes() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        for index in 0..4 {
            seed(
                &mut db,
                source,
                &format!("/a/real{index}.rs"),
                "rs",
                "let zqfuzzymeasure4606 = 1;",
                27,
                Some(1),
            );
        }
        for (index, noise) in ["460 606", "1606 4600", "4606 460"].iter().enumerate() {
            seed(
                &mut db,
                source,
                &format!("/a/noise{index}.txt"),
                "txt",
                noise,
                noise.len() as i64,
                Some(1),
            );
        }

        let response =
            search(&db, "zqfuzzymesure4606", &fuzzy_options(SortKey::Relevance)).unwrap();

        assert_eq!(response.total_files, 4);
        assert_eq!(response.results.len(), 4);
        for result in &response.results {
            assert!(result.path.starts_with("/a/real"));
            assert!(!result.strict);
            assert!(result.hits.iter().all(
                |hit| hit.fuzzy && hit.matched_lexeme.as_deref() == Some("zqfuzzymeasure4606")
            ));
        }
    }

    #[test]
    fn q10_fuzzy_identifier_returns_original_range_and_metadata() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        seed(
            &mut db,
            source,
            "/a/camera.c",
            "c",
            "dma_alloc_coherent() allocates memory",
            39,
            Some(1),
        );

        let response =
            search(&db, "dma_alloc_cohernt", &fuzzy_options(SortKey::Relevance)).unwrap();

        assert_eq!(response.total_files, 1);
        let result = &response.results[0];
        assert!(!result.strict);
        let hit = result.hits.iter().find(|hit| hit.fuzzy).expect("fuzzy hit");
        assert_eq!(hit.matched_text, "dma_alloc_coherent");
        assert_eq!(hit.edit_distance, Some(1));
        assert_eq!(hit.matched_lexeme.as_deref(), Some("dma_alloc_coherent"));
        assert_eq!(result.hits.len(), 1, "contained part match is dropped");
    }

    #[test]
    fn strict_results_precede_fuzzy_fallbacks_for_every_sort_key() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        let strict = seed(
            &mut db,
            source,
            "/a/zzz_strict.txt",
            "txt",
            "kernel",
            10,
            Some(1),
        );
        let fuzzy = seed(
            &mut db,
            source,
            "/a/aaa_fuzzy.txt",
            "txt",
            &"kernal ".repeat(10),
            999,
            Some(9),
        );

        for sort in [
            SortKey::Relevance,
            SortKey::Path,
            SortKey::Filename,
            SortKey::Size,
            SortKey::ModifiedTime,
            SortKey::HitCount,
        ] {
            let response = search(&db, "kernel", &fuzzy_options(sort)).unwrap();
            assert_eq!(response.total_files, 2, "{sort:?}");
            assert_eq!(response.results[0].file_id, strict, "{sort:?}");
            assert!(response.results[0].strict, "{sort:?}");
            assert_eq!(response.results[1].file_id, fuzzy, "{sort:?}");
            assert!(!response.results[1].strict, "{sort:?}");

            let first_page = search(
                &db,
                "kernel",
                &SearchOptions {
                    limit: 1,
                    ..fuzzy_options(sort)
                },
            )
            .unwrap();
            let second_page = search(
                &db,
                "kernel",
                &SearchOptions {
                    offset: 1,
                    limit: 1,
                    ..fuzzy_options(sort)
                },
            )
            .unwrap();
            assert_eq!(ids(&first_page), vec![strict], "{sort:?}");
            assert_eq!(ids(&second_page), vec![fuzzy], "{sort:?}");
        }

        for mode in [SearchMode::Exact, SearchMode::Substring] {
            let strict_only = search(
                &db,
                "kernel",
                &SearchOptions {
                    mode,
                    ..SearchOptions::default()
                },
            )
            .unwrap();
            assert_eq!(
                ids(&strict_only),
                vec![strict],
                "{mode:?} must not mix fuzzy"
            );
        }
    }

    #[test]
    fn fuzzy_combines_with_and_or_not_and_quoted_phrases() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        let driver = seed(
            &mut db,
            source,
            "/a/kernel_driver.txt",
            "txt",
            "kernel driver",
            14,
            Some(1),
        );
        let windows = seed(
            &mut db,
            source,
            "/a/windows.txt",
            "txt",
            "windows only",
            13,
            Some(1),
        );
        let both = seed(
            &mut db,
            source,
            "/a/kernel_windows.txt",
            "txt",
            "kernel windows",
            15,
            Some(1),
        );
        seed(
            &mut db,
            source,
            "/a/linux_kernel.txt",
            "txt",
            "linux kernel",
            13,
            Some(1),
        );

        let and = search(&db, "kernel drivr", &fuzzy_options(SortKey::Relevance)).unwrap();
        assert_eq!(ids(&and), vec![driver]);

        let or = search(&db, "kernal OR windows", &fuzzy_options(SortKey::Path)).unwrap();
        assert_eq!(or.total_files, 4);
        assert!(or.results[0].strict && or.results[1].strict);
        assert!(!or.results[2].strict && !or.results[3].strict);
        assert!(ids(&or).contains(&windows));
        assert!(ids(&or).contains(&both));

        let not = search(&db, "kernel -windos", &fuzzy_options(SortKey::Path)).unwrap();
        assert_eq!(not.total_files, 3);
        assert!(ids(&not).contains(&both), "negated typo is not expanded");

        let quoted = search(&db, "\"linux kernal\"", &fuzzy_options(SortKey::Path)).unwrap();
        assert!(quoted.results.is_empty());
        let strict_phrase = search(&db, "\"linux kernel\"", &fuzzy_options(SortKey::Path)).unwrap();
        assert_eq!(strict_phrase.total_files, 1);
        assert!(strict_phrase.results[0].strict);
        assert_eq!(strict_phrase.total_hits, 1);
        assert!(!strict_phrase.results[0].hits[0].fuzzy);
    }

    #[test]
    fn fuzzy_mode_keeps_literal_punctuation_and_short_cross_chunk_recall() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        seed(
            &mut db,
            source,
            "/a/code.c",
            "c",
            "#include 100% a_b dma_alloc_coherent()",
            37,
            Some(1),
        );

        for literal in ["#include", "100%", "a_b"] {
            let response = search(&db, literal, &fuzzy_options(SortKey::Relevance)).unwrap();
            assert_eq!(response.total_files, 1, "{literal}");
            let hit = &response.results[0].hits[0];
            assert_eq!(hit.matched_text, literal);
            assert!(!hit.fuzzy);
            assert_eq!(hit.edit_distance, None);
        }

        seed_sections(&mut db, source, "/a/split.txt", &["内", "核"]);
        let phrase = search(&db, "内核", &fuzzy_options(SortKey::Relevance)).unwrap();
        assert_eq!(phrase.total_files, 1);
        let hit = &phrase.results[0].hits[0];
        assert_eq!(hit.matched_text, "内核");
        assert!(hit.crosses_chunk_end);
        assert!(!hit.fuzzy);

        let single = search(&db, "内", &fuzzy_options(SortKey::Relevance)).unwrap();
        assert_eq!(single.total_files, 1);
    }

    #[test]
    fn fuzzy_chinese_character_error_returns_the_original_range() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        seed(
            &mut db,
            source,
            "/a/cn.txt",
            "txt",
            "采集卡寄存器 内核驱动",
            36,
            Some(1),
        );

        let response = search(&db, "内核驱东", &fuzzy_options(SortKey::Relevance)).unwrap();

        assert_eq!(response.total_files, 1);
        let result = &response.results[0];
        assert!(!result.strict);
        assert_eq!(result.hits.len(), 1);
        let hit = &result.hits[0];
        assert_eq!(hit.matched_text, "内核驱动");
        assert_eq!(hit.matched_lexeme.as_deref(), Some("内核驱动"));
        assert_eq!(hit.edit_distance, Some(1));
        assert!(hit.fuzzy);
    }

    #[test]
    fn fuzzy_lexemes_follow_replacement_and_clearing() {
        let mut db = IndexDb::open_in_memory().unwrap();
        let source = db.ensure_source(SourceKind::Local, "/a").unwrap();
        let file = seed(&mut db, source, "/a/one.txt", "txt", "kernel", 6, Some(1));
        assert_eq!(
            search(&db, "kernal", &fuzzy_options(SortKey::Relevance))
                .unwrap()
                .total_files,
            1
        );

        let empty = ExtractedDocument {
            extractor_version: "test".into(),
            encoding: Some("utf-8".into()),
            sections: Vec::new(),
        };
        db.replace_file_content(file, &empty).unwrap();
        assert_eq!(
            search(&db, "kernal", &fuzzy_options(SortKey::Relevance))
                .unwrap()
                .total_files,
            0
        );
        let stats = db.stats().unwrap();
        assert_eq!(stats.lexeme_occurrences, 0);
        assert_eq!(stats.lexemes, 0);

        let replacement = ExtractedDocument {
            extractor_version: "test".into(),
            encoding: Some("utf-8".into()),
            sections: vec![Section {
                text: normalize_search_text("windows"),
                display_text: "windows".into(),
                locator: SourceLocator::Text {
                    start_line: 1,
                    end_line: 1,
                    start_offset: 0,
                    end_offset: 7,
                },
                origin: Origin::Native,
            }],
        };
        db.replace_file_content(file, &replacement).unwrap();
        assert_eq!(
            search(&db, "kernal", &fuzzy_options(SortKey::Relevance))
                .unwrap()
                .total_files,
            0
        );
        assert_eq!(
            search(&db, "windos", &fuzzy_options(SortKey::Relevance))
                .unwrap()
                .total_files,
            1
        );
    }
}
