//! Tauri command handlers: the narrow, typed boundary between the frontend
//! and the core. Handlers translate — they contain no domain logic.

use std::path::PathBuf;

use serde::Serialize;
use tauri::{Emitter, State};

use lumen_core::audio::output::{OutputDevice, OutputMode};
use lumen_core::audio::StateSnapshot;
use lumen_core::config::{self, Config};
use lumen_core::error::AudioError;
use lumen_core::library::query::{
    self, AlbumRow, ArtistRow, GenreRow, HistoryRow, LibraryStats, PlaylistRow, TrackRow,
};
use lumen_core::library::scanner::{ScanHandle, Scanner};

use crate::state::AppState;

/// Errors are shaped for the frontend: a human-readable message.
/// Technical detail stays in the logs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub message: String,
}

impl From<AudioError> for CommandError {
    fn from(error: AudioError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl From<String> for CommandError {
    fn from(message: String) -> Self {
        Self { message }
    }
}

fn err(message: impl Into<String>) -> CommandError {
    CommandError {
        message: message.into(),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    pub profile: &'static str,
    pub platform: &'static str,
    pub tagline: &'static str,
}

#[tauri::command]
pub fn get_app_info() -> AppInfo {
    AppInfo {
        name: "LUMEN".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        platform: std::env::consts::OS,
        tagline: "Hear every detail.",
    }
}

#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> Result<Config, CommandError> {
    let config = state
        .config
        .lock()
        .map_err(|_| err("config state lock poisoned"))?;
    Ok(config.clone())
}

/// Serve cached artwork as a data URL.
///
/// Presentation only: reads the content-addressed artwork cache (ADR-012) by
/// its SHA-256 hash. No audio, database-schema, or playback change.
#[tauri::command]
pub fn get_artwork(
    state: State<'_, AppState>,
    hash: Option<String>,
) -> Result<Option<String>, CommandError> {
    let Some(hash) = hash.filter(|h| !h.is_empty()) else {
        return Ok(None);
    };

    let relative = {
        let db = state
            .db
            .lock()
            .map_err(|_| err("database state lock poisoned"))?;
        db.connection()
            .query_row(
                "SELECT path FROM artworks WHERE hash = ?1",
                [&hash],
                |row| row.get::<_, String>(0),
            )
            .ok()
    };
    let Some(relative) = relative else {
        return Ok(None);
    };

    // Defensive: the cache is content-addressed, but never let a relative path
    // escape the artwork directory.
    let path = state.artwork_dir.join(&relative);
    if !path.starts_with(&state.artwork_dir) {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|e| err(format!("artwork read failed: {e}")))?;

    let mime = match relative.rsplit('.').next().unwrap_or_default() {
        "png" => "image/png",
        _ => "image/jpeg",
    };
    Ok(Some(format!(
        "data:{mime};base64,{}",
        base64_encode(&bytes)
    )))
}

/// Minimal standard-alphabet base64 encoder (avoids adding a dependency).
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[tauri::command]
pub fn get_playback_state(state: State<'_, AppState>) -> Result<StateSnapshot, CommandError> {
    Ok(state.engine.snapshot()?)
}

// -------------------------------------------------------------- output control

/// Enumerate output render endpoints (id, name, default, availability).
#[tauri::command]
pub fn list_output_devices() -> Result<Vec<OutputDevice>, CommandError> {
    let backend = lumen_core::audio::output::default_backend();
    backend.enumerate_devices().map_err(|e| err(e.to_string()))
}

/// Explicitly select the output endpoint (`None` → follow system default).
#[tauri::command]
pub fn set_output_device(
    state: State<'_, AppState>,
    device_id: Option<String>,
) -> Result<(), CommandError> {
    state
        .engine
        .send(lumen_core::audio::AudioCommand::SetOutputDevice {
            device_id: device_id.clone(),
        })?;
    let mut config = state
        .config
        .lock()
        .map_err(|_| err("config state lock poisoned"))?;
    config.output_device_id = device_id;
    save_config(&config)?;
    Ok(())
}

/// Switch between shared and exclusive WASAPI output.
#[tauri::command]
pub fn set_output_mode(state: State<'_, AppState>, mode: OutputMode) -> Result<(), CommandError> {
    state
        .engine
        .send(lumen_core::audio::AudioCommand::SetOutputMode { mode })?;
    let mut config = state
        .config
        .lock()
        .map_err(|_| err("config state lock poisoned"))?;
    config.output_mode = mode;
    save_config(&config)?;
    Ok(())
}

// -------------------------------------------------------------- playback control

/// Play a list of tracks starting at `start_index` (replaces the queue).
#[tauri::command]
pub fn play_tracks(
    state: State<'_, AppState>,
    tracks: Vec<i64>,
    start_index: usize,
) -> Result<(), CommandError> {
    if tracks.is_empty() {
        return Err(err("empty track list"));
    }
    if start_index >= tracks.len() {
        return Err(err("start index out of range"));
    }
    state
        .engine
        .send(lumen_core::audio::AudioCommand::PlayQueue {
            tracks,
            start_index,
        })?;
    Ok(())
}

/// One track by id. The player uses this so the console can name the song
/// whatever list the user happened to start it from.
#[tauri::command]
pub fn get_track(
    state: State<'_, AppState>,
    track_id: i64,
) -> Result<Option<TrackRow>, CommandError> {
    let db = state.db.lock().map_err(|_| err("database lock poisoned"))?;
    query::get_track(db.connection(), track_id).map_err(|e| err(e.to_string()))
}

/// Resume when paused, otherwise start the current queue track.
#[tauri::command]
pub fn play(state: State<'_, AppState>) -> Result<(), CommandError> {
    state.engine.send(lumen_core::audio::AudioCommand::Play)?;
    Ok(())
}

#[tauri::command]
pub fn pause(state: State<'_, AppState>) -> Result<(), CommandError> {
    state.engine.send(lumen_core::audio::AudioCommand::Pause)?;
    Ok(())
}

#[tauri::command]
pub fn stop(state: State<'_, AppState>) -> Result<(), CommandError> {
    state.engine.send(lumen_core::audio::AudioCommand::Stop)?;
    Ok(())
}

#[tauri::command]
pub fn seek_to(state: State<'_, AppState>, position_ms: u64) -> Result<(), CommandError> {
    state
        .engine
        .send(lumen_core::audio::AudioCommand::SeekTo { position_ms })?;
    Ok(())
}

#[tauri::command]
pub fn set_volume(state: State<'_, AppState>, volume: f32) -> Result<(), CommandError> {
    let clamped = volume.clamp(0.0, 1.0);
    state
        .engine
        .send(lumen_core::audio::AudioCommand::SetVolume { volume: clamped })?;
    let mut config = state
        .config
        .lock()
        .map_err(|_| err("config state lock poisoned"))?;
    config.volume = clamped;
    save_config(&config)?;
    Ok(())
}

#[tauri::command]
pub fn set_repeat(state: State<'_, AppState>, mode: String) -> Result<(), CommandError> {
    let parsed = match mode.as_str() {
        "off" => lumen_core::playback::RepeatMode::Off,
        "all" => lumen_core::playback::RepeatMode::All,
        "one" => lumen_core::playback::RepeatMode::One,
        other => return Err(err(format!("unknown repeat mode: {other}"))),
    };
    state
        .engine
        .send(lumen_core::audio::AudioCommand::SetRepeat { mode: parsed })?;
    let mut config = state
        .config
        .lock()
        .map_err(|_| err("config state lock poisoned"))?;
    config.repeat = parsed;
    save_config(&config)?;
    Ok(())
}

/// Cross track boundaries without a gap. Persisted, and applied to the engine
/// immediately so the next boundary already behaves.
#[tauri::command]
pub fn set_gapless(state: State<'_, AppState>, enabled: bool) -> Result<(), CommandError> {
    state
        .engine
        .send(lumen_core::audio::AudioCommand::SetGapless { enabled })?;
    let mut config = state
        .config
        .lock()
        .map_err(|_| err("config state lock poisoned"))?;
    config.gapless = enabled;
    save_config(&config)?;
    Ok(())
}

#[tauri::command]
pub fn next_track(state: State<'_, AppState>) -> Result<(), CommandError> {
    state.engine.send(lumen_core::audio::AudioCommand::Next)?;
    Ok(())
}

#[tauri::command]
pub fn previous_track(state: State<'_, AppState>) -> Result<(), CommandError> {
    state
        .engine
        .send(lumen_core::audio::AudioCommand::Previous)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbInfo {
    pub schema_version: u32,
    pub path: String,
}

#[tauri::command]
pub fn get_db_info(state: State<'_, AppState>) -> Result<DbInfo, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    let schema_version = db.schema_version().map_err(|e| err(e.to_string()))?;
    let path = config::default_db_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string());
    Ok(DbInfo {
        schema_version,
        path,
    })
}

// ------------------------------------------------------------- library: folders

fn save_config(config: &Config) -> Result<(), CommandError> {
    let path = config::default_config_path().map_err(|e| err(e.to_string()))?;
    config.save(&path).map_err(|e| err(e.to_string()))
}

fn folder_strings(config: &Config) -> Vec<String> {
    config
        .library_folders
        .iter()
        .map(|p| p.display().to_string())
        .collect()
}

#[tauri::command]
pub fn add_library_folder(
    state: State<'_, AppState>,
    path: String,
) -> Result<Vec<String>, CommandError> {
    let candidate = PathBuf::from(path.trim());
    if !candidate.is_dir() {
        return Err(err(format!(
            "not an accessible directory: {}",
            candidate.display()
        )));
    }
    let canonical = candidate
        .canonicalize()
        .map_err(|e| err(format!("cannot canonicalize path: {e}")))?;

    let mut config = state
        .config
        .lock()
        .map_err(|_| err("config state lock poisoned"))?;
    if !config.library_folders.contains(&canonical) {
        config.library_folders.push(canonical);
        save_config(&config)?;
    }
    Ok(folder_strings(&config))
}

#[tauri::command]
pub fn remove_library_folder(
    state: State<'_, AppState>,
    path: String,
) -> Result<Vec<String>, CommandError> {
    let candidate = PathBuf::from(path.trim());
    let canonical = candidate.canonicalize().unwrap_or(candidate);

    let mut config = state
        .config
        .lock()
        .map_err(|_| err("config state lock poisoned"))?;
    config.library_folders.retain(|p| *p != canonical);
    save_config(&config)?;
    Ok(folder_strings(&config))
}

// ---------------------------------------------------------------- library: scan

#[tauri::command]
pub fn start_scan(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), CommandError> {
    let roots = {
        let config = state
            .config
            .lock()
            .map_err(|_| err("config state lock poisoned"))?;
        config
            .library_folders
            .iter()
            .filter_map(|p| {
                p.canonicalize()
                    .map_err(|e| {
                        tracing::warn!("skipping invalid library folder {}: {e}", p.display())
                    })
                    .ok()
            })
            .collect::<Vec<_>>()
    };
    if roots.is_empty() {
        return Err(err("no valid library folders configured"));
    }

    let mut slot = state
        .scan
        .lock()
        .map_err(|_| err("scan state lock poisoned"))?;
    if slot.as_ref().is_some_and(|h| !h.is_finished()) {
        return Err(err("a scan is already running"));
    }
    *slot = None; // drop a finished handle

    let handle = Scanner::start(state.db_path.clone(), state.artwork_dir.clone(), roots);

    // Forward scan events to the frontend.
    let events = handle.events().clone();
    std::thread::Builder::new()
        .name("lumen-scan-forwarder".into())
        .spawn(move || {
            while let Ok(event) = events.recv() {
                if let Err(e) = app.emit("library-event", &event) {
                    tracing::warn!("failed to forward scan event: {e}");
                    break;
                }
            }
        })
        .map_err(|e| err(format!("spawn scan forwarder: {e}")))?;

    *slot = Some(handle);
    Ok(())
}

#[tauri::command]
pub fn cancel_scan(state: State<'_, AppState>) -> Result<(), CommandError> {
    let slot = state
        .scan
        .lock()
        .map_err(|_| err("scan state lock poisoned"))?;
    match slot.as_ref() {
        Some(handle) if !handle.is_finished() => {
            handle.cancel();
            Ok(())
        }
        _ => Err(err("no scan is running")),
    }
}

/// Take a finished scan handle out of the slot (called by the shell when it
/// observes a terminal event; harmless to call anytime).
#[tauri::command]
pub fn clear_finished_scan(state: State<'_, AppState>) -> Result<(), CommandError> {
    let mut slot = state
        .scan
        .lock()
        .map_err(|_| err("scan state lock poisoned"))?;
    if slot.as_ref().is_some_and(ScanHandle::is_finished) {
        *slot = None;
    }
    Ok(())
}

// -------------------------------------------------------------- library: query

#[tauri::command]
pub fn get_library_stats(state: State<'_, AppState>) -> Result<LibraryStats, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    lumen_core::library::query::stats(db.connection()).map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn list_tracks(
    state: State<'_, AppState>,
    limit: u32,
    offset: u32,
) -> Result<Vec<TrackRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    lumen_core::library::query::list_tracks(db.connection(), limit.min(500), offset)
        .map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn list_artists(state: State<'_, AppState>) -> Result<Vec<ArtistRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    lumen_core::library::query::list_artists(db.connection()).map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn list_albums(state: State<'_, AppState>) -> Result<Vec<AlbumRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    lumen_core::library::query::list_albums(db.connection()).map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn search_tracks(
    state: State<'_, AppState>,
    needle: String,
    limit: u32,
) -> Result<Vec<TrackRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    lumen_core::library::query::search_tracks(db.connection(), &needle, limit.min(200))
        .map_err(|e| err(e.to_string()))
}

/// Tracks of one album, in disc/track order — the queue an album should play.
#[tauri::command]
pub fn list_album_tracks(
    state: State<'_, AppState>,
    album_id: i64,
) -> Result<Vec<TrackRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::tracks_by_album(db.connection(), album_id).map_err(|e| err(e.to_string()))
}

/// Releases by one artist — the library-browse view of an artist.
#[tauri::command]
pub fn list_artist_albums(
    state: State<'_, AppState>,
    artist_id: i64,
) -> Result<Vec<AlbumRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::albums_by_artist(db.connection(), artist_id).map_err(|e| err(e.to_string()))
}

// -------------------------------------------------------------- library: genres

/// Genres the scanner indexed, with active track counts.
#[tauri::command]
pub fn list_genres(state: State<'_, AppState>) -> Result<Vec<GenreRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::list_genres(db.connection()).map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn list_genre_tracks(
    state: State<'_, AppState>,
    genre: String,
    limit: u32,
) -> Result<Vec<TrackRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::tracks_by_genre(db.connection(), &genre, limit.min(500)).map_err(|e| err(e.to_string()))
}

// ----------------------------------------------------------- library: favorites

/// Favorited tracks. Backed by `tracks.favorited_at` (schema v1).
#[tauri::command]
pub fn list_favorites(state: State<'_, AppState>) -> Result<Vec<TrackRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::list_favorites(db.connection(), 500).map_err(|e| err(e.to_string()))
}

/// Favorite/unfavorite a track. Returns the resulting state.
#[tauri::command]
pub fn set_favorite(
    state: State<'_, AppState>,
    track_id: i64,
    favorite: bool,
) -> Result<bool, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::set_favorite(db.connection(), track_id, favorite).map_err(|e| err(e.to_string()))
}

// ----------------------------------------------------------- library: playlists

#[tauri::command]
pub fn list_playlists(state: State<'_, AppState>) -> Result<Vec<PlaylistRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::list_playlists(db.connection()).map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn list_playlist_tracks(
    state: State<'_, AppState>,
    playlist_id: i64,
) -> Result<Vec<TrackRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::playlist_tracks(db.connection(), playlist_id).map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn create_playlist(state: State<'_, AppState>, name: String) -> Result<i64, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::create_playlist(db.connection(), &name).map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn rename_playlist(
    state: State<'_, AppState>,
    playlist_id: i64,
    name: String,
) -> Result<(), CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::rename_playlist(db.connection(), playlist_id, &name).map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn delete_playlist(state: State<'_, AppState>, playlist_id: i64) -> Result<(), CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::delete_playlist(db.connection(), playlist_id).map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn add_to_playlist(
    state: State<'_, AppState>,
    playlist_id: i64,
    track_id: i64,
) -> Result<(), CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::add_to_playlist(db.connection(), playlist_id, track_id).map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn remove_from_playlist(
    state: State<'_, AppState>,
    playlist_id: i64,
    track_id: i64,
) -> Result<(), CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::remove_from_playlist(db.connection(), playlist_id, track_id)
        .map_err(|e| err(e.to_string()))
}

/// Move a playlist entry. Takes a mutable connection: the reorder is one
/// transaction, and the app's single DB handle is not otherwise borrowed.
#[tauri::command]
pub fn reorder_playlist(
    state: State<'_, AppState>,
    playlist_id: i64,
    from: u32,
    to: u32,
) -> Result<(), CommandError> {
    let mut db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    let conn = db
        .connection_mut()
        .ok_or_else(|| err("database unavailable"))?;
    query::reorder_playlist(conn, playlist_id, from, to).map_err(|e| err(e.to_string()))
}

// ------------------------------------------------------------- library: history

/// How far back the listening wall reaches.
const HISTORY_WINDOW_HOURS: u64 = 24;

/// A hard ceiling on retained rows, so the table cannot grow without bound.
///
/// This is the *only* thing that deletes history. The 24-hour wall is a filter,
/// not a purge: what you played last month is not thrown away to keep the view
/// tidy, which is what a prune would do.
const HISTORY_ROW_CAP: u32 = 50_000;

/// Seconds since the epoch, or 0 if the clock is before 1970.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The start of the listening window, as a unix timestamp.
fn history_cutoff() -> i64 {
    now_unix() - (HISTORY_WINDOW_HOURS * 3600) as i64
}

/// Recent plays from the last day, newest first.
///
/// Older rows are kept - they are the raw material for the recap - and only the
/// cap ever deletes. The wall is what narrows.
#[tauri::command]
pub fn list_history(state: State<'_, AppState>) -> Result<Vec<HistoryRow>, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    let conn = db.connection();
    query::trim_history(conn, HISTORY_ROW_CAP).map_err(|e| err(e.to_string()))?;
    query::list_history(conn, 200).map_err(|e| err(e.to_string()))
}

/// Credit listening time to a track's most recent play.
#[tauri::command]
pub fn add_listened_time(
    state: State<'_, AppState>,
    track_id: i64,
    listened_ms: i64,
) -> Result<(), CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::add_listened_time(db.connection(), track_id, listened_ms, history_cutoff())
        .map(|_| ())
        .map_err(|e| err(e.to_string()))
}

/// What was actually listened to inside the window.
///
/// Deliberately separate from the library totals: "1h 7m runtime" over a
/// collection is the length of the collection, which says nothing about what you
/// played today. This is the latter.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistorySummary {
    pub window_hours: u64,
    pub plays: i64,
    pub tracks: i64,
    pub albums: i64,
    pub artists: i64,
    pub listened_ms: i64,
}

/// Totals for the listening window.
#[tauri::command]
pub fn history_summary(state: State<'_, AppState>) -> Result<HistorySummary, CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    let conn = db.connection();
    let cutoff = history_cutoff();
    let (plays, tracks, albums, artists, listened_ms) = conn
        .query_row(
            "SELECT COUNT(*), \
                    COUNT(DISTINCT h.track_id), \
                    COUNT(DISTINCT t.album_id), \
                    COUNT(DISTINCT t.artist_id), \
                    COALESCE(SUM(h.listened_ms), 0) \
             FROM history h JOIN tracks t ON t.id = h.track_id \
             WHERE h.played_at >= ?1",
            [cutoff],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .map_err(|e| err(e.to_string()))?;
    Ok(HistorySummary {
        window_hours: HISTORY_WINDOW_HOURS,
        plays,
        tracks,
        albums,
        artists,
        listened_ms,
    })
}

/// Note that a track started playing. The shell calls this when the engine
/// reports a new stream; `listened_ms` is the duration known at that point.
#[tauri::command]
pub fn record_play(
    state: State<'_, AppState>,
    track_id: i64,
    listened_ms: i64,
) -> Result<(), CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::record_history(db.connection(), track_id, listened_ms).map_err(|e| err(e.to_string()))
}

#[tauri::command]
pub fn clear_history(state: State<'_, AppState>) -> Result<(), CommandError> {
    let db = state
        .db
        .lock()
        .map_err(|_| err("database state lock poisoned"))?;
    query::clear_history(db.connection()).map_err(|e| err(e.to_string()))
}
