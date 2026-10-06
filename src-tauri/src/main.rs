// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod state;

use std::sync::Mutex;

use lumen_core::audio::AudioEngine;
use lumen_core::config::{self, Config};
use lumen_core::db::Database;
use state::AppState;
use tauri::{Emitter, Manager};
use tracing_subscriber::EnvFilter;

fn init_logging() {
    let filter = EnvFilter::try_from_env("LUMEN_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Fatal startup failures are loud and immediate — never limp along silently.
fn fatal(message: &str) -> ! {
    tracing::error!("{message}");
    eprintln!("LUMEN failed to start: {message}");
    std::process::exit(1);
}

fn main() {
    init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "LUMEN starting");

    let config = match config::load_default() {
        Ok(config) => config,
        Err(e) => {
            tracing::error!("failed to load config, falling back to defaults: {e}");
            Config::default()
        }
    };

    let db_path = match config::default_db_path() {
        Ok(path) => path,
        Err(e) => fatal(&format!("cannot resolve database path: {e}")),
    };
    let db = match Database::open(&db_path) {
        Ok(db) => db,
        Err(e) => fatal(&format!(
            "failed to open database at {}: {e}",
            db_path.display()
        )),
    };
    tracing::info!(path = %db_path.display(), "database ready");

    // The track resolver answers path lookups for the engine from SQLite.
    // It owns its own connection; the engine thread never touches the app's
    // database handle (single-writer discipline is unaffected: reads only).
    let resolver: lumen_core::audio::TrackResolver = {
        let db_path = db_path.clone();
        match lumen_core::db::Database::open(&db_path) {
            Ok(db) => {
                let conn = db.into_connection();
                Box::new(move |id: lumen_core::TrackId| {
                    conn.query_row("SELECT path FROM tracks WHERE id = ?1", [id], |row| {
                        row.get::<_, String>(0)
                    })
                    .ok()
                    .map(std::path::PathBuf::from)
                })
            }
            Err(e) => {
                tracing::error!(
                    "resolver database unavailable, playback will not resolve tracks: {e}"
                );
                Box::new(|_| None)
            }
        }
    };

    let engine = AudioEngine::start_production(resolver, config.volume);

    // Apply persisted output preferences before the first Play.
    let _ = engine.send(lumen_core::audio::AudioCommand::SetOutputMode {
        mode: config.output_mode,
    });
    let _ = engine.send(lumen_core::audio::AudioCommand::SetOutputDevice {
        device_id: config.output_device_id.clone(),
    });
    let _ = engine.send(lumen_core::audio::AudioCommand::SetGapless {
        enabled: config.gapless,
    });

    let artwork_dir = match config::default_artwork_dir() {
        Ok(dir) => dir,
        Err(e) => fatal(&format!("cannot resolve artwork directory: {e}")),
    };

    tauri::Builder::default()
        .manage(AppState {
            config: Mutex::new(config),
            db: Mutex::new(db),
            engine,
            scan: Mutex::new(None),
            db_path,
            artwork_dir,
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_app_info,
            commands::get_config,
            commands::get_playback_state,
            commands::get_db_info,
            commands::add_library_folder,
            commands::remove_library_folder,
            commands::start_scan,
            commands::cancel_scan,
            commands::clear_finished_scan,
            commands::get_library_stats,
            commands::list_tracks,
            commands::list_artists,
            commands::list_albums,
            commands::search_tracks,
            commands::list_album_tracks,
            commands::list_artist_albums,
            commands::list_genres,
            commands::list_genre_tracks,
            commands::list_favorites,
            commands::set_favorite,
            commands::list_playlists,
            commands::list_playlist_tracks,
            commands::create_playlist,
            commands::rename_playlist,
            commands::delete_playlist,
            commands::add_to_playlist,
            commands::remove_from_playlist,
            commands::reorder_playlist,
            commands::list_history,
            commands::add_listened_time,
            commands::history_summary,
            commands::record_play,
            commands::clear_history,
            commands::play_tracks,
            commands::get_track,
            commands::play,
            commands::pause,
            commands::stop,
            commands::seek_to,
            commands::set_volume,
            commands::set_repeat,
            commands::set_gapless,
            commands::next_track,
            commands::previous_track,
            commands::list_output_devices,
            commands::set_output_device,
            commands::set_output_mode,
            commands::get_artwork,
        ])
        .setup(|app| {
            // Forward engine events to the frontend as "audio-event".
            let events = app.state::<AppState>().engine.events().clone();
            let handle = app.handle().clone();
            let _forwarder = std::thread::Builder::new()
                .name("lumen-event-forwarder".into())
                .spawn(move || {
                    while let Ok(event) = events.recv() {
                        if let Err(e) = handle.emit("audio-event", &event) {
                            tracing::warn!("failed to forward audio event: {e}");
                            break;
                        }
                    }
                })?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running LUMEN");
}
