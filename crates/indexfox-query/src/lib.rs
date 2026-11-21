//! Query AST, evaluation and ranking.
//!
//! User input is parsed into an explicit AST. It is never passed to FTS5 as a
//! query language; FTS5 only provides substring candidate recall, and every
//! candidate is verified against stored display text.

mod fuzzy;
mod parser;
mod search;

pub use parser::{parse, Expr, ParseError};
pub use search::{
    search, search_cancellable, search_expr, FileResult, MatchKind, QueryError, SearchHit,
    SearchOptions, SearchResponse,
};
