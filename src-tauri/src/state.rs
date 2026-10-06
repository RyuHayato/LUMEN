use std::path::PathBuf;
use std::sync::Mutex;

use lumen_core::audio::EngineHandle;
use lumen_core::config::Config;
use lumen_core::db::Database;
use lumen_core::library::scanner::ScanHandle;

/// Application state managed by Tauri.
///
/// The shell owns config and the database connection; the core engine owns
/// playback state internally; at most one library scan runs at a time.
pub struct AppState {
    pub config: Mutex<Config>,
    pub db: Mutex<Database>,
    pub engine: EngineHandle,
    pub scan: Mutex<Option<ScanHandle>>,
    pub db_path: PathBuf,
    pub artwork_dir: PathBuf,
}
