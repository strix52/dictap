//! SQLite history, dictionary and meta. The core owns the only write connection;
//! the UI opens its own read-only one.

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::path::Path;

pub const OK: &str = "ok";
pub const PROVISIONAL: &str = "provisional";
pub const FAILED: &str = "failed";

const SCHEMA_V1: &str = "
CREATE TABLE transcriptions (
  id          INTEGER PRIMARY KEY,
  created_ms  INTEGER NOT NULL,
  text        TEXT    NOT NULL DEFAULT '',
  duration_ms INTEGER,
  model       TEXT,
  status      TEXT    NOT NULL,
  error       TEXT,
  paste       TEXT,
  audio_path  TEXT,
  source      TEXT    NOT NULL DEFAULT 'gemdict',
  source_id   INTEGER,
  UNIQUE(source, source_id)
);
CREATE INDEX transcriptions_created ON transcriptions(created_ms DESC);
CREATE TABLE dictionary (word TEXT PRIMARY KEY COLLATE NOCASE);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
";

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: i64,
    pub created_ms: i64,
    pub text: String,
    pub duration_ms: Option<i64>,
    pub model: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub paste: Option<String>,
    pub audio_path: Option<String>,
}

/// A row to insert. `source_id` is set only for imported rows.
#[derive(Default)]
pub struct NewRow<'a> {
    pub created_ms: i64,
    pub text: &'a str,
    pub duration_ms: Option<i64>,
    pub model: Option<&'a str>,
    pub status: &'a str,
    pub error: Option<&'a str>,
    pub audio_path: Option<&'a str>,
}

const COLS: &str = "id, created_ms, text, duration_ms, model, status, error, paste, audio_path";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        created_ms: r.get(1)?,
        text: r.get(2)?,
        duration_ms: r.get(3)?,
        model: r.get(4)?,
        status: r.get(5)?,
        error: r.get(6)?,
        paste: r.get(7)?,
        audio_path: r.get(8)?,
    })
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating and migrating if needed) the write connection.
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(&format!(
                "BEGIN; {SCHEMA_V1} PRAGMA user_version = 1; COMMIT;"
            ))?;
        }
        Ok(Store { conn })
    }

    /// Read-only connection for the UI thread.
    pub fn open_read(path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        Ok(Store { conn })
    }

    pub fn insert(&self, r: &NewRow<'_>) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO transcriptions (created_ms, text, duration_ms, model, status, error, audio_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![r.created_ms, r.text, r.duration_ms, r.model, r.status, r.error, r.audio_path],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Imported row; returns false if it was already imported.
    pub fn insert_imported(
        &self,
        source: &str,
        source_id: i64,
        r: &NewRow<'_>,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO transcriptions (created_ms, text, duration_ms, model, status, error, source, source_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![r.created_ms, r.text, r.duration_ms, r.model, r.status, r.error, source, source_id],
        )?;
        Ok(n == 1)
    }

    pub fn set_paste(&self, id: i64, paste: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE transcriptions SET paste = ?2 WHERE id = ?1",
            params![id, paste],
        )?;
        Ok(())
    }

    /// Records a retry outcome. Text is only replaced when the retry produced some.
    pub fn set_result(
        &self,
        id: i64,
        text: Option<&str>,
        status: &str,
        error: Option<&str>,
        audio_path: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE transcriptions SET text = COALESCE(?2, text), status = ?3, error = ?4, audio_path = ?5 WHERE id = ?1",
            params![id, text, status, error, audio_path],
        )?;
        Ok(())
    }

    pub fn get(&self, id: i64) -> rusqlite::Result<Option<Row>> {
        self.conn
            .query_row(
                &format!("SELECT {COLS} FROM transcriptions WHERE id = ?1"),
                [id],
                row,
            )
            .optional()
    }

    /// Newest first. An empty query lists everything.
    pub fn search(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<Row>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {COLS} FROM transcriptions WHERE ?1 = '' OR text LIKE ?2 ESCAPE '\\'
             ORDER BY created_ms DESC, id DESC LIMIT ?3"
        ))?;
        let q = query.trim();
        let pattern = format!("%{}%", like_escape(q));
        stmt.query_map(params![q, pattern, limit as i64], row)?
            .collect()
    }

    /// Deletes a row, returning its kept audio path (the caller deletes the file).
    pub fn delete(&self, id: i64) -> rusqlite::Result<Option<String>> {
        let path: Option<String> = self
            .conn
            .query_row(
                "SELECT audio_path FROM transcriptions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        self.conn
            .execute("DELETE FROM transcriptions WHERE id = ?1", [id])?;
        Ok(path)
    }

    /// Rows holding kept audio, oldest first (for retention).
    pub fn kept_audio(&self) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, audio_path FROM transcriptions WHERE audio_path IS NOT NULL ORDER BY created_ms, id",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }

    pub fn clear_audio(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE transcriptions SET audio_path = NULL WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    pub fn dictionary(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT word FROM dictionary ORDER BY word")?;
        stmt.query_map([], |r| r.get(0))?.collect()
    }

    pub fn set_dictionary(&mut self, words: &[String]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM dictionary", [])?;
        for w in words.iter().map(|w| w.trim()).filter(|w| !w.is_empty()) {
            tx.execute("INSERT OR IGNORE INTO dictionary (word) VALUES (?1)", [w])?;
        }
        tx.commit()
    }

    pub fn add_words(&self, words: &[String]) -> rusqlite::Result<usize> {
        let mut added = 0;
        for w in words.iter().map(|w| w.trim()).filter(|w| !w.is_empty()) {
            added += self
                .conn
                .execute("INSERT OR IGNORE INTO dictionary (word) VALUES (?1)", [w])?;
        }
        Ok(added)
    }

    pub fn meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            [key, value],
        )?;
        Ok(())
    }

    /// Runs `f` in a transaction (used by the importer).
    pub fn in_tx<T>(
        &mut self,
        f: impl FnOnce(&Store) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        self.conn.execute_batch("BEGIN")?;
        match f(self) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                self.conn.execute_batch("ROLLBACK").ok();
                Err(e)
            }
        }
    }
}

/// Escapes `%`, `_` and `\` for `LIKE … ESCAPE '\'`.
pub fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "gemdict-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let _ = std::fs::remove_file(&path);
        (Store::open(&path).unwrap(), path)
    }

    fn new(text: &str, created_ms: i64) -> NewRow<'_> {
        NewRow {
            created_ms,
            text,
            status: OK,
            ..Default::default()
        }
    }

    #[test]
    fn insert_search_delete() {
        let (s, path) = temp_store();
        let a = s.insert(&new("hello world", 1)).unwrap();
        s.insert(&new("100% sure_thing", 2)).unwrap();
        let f = s
            .insert(&NewRow {
                status: FAILED,
                error: Some("offline"),
                audio_path: Some("x.wav"),
                ..new("", 3)
            })
            .unwrap();
        assert_eq!(
            s.search("", 10)
                .unwrap()
                .iter()
                .map(|r| r.created_ms)
                .collect::<Vec<_>>(),
            [3, 2, 1]
        );
        assert_eq!(s.search("WORLD", 10).unwrap()[0].id, a);
        assert_eq!(s.search("0%", 10).unwrap().len(), 1);
        assert_eq!(s.search("e_t", 10).unwrap().len(), 1);
        assert_eq!(s.search("o_w", 10).unwrap().len(), 0, "_ is literal");
        s.set_paste(a, "attempted").unwrap();
        assert_eq!(
            s.get(a).unwrap().unwrap().paste.as_deref(),
            Some("attempted")
        );
        assert_eq!(s.kept_audio().unwrap(), vec![(f, "x.wav".to_string())]);
        s.set_result(f, Some("recovered"), OK, None, None).unwrap();
        let r = s.get(f).unwrap().unwrap();
        assert_eq!(
            (r.text.as_str(), r.status.as_str(), r.audio_path),
            ("recovered", OK, None)
        );
        assert_eq!(s.delete(a).unwrap(), None);
        assert!(s.get(a).unwrap().is_none());
        let reader = Store::open_read(&path).unwrap();
        assert_eq!(reader.search("", 10).unwrap().len(), 2);
    }

    #[test]
    fn import_is_idempotent_and_dictionary_dedups() {
        let (mut s, _) = temp_store();
        assert!(s.insert_imported("openwhispr", 7, &new("a", 1)).unwrap());
        assert!(!s.insert_imported("openwhispr", 7, &new("a", 1)).unwrap());
        assert_eq!(
            s.add_words(&["Orca".into(), "orca".into(), " ".into(), "Gemini".into()])
                .unwrap(),
            2
        );
        s.set_dictionary(&["B".into(), "a".into()]).unwrap();
        assert_eq!(s.dictionary().unwrap(), ["a", "B"]);
        s.set_meta("k", "v").unwrap();
        assert_eq!(s.meta("k").unwrap().as_deref(), Some("v"));
        let r: rusqlite::Result<()> = s.in_tx(|s| {
            s.insert(&new("rolled back", 9))?;
            Err(rusqlite::Error::InvalidQuery)
        });
        assert!(r.is_err());
        assert_eq!(s.search("rolled", 10).unwrap().len(), 0);
    }
}
