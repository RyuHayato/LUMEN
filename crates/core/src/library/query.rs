//! Library query API. Independent from Tauri — the app shell translates
//! these into commands. All list queries exclude missing files unless
//! explicitly asked otherwise (`active_only`).

use rusqlite::{params, Connection};

use crate::error::DbError;
use crate::library::metadata::normalize;

const ACTIVE: &str = "t.missing_since IS NULL";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistRow {
    pub id: i64,
    pub name: String,
    pub track_count: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumRow {
    pub id: i64,
    pub title: String,
    pub artist_name: Option<String>,
    pub year: Option<i32>,
    pub track_count: i64,
    pub artwork_hash: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackRow {
    pub id: i64,
    pub path: String,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub album_id: Option<i64>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub duration_ms: i64,
    pub codec: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u16>,
    pub year: Option<i32>,
    pub artwork_hash: Option<String>,
    /// Derived from `tracks.favorited_at` (see ARCHITECTURE.md §8): the
    /// schema models "favorited" as a nullable timestamp, so the boolean is
    /// a read of that column, never a second store of the same fact.
    pub is_favorite: bool,
}

/// One genre with its active track count.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenreRow {
    pub name: String,
    pub track_count: i64,
}

/// A playlist with its entry count and total runtime.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistRow {
    pub id: i64,
    pub name: String,
    pub track_count: i64,
    pub duration_ms: i64,
    pub updated_at: i64,
}

/// One playback history entry, newest first.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRow {
    pub id: i64,
    pub track_id: i64,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub artwork_hash: Option<String>,
    pub played_at: i64,
    pub listened_ms: i64,
    pub play_count: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStats {
    pub tracks: i64,
    pub albums: i64,
    pub artists: i64,
    pub genres: i64,
    pub missing_tracks: i64,
    pub total_duration_ms: i64,
    pub total_size_bytes: i64,
    pub artworks: i64,
    pub favorites: i64,
    pub playlists: i64,
}

/// Escape LIKE wildcards in a user needle (with ESCAPE '\').
fn like_escape(needle: &str) -> String {
    needle
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

pub fn list_artists(conn: &Connection) -> Result<Vec<ArtistRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT a.id, a.name, COUNT(t.id) \
         FROM artists a \
         LEFT JOIN tracks t ON t.artist_id = a.id AND {ACTIVE} \
         GROUP BY a.id ORDER BY a.name_normalized"
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok(ArtistRow {
            id: row.get(0)?,
            name: row.get(1)?,
            track_count: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn list_albums(conn: &Connection) -> Result<Vec<AlbumRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT al.id, al.title, ar.name, al.year, COUNT(t.id), MIN(t.artwork_hash) \
         FROM albums al \
         LEFT JOIN artists ar ON ar.id = al.album_artist_id \
         LEFT JOIN tracks t ON t.album_id = al.id AND {ACTIVE} \
         GROUP BY al.id ORDER BY al.title_normalized"
    ))?;
    let rows = stmt.query_map([], album_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn albums_by_artist(conn: &Connection, artist_id: i64) -> Result<Vec<AlbumRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT al.id, al.title, ar.name, al.year, COUNT(t.id), MIN(t.artwork_hash) \
         FROM albums al \
         LEFT JOIN artists ar ON ar.id = al.album_artist_id \
         LEFT JOIN tracks t ON t.album_id = al.id AND {ACTIVE} \
         WHERE al.album_artist_id = ?1 \
         GROUP BY al.id ORDER BY al.year, al.title_normalized"
    ))?;
    let rows = stmt.query_map(params![artist_id], album_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn album_row(row: &rusqlite::Row) -> rusqlite::Result<AlbumRow> {
    Ok(AlbumRow {
        id: row.get(0)?,
        title: row.get(1)?,
        artist_name: row.get(2)?,
        year: row.get(3)?,
        track_count: row.get(4)?,
        artwork_hash: row.get(5)?,
    })
}

const TRACK_COLS: &str = "t.id, t.path, t.title, ar.name, al.title, t.album_id, \
     t.track_no, t.disc_no, t.duration_ms, t.codec, t.sample_rate_hz, \
     t.bit_depth, t.channels, al.year, t.artwork_hash, t.favorited_at";

const TRACK_JOINS: &str = "FROM tracks t \
     LEFT JOIN artists ar ON ar.id = t.artist_id \
     LEFT JOIN albums al ON al.id = t.album_id";

fn track_row(row: &rusqlite::Row) -> rusqlite::Result<TrackRow> {
    Ok(TrackRow {
        id: row.get(0)?,
        path: row.get(1)?,
        title: row.get(2)?,
        artist_name: row.get(3)?,
        album_title: row.get(4)?,
        album_id: row.get(5)?,
        track_no: row.get::<_, Option<u32>>(6)?,
        disc_no: row.get::<_, Option<u32>>(7)?,
        duration_ms: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
        codec: row.get(9)?,
        sample_rate_hz: row.get(10)?,
        bit_depth: row.get(11)?,
        channels: row.get(12)?,
        year: row.get(13)?,
        artwork_hash: row.get(14)?,
        is_favorite: row.get::<_, Option<i64>>(15)?.is_some(),
    })
}

pub fn list_tracks(conn: &Connection, limit: u32, offset: u32) -> Result<Vec<TrackRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLS} {TRACK_JOINS} WHERE {ACTIVE} \
         ORDER BY al.title_normalized, t.disc_no, t.track_no, t.title_normalized \
         LIMIT ?1 OFFSET ?2"
    ))?;
    let rows = stmt.query_map(params![limit, offset], track_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn tracks_by_album(conn: &Connection, album_id: i64) -> Result<Vec<TrackRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLS} {TRACK_JOINS} WHERE {ACTIVE} AND t.album_id = ?1 \
         ORDER BY t.disc_no, t.track_no, t.title_normalized"
    ))?;
    let rows = stmt.query_map(params![album_id], track_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn get_track(conn: &Connection, id: i64) -> Result<Option<TrackRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLS} {TRACK_JOINS} WHERE t.id = ?1"
    ))?;
    let mut rows = stmt.query_map(params![id], track_row)?;
    Ok(rows.next().transpose()?)
}

pub fn search_tracks(
    conn: &Connection,
    needle: &str,
    limit: u32,
) -> Result<Vec<TrackRow>, DbError> {
    let pattern = format!("%{}%", like_escape(&normalize(needle)));
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLS} {TRACK_JOINS} WHERE {ACTIVE} AND t.title_normalized \
         LIKE ?1 ESCAPE '\\' ORDER BY t.title_normalized LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![pattern, limit], track_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn search_artists(
    conn: &Connection,
    needle: &str,
    limit: u32,
) -> Result<Vec<ArtistRow>, DbError> {
    let pattern = format!("%{}%", like_escape(&normalize(needle)));
    let mut stmt = conn.prepare(
        "SELECT id, name, 0 FROM artists WHERE name_normalized LIKE ?1 ESCAPE '\\' \
         ORDER BY name_normalized LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![pattern, limit], |row| {
        Ok(ArtistRow {
            id: row.get(0)?,
            name: row.get(1)?,
            track_count: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn search_albums(
    conn: &Connection,
    needle: &str,
    limit: u32,
) -> Result<Vec<AlbumRow>, DbError> {
    let pattern = format!("%{}%", like_escape(&normalize(needle)));
    let mut stmt = conn.prepare(
        "SELECT al.id, al.title, ar.name, al.year, 0, NULL \
         FROM albums al LEFT JOIN artists ar ON ar.id = al.album_artist_id \
         WHERE al.title_normalized LIKE ?1 ESCAPE '\\' \
         ORDER BY al.title_normalized LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![pattern, limit], album_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn tracks_by_genre(
    conn: &Connection,
    genre: &str,
    limit: u32,
) -> Result<Vec<TrackRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLS} {TRACK_JOINS} \
         JOIN track_genres tg ON tg.track_id = t.id \
         JOIN genres g ON g.id = tg.genre_id \
         WHERE {ACTIVE} AND g.name = ?1 COLLATE NOCASE \
         ORDER BY t.title_normalized LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![genre, limit], track_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Every indexed genre with its active track count.
///
/// Built from the genre rows the scanner already wrote, so this never
/// invents a category: a genre with no playable track is reported as zero
/// rather than hidden, because the user scanned it and the fact is real.
pub fn list_genres(conn: &Connection) -> Result<Vec<GenreRow>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT g.name, COUNT(t.id) \
         FROM genres g \
         LEFT JOIN track_genres tg ON tg.genre_id = g.id \
         LEFT JOIN tracks t ON t.id = tg.track_id \
              AND t.missing_since IS NULL \
         GROUP BY g.id ORDER BY g.name COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(GenreRow {
            name: row.get(0)?,
            track_count: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Tracks the user has favorited, newest favorite first.
///
/// Reads the `favorited_at` column that schema v1 already reserved (see
/// ARCHITECTURE.md §8); no parallel store is kept.
pub fn list_favorites(conn: &Connection, limit: u32) -> Result<Vec<TrackRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLS} {TRACK_JOINS} \
         WHERE {ACTIVE} AND t.favorited_at IS NOT NULL \
         ORDER BY t.favorited_at DESC, t.title_normalized LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![limit], track_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Set or clear a track's favorite state. Returns the new state.
///
/// The timestamp doubles as the flag: setting writes `unixepoch()`, clearing
/// writes NULL, so "when was this favorited" stays answerable without a
/// second table.
pub fn set_favorite(conn: &Connection, track_id: i64, favorite: bool) -> Result<bool, DbError> {
    if favorite {
        conn.execute(
            "UPDATE tracks SET favorited_at = unixepoch(), updated_at = unixepoch() \
             WHERE id = ?1",
            params![track_id],
        )?;
    } else {
        conn.execute(
            "UPDATE tracks SET favorited_at = NULL, updated_at = unixepoch() WHERE id = ?1",
            params![track_id],
        )?;
    }
    // Report the state the row actually ended up in (a missing id stays false).
    let state: Option<i64> = conn
        .query_row(
            "SELECT favorited_at FROM tracks WHERE id = ?1",
            params![track_id],
            |row| row.get(0),
        )
        .ok();
    Ok(state.is_some())
}

// -------------------------------------------------------------- playlists

/// Every playlist with its entry count and summed runtime.
pub fn list_playlists(conn: &Connection) -> Result<Vec<PlaylistRow>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, COUNT(pe.track_id), \
                COALESCE(SUM(t.duration_ms), 0), p.updated_at \
         FROM playlists p \
         LEFT JOIN playlist_entries pe ON pe.playlist_id = p.id \
         LEFT JOIN tracks t ON t.id = pe.track_id AND t.missing_since IS NULL \
         GROUP BY p.id ORDER BY p.name COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(PlaylistRow {
            id: row.get(0)?,
            name: row.get(1)?,
            track_count: row.get(2)?,
            duration_ms: row.get(3)?,
            updated_at: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Create a playlist and return its id. Blank names are rejected.
pub fn create_playlist(conn: &Connection, name: &str) -> Result<i64, DbError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(DbError::Sqlite(rusqlite::Error::InvalidParameterName(
            "playlist name must not be empty".to_string(),
        )));
    }
    conn.execute("INSERT INTO playlists (name) VALUES (?1)", params![name])?;
    Ok(conn.last_insert_rowid())
}

pub fn rename_playlist(conn: &Connection, playlist_id: i64, name: &str) -> Result<(), DbError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(DbError::Sqlite(rusqlite::Error::InvalidParameterName(
            "playlist name must not be empty".to_string(),
        )));
    }
    conn.execute(
        "UPDATE playlists SET name = ?1, updated_at = unixepoch() WHERE id = ?2",
        params![name, playlist_id],
    )?;
    Ok(())
}

/// Delete a playlist. Its entries go with it (`ON DELETE CASCADE`).
pub fn delete_playlist(conn: &Connection, playlist_id: i64) -> Result<(), DbError> {
    conn.execute("DELETE FROM playlists WHERE id = ?1", params![playlist_id])?;
    Ok(())
}

/// Entries of a playlist in stored order.
pub fn playlist_tracks(conn: &Connection, playlist_id: i64) -> Result<Vec<TrackRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLS} {TRACK_JOINS} \
         JOIN playlist_entries pe ON pe.track_id = t.id \
         WHERE pe.playlist_id = ?1 AND t.missing_since IS NULL \
         ORDER BY pe.position"
    ))?;
    let rows = stmt.query_map(params![playlist_id], track_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Append a track to the end of a playlist.
///
/// Duplicate rows are rejected: a playlist may legitimately hold the same
/// release once, not as a loop of repeats. The UI surfaces this error to
/// the caller rather than silently allowing it.
pub fn add_to_playlist(conn: &Connection, playlist_id: i64, track_id: i64) -> Result<(), DbError> {
    let already: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM playlist_entries WHERE playlist_id = ?1 AND track_id = ?2)",
        params![playlist_id, track_id],
        |row| row.get(0),
    )?;
    if already {
        return Err(DbError::Conflict(format!(
            "track {track_id} is already in playlist {playlist_id}"
        )));
    }
    conn.execute(
        "INSERT INTO playlist_entries (playlist_id, track_id, position) \
         VALUES (?1, ?2, COALESCE(\
             (SELECT MAX(position) + 1 FROM playlist_entries WHERE playlist_id = ?1), 0))",
        params![playlist_id, track_id],
    )?;
    touch_playlist(conn, playlist_id)?;
    Ok(())
}

/// Remove one occurrence of a track from a playlist.
pub fn remove_from_playlist(
    conn: &Connection,
    playlist_id: i64,
    track_id: i64,
) -> Result<(), DbError> {
    // Delete only the lowest-positioned match so duplicate entries survive.
    // `position` identifies the row: playlist_entries has no surrogate key,
    // and the (playlist_id, position) pair is unique.
    conn.execute(
        "DELETE FROM playlist_entries \
         WHERE playlist_id = ?1 AND track_id = ?2 \
           AND position = (SELECT MIN(position) FROM playlist_entries \
                           WHERE playlist_id = ?1 AND track_id = ?2)",
        params![playlist_id, track_id],
    )?;
    touch_playlist(conn, playlist_id)?;
    Ok(())
}

/// Move the entry at `from` to `to`, shifting the rest to stay contiguous.
///
/// Positions are rewritten from scratch inside one transaction: the schema
/// makes `(playlist_id, position)` unique, so a naive swap would violate it
/// mid-update.
pub fn reorder_playlist(
    conn: &mut Connection,
    playlist_id: i64,
    from: u32,
    to: u32,
) -> Result<(), DbError> {
    let tx = conn.transaction()?;
    let mut ids: Vec<i64> = {
        let mut stmt = tx.prepare(
            "SELECT track_id FROM playlist_entries WHERE playlist_id = ?1 \
                        ORDER BY position",
        )?;
        let rows = stmt.query_map(params![playlist_id], |row| row.get::<_, i64>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    if ids.is_empty() {
        return Ok(());
    }
    let from = from as usize;
    let to = (to as usize).min(ids.len() - 1);
    if from >= ids.len() || from == to {
        tx.rollback()?;
        return Ok(());
    }
    let moved = ids.remove(from);
    ids.insert(to, moved);

    // Two-phase write: `(playlist_id, position)` is UNIQUE, so assigning the
    // final positions one at a time would collide with rows that have not been
    // moved yet. Park every row in a disjoint negative range first, then write
    // the real 0..n sequence.
    {
        let mut stmt = tx.prepare(
            "UPDATE playlist_entries SET position = -1 - position \
             WHERE playlist_id = ?1",
        )?;
        stmt.execute(params![playlist_id])?;
    }
    {
        let mut stmt = tx.prepare(
            "UPDATE playlist_entries SET position = ?1 WHERE playlist_id = ?2 \
                        AND track_id = ?3",
        )?;
        for (index, track_id) in ids.iter().enumerate() {
            stmt.execute(params![index as i64, playlist_id, track_id])?;
        }
    }
    touch_playlist(&tx, playlist_id)?;
    tx.commit()?;
    Ok(())
}

fn touch_playlist(conn: &Connection, playlist_id: i64) -> Result<(), DbError> {
    conn.execute(
        "UPDATE playlists SET updated_at = unixepoch() WHERE id = ?1",
        params![playlist_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- history

/// Record that a track started playing.
///
/// Only appends when this track is not already the most recent entry, so
/// repeated engine events for one playback do not flood the log.
pub fn record_history(conn: &Connection, track_id: i64, listened_ms: i64) -> Result<(), DbError> {
    let last: Option<i64> = conn
        .query_row(
            "SELECT track_id FROM history ORDER BY played_at DESC, id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();
    if last == Some(track_id) {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO history (track_id, played_at, listened_ms) \
         VALUES (?1, unixepoch(), ?2)",
        params![track_id, listened_ms.max(0)],
    )?;
    Ok(())
}

/// Recent plays, newest first, one row per play.
pub fn list_history(conn: &Connection, limit: u32) -> Result<Vec<HistoryRow>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT h.id, h.track_id, t.title, ar.name, al.title, t.artwork_hash, \
                h.played_at, h.listened_ms, \
                (SELECT COUNT(*) FROM history h2 WHERE h2.track_id = h.track_id) \
         FROM history h \
         JOIN tracks t ON t.id = h.track_id \
         LEFT JOIN artists ar ON ar.id = t.artist_id \
         LEFT JOIN albums al ON al.id = t.album_id \
         ORDER BY h.played_at DESC, h.id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(HistoryRow {
            id: row.get(0)?,
            track_id: row.get(1)?,
            title: row.get(2)?,
            artist_name: row.get(3)?,
            album_title: row.get(4)?,
            artwork_hash: row.get(5)?,
            played_at: row.get(6)?,
            listened_ms: row.get(7)?,
            play_count: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Drop plays that happened strictly before `cutoff` (a unix timestamp).
pub fn prune_history_before(conn: &Connection, cutoff: i64) -> Result<usize, DbError> {
    let removed = conn.execute("DELETE FROM history WHERE played_at < ?1", params![cutoff])?;
    Ok(removed)
}

/// Credit listening time to a track's most recent play.
///
/// Listening time cannot be written when a play is *recorded*: at that moment
/// the track has only just started, so nothing is known about how long it will
/// be heard. It is credited afterwards instead, when the engine moves on and the
/// elapsed position is finally a fact.
///
/// Credits the newest row for that track, and only within the window, so this
/// can never edit history that the UI no longer shows.
pub fn add_listened_time(
    conn: &Connection,
    track_id: i64,
    listened_ms: i64,
    cutoff: i64,
) -> Result<usize, DbError> {
    let added = listened_ms.max(0);
    if added == 0 {
        return Ok(0);
    }
    let changed = conn.execute(
        "UPDATE history SET listened_ms = listened_ms + ?1 \
         WHERE id = (SELECT id FROM history WHERE track_id = ?2 AND played_at >= ?3 \
                     ORDER BY played_at DESC, id DESC LIMIT 1)",
        params![added, track_id, cutoff],
    )?;
    Ok(changed)
}

/// Drop history entries older than `keep` most recent rows.
pub fn trim_history(conn: &Connection, keep: u32) -> Result<usize, DbError> {
    let removed = conn.execute(
        "DELETE FROM history WHERE id NOT IN \
         (SELECT id FROM history ORDER BY played_at DESC, id DESC LIMIT ?1)",
        params![keep as i64],
    )?;
    Ok(removed)
}

pub fn clear_history(conn: &Connection) -> Result<(), DbError> {
    conn.execute("DELETE FROM history", [])?;
    Ok(())
}

pub fn stats(conn: &Connection) -> Result<LibraryStats, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT \
            (SELECT COUNT(*) FROM tracks t WHERE {ACTIVE}), \
            (SELECT COUNT(*) FROM albums), \
            (SELECT COUNT(*) FROM artists), \
            (SELECT COUNT(*) FROM genres), \
            (SELECT COUNT(*) FROM tracks WHERE missing_since IS NOT NULL), \
            (SELECT COALESCE(SUM(duration_ms), 0) FROM tracks t WHERE {ACTIVE}), \
            (SELECT COALESCE(SUM(file_size), 0) FROM tracks t WHERE {ACTIVE}), \
            (SELECT COUNT(*) FROM artworks), \
            (SELECT COUNT(*) FROM tracks t WHERE {ACTIVE} AND t.favorited_at IS NOT NULL), \
            (SELECT COUNT(*) FROM playlists)"
    ))?;
    let stats = stmt.query_row([], |row| {
        Ok(LibraryStats {
            tracks: row.get(0)?,
            albums: row.get(1)?,
            artists: row.get(2)?,
            genres: row.get(3)?,
            missing_tracks: row.get(4)?,
            total_duration_ms: row.get(5)?,
            total_size_bytes: row.get(6)?,
            artworks: row.get(7)?,
            favorites: row.get(8)?,
            playlists: row.get(9)?,
        })
    })?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db::Database;

    /// Seconds since the epoch, the same clock `record_history` stamps rows with.
    fn now_unix() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn like_escape_neutralizes_wildcards() {
        assert_eq!(like_escape("100%"), "100\\%");
        assert_eq!(like_escape("a_b\\c"), "a\\_b\\\\c");
        assert_eq!(like_escape("plain"), "plain");
    }

    /// Seed a small library: 3 tracks, 2 artists, 2 albums, 2 genres.
    fn seeded() -> Connection {
        let db = Database::open_in_memory().unwrap();
        let conn = db.into_connection();
        for (path, artist, album, genre) in [
            ("/a1.wav", "Aurora", "Dawn", "Ambient"),
            ("/a2.wav", "Aurora", "Dawn", "Ambient"),
            ("/b1.wav", "Basalt", "Ember", "Techno"),
        ] {
            conn.execute(
                "INSERT INTO tracks (path, title, title_normalized, duration_ms) \
                 VALUES (?1, ?1, ?1, 1000)",
                params![path],
            )
            .unwrap();
            let track_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT OR IGNORE INTO artists (name, name_normalized) VALUES (?1, ?1)",
                params![artist],
            )
            .unwrap();
            let artist_id: i64 = conn
                .query_row(
                    "SELECT id FROM artists WHERE name_normalized = ?1",
                    params![artist],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO albums (title, title_normalized, album_artist_id) \
                 VALUES (?1, ?1, ?2)",
                params![album, artist_id],
            )
            .unwrap();
            let album_id: i64 = conn
                .query_row(
                    "SELECT id FROM albums WHERE title_normalized = ?1",
                    params![album],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "UPDATE tracks SET artist_id = ?1, album_id = ?2 WHERE id = ?3",
                params![artist_id, album_id, track_id],
            )
            .unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO genres (name) VALUES (?1)",
                params![genre],
            )
            .unwrap();
            let genre_id: i64 = conn
                .query_row(
                    "SELECT id FROM genres WHERE name = ?1",
                    params![genre],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO track_genres (track_id, genre_id) VALUES (?1, ?2)",
                params![track_id, genre_id],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn list_genres_counts_active_tracks() {
        let conn = seeded();
        let genres = list_genres(&conn).unwrap();
        assert_eq!(genres.len(), 2);
        assert_eq!(genres[0].name, "Ambient");
        assert_eq!(genres[0].track_count, 2);
        assert_eq!(genres[1].name, "Techno");
        assert_eq!(genres[1].track_count, 1);
    }

    #[test]
    fn missing_tracks_leave_the_genre_count() {
        let conn = seeded();
        conn.execute(
            "UPDATE tracks SET missing_since = unixepoch() WHERE path = '/a1.wav'",
            [],
        )
        .unwrap();
        let genres = list_genres(&conn).unwrap();
        let ambient = genres.iter().find(|g| g.name == "Ambient").unwrap();
        assert_eq!(ambient.track_count, 1, "a gone file is not playable");
    }

    #[test]
    fn tracks_by_genre_matches_the_count() {
        let conn = seeded();
        let ambient = tracks_by_genre(&conn, "ambient", 100).unwrap();
        assert_eq!(ambient.len(), 2);
        assert!(ambient.iter().all(|t| t.title.starts_with("/a")));
    }

    #[test]
    fn favorites_round_trip_through_the_timestamp_column() {
        let conn = seeded();
        let id = 1_i64;

        assert!(!get_track(&conn, id).unwrap().unwrap().is_favorite);
        assert_eq!(list_favorites(&conn, 100).unwrap().len(), 0);

        assert!(set_favorite(&conn, id, true).unwrap());
        assert!(get_track(&conn, id).unwrap().unwrap().is_favorite);
        let favorites = list_favorites(&conn, 100).unwrap();
        assert_eq!(favorites.len(), 1);
        assert_eq!(favorites[0].id, id);

        // Favoriting twice keeps exactly one entry (timestamp overwritten).
        assert!(set_favorite(&conn, id, true).unwrap());
        assert_eq!(list_favorites(&conn, 100).unwrap().len(), 1);

        assert!(!set_favorite(&conn, id, false).unwrap());
        assert_eq!(list_favorites(&conn, 100).unwrap().len(), 0);
    }

    #[test]
    fn favoriting_a_missing_track_id_is_harmless() {
        let conn = seeded();
        assert!(!set_favorite(&conn, 9999, true).unwrap());
    }

    #[test]
    fn favorites_exclude_missing_files() {
        let conn = seeded();
        let id = 1_i64;
        set_favorite(&conn, id, true).unwrap();
        conn.execute(
            "UPDATE tracks SET missing_since = unixepoch() WHERE id = ?1",
            params![id],
        )
        .unwrap();
        assert_eq!(list_favorites(&conn, 100).unwrap().len(), 0);
    }

    #[test]
    fn playlist_lifecycle_persists() {
        let conn = seeded();
        let track = 1_i64;

        let id = create_playlist(&conn, "Late Night").unwrap();
        assert_eq!(list_playlists(&conn).unwrap().len(), 1);

        add_to_playlist(&conn, id, track).unwrap();
        let dup = add_to_playlist(&conn, id, track);
        assert!(dup.is_err(), "duplicate add must be rejected");
        let rows = playlist_tracks(&conn, id).unwrap();
        assert_eq!(rows.len(), 1, "only one copy may live in a playlist");
        assert_eq!(rows[0].id, track);

        let playlists = list_playlists(&conn).unwrap();
        assert_eq!(playlists[0].name, "Late Night");
        assert_eq!(playlists[0].track_count, 1);
        assert_eq!(playlists[0].duration_ms, 1000);

        // Removing takes the first occurrence only.
        remove_from_playlist(&conn, id, track).unwrap();
        assert_eq!(playlist_tracks(&conn, id).unwrap().len(), 0);

        rename_playlist(&conn, id, "Dawn Set").unwrap();
        assert_eq!(list_playlists(&conn).unwrap()[0].name, "Dawn Set");

        delete_playlist(&conn, id).unwrap();
        assert_eq!(list_playlists(&conn).unwrap().len(), 0);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM playlist_entries", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            0,
            "entries cascade with the playlist"
        );
    }

    #[test]
    fn blank_playlist_names_are_rejected() {
        let conn = seeded();
        assert!(create_playlist(&conn, "   ").is_err());
        assert!(rename_playlist(&conn, 1, "").is_err());
    }

    #[test]
    fn reorder_moves_an_entry_and_keeps_positions_contiguous() {
        let mut conn = seeded();
        let tracks: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT id FROM tracks ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<Vec<i64>, _>>()
                .unwrap()
        };

        let pl = create_playlist(&conn, "Set").unwrap();
        for t in &tracks {
            add_to_playlist(&conn, pl, *t).unwrap();
        }
        assert_eq!(
            playlist_tracks(&conn, pl)
                .unwrap()
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            tracks
        );

        let last = tracks.len() - 1;
        reorder_playlist(&mut conn, pl, last as u32, 0).unwrap();
        let after = playlist_tracks(&conn, pl)
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>();
        let mut expected = tracks.clone();
        let moved = expected.remove(last);
        expected.insert(0, moved);
        assert_eq!(after, expected);

        // Positions are still a contiguous 0..n sequence (the UNIQUE
        // constraint and the gap-filling both hold).
        let positions: Vec<i64> = {
            let mut stmt = conn
                .prepare(
                    "SELECT position FROM playlist_entries WHERE playlist_id = ?1 \
                          ORDER BY position",
                )
                .unwrap();
            stmt.query_map(params![pl], |r| r.get(0))
                .unwrap()
                .collect::<Result<Vec<i64>, _>>()
                .unwrap()
        };
        assert_eq!(positions, (0..tracks.len() as i64).collect::<Vec<i64>>());
    }

    #[test]
    fn reorder_out_of_range_is_a_no_op() {
        let mut conn = seeded();
        let pl = create_playlist(&conn, "Set").unwrap();
        add_to_playlist(&conn, pl, 1).unwrap();
        reorder_playlist(&mut conn, pl, 9, 0).unwrap();
        reorder_playlist(&mut conn, pl, 0, 0).unwrap();
        assert_eq!(playlist_tracks(&conn, pl).unwrap().len(), 1);
    }

    #[test]
    fn history_records_each_track_in_order() {
        let conn = seeded();
        record_history(&conn, 1, 1000).unwrap();
        record_history(&conn, 2, 2000).unwrap();
        record_history(&conn, 1, 1500).unwrap();

        let history = list_history(&conn, 100).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].track_id, 1, "newest first");
        assert_eq!(history[0].play_count, 2, "aggregated play count");
        assert_eq!(history[1].track_id, 2);
        assert_eq!(history[1].listened_ms, 2000);
    }

    #[test]
    fn repeated_history_for_the_same_track_does_not_duplicate() {
        let conn = seeded();
        record_history(&conn, 1, 100).unwrap();
        record_history(&conn, 1, 200).unwrap();
        record_history(&conn, 1, 300).unwrap();
        assert_eq!(list_history(&conn, 100).unwrap().len(), 1);
    }

    #[test]
    fn history_pruned_to_the_window() {
        let conn = seeded();
        record_history(&conn, 1, 10).unwrap();
        record_history(&conn, 2, 10).unwrap();
        record_history(&conn, 3, 10).unwrap();

        // Rewrite the oldest row to a day and a half ago.
        conn.execute(
            "UPDATE history SET played_at = played_at - 129600 WHERE track_id = 1",
            [],
        )
        .unwrap();
        assert_eq!(prune_history_before(&conn, now_unix() - 86_400).unwrap(), 1);
        let kept = list_history(&conn, 100).unwrap();
        assert_eq!(kept.len(), 2);
        assert!(
            !kept.iter().any(|row| row.track_id == 1),
            "the stale play is gone"
        );
        assert_eq!(
            prune_history_before(&conn, now_unix() - 86_400).unwrap(),
            0,
            "a second sweep has nothing left to remove"
        );
    }

    #[test]
    fn listened_time_is_credited_to_the_newest_play() {
        let conn = seeded();
        let now = now_unix();
        record_history(&conn, 1, 0).unwrap();
        record_history(&conn, 2, 0).unwrap();

        assert_eq!(
            add_listened_time(&conn, 1, 90_000, now - 86_400).unwrap(),
            1
        );
        let rows = list_history(&conn, 10).unwrap();
        assert_eq!(rows[0].track_id, 2, "newest play leads");
        assert_eq!(rows[1].track_id, 1);
        assert_eq!(rows[1].listened_ms, 90_000, "credited to track 1's play");
        assert_eq!(rows[0].listened_ms, 0, "and to nothing else");

        // A second credit accumulates rather than replacing.
        add_listened_time(&conn, 1, 10_000, now - 86_400).unwrap();
        assert_eq!(list_history(&conn, 10).unwrap()[1].listened_ms, 100_000);

        // An unknown track credits nothing rather than inventing a row.
        assert_eq!(
            add_listened_time(&conn, 9999, 5_000, now - 86_400).unwrap(),
            0
        );
    }

    #[test]
    fn listened_time_ignores_zero_and_rows_outside_the_window() {
        let conn = seeded();
        let now = now_unix();
        record_history(&conn, 1, 0).unwrap();

        assert_eq!(add_listened_time(&conn, 1, 0, now - 86_400).unwrap(), 0);
        assert_eq!(add_listened_time(&conn, 1, -500, now - 86_400).unwrap(), 0);
        assert_eq!(list_history(&conn, 10).unwrap()[0].listened_ms, 0);

        // A play older than the window is outside what the wall can show, so it
        // must not be edited.
        conn.execute("UPDATE history SET played_at = played_at - 200000", [])
            .unwrap();
        assert_eq!(
            add_listened_time(&conn, 1, 30_000, now - 86_400).unwrap(),
            0
        );
        assert_eq!(list_history(&conn, 10).unwrap()[0].listened_ms, 0);
    }

    #[test]
    fn history_trim_and_clear() {
        let conn = seeded();
        // Distinct tracks: recording the same track twice in a row is deduped.
        for id in [1_i64, 2, 3] {
            record_history(&conn, id, 10).unwrap();
        }
        assert_eq!(list_history(&conn, 100).unwrap().len(), 3);
        assert_eq!(trim_history(&conn, 2).unwrap(), 1);
        let kept = list_history(&conn, 100).unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].track_id, 3, "newest survive the trim");
        assert_eq!(kept[1].track_id, 2);
        clear_history(&conn).unwrap();
        assert_eq!(list_history(&conn, 100).unwrap().len(), 0);
    }

    #[test]
    fn stats_include_favorites_and_playlists() {
        let conn = seeded();
        set_favorite(&conn, 1, true).unwrap();
        create_playlist(&conn, "A").unwrap();
        create_playlist(&conn, "B").unwrap();

        let s = stats(&conn).unwrap();
        assert_eq!(s.tracks, 3);
        assert_eq!(s.favorites, 1);
        assert_eq!(s.playlists, 2);
    }
}
