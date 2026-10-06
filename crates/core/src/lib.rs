//! lumen-core — domain, playback, audio, library, and persistence for LUMEN.
//!
//! This crate has no UI-framework dependencies. The UI talks to the core
//! through command/event boundaries defined here.

pub mod audio;
pub mod config;
pub mod db;
pub mod error;
pub mod library;
pub mod playback;

pub use error::{AudioError, ConfigError, DbError, LibraryError, PlaybackError};

/// Stable identifier for a track. Matches the SQLite `tracks.id` rowid.
pub type TrackId = i64;
