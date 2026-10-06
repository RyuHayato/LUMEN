-- LUMEN database schema v1.
-- See docs/ARCHITECTURE.md §8 for rationale and deliberately omitted fields.

CREATE TABLE artists (
    id              INTEGER PRIMARY KEY,
    name            TEXT NOT NULL,
    name_normalized TEXT NOT NULL
);
CREATE INDEX idx_artists_name ON artists (name_normalized);

CREATE TABLE albums (
    id              INTEGER PRIMARY KEY,
    title           TEXT NOT NULL,
    title_normalized TEXT NOT NULL,
    album_artist_id INTEGER REFERENCES artists (id) ON DELETE SET NULL,
    year            INTEGER,
    UNIQUE (title_normalized, album_artist_id)
);
CREATE INDEX idx_albums_title ON albums (title_normalized);

CREATE TABLE tracks (
    id               INTEGER PRIMARY KEY,
    path             TEXT NOT NULL UNIQUE,
    title            TEXT NOT NULL,
    title_normalized TEXT NOT NULL,
    album_id         INTEGER REFERENCES albums (id) ON DELETE SET NULL,
    artist_id        INTEGER REFERENCES artists (id) ON DELETE SET NULL,
    track_no         INTEGER,
    disc_no          INTEGER,
    duration_ms      INTEGER NOT NULL DEFAULT 0,
    codec            TEXT,
    container        TEXT,
    sample_rate_hz   INTEGER,
    bit_depth        INTEGER,
    channels         INTEGER,
    file_size        INTEGER NOT NULL DEFAULT 0,
    file_mtime       INTEGER NOT NULL DEFAULT 0,
    rg_track_gain_db REAL,
    rg_album_gain_db REAL,
    favorited_at     INTEGER,
    added_at         INTEGER NOT NULL DEFAULT (unixepoch ()),
    updated_at       INTEGER NOT NULL DEFAULT (unixepoch ())
);
CREATE INDEX idx_tracks_album ON tracks (album_id);
CREATE INDEX idx_tracks_artist ON tracks (artist_id);
CREATE INDEX idx_tracks_title ON tracks (title_normalized);

CREATE TABLE genres (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE track_genres (
    track_id INTEGER NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    genre_id INTEGER NOT NULL REFERENCES genres (id) ON DELETE CASCADE,
    PRIMARY KEY (track_id, genre_id)
);

CREATE TABLE playlists (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch ()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch ())
);

CREATE TABLE playlist_entries (
    playlist_id INTEGER NOT NULL REFERENCES playlists (id) ON DELETE CASCADE,
    track_id    INTEGER NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    UNIQUE (playlist_id, position)
);

CREATE TABLE history (
    id          INTEGER PRIMARY KEY,
    track_id    INTEGER NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    played_at   INTEGER NOT NULL DEFAULT (unixepoch ()),
    listened_ms INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_history_played_at ON history (played_at);

-- Singleton: exactly one row (id = 1) holding the persisted playback session.
CREATE TABLE playback_state (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    queue_json   TEXT NOT NULL DEFAULT '[]',
    current_idx  INTEGER,
    position_ms  INTEGER NOT NULL DEFAULT 0,
    volume       REAL NOT NULL DEFAULT 1.0,
    repeat_mode  TEXT NOT NULL DEFAULT 'off',
    shuffle      INTEGER NOT NULL DEFAULT 0,
    updated_at   INTEGER NOT NULL DEFAULT (unixepoch ())
);

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
