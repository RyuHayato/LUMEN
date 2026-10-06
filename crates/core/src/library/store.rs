//! Library persistence: the single-writer layer between scan results and
//! SQLite. All writes happen here, in caller-controlled transactions.
//!
//! Write rules (data-safety contract):
//! - Batched transactions; a crash between batches leaves a consistent DB.
//! - Tracks are upserted by path; `added_at` is never overwritten.
//! - Missing files are soft-marked (`missing_since`), never deleted.
//! - Move/rename relinks delete the freshly inserted duplicate row (safe: it
//!   was created by the current scan and carries no user data) and repoint
//!   the identity-rich old row.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, Transaction};

use crate::db::Database;
use crate::error::DbError;
use crate::library::artwork::StoredArtwork;
use crate::library::metadata::{normalize, ExtractedMetadata};
use crate::library::probe::AudioProperties;

/// One fully processed file, ready to persist.
pub struct TrackRecord {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub mtime_ms: i64,
    pub meta: ExtractedMetadata,
    pub props: AudioProperties,
    pub artwork: Option<StoredArtwork>,
}

/// What the DB knows about a path (the scan-start snapshot row).
#[derive(Debug, Clone, Copy)]
pub struct FileSnapshot {
    pub id: i64,
    pub size_bytes: u64,
    pub mtime_ms: i64,
    pub missing_since: Option<i64>,
}

pub struct LibraryStore {
    conn: Connection,
}

impl LibraryStore {
    pub fn open(db_path: &Path) -> Result<Self, DbError> {
        Ok(Self {
            conn: Database::open(db_path)?.into_connection(),
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Load id/size/mtime/missing for every track. Memory is bounded by
    /// library size (paths); this is what makes classification O(1) per file
    /// instead of one query per file.
    pub fn snapshot(&self) -> Result<HashMap<PathBuf, FileSnapshot>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, file_size, file_mtime, missing_since FROM tracks")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                FileSnapshot {
                    id: row.get(0)?,
                    size_bytes: row.get::<_, i64>(2)? as u64,
                    mtime_ms: row.get(3)?,
                    missing_since: row.get(4)?,
                },
            ))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, snap) = row?;
            map.insert(PathBuf::from(path), snap);
        }
        Ok(map)
    }

    /// Run `f` inside one immediate transaction.
    pub fn in_transaction<T>(
        &mut self,
        f: impl FnOnce(&Transaction) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let tx = self.conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}

// --- Write operations (all take &Transaction; batching is the caller's job) ---

fn get_or_create_artist(tx: &Transaction, name: &str) -> Result<i64, DbError> {
    let normalized = normalize(name);
    let existing = tx
        .prepare_cached("SELECT id FROM artists WHERE name_normalized = ?1")?
        .query_row([&normalized], |row| row.get(0))
        .ok();
    if let Some(id) = existing {
        return Ok(id);
    }
    tx.prepare_cached("INSERT INTO artists (name, name_normalized) VALUES (?1, ?2)")?
        .execute(params![name, normalized])?;
    Ok(tx.last_insert_rowid())
}

fn get_or_create_album(
    tx: &Transaction,
    title: &str,
    album_artist_id: Option<i64>,
    year: Option<i32>,
) -> Result<i64, DbError> {
    let normalized = normalize(title);
    // `IS ?` is the NULL-safe comparison: two albums with unknown artist and
    // the same title still merge into one row.
    let existing = tx
        .prepare_cached(
            "SELECT id FROM albums WHERE title_normalized = ?1 AND album_artist_id IS ?2",
        )?
        .query_row(params![normalized, album_artist_id], |row| row.get(0))
        .ok();
    if let Some(id) = existing {
        return Ok(id);
    }
    tx.prepare_cached(
        "INSERT INTO albums (title, title_normalized, album_artist_id, year) \
         VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute(params![title, normalized, album_artist_id, year])?;
    Ok(tx.last_insert_rowid())
}

fn store_artwork(tx: &Transaction, artwork: &StoredArtwork) -> Result<(), DbError> {
    tx.prepare_cached(
        "INSERT OR IGNORE INTO artworks (hash, path, mime, source, size_bytes) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![
        artwork.hash,
        artwork.rel_path,
        artwork.mime,
        artwork.source.as_str(),
        artwork.size_bytes as i64,
    ])?;
    Ok(())
}

/// Insert or refresh one track. Returns the track id.
pub fn upsert_track(tx: &Transaction, rec: &TrackRecord) -> Result<i64, DbError> {
    if let Some(artwork) = &rec.artwork {
        store_artwork(tx, artwork)?;
    }

    let artist_id = match &rec.meta.artist {
        Some(name) => Some(get_or_create_artist(tx, name)?),
        None => None,
    };

    let album_id = match &rec.meta.album {
        Some(title) => {
            // Album grouping: album artist tag if present, else the track's
            // artist (documented fallback), else unknown (NULL).
            let album_artist_id = match rec.meta.album_artist.as_ref().or(rec.meta.artist.as_ref())
            {
                Some(name) => Some(get_or_create_artist(tx, name)?),
                None => None,
            };
            Some(get_or_create_album(
                tx,
                title,
                album_artist_id,
                rec.meta.year,
            )?)
        }
        None => None,
    };

    // Title fallback: file name stem. Nothing else is invented.
    let title = rec.meta.title.clone().unwrap_or_else(|| {
        rec.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| rec.path.display().to_string())
    });
    let title_normalized = normalize(&title);

    let p = &rec.props;
    tx.prepare_cached(
        "INSERT INTO tracks (
            path, title, title_normalized, album_id, artist_id, track_no, disc_no,
            duration_ms, codec, container, sample_rate_hz, bit_depth, channels,
            file_size, file_mtime, artwork_hash, missing_since, added_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
            NULL, unixepoch(), unixepoch()
        )
        ON CONFLICT(path) DO UPDATE SET
            title = excluded.title,
            title_normalized = excluded.title_normalized,
            album_id = excluded.album_id,
            artist_id = excluded.artist_id,
            track_no = excluded.track_no,
            disc_no = excluded.disc_no,
            duration_ms = excluded.duration_ms,
            codec = excluded.codec,
            container = excluded.container,
            sample_rate_hz = excluded.sample_rate_hz,
            bit_depth = excluded.bit_depth,
            channels = excluded.channels,
            file_size = excluded.file_size,
            file_mtime = excluded.file_mtime,
            artwork_hash = excluded.artwork_hash,
            missing_since = NULL,
            updated_at = unixepoch()",
    )?
    .execute(params![
        rec.path.to_string_lossy(),
        title,
        title_normalized,
        album_id,
        artist_id,
        rec.meta.track_no,
        rec.meta.disc_no,
        p.duration_ms.map(|v| v as i64),
        p.codec,
        p.container,
        p.sample_rate_hz,
        p.bit_depth,
        p.channels,
        rec.size_bytes as i64,
        rec.mtime_ms,
        rec.artwork.as_ref().map(|a| &a.hash),
    ])?;

    let track_id: i64 = tx.query_row(
        "SELECT id FROM tracks WHERE path = ?1",
        params![rec.path.to_string_lossy()],
        |row| row.get(0),
    )?;

    // Genres: replace the set (DELETE + INSERT is cheaper than diffing at
    // this cardinality).
    tx.prepare_cached("DELETE FROM track_genres WHERE track_id = ?1")?
        .execute(params![track_id])?;
    for genre in &rec.meta.genres {
        tx.prepare_cached("INSERT OR IGNORE INTO genres (name) VALUES (?1)")?
            .execute(params![genre])?;
        let genre_id: i64 = tx.query_row(
            "SELECT id FROM genres WHERE name = ?1 COLLATE NOCASE",
            params![genre],
            |row| row.get(0),
        )?;
        tx.prepare_cached(
            "INSERT OR IGNORE INTO track_genres (track_id, genre_id) VALUES (?1, ?2)",
        )?
        .execute(params![track_id, genre_id])?;
    }

    Ok(track_id)
}

/// Repoint an existing track row at a new path (move/rename reconciliation).
/// Deletes the duplicate row the current scan inserted at `new_path` first —
/// that row was created moments ago and carries no user data.
pub fn relink_path(tx: &Transaction, old_id: i64, new_path: &Path) -> Result<(), DbError> {
    let new_path = new_path.to_string_lossy();
    tx.prepare_cached("DELETE FROM tracks WHERE path = ?1 AND id != ?2")?
        .execute(params![new_path, old_id])?;
    tx.prepare_cached(
        "UPDATE tracks SET path = ?1, missing_since = NULL, updated_at = unixepoch() \
         WHERE id = ?2",
    )?
    .execute(params![new_path, old_id])?;
    Ok(())
}

/// Soft-mark files as missing. Never deletes rows.
pub fn mark_missing(tx: &Transaction, ids: &[i64], since_ms: i64) -> Result<usize, DbError> {
    let mut stmt = tx.prepare_cached(
        "UPDATE tracks SET missing_since = ?1, updated_at = unixepoch() \
         WHERE id = ?2 AND missing_since IS NULL",
    )?;
    let mut changed = 0;
    for id in ids {
        changed += stmt.execute(params![since_ms, id])?;
    }
    Ok(changed)
}

/// Clear the missing mark for files that are present again
/// (e.g. removable drive reconnected).
pub fn clear_missing(tx: &Transaction, ids: &[i64]) -> Result<usize, DbError> {
    let mut stmt = tx.prepare_cached(
        "UPDATE tracks SET missing_since = NULL, updated_at = unixepoch() \
         WHERE id = ?1 AND missing_since IS NOT NULL",
    )?;
    let mut changed = 0;
    for id in ids {
        changed += stmt.execute(params![id])?;
    }
    Ok(changed)
}
