//! SQLite history, dictionary and meta. The core owns the only write connection;
//! the UI opens its own read-only one.

use crate::event::SessionId;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::path::Path;

pub const OK: &str = "ok";
pub const PROVISIONAL: &str = "provisional";
pub const FAILED: &str = "failed";
/// `source` of rows this app created itself; `source_id` is then the session id.
pub const NATIVE: &str = "dictap";
const TOMB: &str = "tomb:";

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
  source      TEXT    NOT NULL DEFAULT 'dictap',
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

/// A row to insert.
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

    /// A row with no native identity (tests only; production rows go through
    /// `insert_dictation_once`).
    #[cfg(test)]
    pub fn insert(&self, r: &NewRow<'_>) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO transcriptions (created_ms, text, duration_ms, model, status, error, audio_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![r.created_ms, r.text, r.duration_ms, r.model, r.status, r.error, r.audio_path],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Inserts the row for a dictation session exactly once. Returns the row id and whether
    /// this call created it; a duplicate completion finds the first row and changes nothing.
    pub fn insert_dictation_once(
        &self,
        session: SessionId,
        r: &NewRow<'_>,
    ) -> rusqlite::Result<(i64, bool)> {
        self.insert_dictation_with_audio_rule(session, r, false)
    }

    pub fn insert_dictation_with_audio_rule(
        &self,
        session: SessionId,
        r: &NewRow<'_>,
        keep_audio: bool,
    ) -> rusqlite::Result<(i64, bool)> {
        let tx = self.conn.unchecked_transaction()?;
        let sid = session_i64(session)?;
        let n = tx.execute(
            "INSERT INTO transcriptions (created_ms, text, duration_ms, model, status, error, audio_path, source, source_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(source, source_id) DO NOTHING",
            params![
                r.created_ms,
                r.text,
                r.duration_ms,
                r.model,
                r.status,
                r.error,
                r.audio_path,
                NATIVE,
                sid
            ],
        )?;
        if n == 1 {
            let row = tx.last_insert_rowid();
            if keep_audio {
                tx.execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, 'keep')",
                    [format!("audio-rule:{sid}")],
                )?;
            }
            tx.commit()?;
            return Ok((row, true));
        }
        let id = tx.query_row(
            "SELECT id FROM transcriptions WHERE source = ?1 AND source_id = ?2",
            params![NATIVE, sid],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok((id, false))
    }

    pub fn tombstone_audio(&self, path: &Path) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        write_tomb(&tx, path)?;
        tx.commit()
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

    pub fn native_row(&self, session: SessionId) -> rusqlite::Result<Option<Row>> {
        self.conn
            .query_row(
                &format!("SELECT {COLS} FROM transcriptions WHERE source = ?1 AND source_id = ?2"),
                params![NATIVE, session_i64(session)?],
                row,
            )
            .optional()
    }

    /// The highest native session id ever stored (0 if none): the allocator's floor.
    pub fn max_native_session(&self) -> rusqlite::Result<i64> {
        self.conn.query_row(
            "SELECT COALESCE(MAX(source_id), 0) FROM transcriptions WHERE source = ?1",
            [NATIVE],
            |r| r.get(0),
        )
    }

    /// The row whose kept audio is exactly this path.
    pub fn row_for_audio(&self, path: &Path) -> rusqlite::Result<Option<Row>> {
        self.conn
            .query_row(
                &format!("SELECT {COLS} FROM transcriptions WHERE audio_path = ?1 LIMIT 1"),
                [path_str(path)],
                row,
            )
            .optional()
    }

    /// Points a row at the file's new location, only if it still points at `expected`.
    pub fn set_audio_path(
        &self,
        id: i64,
        expected: &Path,
        actual: &Path,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "UPDATE transcriptions SET audio_path = ?3 WHERE id = ?1 AND audio_path = ?2",
            params![id, path_str(expected), path_str(actual)],
        )?;
        Ok(n == 1)
    }

    pub fn clear_audio_if_matches(&self, id: i64, expected: &Path) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "UPDATE transcriptions SET audio_path = NULL WHERE id = ?1 AND audio_path = ?2",
            params![id, path_str(expected)],
        )?;
        Ok(n == 1)
    }

    /// Reattaches an existing file to a row that has lost its reference.
    pub fn link_audio_if_unset(&self, id: i64, path: &Path) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "UPDATE transcriptions SET audio_path = ?2 WHERE id = ?1 AND audio_path IS NULL",
            params![id, path_str(path)],
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

    /// A successful Retry: new text and model, audio released. Applies only while the row
    /// still holds the audio the Retry read, and leaves a tombstone for that file so a crash
    /// before its deletion cannot turn it back into a recovered dictation. Returns whether
    /// the row was updated.
    pub fn apply_retry(
        &mut self,
        id: i64,
        expected_audio: &Path,
        text: &str,
        model: &str,
    ) -> rusqlite::Result<bool> {
        let tx = self.conn.transaction()?;
        let n = tx.execute(
            "UPDATE transcriptions SET text = ?3, model = ?4, status = ?5, error = NULL, audio_path = NULL
             WHERE id = ?1 AND audio_path = ?2",
            params![id, path_str(expected_audio), text, model, OK],
        )?;
        if n == 1 {
            write_tomb(&tx, expected_audio)?;
        }
        tx.commit()?;
        Ok(n == 1)
    }

    /// A failed Retry: keep text, status and audio, record why.
    pub fn set_retry_error(
        &self,
        id: i64,
        expected_audio: &Path,
        error: &str,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "UPDATE transcriptions SET error = ?3 WHERE id = ?1 AND audio_path = ?2",
            params![id, path_str(expected_audio), error],
        )?;
        Ok(n == 1)
    }

    /// Late text for a dictation that was already stored empty (cancelled or timed out).
    /// Never overwrites text; the row becomes provisional because nothing confirmed it.
    pub fn apply_late_text(
        &self,
        id: i64,
        text: &str,
        model: &str,
        error: Option<&str>,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "UPDATE transcriptions SET text = ?2, model = ?3, status = ?4, error = ?5
             WHERE id = ?1 AND text = ''",
            params![id, text, model, PROVISIONAL, error],
        )?;
        Ok(n == 1)
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

    pub fn latest_text(&self) -> rusqlite::Result<Option<Row>> {
        self.conn.query_row(
            &format!("SELECT {COLS} FROM transcriptions WHERE trim(text) != '' ORDER BY created_ms DESC, id DESC LIMIT 1"),
            [], row,
        ).optional()
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

    /// Deletes a row and, in the same transaction, tombstones its kept audio so restart
    /// recovery cannot resurrect it. Returns the audio path (the caller deletes the file,
    /// then clears the tombstone).
    pub fn delete(&mut self, id: i64) -> rusqlite::Result<Option<String>> {
        let tx = self.conn.transaction()?;
        let path: Option<String> = tx
            .query_row(
                "SELECT audio_path FROM transcriptions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(p) = &path {
            write_tomb(&tx, Path::new(p))?;
        }
        tx.execute("DELETE FROM transcriptions WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(path)
    }

    /// How many rows were created before `cutoff_ms`.
    pub fn count_before(&self, cutoff_ms: i64) -> rusqlite::Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM transcriptions WHERE created_ms < ?1",
            [cutoff_ms],
            |r| r.get(0),
        )
    }

    /// Deletes rows created before `cutoff_ms`, tombstoning their kept audio in the same
    /// transaction, and returns those paths (the caller deletes the files).
    pub fn delete_before(&mut self, cutoff_ms: i64) -> rusqlite::Result<Vec<String>> {
        let tx = self.conn.transaction()?;
        let paths = tx
            .prepare(
                "SELECT audio_path FROM transcriptions WHERE created_ms < ?1 AND audio_path IS NOT NULL",
            )?
            .query_map([cutoff_ms], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        for p in &paths {
            write_tomb(&tx, Path::new(p))?;
        }
        tx.execute(
            "DELETE FROM transcriptions WHERE created_ms < ?1",
            [cutoff_ms],
        )?;
        tx.commit()?;
        Ok(paths)
    }

    /// Files whose rows were deleted but whose removal hasn't been confirmed: (stem, path).
    pub fn tombstones(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM meta WHERE key LIKE 'tomb:%'")?;
        stmt.query_map([], |r| {
            let key: String = r.get(0)?;
            Ok((key[TOMB.len()..].to_string(), r.get(1)?))
        })?
        .collect()
    }

    pub fn clear_tombstone(&self, stem: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM meta WHERE key = ?1", [format!("{TOMB}{stem}")])?;
        Ok(())
    }

    /// Rows holding kept audio, oldest first (for retention).
    pub fn kept_audio(&self) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, audio_path FROM transcriptions WHERE audio_path IS NOT NULL ORDER BY created_ms, id",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
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

fn session_i64(session: SessionId) -> rusqlite::Result<i64> {
    i64::try_from(session.0).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Records that the file at `path` belongs to a deleted row, keyed by its file stem.
fn write_tomb(conn: &Connection, path: &Path) -> rusqlite::Result<()> {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return Ok(());
    };
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![format!("{TOMB}{stem}"), path_str(path)],
    )?;
    Ok(())
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
pub mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh store in its own temp directory (returned so callers can put files beside it).
    pub fn temp_store() -> (Store, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dictap-store-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(&dir.join("t.db")).unwrap(), dir)
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
        let (mut s, dir) = temp_store();
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
        assert_eq!(s.delete(a).unwrap(), None);
        assert!(s.get(a).unwrap().is_none());
        let reader = Store::open_read(&dir.join("t.db")).unwrap();
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

    #[test]
    fn native_insert_is_once_and_never_overwrites() {
        let (s, _) = temp_store();
        let id = SessionId(1_700_000_000_000);
        let (a, fresh) = s.insert_dictation_once(id, &new("first", 5)).unwrap();
        assert!(fresh);
        let (b, fresh) = s.insert_dictation_once(id, &new("second", 5)).unwrap();
        assert!(!fresh);
        assert_eq!(a, b);
        assert_eq!(s.get(a).unwrap().unwrap().text, "first");
        assert_eq!(s.native_row(id).unwrap().unwrap().id, a);
        assert_eq!(s.max_native_session().unwrap(), 1_700_000_000_000);
        assert!(s.native_row(SessionId(3)).unwrap().is_none());
        assert!(
            s.insert_dictation_once(SessionId(u64::MAX), &new("x", 1))
                .is_err()
        );
    }

    #[test]
    fn audio_path_updates_are_guarded() {
        let (s, _) = temp_store();
        let (a, b) = (Path::new("a.wav"), Path::new("b.wav"));
        let (id, _) = s
            .insert_dictation_once(
                SessionId(9),
                &NewRow {
                    audio_path: Some("a.wav"),
                    ..new("t", 1)
                },
            )
            .unwrap();
        assert_eq!(s.row_for_audio(a).unwrap().unwrap().id, id);
        assert!(
            !s.set_audio_path(id, b, a).unwrap(),
            "expected path differs"
        );
        assert!(s.set_audio_path(id, a, b).unwrap());
        assert!(!s.clear_audio_if_matches(id, a).unwrap());
        assert!(s.clear_audio_if_matches(id, b).unwrap());
        assert!(s.link_audio_if_unset(id, a).unwrap());
        assert!(!s.link_audio_if_unset(id, b).unwrap(), "already linked");
    }

    #[test]
    fn delete_tombstones_audio_atomically() {
        let (mut s, _) = temp_store();
        let id = s
            .insert(&NewRow {
                audio_path: Some("failed/123.wav"),
                status: FAILED,
                ..new("", 1)
            })
            .unwrap();
        assert_eq!(s.delete(id).unwrap().as_deref(), Some("failed/123.wav"));
        assert_eq!(
            s.tombstones().unwrap(),
            [("123".to_string(), "failed/123.wav".to_string())]
        );
        s.clear_tombstone("123").unwrap();
        assert!(s.tombstones().unwrap().is_empty());
        let id = s
            .insert(&NewRow {
                audio_path: Some("failed/5.wav"),
                status: FAILED,
                ..new("", 1)
            })
            .unwrap();
        s.insert(&new("newer", 100)).unwrap();
        assert_eq!(s.delete_before(50).unwrap(), ["failed/5.wav"]);
        assert!(s.get(id).unwrap().is_none());
        assert_eq!(s.tombstones().unwrap().len(), 1);
    }

    #[test]
    fn retry_applies_only_to_the_audio_it_read() {
        let (mut s, _) = temp_store();
        let wav = Path::new("failed/7.wav");
        let id = s
            .insert(&NewRow {
                audio_path: Some("failed/7.wav"),
                status: FAILED,
                ..new("", 1)
            })
            .unwrap();
        assert!(!s.apply_retry(id, Path::new("other.wav"), "t", "m").unwrap());
        assert!(s.set_retry_error(id, wav, "offline").unwrap());
        assert!(s.apply_retry(id, wav, "hello", "m").unwrap());
        let r = s.get(id).unwrap().unwrap();
        assert_eq!(
            (r.text.as_str(), r.status.as_str(), r.model.as_deref()),
            ("hello", OK, Some("m"))
        );
        assert_eq!((r.error, r.audio_path), (None, None));
        assert_eq!(s.tombstones().unwrap().len(), 1, "file deletion is owed");
        assert!(!s.apply_retry(id, wav, "again", "m").unwrap());
        s.delete(id).unwrap();
        assert!(
            !s.apply_retry(id, wav, "late", "m").unwrap(),
            "no resurrection"
        );
    }

    #[test]
    fn late_text_never_overwrites() {
        let (s, _) = temp_store();
        let (id, _) = s
            .insert_dictation_once(
                SessionId(4),
                &NewRow {
                    status: FAILED,
                    error: Some("Cancelled"),
                    ..new("", 1)
                },
            )
            .unwrap();
        assert!(s.apply_late_text(id, "words", "m", None).unwrap());
        let r = s.get(id).unwrap().unwrap();
        assert_eq!((r.text.as_str(), r.status.as_str()), ("words", PROVISIONAL));
        assert!(!s.apply_late_text(id, "other", "m", None).unwrap());
        assert_eq!(s.get(id).unwrap().unwrap().text, "words");
    }
}
