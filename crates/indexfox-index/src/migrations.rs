use rusqlite::{Connection, TransactionBehavior};

use crate::dictionary;
use crate::{IndexError, Result};

pub const SCHEMA_VERSION: i64 = 5;

const V1: &str = r#"
CREATE TABLE sources (
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL CHECK(kind IN ('local','smb','wsl','removable')),
  root_path TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  online_state TEXT NOT NULL DEFAULT 'unknown',
  scan_mode TEXT NOT NULL DEFAULT 'auto',
  last_success_scan_at TEXT,
  config_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE files (
  id INTEGER PRIMARY KEY,
  source_id INTEGER NOT NULL REFERENCES sources(id),
  path_key TEXT NOT NULL,
  display_path TEXT NOT NULL,
  filename TEXT NOT NULL,
  ext TEXT NOT NULL,
  size_bytes INTEGER NOT NULL,
  mtime_ns INTEGER,
  content_fingerprint TEXT,
  state TEXT NOT NULL,
  indexed_at TEXT,
  error_message TEXT,
  UNIQUE(source_id, path_key)
);
CREATE INDEX idx_files_source ON files(source_id);

CREATE TABLE chunks (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  ordinal INTEGER NOT NULL,
  text TEXT NOT NULL,
  display_text TEXT NOT NULL,
  source_locator_json TEXT NOT NULL,
  origin TEXT NOT NULL,
  UNIQUE(file_id, ordinal)
);
CREATE INDEX idx_chunks_file ON chunks(file_id, ordinal);

CREATE VIRTUAL TABLE chunks_fts USING fts5(
  text, content='chunks', content_rowid='id', tokenize='trigram'
);

CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER chunks_au AFTER UPDATE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
  INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;

CREATE TABLE short_grams (
  gram TEXT NOT NULL,
  chunk_id INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
  PRIMARY KEY(gram, chunk_id)
) WITHOUT ROWID;

CREATE TABLE search_history (
  id INTEGER PRIMARY KEY,
  query_text TEXT NOT NULL,
  search_mode TEXT NOT NULL,
  filters_json TEXT NOT NULL,
  last_used_at TEXT NOT NULL,
  UNIQUE(query_text, search_mode, filters_json)
);

CREATE TABLE index_jobs (
  id INTEGER PRIMARY KEY,
  file_id INTEGER REFERENCES files(id),
  source_id INTEGER NOT NULL REFERENCES sources(id),
  kind TEXT NOT NULL,
  state TEXT NOT NULL,
  attempt_count INTEGER NOT NULL DEFAULT 0,
  requested_at TEXT NOT NULL,
  last_error TEXT
);
CREATE INDEX idx_jobs_state ON index_jobs(state);

CREATE TABLE app_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

const V2: &str = r#"
ALTER TABLE sources ADD COLUMN scan_generation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE files ADD COLUMN seen_generation INTEGER NOT NULL DEFAULT 0;
"#;

const V3: &str = r#"
CREATE TABLE lexemes (
  id INTEGER PRIMARY KEY,
  text TEXT NOT NULL UNIQUE,
  length INTEGER NOT NULL,
  kind TEXT NOT NULL
);
CREATE INDEX idx_lexemes_length ON lexemes(length);

CREATE TABLE lexeme_grams (
  gram TEXT NOT NULL,
  lexeme_id INTEGER NOT NULL REFERENCES lexemes(id) ON DELETE CASCADE,
  PRIMARY KEY(gram, lexeme_id)
) WITHOUT ROWID;
CREATE INDEX idx_lexeme_grams_lexeme ON lexeme_grams(lexeme_id);

CREATE TABLE lexeme_occurrences (
  lexeme_id INTEGER NOT NULL REFERENCES lexemes(id) ON DELETE CASCADE,
  chunk_id INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
  start INTEGER NOT NULL,
  end INTEGER NOT NULL,
  PRIMARY KEY(lexeme_id, chunk_id, start)
) WITHOUT ROWID;
CREATE INDEX idx_lexeme_occurrences_chunk ON lexeme_occurrences(chunk_id);

-- Chunk deletes (including app cache clearing) cascade occurrences; drop
-- dictionary words that no occurrence references any more.
CREATE TRIGGER lexeme_occurrences_ad AFTER DELETE ON lexeme_occurrences
BEGIN
  DELETE FROM lexemes
  WHERE id = old.lexeme_id
    AND NOT EXISTS (
      SELECT 1 FROM lexeme_occurrences WHERE lexeme_id = old.lexeme_id
    );
END;
"#;

pub fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(IndexError::SchemaTooNew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }

    // Concurrent process starts can both observe an outdated version.
    // BEGIN IMMEDIATE serializes them on SQLite's write lock; the winner
    // migrates, and the loser re-reads user_version after acquiring the lock.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(IndexError::SchemaTooNew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    let mut version = version;
    if version == 0 {
        tx.execute_batch(V1)?;
        version = 1;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version < 2 {
        // v2 is additive: existing tables, FTS5 external-content rows and
        // short grams stay untouched.
        tx.execute_batch(V2)?;
        version = 2;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version < 3 {
        // v3 adds the independent fuzzy dictionary and backfills it from
        // chunks already committed by v1/v2. FTS rows and short grams stay.
        tx.execute_batch(V3)?;
        dictionary::backfill(&tx)?;
        version = 3;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version < 4 {
        tx.execute_batch(crate::ocr::OCR_SCHEMA)?;
        version = 4;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version < 5 {
        // The primary key starts with gram. Cascading chunk deletes need
        // an index starting with chunk_id to avoid scanning all short grams.
        tx.execute_batch("CREATE INDEX idx_short_grams_chunk ON short_grams(chunk_id);")?;
        version = 5;
        tx.pragma_update(None, "user_version", version)?;
    }
    debug_assert_eq!(version, SCHEMA_VERSION);
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_upgrade_is_additive_and_preserves_index_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO sources(kind, root_path, online_state) VALUES ('local', '/old', 'online')",
            [],
        )
        .unwrap();
        let source_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO files(source_id, path_key, display_path, filename, ext, size_bytes, state)
             VALUES (?1, 'a.txt', 'a.txt', 'a.txt', 'txt', 10, 'indexed')",
            [source_id],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chunks(file_id, ordinal, text, display_text, source_locator_json, origin)
             VALUES (?1, 0, 'linux kernel', 'linux kernel', '{}', 'native')",
            [file_id],
        )
        .unwrap();
        let chunk_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO short_grams(gram, chunk_id) VALUES ('l', ?1), ('li', ?1)",
            [chunk_id],
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(
            conn.query_row(
                "SELECT scan_generation FROM sources WHERE id = ?1",
                [source_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT seen_generation FROM files WHERE id = ?1",
                [file_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM chunks", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'linux'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM short_grams", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn v2_upgrade_backfills_dictionary_for_committed_chunks() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(V2).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();

        conn.execute(
            "INSERT INTO sources(kind, root_path, online_state) VALUES ('local', '/v2', 'online')",
            [],
        )
        .unwrap();
        let source_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO files(source_id, path_key, display_path, filename, ext, size_bytes, state)
             VALUES (?1, 'a.txt', 'a.txt', 'a.txt', 'txt', 10, 'indexed')",
            [source_id],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();
        let display = "Linux 内核驱动 dma_alloc_coherent";
        conn.execute(
            "INSERT INTO chunks(file_id, ordinal, text, display_text, source_locator_json, origin)
             VALUES (?1, 0, ?2, ?3, '{}', 'native')",
            rusqlite::params![file_id, display.to_ascii_lowercase(), display],
        )
        .unwrap();
        let chunk_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO short_grams(gram, chunk_id) VALUES ('l', ?1), ('li', ?1)",
            [chunk_id],
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM lexemes
                 WHERE text IN ('linux', '内核驱动', 'dma_alloc_coherent', 'coherent')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            4
        );
        assert!(
            conn.query_row(
                "SELECT count(*) FROM lexeme_occurrences WHERE chunk_id = ?1",
                [chunk_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
                > 0
        );
        assert!(
            conn.query_row("SELECT count(*) FROM lexeme_grams", [], |row| row
                .get::<_, i64>(0))
                .unwrap()
                > 0
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'linux'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM short_grams", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn v4_upgrade_indexes_short_grams_by_chunk() {
        let mut conn = Connection::open_in_memory().unwrap();
        for schema in [V1, V2, V3, crate::ocr::OCR_SCHEMA] {
            conn.execute_batch(schema).unwrap();
        }
        conn.pragma_update(None, "user_version", 4).unwrap();

        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();

        let indexed_column: String = conn
            .query_row(
                "SELECT name FROM pragma_index_info('idx_short_grams_chunk') WHERE seqno = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed_column, "chunk_id");
    }
}
