//! SQLite persistence foundation.
//!
//! - WAL journal mode, foreign keys enforced.
//! - `PRAGMA user_version` drives migrations; v1 is `schema.sql`.
//! - Phase 0 provides open/migrate only. Query APIs arrive with the
//!   features that need them (Phase 1: library scanner writes; Phase 2:
//!   playback-state persistence).

use std::fs;
use std::path::Path;

use rusqlite::Connection;

use crate::error::DbError;

pub const SCHEMA_VERSION: u32 = 2;
const V1_SQL: &str = include_str!("schema.sql");
const V2_SQL: &str = include_str!("migrations/v2.sql");

pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open (creating if necessary) the database at `path` and migrate it
    /// to the current schema version.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| DbError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// In-memory database for tests.
    pub fn open_in_memory() -> Result<Self, DbError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, DbError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Apply migrations sequentially inside transactions until the database
    /// reaches `SCHEMA_VERSION`. Never destructive: every migration must
    /// preserve existing data (see ARCHITECTURE.md §8).
    fn migrate(&self) -> Result<(), DbError> {
        loop {
            let version: u32 = self
                .conn
                .pragma_query_value(None, "user_version", |row| row.get(0))?;

            match version {
                0 => {
                    self.conn
                        .execute_batch(&format!("BEGIN;\n{V1_SQL}\nCOMMIT;"))?;
                    self.conn.pragma_update(None, "user_version", 1)?;
                }
                1 => {
                    self.conn
                        .execute_batch(&format!("BEGIN;\n{V2_SQL}\nCOMMIT;"))?;
                    self.conn.pragma_update(None, "user_version", 2)?;
                }
                v if v == SCHEMA_VERSION => return Ok(()),
                v => {
                    return Err(DbError::UnsupportedVersion {
                        found: v,
                        supported: SCHEMA_VERSION,
                    })
                }
            }
        }
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Mutable access, for the rare operation that needs its own transaction
    /// (e.g. a playlist reorder that rewrites a UNIQUE column).
    ///
    /// Callers must hold the lock for the whole borrow — this is why the app
    /// shell keeps a single `Mutex<Database>` rather than several.
    pub fn connection_mut(&mut self) -> Option<&mut Connection> {
        Some(&mut self.conn)
    }

    /// Hand over the raw connection (used by the library writer thread,
    /// which owns its connection for the lifetime of a scan).
    pub fn into_connection(self) -> Connection {
        self.conn
    }

    pub fn schema_version(&self) -> Result<u32, DbError> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_is_migrated_to_current_version() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn all_tables_exist_at_v2() {
        let db = Database::open_in_memory().unwrap();
        let mut stmt = db
            .connection()
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        for expected in [
            "albums",
            "artists",
            "artworks",
            "genres",
            "history",
            "playback_state",
            "playlist_entries",
            "playlists",
            "settings",
            "track_genres",
            "tracks",
        ] {
            assert!(tables.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[test]
    fn v2_adds_track_columns() {
        let db = Database::open_in_memory().unwrap();
        let mut stmt = db
            .connection()
            .prepare("PRAGMA table_info(tracks)")
            .unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(columns.contains(&"artwork_hash".to_string()));
        assert!(columns.contains(&"missing_since".to_string()));
    }

    #[test]
    fn v1_to_v2_migration_preserves_existing_data() {
        // Build a v1 database by hand, populate it, then let Database::init
        // run the v1→v2 migration.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_SQL).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO artists (name, name_normalized) VALUES ('Aphex Twin', 'aphex twin')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tracks (path, title, title_normalized, duration_ms) \
             VALUES ('/music/a.flac', 'Xtal', 'xtal', 294000)",
            [],
        )
        .unwrap();

        let db = Database::init(conn).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        let artist: String = db
            .connection()
            .query_row("SELECT name FROM artists WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(artist, "Aphex Twin");

        let (title, missing): (String, Option<i64>) = db
            .connection()
            .query_row(
                "SELECT title, missing_since FROM tracks WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(title, "Xtal");
        assert_eq!(missing, None);
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let db = Database::open_in_memory().unwrap();
        let result = db.connection().execute(
            "INSERT INTO tracks (path, title, title_normalized, album_id) \
             VALUES ('/x.flac', 'X', 'x', 999)",
            [],
        );
        assert!(result.is_err(), "orphan album_id must be rejected");
    }

    #[test]
    fn track_path_is_unique() {
        let db = Database::open_in_memory().unwrap();
        let insert = || {
            db.connection().execute(
                "INSERT INTO tracks (path, title, title_normalized) \
                 VALUES ('/a.flac', 'A', 'a')",
                [],
            )
        };
        insert().unwrap();
        assert!(insert().is_err());
    }

    #[test]
    fn migration_is_idempotent() {
        let db = Database::open_in_memory().unwrap();
        db.migrate().unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn newer_version_is_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        match Database::init(conn) {
            Err(DbError::UnsupportedVersion { found, supported }) => {
                assert_eq!(found, SCHEMA_VERSION + 1);
                assert_eq!(supported, SCHEMA_VERSION);
            }
            Ok(_) => panic!("expected UnsupportedVersion, got Ok"),
            Err(e) => panic!("expected UnsupportedVersion, got {e}"),
        }
    }
}
