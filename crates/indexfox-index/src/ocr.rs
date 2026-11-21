use std::collections::BTreeMap;

use indexfox_core::{ExtractedDocument, FileState, OcrConfig};
use rusqlite::{params, OptionalExtension, Transaction};

use crate::{IndexDb, NewFile, Result};

pub(crate) const OCR_SCHEMA: &str = "
ALTER TABLE files ADD COLUMN ocr_profile TEXT;

CREATE TABLE ocr_page_cache (
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    cache_key TEXT NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY (file_id, cache_key)
) WITHOUT ROWID;
";

impl IndexDb {
    pub fn ocr_config(&self) -> Result<OcrConfig> {
        let saved: Option<String> = self
            .conn()
            .query_row(
                "SELECT value FROM app_meta WHERE key = 'ocr_settings'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match saved {
            Some(value) => Ok(serde_json::from_str(&value)?),
            None => Ok(OcrConfig::default()),
        }
    }

    pub fn set_ocr_config(&self, config: &OcrConfig) -> Result<()> {
        self.conn().execute(
            "INSERT INTO app_meta(key, value) VALUES ('ocr_settings', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [serde_json::to_string(config)?],
        )?;
        Ok(())
    }

    pub fn pdf_profile_matches(&self, file_id: i64, profile: &str) -> Result<bool> {
        Ok(self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE id = ?1 AND ocr_profile = ?2)",
            params![file_id, profile],
            |row| row.get(0),
        )?)
    }

    pub fn cached_ocr_pages(&self, file_id: i64) -> Result<BTreeMap<String, String>> {
        let mut query = self
            .conn()
            .prepare("SELECT cache_key, text FROM ocr_page_cache WHERE file_id = ?1")?;
        let pages = query.query_map([file_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(pages.collect::<rusqlite::Result<_>>()?)
    }

    /// Publish the PDF index and page cache together, preserving an existing
    /// searchable version when a replacement still has failed OCR pages.
    pub fn store_pdf_index(
        &mut self,
        file: &NewFile,
        doc: Option<&ExtractedDocument>,
        profile: &str,
        pages: &BTreeMap<String, String>,
        failure: Option<&str>,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(self.conn(), rusqlite::TransactionBehavior::Immediate)?;
        let previous: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, state FROM files WHERE source_id = ?1 AND path_key = ?2",
                params![file.source_id, file.path_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let file_id = match previous {
            Some((id, state))
                if failure.is_some()
                    && [FileState::Indexed.as_str(), FileState::Stale.as_str()]
                        .contains(&state.as_str()) =>
            {
                id
            }
            _ => {
                let id = Self::write_file(&tx, file)?;
                let doc = doc.filter(|doc| failure.is_none() || !doc.sections.is_empty());
                Self::write_file_content(&tx, id, doc)?;
                id
            }
        };
        tx.execute("DELETE FROM ocr_page_cache WHERE file_id = ?1", [file_id])?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO ocr_page_cache(file_id, cache_key, text) VALUES (?1, ?2, ?3)",
            )?;
            for (key, text) in pages {
                insert.execute(params![file_id, key, text])?;
            }
        }
        tx.execute(
            "UPDATE files SET ocr_profile = ?2 WHERE id = ?1",
            params![file_id, failure.is_none().then_some(profile)],
        )?;
        if let Some(error) = failure {
            Self::write_file_failure(&tx, file_id, error)?;
        }
        tx.commit()?;
        Ok(())
    }
}
