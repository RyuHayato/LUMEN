# LUMEN — Architecture

> "LUMEN — Hear every detail."
> A premium, native-first desktop music player for local music libraries and high-fidelity playback.

**Status:** Phase 0 (architecture + foundation)
**Primary platform:** Windows 10/11 (x64)
**Language:** Rust (stable, `x86_64-pc-windows-msvc`)

---

## 1. Product Scope

LUMEN is a Windows-first desktop application for playing music the user owns.

Core capabilities the architecture must support:

- Folder-based local library scanning with incremental rescans
- Metadata + album artwork extraction and caching
- Browsing by artists / albums / tracks / genres / folders
- Fast full-text search over large libraries (tens to hundreds of thousands of tracks)
- Playlists, favorites, listening history
- Queue management (order, repeat, shuffle, persistence)
- Gapless playback, accurate seeking, ReplayGain
- Multiple formats: FLAC, WAV, MP3, AAC/M4A, OGG/Vorbis (Opus and others only with a concrete product reason)
- WASAPI output with shared and exclusive modes, output-device selection, device-change handling
- Sample-rate / bit-depth visibility that reflects the actual playback path
- Media keys, system tray, mini-player, keyboard-first operation
- Robust handling of corrupt, missing, moved, and renamed files
- Local-first: no account, no cloud dependency for core playback

## 2. Non-Goals

- Streaming services, cloud sync, accounts, subscriptions
- Remote telemetry or analytics backends
- Feature-list-driven format/DSP expansion
- macOS/Linux builds *now* (abstraction boundaries exist; implementations wait)
- "Bit-perfect" marketing claims without a validated end-to-end path

## 3. Technical Stack

| Concern | Choice | Phase introduced |
| --- | --- | --- |
| Language | Rust, stable MSVC toolchain | 0 |
| UI shell | Tauri v2 (WebView2 on Windows) | 0 |
| Frontend | Static HTML/CSS/JS (no bundler in Phase 0; Node 24 available when a build step is justified) | 0 |
| Audio decode | symphonia (pure Rust) | 1 |
| Audio output | Custom WASAPI backend on the `windows` crate, behind an `OutputBackend` trait | 2 |
| Resampling | rubato (only where the output path requires it) | 2 |
| Metadata/artwork | lofty | 1 |
| Database | SQLite via rusqlite (`bundled`) | 0 (foundation), 1 (real use) |
| Concurrency | Dedicated threads + `crossbeam-channel`; no async runtime in core | 0 |
| Logging | tracing + tracing-subscriber | 0 |
| Errors | thiserror (libraries) / anyhow (application edge) | 0 |
| Config | serde + TOML under `%APPDATA%/LUMEN` | 0 |
| Packaging | Tauri bundler: NSIS installer + portable zip | later |

## 4. Technology Choices — Reasons

Full rationale lives in `docs/DECISIONS.md`. Summary:

- **Tauri v2** gives the highest design ceiling for a *premium* UI (CSS animations, mature accessibility, fast iteration) while the entire product core — audio, library, database — stays in Rust behind a strict command/event boundary. "Native-first" here means a real local desktop application with native OS integrations, not a specific widget toolkit.
- **Custom WASAPI backend** because cpal/rodio do not expose exclusive mode, format negotiation, or device-event semantics at the level the audio skill requires.
- **symphonia** for decoding: pure Rust, actively maintained, covers the required formats, no C toolchain coupling.
- **SQLite**: embedded, transactional, single-file, proven at this scale; FTS5 covers search without extra services.
- **Threads + channels over async**: the core is a real-time-adjacent system with explicit ownership; a handful of well-named threads with bounded channels is easier to reason about and test than an async graph.

## 5. High-Level Architecture

```
┌────────────────────────────────────────────────────┐
│ UI (WebView2 — presentation only)                  │
└──────────────▲───────────────────▲─────────────────┘
        events │                   │ commands (invoke)
┌──────────────┴─────────────────────────────────────┐
│ lumen-app (Tauri)                                  │
│  - command handlers                                │
│  - event fan-out                                   │
│  - app state handles                               │
└──────────────▲───────────────────▲─────────────────┘
               │                   │
┌──────────────┴─────────────────────────────────────┐
│ lumen-core (no Tauri dependencies)                 │
│                                                    │
│  config ── db ── library ── playback ── audio      │
│                              │                     │
│                    ┌─────────▼─────────┐           │
│                    │ AudioEngine thread │           │
│                    │  state machine     │           │
│                    │  queue             │           │
│                    └─────────▲─────────┘           │
│                    commands  │ events              │
│                    ┌─────────┴─────────┐           │
│                    │ Decoder (symphonia)│  Phase 1  │
│                    │ Resampler (rubato) │  Phase 2  │
│                    │ OutputBackend trait│           │
│                    │  └─ windows/wasapi │  Phase 2  │
└────────────────────┴────────────────────┴───────────┘
```

Rules:

- The UI never touches audio buffers, platform audio APIs, or the database directly.
- `lumen-core` never depends on Tauri or any UI framework.
- Platform-specific code lives behind traits in clearly named modules.

## 6. Module Boundaries

| Module | Responsibility | Must not |
| --- | --- | --- |
| `lumen_app` (src-tauri) | Window, commands, events, tray/shortcuts (later) | Contain domain logic |
| `core::config` | Load/save/validate user configuration | Know about UI |
| `core::db` | Schema, migrations, connection management, queries | Contain playback logic |
| `core::library` | Scan orchestration, metadata pipeline, file classification | Touch the audio engine |
| `core::playback` | Playback state machine, queue, repeat/shuffle | Touch OS audio APIs |
| `core::audio` | Engine thread, command/event protocol, format types | Touch UI or database |
| `core::audio::output` | `OutputBackend` trait + device descriptions | Assume WASAPI |
| `core::audio::output::windows` | WASAPI implementation | Leak into generic code |

## 7. Directory Structure

```
LUMEN/
├── Cargo.toml                  # workspace
├── docs/
│   ├── ARCHITECTURE.md
│   └── DECISIONS.md
├── crates/
│   └── core/                   # lumen-core — pure Rust, fully testable
│       └── src/
│           ├── lib.rs
│           ├── config.rs
│           ├── error.rs
│           ├── db/
│           │   ├── mod.rs
│           │   └── schema.sql
│           ├── playback/
│           │   ├── mod.rs
│           │   ├── state.rs
│           │   └── queue.rs
│           ├── audio/
│           │   ├── mod.rs
│           │   ├── engine.rs
│           │   └── output/
│           │       ├── mod.rs
│           │       └── windows/
│           │           ├── mod.rs
│           │           └── wasapi.rs   # Phase 2
│           └── library/
│               └── mod.rs              # scanner boundary, types (impl Phase 1)
├── src-tauri/                  # lumen-app — Tauri shell
│   ├── Cargo.toml
│   ├── build.rs
│   ├── tauri.conf.json
│   └── src/
│       ├── main.rs
│       ├── commands.rs
│       └── state.rs
└── ui/                         # static frontend (Phase 0 shell)
    ├── index.html
    ├── style.css
    └── main.js
```

## 8. Data Model

SQLite, single database file at `%APPDATA%/LUMEN/lumen.db`. `PRAGMA user_version` drives migrations. Foreign keys enforced (`PRAGMA foreign_keys = ON`). WAL journal mode for concurrent reader/writer behavior.

Identifiers: `INTEGER PRIMARY KEY` (rowid). Natural keys are not trusted — files move.

### Tables (v1)

- **artists** — `id`, `name`, `name_normalized` (indexed)
- **albums** — `id`, `title`, `title_normalized`, `album_artist_id → artists`, `year` (nullable); unique `(title_normalized, album_artist_id)`
- **tracks** — `id`, `path` (unique), `title`, `title_normalized`, `album_id → albums` (nullable), `artist_id → artists` (nullable), `track_no`, `disc_no`, `duration_ms`, `codec`, `container`, `sample_rate_hz`, `bit_depth`, `channels`, `file_size`, `file_mtime`, `rg_track_gain_db`, `rg_album_gain_db` (nullable), `favorited_at` (nullable), `added_at`, `updated_at`; indexes on `album_id`, `artist_id`, `title_normalized`
- **genres** + **track_genres** — many-to-many
- **playlists** — `id`, `name`, `created_at`, `updated_at`
- **playlist_entries** — `playlist_id → playlists`, `track_id → tracks`, `position`; unique `(playlist_id, position)`
- **history** — `id`, `track_id → tracks`, `played_at`, `listened_ms`
- **playback_state** — singleton row: serialized queue snapshot, current index, position, volume, repeat mode, shuffle flag (session restore)
- **settings** — `key TEXT PRIMARY KEY`, `value TEXT` (app-internal state; user-editable config stays in TOML)
- **artworks** (v2) — `hash` (sha256 PK), `path` (relative to cache dir), `mime`, `source` (`embedded`/`sidecar`), `size_bytes`
- **tracks additions (v2)** — `artwork_hash → artworks`, `missing_since` (soft-delete marker, NULL = present)

Deliberately omitted until needed: featured/guest artist junctions, artwork as table rows (artwork is cached as files; the DB stores only a cache key/path), play counts as separate columns (derivable from `history`).

Search: FTS5 virtual table over track/album/artist names, added in Phase 1 with the scanner.

## 9. Library Architecture (implemented in Phase 1)

Pipeline, all on background threads; the UI thread is never involved:

```
walk thread (walkdir, no link-following)
   │  bounded channel (256)
   ▼
metadata pool (N = min(4, cores)): lofty tags + symphonia probe + artwork
   │  bounded channel (256)
   ▼
scanner thread = single DB writer: batched transactions (500 records)
   │
   ▼
finalize: move reconciliation → missing-marking → summary event
```

- **Classification** (per file, O(1) against a scan-start snapshot): new / unchanged (size+mtime match → skipped, no re-extraction) / modified.
- **Metadata**: lofty for tags; symphonia as the authority for stream properties (codec, rate, channels, bit depth, duration) and as a decodability gate — unprobenly files are categorized failures, never inserted. Unsupported codecs (outside the ADR-003 commitment) are rejected at probe time.
- **Normalization contract**: trimmed/collapsed/lowercased `*_normalized` columns; title falls back to file stem; missing artist/album stay NULL (never invented); `3/12` → first component; genres split on `;`. (See `library/metadata.rs`.)
- **Identity** (ADR-011): path + (size, mtime); exact single-candidate move reconciliation; soft-missing, never delete.
- **Artwork** (ADR-012): embedded front cover → conventional sidecar; magic-byte validation; content-addressed cache; failures never fail the track.
- **Cancellation**: cooperative flag; current batch commits; reconciliation/missing-marking skipped; DB always consistent.
- **Progress**: throttled `ScanEvent`s (started/progress/completed/canceled/failed) — domain events, no Tauri coupling.
- **Search** (ADR-013): escaped LIKE over normalized columns; FTS5 migration has a defined trigger.

## 10. Audio Architecture (implemented in Phase 2A + 2B)

Layering (per the audio-engineering skill):

```
UI → commands → engine thread (single authoritative playback state)
     → pipeline thread (ADR-014: decoder + FIFO + stream, one owner)
     → OutputBackend → WASAPI shared/exclusive → device
```

- **Engine thread (control plane):** state machine, queue, track-id → path resolution via injected resolver (the engine never touches the DB), pipeline command dispatch, position ticks (~4 Hz), end-of-track advancement, and the deterministic device-change reactions (ADR-016).
- **Pipeline thread (data plane):** owns `TrackDecoder` (symphonia → interleaved f32), a ~60 ms `VecDeque` FIFO, and the `OutputStream`. No locks in the data plane. Seek = command applied between render cycles; FIFO cleared before any post-seek frame decodes — race-free by construction. `SwitchDevice`/`SetOutputMode` re-open the stream while preserving the track and position via a decoder seek.
- **Decoder:** streaming, one packet at a time; corrupt packets skipped; truncation = clean EOS; seek reports the actual landed position (container/codec granularity, not sample-exact).
- **Output backend:** event-driven WASAPI (`wait_ready` → `writable_frames` → `write`) in shared mode (2A) and exclusive mode (2B, ADR-017). Shared-mode conversion is delegated to AUTOCONVERTPCM and reported (ADR-015). Exclusive negotiation is honest (F32 → S32 → S16, no resampling), with a truthful shared fallback.
- **Diagnostics:** `StreamInfo` exposes device id/name, requested vs actual mode, source/decoded/output formats, mix format, conversion, and endpoint id — the honest playback path. Exclusive fallback shows `requested_mode=Exclusive` with `output_mode=Shared`.
- **Volume:** linear application gain in the pipeline at write time; system/DAC volume untouched.
- **Underruns:** FIFO starvation writes silence and increments `underrun_frames` (visible in snapshots). End-of-stream is a separate, non-underrun path.
- **Not yet (by design):** gapless, ReplayGain, DSP, lyrics/visualizers, system tray/media keys.

Gapless strategy (future): pre-open + pre-buffer the next track while the current one finishes; hand off at the stream level without tearing down the output stream where the format permits. Decoder delay/padding (MP3/AAC) must be honored — "gapless" is claimed only after measured verification.

## 11. WASAPI Strategy (implemented in Phase 2A + 2B)

- Implement directly on `windows::Win32::Media::Audio::*` (IAudioClient, IMMDeviceEnumerator, IMMNotificationClient).
- **Shared mode** is the default: negotiate against the device mix format; rate/channel conversion is delegated to Windows AUTOCONVERTPCM and reported. Never claim the output equals the source format in shared mode.
- **Exclusive mode** is a real `AUDCLNT_SHAREMODE_EXCLUSIVE` path (ADR-017): the source rate and channel count are kept verbatim; the device confirms an encoding via `IsFormatSupported` (F32 → S32 → S16). No resampling, no "bit-perfect" claim; exclusive open failures fall back to shared and are reported via `requested_mode` vs `output_mode`.
- Device lifecycle: `IMMNotificationClient` drives a bounded event stream to the playback owner (ADR-016). On disconnect/removal the engine stops the stream and reports an error (no silent cross-device jump); on a default-device change while following the default, the stream re-arms on the new default with the track position preserved. Stale endpoint IDs fail open with a clear error.
- The audio callback thread never allocates, locks contentiously, logs, or does I/O; it pulls from a lock-free/ring buffer fed by the decode pipeline.

## 12. UI Architecture

- WebView2 frontend; Phase 0 is a static shell (`ui/index.html`) with no build step.
- All state comes from the Rust core via **commands** (request/response) and **events** (push). The frontend holds no authoritative domain state.
- When UI complexity justifies it (library views, queue editor), a build step (Vite + TypeScript) is introduced; Node 24 is already available on the dev machine. This is deferred, not decided away.
- Keyboard-first: global shortcuts and in-app keymap are Phase 2+ features with a dedicated design pass.

## 13. State Management

| State | Owner | Persistence |
| --- | --- | --- |
| Playback state (playing/paused/position/queue) | Engine thread (authoritative) | `playback_state` table (session snapshot) |
| Library entities | SQLite | `lumen.db` |
| User settings | `core::config` | `config.toml` |
| UI state (window size, selected view) | `lumen-app` | `settings` table / TOML |

Single source of truth per concern. No duplicated playback state in UI components.

## 14. Concurrency

Threads (all named, all joined on shutdown):

| Thread | Role | Communicates via |
| --- | --- | --- |
| main/UI | Tauri event loop | invoke/events |
| engine | Playback state machine, decode orchestration | bounded channels |
| audio callback | WASAPI render (Phase 2) | ring buffer only |
| library walk | Filesystem enumeration | bounded channel |
| metadata pool (N) | Tag extraction | bounded channels |
| db writer | Single SQLite writer | command channel |

Rules:

- Cancellation: every long-running operation carries a `CancellationToken`-style flag checked at natural boundaries.
- Shutdown: commands stop being accepted → workers drain → engine finishes current buffer → threads join → config/state flushed.
- No `tokio` in core. Tauri's internal runtime is not used for domain work.
- Locks are for protecting small invariants, never held across I/O or in audio paths.

## 15. Error Handling

- `lumen-core` modules define typed errors with `thiserror` (`ConfigError`, `DbError`, `LibraryError`, `AudioError`, `PlaybackError`). No `unwrap`/`expect` outside tests and clearly infallible paths.
- `lumen-app` converts module errors into a serializable `AppError` { kind, message, context } for the frontend; technical detail goes to logs.
- Classification:
  - **Recoverable** (bad file, missing artwork): record, skip, continue.
  - **User-facing** (device unavailable, unsupported format): explicit event + UI state.
  - **Diagnostic** (negotiation details, underruns): structured logs.
  - **Fatal** (config unwritable, DB corrupt beyond open): fail fast with a clear message; never limp along silently.

## 16. Logging & Diagnostics

- `tracing` with structured fields; `tracing-subscriber` fmt layer.
- Destinations: rolling file under `%APPDATA%/LUMEN/logs` (later; Phase 0: stderr + file via `tracing-appender` when justified) and stderr in debug builds.
- Levels: `INFO` default; `DEBUG`/`TRACE` via `LUMEN_LOG` env filter (`EnvFilter`).
- Audio diagnostics (Phase 2+): negotiated format, shared/exclusive, buffer size, underrun counters, resampling status — real values only.
- No remote telemetry. Local diagnostics must be sufficient to debug field failures.

## 17. Security & Privacy

- Local-first: no account, no required network access. Phase 0 makes zero network calls.
- Media parsing uses memory-safe decoders (symphonia, lofty) — malformed files are an availability concern, not a memory-safety one, and are still handled as errors.
- Paths: canonicalized and confined to user-selected library folders; no shell-out for media operations (no command-injection surface).
- Config/DB live in `%APPDATA%/LUMEN`; nothing written into library folders.
- Dependencies: vetted, mature, minimal; `cargo deny`/`cargo audit` considered for CI later.

## 18. Testing Strategy

| Level | Scope | Environment |
| --- | --- | --- |
| Unit | State machine, queue determinism, config roundtrip, schema/migrations, error mapping | CI-safe, deterministic |
| Integration (core) | DB writer batches, scan classification against temp fixtures (Phase 1) | CI-safe |
| Integration (app) | Command/event contract between frontend and core | dev machine |
| Hardware-dependent | WASAPI shared/exclusive, device disconnect, DAC behavior | manual checklist only — never claimed as automated |
| UI | manual until a justified need for automation exists | dev machine |

Rule (from the audio skill): never pretend hardware-dependent behavior was tested when it was not.

## 19. Performance Strategy

- Measure first; no speculative optimization.
- Priorities in order: glitch-free playback → predictable latency → low CPU → reasonable memory → large-library responsiveness.
- Large libraries: virtualized lists in UI; indexed queries; batched DB writes; no N+1 paths in browsing.
- Audio: bounded buffers sized deliberately; zero-alloc callback; decode ahead of consumption.

## 20. Packaging & Distribution (documented, not implemented)

- Tauri bundler → **NSIS installer** (per-user) and a **portable zip**.
- Versioning: semver; version surfaced in the app and diagnostics.
- Code signing: required before public release (certificate procurement is an approval item for a later phase).
- Auto-update: Tauri updater with signed artifacts, evaluated in a later phase — not built now.
- WebView2: Evergreen runtime is present on supported Windows 11 and most Windows 10; the installer will use Tauri's bootstrapper fallback.

## 21. Future Cross-Platform Strategy

- All OS-specific audio lives behind `OutputBackend` (macOS: CoreAudio; Linux: PipeWire/ALSA) — interfaces now, implementations when required.
- File paths, config locations, and media-key handling go through small platform modules.
- No speculative code: only the seams exist today.

## 22. Explicit Architectural Constraints

1. `lumen-core` has zero UI-framework dependencies.
2. The UI never calls OS audio APIs or the database directly.
3. The audio callback never blocks, allocates, or locks contentiously.
4. Playback state has exactly one owner.
5. Shared-mode output is never described as bit-identical to the source.
6. No new dependency without a recorded reason (see DECISIONS.md).
7. No network access without a concrete product requirement.
8. Corrupt media must never crash playback or scans.
9. Every audio architectural decision needs a concrete reason.
10. Phase discipline: do not build later-phase features early.

## 23. Intentionally NOT Being Built (yet)

Library scanning implementation, metadata/artwork extraction, full playback pipeline, WASAPI implementation, exclusive mode, resampling, ReplayGain application, playlists/favorites/history features, search UI, tray, mini-player, media keys, animations, polished design system, packaging, updater, signing, any cloud capability.

## 23b. Development Environment

Requirements to build LUMEN on Windows:

- **Rust** (stable). Production target: `x86_64-pc-windows-msvc` — requires
  Visual Studio 2022 Build Tools with the "Desktop development with C++"
  workload (`Microsoft.VisualStudio.Workload.VCTools`).
- **Fallback/CI target:** `x86_64-pc-windows-gnu` plus a MinGW-w64 toolchain
  on PATH (`gcc.exe`, `dlltool.exe`; e.g. `scoop install mingw`). Needed
  because `rusqlite` (bundled SQLite) compiles C and `windows-sys` invokes
  `dlltool`.
- **Build artifacts stay inside the project.** `.cargo/config.toml` pins
  `build.target-dir = "target"`, so the checkout is self-contained and no
  external build directory is needed or used. `target/` is git-ignored.
- **The project path contains a space** (`...\claude setup\LUMEN`). This is
  fine on the default MSVC toolchain: `cargo build`, `clippy`, `test`, and
  the app launch all pass in place. Only the *GNU* fallback toolchain is
  affected, because GNU `windres` misquotes paths when compiling the Windows
  resource file. To use the GNU cross-check from a spaced path, point only
  that build at a space-free target dir (`CARGO_TARGET_DIR=<no-spaces>`);
  never point the MSVC build away from `target/`.
- Building inside OneDrive-synced folders is discouraged (sync churn, file
  locks, slow builds). This is the owner's accepted tradeoff for keeping the
  project self-contained.
- WebView2 Evergreen runtime is required to run the app (preinstalled on
  Windows 11; Tauri's installer bootstraps it otherwise).

## 24. Phase 0 Deliverables (this phase)

- This document and `docs/DECISIONS.md`
- Cargo workspace: `lumen-core` + `lumen-app`
- Core: config load/save, typed errors, DB schema + migrations foundation, playback state machine + queue (real, tested), audio engine thread + command/event protocol (real, tested), `OutputBackend` boundary with an honest unimplemented WASAPI module
- App: Tauri window, command/event wiring, static UI shell
- Verification: `fmt`, `clippy`, `test`, `check`, window launch
