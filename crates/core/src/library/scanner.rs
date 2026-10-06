//! The library scanner: filesystem → classification → metadata pool →
//! single batched DB writer, with cooperative cancellation and progress
//! events.
//!
//! Thread model (ADR-005, no async):
//! - **walk thread**: recursive enumeration + classification. Never touches
//!   the database directly.
//! - **metadata pool** (N = min(4, cores)): lofty tags + symphonia probe +
//!   artwork discovery per file. Pure workers; failures become items, not
//!   panics.
//! - **scanner thread (the writer)**: consumes results, commits batches in
//!   transactions, then performs move reconciliation and missing-marking.
//!
//! Cancellation: a shared flag checked at every loop boundary. On cancel the
//! writer commits its current batch and skips reconciliation/missing-marking
//! (an incomplete walk cannot judge absence) — the DB stays consistent.
//!
//! Backpressure: bounded channels between every stage. Memory never grows
//! with the number of unprocessed files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
use serde::Serialize;

use crate::error::LibraryError;
use crate::library::artwork::{self, ArtworkSource};
use crate::library::metadata::{self, ExtractedMetadata};
use crate::library::probe;
use crate::library::store::{self, FileSnapshot, LibraryStore, TrackRecord};
use crate::library::{
    classify, is_supported_audio_extension, ObservedFile, ScanClassification, StoredFile,
};

const WORK_QUEUE_DEPTH: usize = 256;
const RESULT_QUEUE_DEPTH: usize = 256;
const WRITE_BATCH_SIZE: usize = 500;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const FAILURE_CAP: usize = 512;

/// A per-file failure. Capped in summaries; the count is always exact.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanFailure {
    pub path: String,
    /// 'io' | 'metadata' | 'probe' | 'artwork' | 'database'
    pub kind: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummary {
    pub files_seen: u64,
    pub inserted_or_updated: u64,
    pub skipped_unchanged: u64,
    pub failed: u64,
    pub relinked_moved: u64,
    pub marked_missing: u64,
    pub unmarked_missing: u64,
    pub elapsed_ms: u64,
    pub canceled: bool,
    pub failures: Vec<ScanFailure>,
}

impl ScanSummary {
    fn record_failure(&mut self, failure: ScanFailure) {
        self.failed += 1;
        if self.failures.len() < FAILURE_CAP {
            self.failures.push(failure);
        }
    }
}

/// Events emitted on the scan channel. `Progress` is throttled.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "scan", rename_all = "kebab-case")]
pub enum ScanEvent {
    Started {
        roots: usize,
    },
    Progress {
        discovered: u64,
        processed: u64,
        skipped: u64,
        failed: u64,
    },
    Completed {
        summary: ScanSummary,
    },
    Canceled {
        summary: ScanSummary,
    },
    /// Fatal: the scan itself could not run (e.g. database unreachable).
    Failed {
        message: String,
    },
}

/// Handle to a running scan. Dropping it cancels and joins the scan.
pub struct ScanHandle {
    cancel: Arc<AtomicBool>,
    events: Receiver<ScanEvent>,
    thread: Option<JoinHandle<()>>,
}

impl ScanHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn events(&self) -> &Receiver<ScanEvent> {
        &self.events
    }

    pub fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .map(|t| t.is_finished())
            .unwrap_or(true)
    }

    /// Block until the scan thread exits (cancel first to stop early).
    pub fn wait(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ScanHandle {
    fn drop(&mut self) {
        self.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct Scanner;

impl Scanner {
    /// Start a scan of `roots` against the library at `db_path`.
    /// `roots` must be canonicalized, validated directories (the command
    /// layer guarantees this).
    pub fn start(db_path: PathBuf, artwork_dir: PathBuf, roots: Vec<PathBuf>) -> ScanHandle {
        let cancel = Arc::new(AtomicBool::new(false));
        let (evt_tx, evt_rx) = crossbeam_channel::bounded::<ScanEvent>(64);

        let thread_cancel = cancel.clone();
        let thread = thread::Builder::new()
            .name("lumen-library-scan".into())
            .spawn(move || run(db_path, artwork_dir, roots, thread_cancel, evt_tx))
            .expect("failed to spawn lumen-library-scan thread");

        ScanHandle {
            cancel,
            events: evt_rx,
            thread: Some(thread),
        }
    }
}

struct WorkItem {
    path: PathBuf,
    size_bytes: u64,
    mtime_ms: i64,
}

struct Counters {
    discovered: AtomicU64,
    queued: AtomicU64,
    processed: AtomicU64,
    skipped: AtomicU64,
    failed: AtomicU64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn mtime_ms_of(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn is_under_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn emit(events: &Sender<ScanEvent>, event: ScanEvent) {
    // The shell forwards these to the UI; a stuck consumer must not stall
    // the scanner. Progress loss is acceptable; terminal events are sent
    // blocking because they must not be lost.
    match &event {
        ScanEvent::Completed { .. } | ScanEvent::Canceled { .. } | ScanEvent::Failed { .. } => {
            let _ = events.send(event);
        }
        _ => {
            if let Err(e) = events.try_send(event) {
                tracing::warn!("scan event dropped: {e}");
            }
        }
    }
}

/// Metadata-pool worker: turn one file into a persistable record, or a
/// categorized failure. Never panics on bad media.
fn process_file(item: WorkItem, artwork_dir: &Path) -> Result<TrackRecord, ScanFailure> {
    let path_str = item.path.display().to_string();

    let props = probe::inspect(&item.path).map_err(|e| ScanFailure {
        path: path_str.clone(),
        kind: "probe",
        message: e.to_string(),
    })?;

    let meta = metadata::extract(&item.path).map_err(|e| ScanFailure {
        path: path_str.clone(),
        kind: "metadata",
        message: e.to_string(),
    })?;

    let artwork_result = resolve_artwork(&item.path, &meta, artwork_dir);
    let artwork = match artwork_result {
        Ok(stored) => stored,
        Err(failure) => {
            // Artwork failure is not fatal for the track: log and continue
            // without artwork. (Recorded at debug level to keep the summary
            // focused on actual library-affecting failures.)
            tracing::debug!("{}: {}", failure.path, failure.message);
            None
        }
    };

    Ok(TrackRecord {
        path: item.path,
        size_bytes: item.size_bytes,
        mtime_ms: item.mtime_ms,
        meta,
        props,
        artwork,
    })
}

/// Embedded first, then sidecar (documented priority).
fn resolve_artwork(
    track_path: &Path,
    meta: &ExtractedMetadata,
    artwork_dir: &Path,
) -> Result<Option<artwork::StoredArtwork>, ScanFailure> {
    let path_str = track_path.display().to_string();

    if let Some(embedded) = &meta.embedded_artwork {
        match artwork::store(artwork_dir, &embedded.data, ArtworkSource::Embedded) {
            Ok(stored) => return Ok(Some(stored)),
            Err(LibraryError::Artwork { message, .. }) => {
                tracing::debug!("invalid embedded artwork in {path_str}: {message}");
            }
            Err(e) => {
                return Err(ScanFailure {
                    path: path_str,
                    kind: "artwork",
                    message: e.to_string(),
                })
            }
        }
    }

    if let Some(dir) = track_path.parent() {
        if let Some(sidecar) = artwork::find_sidecar(dir) {
            let data = std::fs::read(&sidecar).map_err(|e| ScanFailure {
                path: sidecar.display().to_string(),
                kind: "artwork",
                message: format!("read sidecar failed: {e}"),
            })?;
            return artwork::store(artwork_dir, &data, ArtworkSource::Sidecar)
                .map(Some)
                .map_err(|e| ScanFailure {
                    path: sidecar.display().to_string(),
                    kind: "artwork",
                    message: e.to_string(),
                });
        }
    }

    Ok(None)
}

fn run(
    db_path: PathBuf,
    artwork_dir: PathBuf,
    roots: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
    events: Sender<ScanEvent>,
) {
    let started = Instant::now();
    let mut summary = ScanSummary::default();

    let outcome = run_inner(
        db_path,
        artwork_dir,
        roots,
        &cancel,
        &events,
        &mut summary,
        started,
    );

    if let Err(message) = outcome {
        emit(&events, ScanEvent::Failed { message });
    }
}

fn run_inner(
    db_path: PathBuf,
    artwork_dir: PathBuf,
    roots: Vec<PathBuf>,
    cancel: &Arc<AtomicBool>,
    events: &Sender<ScanEvent>,
    summary: &mut ScanSummary,
    started: Instant,
) -> Result<(), String> {
    let mut store = LibraryStore::open(&db_path).map_err(|e| format!("open database: {e}"))?;
    let snapshot = Arc::new(
        store
            .snapshot()
            .map_err(|e| format!("load file snapshot: {e}"))?,
    );

    emit(events, ScanEvent::Started { roots: roots.len() });

    let counters = Arc::new(Counters {
        discovered: AtomicU64::new(0),
        queued: AtomicU64::new(0),
        processed: AtomicU64::new(0),
        skipped: AtomicU64::new(0),
        failed: AtomicU64::new(0),
    });

    let (work_tx, work_rx) = crossbeam_channel::bounded::<WorkItem>(WORK_QUEUE_DEPTH);
    let (result_tx, result_rx) =
        crossbeam_channel::bounded::<Result<TrackRecord, ScanFailure>>(RESULT_QUEUE_DEPTH);

    // --- Metadata pool ---
    let workers = std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(2)
        .max(1);
    let mut pool = Vec::with_capacity(workers);
    for i in 0..workers {
        let rx = work_rx.clone();
        let tx = result_tx.clone();
        let art_dir = artwork_dir.clone();
        let worker_cancel = cancel.clone();
        pool.push(
            thread::Builder::new()
                .name(format!("lumen-scan-metadata-{i}"))
                .spawn(move || {
                    while let Ok(item) = rx.recv() {
                        if worker_cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        // Bounded channel: if the writer is gone, stop.
                        if tx.send(process_file(item, &art_dir)).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|e| format!("spawn metadata worker: {e}"))?,
        );
    }
    // The writer's EOF on results requires every sender to drop.
    drop(result_tx);
    drop(work_rx);

    // --- Walk thread state shared with the writer ---
    let seen_paths: Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let new_files: Arc<std::sync::Mutex<Vec<(u64, i64, PathBuf)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let walk_handle = {
        let snapshot_ref = snapshot.clone();
        let counters = counters.clone();
        let walk_cancel = cancel.clone();
        let seen_paths = seen_paths.clone();
        let new_files = new_files.clone();
        let roots = roots.clone();

        thread::Builder::new()
            .name("lumen-scan-walk".into())
            .spawn(move || {
                walk_roots(
                    roots,
                    snapshot_ref,
                    counters,
                    walk_cancel,
                    work_tx,
                    seen_paths,
                    new_files,
                );
            })
            .map_err(|e| format!("spawn walk thread: {e}"))?
    };

    // --- Writer loop (this thread) ---
    let mut last_progress = Instant::now();
    let mut batch: Vec<TrackRecord> = Vec::with_capacity(WRITE_BATCH_SIZE);
    let mut writer_done = false;

    while !writer_done {
        let item = match result_rx.recv_timeout(PROGRESS_INTERVAL) {
            Ok(item) => Some(item),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                writer_done = true;
                None
            }
        };

        let had_item = item.is_some();
        if let Some(result) = item {
            match result {
                Ok(record) => batch.push(record),
                Err(failure) => {
                    counters.failed.fetch_add(1, Ordering::Relaxed);
                    summary.record_failure(failure);
                }
            }
        }

        let should_flush =
            batch.len() >= WRITE_BATCH_SIZE || (!batch.is_empty() && (!had_item || writer_done));
        if should_flush {
            let written = flush_batch(&mut store, &mut batch, summary)?;
            summary.inserted_or_updated += written;
            counters.processed.fetch_add(written, Ordering::Relaxed);
        }

        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            last_progress = Instant::now();
            emit(
                events,
                ScanEvent::Progress {
                    discovered: counters.discovered.load(Ordering::Relaxed),
                    processed: summary.inserted_or_updated + summary.failed,
                    skipped: counters.skipped.load(Ordering::Relaxed),
                    failed: summary.failed,
                },
            );
        }

        if cancel.load(Ordering::Relaxed) && batch.is_empty() && writer_done {
            break;
        }
    }

    // Wait for workers and walker to exit (they observe the cancel flag or
    // closed channels).
    for handle in pool {
        let _ = handle.join();
    }
    let _ = walk_handle.join();

    let canceled = cancel.load(Ordering::Relaxed);
    summary.canceled = canceled;

    if !canceled {
        finalize(
            &mut store, &snapshot, &roots, seen_paths, new_files, summary,
        )?;
    }

    summary.elapsed_ms = started.elapsed().as_millis() as u64;
    summary.skipped_unchanged = counters.skipped.load(Ordering::Relaxed);
    summary.files_seen = counters.discovered.load(Ordering::Relaxed);

    emit(
        events,
        if canceled {
            ScanEvent::Canceled {
                summary: summary.clone(),
            }
        } else {
            ScanEvent::Completed {
                summary: summary.clone(),
            }
        },
    );
    Ok(())
}

fn flush_batch(
    store: &mut LibraryStore,
    batch: &mut Vec<TrackRecord>,
    summary: &mut ScanSummary,
) -> Result<u64, String> {
    let records = std::mem::take(batch);
    let count = records.len() as u64;
    store
        .in_transaction(|tx| {
            for record in &records {
                store::upsert_track(tx, record)?;
            }
            Ok(())
        })
        .map_err(|e| {
            summary.record_failure(ScanFailure {
                path: String::new(),
                kind: "database",
                message: format!("batch commit failed: {e}"),
            });
            format!("database batch commit: {e}")
        })?;
    Ok(count)
}

#[allow(clippy::too_many_arguments)]
fn walk_roots(
    roots: Vec<PathBuf>,
    snapshot: Arc<HashMap<PathBuf, FileSnapshot>>,
    counters: Arc<Counters>,
    cancel: Arc<AtomicBool>,
    work_tx: Sender<WorkItem>,
    seen_paths: Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>,
    new_files: Arc<std::sync::Mutex<Vec<(u64, i64, PathBuf)>>>,
) {
    for root in &roots {
        if cancel.load(Ordering::Relaxed) {
            return;
        }

        for entry in walkdir::WalkDir::new(root).follow_links(false).into_iter() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    // Inaccessible entries are skipped, logged, and never
                    // abort the walk.
                    tracing::warn!("walk entry error: {e}");
                    continue;
                }
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if !is_supported_audio_extension(ext) {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("stat failed for {}: {e}", path.display());
                    continue;
                }
            };

            counters.discovered.fetch_add(1, Ordering::Relaxed);
            let path_buf = path.to_path_buf();
            seen_paths.lock().unwrap().insert(path_buf.clone());

            let observed = ObservedFile {
                size_bytes: metadata.len(),
                mtime_ms: mtime_ms_of(&metadata),
            };
            let stored = snapshot.get(&path_buf).map(|s| StoredFile {
                size_bytes: s.size_bytes,
                mtime_ms: s.mtime_ms,
            });

            match classify(Some(&observed), stored.as_ref()) {
                Some(ScanClassification::Unchanged) => {
                    counters.skipped.fetch_add(1, Ordering::Relaxed);
                }
                Some(ScanClassification::New) => {
                    new_files.lock().unwrap().push((
                        observed.size_bytes,
                        observed.mtime_ms,
                        path_buf.clone(),
                    ));
                    counters.queued.fetch_add(1, Ordering::Relaxed);
                    if work_tx
                        .send(WorkItem {
                            path: path_buf,
                            size_bytes: observed.size_bytes,
                            mtime_ms: observed.mtime_ms,
                        })
                        .is_err()
                    {
                        return; // Writer gone.
                    }
                }
                Some(ScanClassification::Modified) => {
                    counters.queued.fetch_add(1, Ordering::Relaxed);
                    if work_tx
                        .send(WorkItem {
                            path: path_buf,
                            size_bytes: observed.size_bytes,
                            mtime_ms: observed.mtime_ms,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}

/// After a *complete* walk: reconcile moves, then mark missing / unmark
/// resurrected files.
fn finalize(
    store: &mut LibraryStore,
    snapshot: &HashMap<PathBuf, FileSnapshot>,
    roots: &[PathBuf],
    seen_paths: Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>,
    new_files: Arc<std::sync::Mutex<Vec<(u64, i64, PathBuf)>>>,
    summary: &mut ScanSummary,
) -> Result<(), String> {
    let seen = seen_paths.lock().unwrap();
    let new = new_files.lock().unwrap();

    // Missing candidates: snapshot rows under the scanned roots that the
    // walk did not see.
    let missing: Vec<(PathBuf, FileSnapshot)> = snapshot
        .iter()
        .filter(|(path, _)| is_under_any_root(path, roots))
        .filter(|(path, _)| !seen.contains(*path))
        .map(|(path, snap)| (path.clone(), *snap))
        .collect();

    // Index new files by (size, mtime) for move reconciliation.
    let mut new_by_identity: HashMap<(u64, i64), Vec<&PathBuf>> = HashMap::new();
    for (size, mtime, path) in new.iter() {
        new_by_identity
            .entry((*size, *mtime))
            .or_default()
            .push(path);
    }

    let mut relinks: Vec<(i64, PathBuf)> = Vec::new();
    let mut still_missing: Vec<i64> = Vec::new();
    let mut resurrected: Vec<i64> = Vec::new();

    for (_, snap) in &missing {
        let identity = (snap.size_bytes, snap.mtime_ms);
        let candidates = new_by_identity.get(&identity);
        match candidates {
            Some(paths) if paths.len() == 1 => {
                // Exactly one new file with identical size+mtime: treat as
                // the same file, moved (documented heuristic).
                relinks.push((snap.id, (*paths[0]).clone()));
            }
            _ => still_missing.push(snap.id),
        }
    }

    // Files seen present that were previously marked missing came back.
    for (path, snap) in snapshot.iter() {
        if snap.missing_since.is_some() && seen.contains(path) {
            resurrected.push(snap.id);
        }
    }

    drop(seen);
    drop(new);

    let since = now_ms();
    store
        .in_transaction(|tx| {
            for (id, new_path) in &relinks {
                store::relink_path(tx, *id, new_path)?;
            }
            summary.relinked_moved = relinks.len() as u64;
            summary.marked_missing = store::mark_missing(tx, &still_missing, since)? as u64;
            summary.unmarked_missing = store::clear_missing(tx, &resurrected)? as u64;
            Ok(())
        })
        .map_err(|e| format!("finalize transaction: {e}"))?;

    Ok(())
}
