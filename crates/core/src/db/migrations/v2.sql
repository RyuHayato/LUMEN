-- LUMEN database schema v2 (Phase 1: library engine).
-- Applied transactionally after v1. Existing data is preserved.

-- Artwork cache registry. Image bytes live in the content-addressed cache on
-- disk (see ARCHITECTURE.md §9); this table maps a content hash to a file.
CREATE TABLE artworks (
    hash        TEXT PRIMARY KEY,           -- sha256 hex of image bytes
    path        TEXT NOT NULL,              -- relative to the artwork cache dir
    mime        TEXT NOT NULL,              -- 'image/jpeg' | 'image/png'
    source      TEXT NOT NULL,              -- 'embedded' | 'sidecar'
    size_bytes  INTEGER NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch ())
);

-- Track-level artwork (album-level art is derived at query time from any
-- artwork-bearing track of the album).
ALTER TABLE tracks ADD COLUMN artwork_hash TEXT REFERENCES artworks (hash);

-- Soft-delete for library identity: a file that disappeared is marked, never
-- deleted (removable drives come back; favorites/history must survive).
-- NULL means the file was present at the last completed scan.
ALTER TABLE tracks ADD COLUMN missing_since INTEGER;
