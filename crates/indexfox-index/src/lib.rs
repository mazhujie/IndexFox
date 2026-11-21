//! SQLite persistence: migrations, atomic file-content replacement, FTS5
//! trigram recall and the one/two-character auxiliary gram index.

mod db;
mod dictionary;
mod grams;
mod migrations;
mod ocr;

pub use db::{
    ChunkRecord, FileRecord, IndexDb, IndexError, IndexJobRecord, LexemeCandidate,
    LexemeOccurrence, NewFile, SearchHistoryRecord, SourceRecord,
};
pub use dictionary::{
    fuzzy_query_tokens, grams_for_lexeme, grams_of_len, lexemes_for_text, LexemeEntry,
    CJK_WINDOW_MAX_CHARS, MAX_CANDIDATE_LEXEMES, MAX_GRAMS_PER_LEXEME, MAX_LEXEMES_PER_CHUNK,
    MAX_LEXEME_CHARS, MAX_QUERY_TOKENS,
};
pub use grams::{bigrams_for_query, grams_for_query, short_grams_for_chunk, trigrams_for_query};

pub type Result<T, E = IndexError> = std::result::Result<T, E>;
