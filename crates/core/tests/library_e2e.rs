//! End-to-end library engine tests against synthetic WAV fixtures.
//!
//! Fixtures are generated programmatically (PCM WAV with RIFF INFO tags) so
//! every assertion runs against real files parsed by lofty + symphonia.
//! All media writes happen in temp dirs; the scanner is read-only with
//! respect to these files.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lumen_core::library::query;
use lumen_core::library::scanner::{ScanEvent, ScanSummary, Scanner};

// ---------------------------------------------------------------- fixtures

/// Minimal but valid PCM WAV with an optional RIFF INFO tag chunk.
/// Low sample rate keeps fixtures tiny: 8000 Hz, mono, 8-bit.
fn wav_bytes(duration_ms: u64, tags: &[(&str, &str)]) -> Vec<u8> {
    let sample_rate: u32 = 8_000;
    let channels: u16 = 1;
    let bits: u16 = 8;
    let byte_rate = sample_rate * u32::from(channels) * u32::from(bits / 8);
    let data_len = (duration_ms as usize) * byte_rate as usize / 1000;

    let mut fmt_chunk = Vec::with_capacity(16);
    fmt_chunk.extend_from_slice(&1u16.to_le_bytes()); // PCM
    fmt_chunk.extend_from_slice(&channels.to_le_bytes());
    fmt_chunk.extend_from_slice(&sample_rate.to_le_bytes());
    fmt_chunk.extend_from_slice(&byte_rate.to_le_bytes());
    fmt_chunk.extend_from_slice(&(channels * (bits / 8)).to_le_bytes());
    fmt_chunk.extend_from_slice(&bits.to_le_bytes());

    let mut info_payload = b"INFO".to_vec();
    for (fourcc, value) in tags {
        assert_eq!(fourcc.len(), 4, "INFO keys must be four characters");
        let bytes = value.as_bytes();
        info_payload.extend_from_slice(fourcc.as_bytes());
        // RIFF strings are NUL-terminated.
        info_payload.extend_from_slice(&((bytes.len() + 1) as u32).to_le_bytes());
        info_payload.extend_from_slice(bytes);
        info_payload.push(0);
        if bytes.len() % 2 == 0 {
            info_payload.push(0); // chunks are word-aligned (size includes NUL)
        }
    }

    let mut riff_payload = b"WAVE".to_vec();
    riff_payload.extend_from_slice(b"fmt ");
    riff_payload.extend_from_slice(&(fmt_chunk.len() as u32).to_le_bytes());
    riff_payload.extend_from_slice(&fmt_chunk);
    if !tags.is_empty() {
        riff_payload.extend_from_slice(b"LIST");
        riff_payload.extend_from_slice(&(info_payload.len() as u32).to_le_bytes());
        riff_payload.extend_from_slice(&info_payload);
        if info_payload.len() % 2 == 1 {
            riff_payload.push(0);
        }
    }
    riff_payload.extend_from_slice(b"data");
    riff_payload.extend_from_slice(&(data_len as u32).to_le_bytes());
    riff_payload.extend(std::iter::repeat_n(128u8, data_len)); // DC "silence"

    let mut file = b"RIFF".to_vec();
    file.extend_from_slice(&(riff_payload.len() as u32).to_le_bytes());
    file.extend_from_slice(&riff_payload);
    file
}

fn write_wav(path: &Path, duration_ms: u64, tags: &[(&str, &str)]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, wav_bytes(duration_ms, tags)).unwrap();
}

/// Tiny structurally-valid JPEG (magic bytes + minimal JFIF segment + EOI).
const JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
];

struct Fixture {
    root: PathBuf,
    db: PathBuf,
    artwork: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "lumen-e2e-{}-{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let fixture = Self {
            root: base.join("music"),
            db: base.join("db").join("lumen.db"),
            artwork: base.join("artwork"),
        };
        fs::create_dir_all(&fixture.root).unwrap();
        fixture
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.root.parent().unwrap());
    }
}

// ------------------------------------------------------------------ helpers

fn run_scan(fixture: &Fixture) -> ScanSummary {
    let handle = Scanner::start(
        fixture.db.clone(),
        fixture.artwork.clone(),
        vec![fixture.root.clone()],
    );
    let mut terminal = None;
    while let Ok(event) = handle.events().recv() {
        match event {
            ScanEvent::Completed { summary } | ScanEvent::Canceled { summary } => {
                terminal = Some(summary);
                break;
            }
            ScanEvent::Failed { message } => panic!("scan failed fatally: {message}"),
            _ => {}
        }
    }
    handle.wait();
    terminal.expect("scan must emit a terminal event")
}

fn conn(fixture: &Fixture) -> rusqlite::Connection {
    rusqlite::Connection::open(&fixture.db).unwrap()
}

fn scalar(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

// -------------------------------------------------------------------- tests

#[test]
fn full_scan_inserts_metadata_artwork_and_categorizes_failures() {
    let f = Fixture::new("full");
    write_wav(
        &f.root.join("artist/album/01 - First.wav"),
        1000,
        &[
            ("INAM", "First Track"),
            ("IART", "Test Artist"),
            ("IPRD", "Test Album"),
            ("IGNR", "Ambient; IDM"),
            ("ITRK", "1"),
        ],
    );
    write_wav(
        &f.root.join("artist/album/02 - Second.wav"),
        2000,
        &[("INAM", "Second")],
    );
    write_wav(&f.root.join("loose.wav"), 500, &[]); // no tags at all
    fs::write(f.root.join("artist/album/cover.jpg"), JPEG).unwrap();
    fs::write(f.root.join("notes.txt"), b"not audio").unwrap();
    fs::write(f.root.join("broken.flac"), b"definitely not flac data").unwrap();

    let summary = run_scan(&f);
    assert!(!summary.canceled);
    assert_eq!(summary.files_seen, 4, "3 wav + 1 broken.flac");
    assert_eq!(summary.inserted_or_updated, 3);
    assert_eq!(summary.failed, 1, "corrupt flac must fail probing");
    assert_eq!(summary.failures[0].kind, "probe");

    let conn = conn(&f);
    assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM tracks"), 3);
    assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM artists"), 1);
    assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM albums"), 1);
    assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM genres"), 2);

    // Stream properties come from symphonia (8 kHz/mono/8-bit PCM).
    let (codec, rate, channels, bit_depth, duration): (String, i64, i64, i64, i64) = conn
        .query_row(
            "SELECT codec, sample_rate_hz, channels, bit_depth, duration_ms \
             FROM tracks WHERE title = 'First Track'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(codec, "pcm");
    assert_eq!(rate, 8000);
    assert_eq!(channels, 1);
    assert_eq!(bit_depth, 8);
    assert!(
        (900..=1100).contains(&duration),
        "duration ~1000ms, got {duration}"
    );

    // Untagged file: title falls back to the file stem; artist stays NULL.
    let (title, artist): (String, Option<String>) = conn
        .query_row(
            "SELECT title, (SELECT name FROM artists WHERE id = tracks.artist_id) \
             FROM tracks WHERE path LIKE '%loose.wav'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(title, "loose");
    assert_eq!(artist, None);

    // Sidecar artwork: content-addressed, shared by both album tracks.
    let artworks = scalar(&conn, "SELECT COUNT(*) FROM artworks");
    assert_eq!(artworks, 1);
    let with_art = scalar(
        &conn,
        "SELECT COUNT(*) FROM tracks WHERE artwork_hash IS NOT NULL",
    );
    assert_eq!(with_art, 2, "loose.wav lives outside the album dir");
    assert!(f.artwork.read_dir().unwrap().next().is_some());
}

#[test]
fn incremental_scan_skips_unchanged_files() {
    let f = Fixture::new("incremental");
    for i in 0..10 {
        write_wav(
            &f.root.join(format!("t{i}.wav")),
            100,
            &[("IART", "Artist")],
        );
    }
    let first = run_scan(&f);
    assert_eq!(first.inserted_or_updated, 10);

    let second = run_scan(&f);
    assert_eq!(second.inserted_or_updated, 0);
    assert_eq!(second.skipped_unchanged, 10);
    assert_eq!(second.marked_missing, 0);
}

#[test]
fn modified_file_is_reprocessed() {
    let f = Fixture::new("modified");
    let path = f.root.join("track.wav");
    write_wav(&path, 1000, &[("INAM", "Before")]);
    run_scan(&f);

    // Different duration → different size → Modified classification.
    write_wav(&path, 3000, &[("INAM", "After")]);
    let summary = run_scan(&f);
    assert_eq!(summary.inserted_or_updated, 1);

    let conn = conn(&f);
    let (title, duration): (String, i64) = conn
        .query_row("SELECT title, duration_ms FROM tracks", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(title, "After");
    assert!((2900..=3100).contains(&duration));
    assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM tracks"), 1);
}

#[test]
fn deleted_file_is_soft_marked_then_restored() {
    let f = Fixture::new("deleted");
    let path = f.root.join("track.wav");
    let bytes = wav_bytes(100, &[("INAM", "Boomerang")]);
    fs::write(&path, &bytes).unwrap();
    let original_mtime =
        filetime::FileTime::from_last_modification_time(&fs::metadata(&path).unwrap());
    run_scan(&f);

    // "Drive unplugged": file disappears.
    fs::remove_file(&path).unwrap();
    let summary = run_scan(&f);
    assert_eq!(summary.marked_missing, 1);

    let conn = conn(&f);
    assert_eq!(
        scalar(
            &conn,
            "SELECT COUNT(*) FROM tracks WHERE missing_since IS NOT NULL"
        ),
        1
    );
    assert_eq!(
        scalar(&conn, "SELECT COUNT(*) FROM tracks"),
        1,
        "missing rows are never deleted"
    );

    // "Drive reconnected": same content, same mtime → unchanged → the
    // resurrected path (clear_missing) applies, not reprocessing.
    fs::write(&path, &bytes).unwrap();
    filetime::set_file_mtime(&path, original_mtime).unwrap();
    let summary = run_scan(&f);
    assert_eq!(summary.unmarked_missing, 1);
    assert_eq!(summary.inserted_or_updated, 0);
    assert_eq!(
        scalar(
            &conn,
            "SELECT COUNT(*) FROM tracks WHERE missing_since IS NOT NULL"
        ),
        0
    );
}

#[test]
fn moved_file_keeps_its_identity() {
    let f = Fixture::new("moved");
    let original = f.root.join("old/track.wav");
    write_wav(&original, 777, &[("INAM", "Mover")]);
    run_scan(&f);

    let conn = conn(&f);
    let id_before: i64 = scalar(&conn, "SELECT id FROM tracks");

    let moved = f.root.join("new/nested/track-renamed.wav");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(&original, &moved).unwrap();

    let summary = run_scan(&f);
    assert_eq!(summary.relinked_moved, 1, "size+mtime match must relink");
    assert_eq!(summary.marked_missing, 0);

    let (id_after, path_after): (i64, String) = conn
        .query_row("SELECT id, path FROM tracks", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(id_after, id_before, "identity must survive the move");
    assert!(path_after.ends_with("track-renamed.wav"));
    assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM tracks"), 1);
}

#[test]
fn empty_directory_scans_cleanly() {
    let f = Fixture::new("empty");
    let summary = run_scan(&f);
    assert!(!summary.canceled);
    assert_eq!(summary.files_seen, 0);
    assert_eq!(summary.inserted_or_updated, 0);
    assert_eq!(summary.failed, 0);
}

#[test]
fn cancellation_is_cooperative_and_leaves_consistent_db() {
    let f = Fixture::new("cancel");
    for i in 0..300 {
        write_wav(
            &f.root.join(format!("t{i:04}.wav")),
            50,
            &[("IART", "Artist")],
        );
    }

    let handle = Scanner::start(f.db.clone(), f.artwork.clone(), vec![f.root.clone()]);
    // Cancel before the pipeline can make progress: deterministic.
    handle.cancel();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut canceled = None;
    while Instant::now() < deadline {
        match handle.events().recv_timeout(Duration::from_secs(1)) {
            Ok(ScanEvent::Canceled { summary }) => {
                canceled = Some(summary);
                break;
            }
            Ok(ScanEvent::Completed { summary }) => {
                // A scan that finished before observing cancel is not a
                // failure of cancellation — but with 300 files and an
                // already-set flag this must not happen.
                panic!("scan completed despite pre-set cancel flag: {summary:?}");
            }
            Ok(ScanEvent::Failed { message }) => panic!("fatal scan error: {message}"),
            _ => continue,
        }
    }
    handle.wait();
    let summary = canceled.expect("expected a Canceled event");
    assert!(summary.canceled);

    // The database is queryable and internally consistent after cancel.
    let c = conn(&f);
    let stats = query::stats(&c).unwrap();
    assert!(stats.tracks <= 300);
}

#[test]
fn query_api_exercises_library_views() {
    let f = Fixture::new("query");
    write_wav(
        &f.root.join("a/01.wav"),
        100,
        &[
            ("INAM", "Morning Song"),
            ("IART", "Alpha"),
            ("IPRD", "First Record"),
            ("IGNR", "Jazz"),
            ("ITRK", "1"),
        ],
    );
    write_wav(
        &f.root.join("a/02.wav"),
        100,
        &[
            ("INAM", "Evening Song"),
            ("IART", "Alpha"),
            ("IPRD", "First Record"),
            ("IGNR", "Jazz"),
            ("ITRK", "2"),
        ],
    );
    write_wav(
        &f.root.join("b/01.wav"),
        100,
        &[
            ("INAM", "Different"),
            ("IART", "Beta"),
            ("IPRD", "Other"),
            ("IGNR", "Rock"),
        ],
    );
    run_scan(&f);

    let conn = conn(&f);

    let artists = query::list_artists(&conn).unwrap();
    assert_eq!(artists.len(), 2);
    let alpha = artists.iter().find(|a| a.name == "Alpha").unwrap();
    assert_eq!(alpha.track_count, 2);

    let albums = query::albums_by_artist(&conn, alpha.id).unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].title, "First Record");
    assert_eq!(albums[0].track_count, 2);

    let tracks = query::tracks_by_album(&conn, albums[0].id).unwrap();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].title, "Morning Song", "ordered by track number");

    let found = query::search_tracks(&conn, "song", 10).unwrap();
    assert_eq!(found.len(), 2);
    let found = query::search_tracks(&conn, "EVENING", 10).unwrap();
    assert_eq!(found.len(), 1, "normalized case-insensitive search");

    let jazz = query::tracks_by_genre(&conn, "jazz", 10).unwrap();
    assert_eq!(jazz.len(), 2);

    let page = query::list_tracks(&conn, 2, 0).unwrap();
    assert_eq!(page.len(), 2);
    let rest = query::list_tracks(&conn, 10, 2).unwrap();
    assert_eq!(rest.len(), 1);

    let stats = query::stats(&conn).unwrap();
    assert_eq!(stats.tracks, 3);
    assert_eq!(stats.albums, 2);
    assert_eq!(stats.artists, 2);
    assert_eq!(stats.genres, 2);
}

#[test]
fn scan_throughput_smoke_test() {
    // Synthetic throughput check: 2 000 tiny files. Numbers are printed for
    // the report; assertions stay loose to avoid CI noise.
    let f = Fixture::new("perf");
    let build_start = Instant::now();
    for album in 0..40 {
        for track in 0..50 {
            write_wav(
                &f.root
                    .join(format!("artist{album:02}/album{album:02}/t{track:02}.wav")),
                50,
                &[
                    ("INAM", &format!("Track {track}")[..]),
                    ("IART", "Perf Artist"),
                    ("IPRD", "Perf Album"),
                ],
            );
        }
    }
    let build_ms = build_start.elapsed().as_millis();

    let scan_start = Instant::now();
    let first = run_scan(&f);
    let first_ms = scan_start.elapsed().as_millis();

    let inc_start = Instant::now();
    let second = run_scan(&f);
    let inc_ms = inc_start.elapsed().as_millis();

    println!(
        "PERF fixtures={}ms full_scan={}ms ({} files, {} inserted) \
         incremental={}ms ({} skipped)",
        build_ms,
        first_ms,
        first.files_seen,
        first.inserted_or_updated,
        inc_ms,
        second.skipped_unchanged
    );

    assert_eq!(first.inserted_or_updated, 2000);
    assert_eq!(second.inserted_or_updated, 0);
    assert_eq!(second.skipped_unchanged, 2000);
    assert!(
        inc_ms < first_ms,
        "incremental ({inc_ms}ms) must beat full scan ({first_ms}ms)"
    );
}
