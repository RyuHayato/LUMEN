//! Typed errors for lumen-core modules.
//!
//! Policy: libraries return typed errors; the application shell maps them to
//! user-facing messages. No silent swallowing.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config at {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write config at {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config at {path}: {source}")]
    Parse {
        path: PathBuf,
        // Boxed: toml::de::Error is large; keep ConfigError small (clippy::result_large_err).
        source: Box<toml::de::Error>,
    },
    #[error("failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("could not determine the application data directory")]
    NoAppDataDir,
}

#[derive(Debug, Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("failed to create database directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("database schema version {found} is newer than supported version {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("conflict: {0}")]
    Conflict(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AudioError {
    #[error("audio backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("output device unavailable: {0}")]
    DeviceUnavailable(String),
    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),
    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlaybackError {
    #[error("invalid playback state transition: {from} -> {to}")]
    InvalidTransition { from: String, to: String },
    #[error("queue is empty")]
    EmptyQueue,
    #[error("no current track")]
    NoCurrentTrack,
}

#[derive(Debug, Error)]
pub enum LibraryError {
    #[error("i/o error while scanning {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("unreadable tags in {path}: {message}")]
    Metadata { path: PathBuf, message: String },
    #[error("unreadable audio stream in {path}: {message}")]
    Probe { path: PathBuf, message: String },
    #[error("artwork failure at {path}: {message}")]
    Artwork { path: PathBuf, message: String },
    #[error("database error: {0}")]
    Db(#[from] DbError),
}
