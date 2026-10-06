# LUMEN — Architectural Decision Records

Format per decision: **Decision / Context / Alternatives / Why the chosen option won / Consequences.**

---

## ADR-001: UI Framework — Tauri v2

**Decision:** Use Tauri v2 with a WebView2-hosted frontend. Phase 0 ships a static HTML/CSS/JS frontend with no build step; a bundler (Vite + TypeScript) is introduced only when UI complexity justifies it.

**Context:** LUMEN must feel *premium*: smooth animation, strong typography, dense browsable views over very large libraries, excellent keyboard navigation, and accessibility. It must also keep all product logic (audio, library, database) in Rust with a hard boundary to the UI.

**Alternatives considered:**

| Option | Notes |
| --- | --- |
| egui/eframe | Pure Rust, fast, simple deploys. Reaching a premium look requires building most widgets (album grids, smooth virtualized views, theming) by hand; animation and text layout cost real effort. |
| Slint | Declarative, good performance, designed for desktop. Licensing is the blocker for a proprietary product: GPLv3, paid commercial license, or royalty-free with mandatory attribution badge. |
| iced | Pleasant Elm-style architecture, but smaller ecosystem and less proven for this class of polished, content-dense app. |
| gpui | Excellent engineering pedigree (Zed), but Windows support is too new to bet a product on today. |
| Tauri | WebView2 frontend + Rust core. Industry-proven for music apps; highest design ceiling; mature accessibility and keyboard handling; native integrations (tray, global shortcuts, single instance, packaging) built in. |

**Why Tauri won:** The design ceiling and iteration speed of web tech for a *premium* UI, while the Rust core owns everything correctness-critical. The command/event boundary enforces the UI/core separation the audio skill demands. WebView2 Evergreen is present on the target OS (Windows 11, and the vast majority of maintained Windows 10). "Native-first" is interpreted as a real local desktop app with native OS integrations and a native Rust core — not as a specific widget toolkit. This interpretation is recorded explicitly so it can be challenged.

**Consequences:** Two languages in the repo (Rust + HTML/CSS/JS, TypeScript later); an IPC boundary that must be kept narrow and versioned deliberately; WebView2 runtime dependency (mitigated by Tauri's installer bootstrapper); frontend build tooling arrives later with its own dependency discipline.

---

## ADR-002: Audio Output — Custom WASAPI Backend via `windows` crate

**Decision:** Implement audio output directly on WASAPI using the official `windows` crate, behind a platform-neutral `OutputBackend` trait. cpal/rodio are not used for output.

**Context:** The product requires shared *and* exclusive modes, explicit format negotiation, device-change notifications, buffer/latency control, and honest reporting of the actual output path.

**Alternatives considered:**

| Option | Notes |
| --- | --- |
| rodio | High-level, built on cpal; inherits cpal's limitations; wrong altitude for this product. |
| cpal | Good portability, but exposes a lowest-common-denominator stream API: no real exclusive-mode control, limited negotiation, device events abstracted away. Conflicts with hard product requirements. |
| wasapi crate | Focused and usable, but adds a wrapper dependency over APIs we must understand and control anyway; the `windows` crate is already required for device notifications and future shell integrations. |
| `windows` crate (chosen) | Official Microsoft bindings; complete WASAPI + MMDevice + notification surface; no middleman. |

**Why it won:** Only direct WASAPI access satisfies exclusive mode, negotiation, device lifecycle, and diagnostic-honesty requirements without fighting an abstraction. Platform code stays isolated behind `OutputBackend`, so the portability cost is contained.

**Consequences:** More implementation work in Phase 2 (owned, understood code — not a black box); Windows-only implementation initially; the trait seam keeps macOS/Linux possible without speculative code now.

---

## ADR-003: Decoding — symphonia

**Decision:** Use symphonia for audio decoding (FLAC, WAV, MP3, AAC/M4A, OGG/Vorbis). Opus and further formats wait for a concrete product reason.

**Context:** Decoding is correctness-critical; malformed files must never crash playback; pure Rust avoids C toolchain coupling for codecs.

**Alternatives:** FFmpeg bindings (maximal format coverage but a heavy C dependency, build complexity, and unsafe surface for a local-first app); per-codec crates (claxon, minimp3,lewton — more moving parts to integrate and keep consistent); symphonia (pure Rust, actively maintained, uniform API, gapless-relevant delay/padding metadata where containers provide it).

**Why it won:** Best balance of safety, coverage of the required formats, maintenance, and integration cost. Format expansion is deliberate, not opportunistic.

**Consequences:** Opus support is not automatic; if/when required it is evaluated as its own ADR. Decoder sits behind a `Decoder` trait so the engine never sees symphonia types.

---

## ADR-004: Database — SQLite via rusqlite (bundled)

**Decision:** SQLite through rusqlite with the bundled SQLite; WAL mode; foreign keys on; single writer thread; `user_version`-driven migrations.

**Context:** Must scale to hundreds of thousands of tracks with fast browsing and search, remain single-file and local-first, and never block the UI or audio paths.

**Alternatives:** sqlx (async-first — pulls an async runtime into the core for no benefit here); redb/sled (embedded KV stores — weak fit for relational browsing and FTS); filesystem-only storage (fails query/relationship requirements immediately); SQLite (proven, transactional, FTS5 built in).

**Why it won:** Relational fit, FTS5 for search, single-file deployment, operational simplicity, mature Rust bindings with zero system dependencies when bundled.

**Consequences:** Write concurrency is single-writer by design (the db-writer thread owns writes; batching absorbs scan load); schema migrations must be disciplined from v1.

---

## ADR-005: Concurrency — Threads + Channels, No Async Runtime in Core

**Decision:** `lumen-core` uses named OS threads and `crossbeam-channel` bounded channels. No tokio/async in core. Tauri's internal runtime exists only at the app shell.

**Context:** The system is real-time-adjacent with a few long-lived roles (engine, audio callback, scanner, metadata pool, db writer). Explicit threads make ownership, shutdown, and backpressure legible.

**Alternatives:** tokio (excellent for many-small-I/O workloads; adds runtime complexity, `Send`/`'static` plumbing, and obscures the audio thread model); rayon (data-parallelism — useful later inside the metadata pool, not as the backbone); std::sync::mpsc (usable; crossbeam-channel adds select + bounded semantics worth the single dependency).

**Why it won:** Clarity under correctness pressure; the audio callback's no-blocking rules are easier to uphold and review; testing is deterministic.

**Consequences:** Async libraries are excluded from core (reinforces rusqlite choice); any future network feature introduces async deliberately and locally, never in audio paths.

---

## ADR-006: Application Structure — Cargo Workspace, `lumen-core` + `lumen-app`

**Decision:** Two crates. `lumen-core`: domain, playback, audio, db, config — zero UI dependencies. `lumen-app` (src-tauri): window, command handlers, event fan-out.

**Context:** The skill and product principles demand a hard UI/core boundary and testability of the core without a windowing environment.

**Alternatives:** Single crate with modules (simpler start, but the UI/core boundary erodes in practice); many crates (over-engineering at this stage).

**Why it won:** The smallest structure that makes the most important boundary compile-enforced.

**Consequences:** Slightly more Cargo configuration; core tests run fast without compiling Tauri.

---

## ADR-007: Library Scanning — Classify-by-State Pipeline with Single DB Writer

**Decision:** (Design for Phase 1) Walk → classify (new/unchanged/modified/missing from `path+mtime+size`) → bounded metadata pool → single batched DB writer → throttled progress events.

**Context:** Very large libraries; scans must never freeze the UI, must survive interruption, and must handle corrupt files gracefully.

**Alternatives:** Inotify-style watch-only designs (Windows directory watching is noisy and unreliable at scale — watching supplements scanning later, not replaces it); full-rescan-only (wasteful); embedded message queues (unjustified).

**Why it won:** State-derived classification makes scans idempotent and interruption-safe by construction; the single writer matches SQLite's model.

**Consequences:** Moved-file reconciliation needs an explicit heuristic pass (documented; sized+metadata based); watch-based live updates are a separate later ADR.

---

## ADR-008: Configuration & State Locations

**Decision:** User-editable config: TOML at `%APPDATA%/LUMEN/config.toml`. App-internal state and library DB: `%APPDATA%/LUMEN/lumen.db`. Artwork cache and logs under the same root.

**Context:** Clear separation between what users may edit by hand and what the app owns.

**Alternatives:** Everything in the DB (opaque to users); everything in files (loses transactional integrity); platform config dirs crate (chosen mechanism is explicit path construction via known env vars — one less dependency, full control; revisited if portability work begins).

**Why it won:** Transparency for users, integrity for the app, no speculative abstraction.

**Consequences:** Windows-first path logic; a small platform-path seam must be introduced when macOS/Linux work actually starts.

---

## ADR-009: Packaging — Tauri Bundler (NSIS + Portable Zip)

**Decision:** Installer: per-user NSIS via Tauri bundler. Also ship a portable zip. Code signing before any public release. Auto-update evaluated later; not built now.

**Context:** Real product distribution on Windows without premature infrastructure.

**Alternatives:** MSIX (store-oriented packaging constraints; revisit if Store distribution is wanted); MSI/WiX (heavier tooling than the benefit at this stage); Inno Setup (fine, but duplicative when Tauri already bundles NSIS).

**Why it won:** Integrated with the chosen UI stack; covers both install and portable expectations of a music player's audience.

**Consequences:** Signing certificate procurement becomes an approval item before release; updater design is deferred with packaging hooks left available.

---

## ADR-010: Dependency Discipline

**Decision:** Every dependency requires a one-line justification recorded here or in the module that introduces it. Current foundation set and reasons:

| Crate | Reason |
| --- | --- |
| tauri, tauri-build | Application shell and packaging (ADR-001) |
| serde, toml | Configuration format (ADR-008) |
| rusqlite (bundled) | Database (ADR-004) |
| crossbeam-channel | Bounded channels + select for core concurrency (ADR-005) |
| thiserror | Typed library errors |
| anyhow | Error ergonomics at the application edge only |
| tracing, tracing-subscriber | Structured logging/diagnostics |
| lofty | Tag/metadata extraction — activated Phase 1 (ADR-003) |
| symphonia | Stream probing + future decode boundary — activated Phase 1 (ADR-003) |
| walkdir | Recursive traversal with per-entry error handling and no-link-following — activated Phase 1 (ADR-007) |
| sha2 | Content-addressed artwork deduplication — activated Phase 1 (ADR-012) |
| windows | WASAPI + shell integrations (ADR-002) — activated for implementation in Phase 2 |

Dev-only: `rusqlite` (integration-test assertions), `filetime` (mtime control in identity tests).

Deferred until their phase: rubato (resampling).

---

## ADR-011: File Identity — Path + (size, mtime), Move Reconciliation by Exact Identity Match

**Decision:** A library file is identified by its canonical path. Change detection uses `(size_bytes, mtime_ms)`. Moved/renamed files are reconciled by an *exact single-candidate* `(size, mtime)` match between missing rows and newly discovered files: the old row's path is repointed, preserving identity (favorites, history, playlists). Ambiguous matches fall back to safe behavior (old row marked missing, new file inserted fresh). Missing rows are soft-marked (`missing_since`), never deleted.

**Context:** Requirements: survive renames/moves without losing user data; stay fast at 100k+ files; no full-file hashing of gigabyte-scale libraries.

**Alternatives:** Content hashing everything (rejected: defeats incremental scanning; no concrete reason at this scale); inode/file-ID tracking (rejected: unreliable across filesystems on Windows, breaks on copy); no reconciliation (rejected: moving an album would orphan favorites/history).

**Consequences:** `(size, mtime)` collisions are theoretically possible (two files, same size, same mtime) — the single-candidate rule makes the failure mode safe (no relink, both treated independently). `missing_since` soft-marking means removable drives never destroy library state; a full-rescan "prune" feature is a deliberate future decision, not an accident of the scanner.

---

## ADR-012: Artwork — Local-Only, Content-Addressed, Deterministic

**Decision:** Artwork priority: (1) embedded front cover, (2) conventional sidecar file (`cover|folder|front|album|artwork` × `jpg|jpeg|png`, case-insensitive, first in priority order). Image validity = magic bytes only. Storage is content-addressed (SHA-256) under `%APPDATA%/LUMEN/artwork/<aa>/<hash>.<ext>`; the DB stores hash + relative path + source. No network artwork, ever. Artwork failures never fail the track.

**Alternatives:** Blobs in SQLite (rejected: bloats the DB file, complicates backups, duplicates across tracks); sidecar-only (rejected: misses embedded art, the dominant case for tagged libraries); arbitrary "first image in folder" (rejected: nondeterministic, picks up unrelated images).

**Consequences:** 500 tracks of one album store one image. Embedded art wins per-track; an album-level "prefer sidecar" refinement is possible later at query time without schema change.

---

## ADR-013: Search — Normalized LIKE Now, FTS5 Deferred with a Trigger

**Decision:** Phase 1 search uses parameterized, escaped `LIKE` over the `*_normalized` columns (normalized = trimmed, whitespace-collapsed, Unicode-lowercased at write time).

**Context:** ADR-004 anticipated FTS5 in Phase 1. FTS5 adds external-content sync complexity (triggers or manual maintenance) whose correctness cost is not justified before real query patterns exist.

**Consequences:** LIKE substring scans are acceptable into the tens of thousands of tracks (measured Phase 1 scale). **Trigger to migrate:** when the library browse/search UX work begins in Phase 2, or when profiling shows search latency > ~50 ms at realistic library sizes, FTS5 becomes a dedicated ADR with a migration.

---

## ADR-014: Audio Pipeline — Single Owner Thread (Decode + Render), Not a Lock-Free Split

**Decision:** Phase 2A's pipeline is ONE thread that owns the decoder, a plain `VecDeque<f32>` FIFO (~60 ms), and the WASAPI stream. The WASAPI buffer event drives the cadence: wake → top up FIFO from the decoder → write what the device accepts. Underruns are written as silence and counted. There are no locks and no cross-thread buffer handoffs in the data plane; control arrives on a channel, facts leave as atomics + messages.

**Context:** The audio skill forbids filesystem/metadata/DB/locking work "in the real-time audio callback." Two designs were weighed:

| Option | Trade |
| --- | --- |
| **A. Single pipeline thread (chosen)** | Decode-time file reads happen on the timing thread — a deviation from the strict letter of the rule, mitigated by the FIFO. Zero locks, zero cross-thread seek races (single owner clears the FIFO between render cycles). |
| B. Decode thread + SPSC lock-free ring + render thread | Strict "no FS in audio thread," but seek requires a flush handshake across two threads — the highest bug-risk area in any player — plus a new dependency and materially more concurrency surface. |

**Why A won:** The skill also says "do not introduce concurrency complexity unless the problem actually requires it" and "keep concurrency understandable." In event-driven shared mode there is no OS callback thread — we own the cadence, and the FIFO absorbs decode stalls. Seek correctness is trivial under single ownership, which is exactly where Option B pays its complexity tax.

**Consequences:** Decode-time I/O latency beyond the FIFO depth (~60 ms) becomes an underrun (audible silence, counted in `underrun_frames`). This is measured and exposed in the state snapshot — not hidden. **Trigger to revisit:** if underrun metrics on real libraries show the FIFO is insufficient (slow HDDs, NAS), Phase 2B splits the decoder onto its own thread with a lock-free ring and a documented seek-flush protocol.

---

## ADR-015: Format Conversion — Windows AUTOCONVERTPCM Now, LUMEN-Controlled Resampling Later

**Decision:** In shared mode the pipeline negotiates the *source* PCM format (f32, source rate/channels) and, when it differs from the device mix format, enables `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`. The Windows audio engine performs rate/channel conversion. When source matches the mix format, no conversion is inserted.

**Context:** The mix format is the only format shared mode guarantees. Alternatives: activate rubato now (adds a LUMEN-controlled SRC before the output has proven stable — premature); force mix format and let the driver reject (some drivers do); AUTOCONVERTPCM (supported since Windows 7, well-documented behavior).

**Consequences:** The conversion path is always reported in `StreamInfo.conversion` (e.g. "windows-audio-engine (AUTOCONVERTPCM): 44100Hz/2ch f32 → 48000Hz/2ch f32", or "none"). Shared-mode output is never described as bit-identical. rubato (LUMEN-controlled resampling) remains the Phase 2B+ decision, most relevant when exclusive mode lands.

**Consequences:** Adding a dependency without a recorded reason is a review defect.

---

## ADR-016: Device Notification Ownership — Watcher Thread → Channel → Playback Owner

**Decision:** Device-change handling uses WASAPI's own notification mechanism: `IMMNotificationClient` callbacks registered on a dedicated, long-lived watcher thread (`lumen-device-watcher`). The callback body does the minimum work — translate arguments to an owned `DeviceEvent` and a non-blocking `try_send` — then returns. The engine thread `select!`s on the event channel and is the exclusive owner of all playback decisions. The notification callback never opens a stream, never blocks, never allocates unboundedly, never logs expensively, and never talks to the UI. Correctness is verified against `IMMDevice` state at open time (the event stream is a hint, not ground truth).

**Context:** §11 and the skill require device enumeration, default-device changes, disconnection/reconnection, and device failures to be handled deterministically. Polling is explicitly rejected by the requirement. A naive "open a stream / switch device inside the COM callback" design would violate the audio rules (the callback is not the playback owner and must not perform COM/playback work).

**Alternatives considered:**

| Option | Notes |
| --- | --- |
| Engine directly owns the COM registration | The engine is busy with the WASAPI buffer cadence; a single thread cannot both drain a ~30 ms period and absorb a blocking COM shutdown. |
| Poll `IMMDeviceEnumerator` from the engine | Rejected: requirement says devices must not be polled; latency and timer load for little benefit. |
| The watcher thread decides recovery itself | Rejected: the engine is the sole playback-state owner; a second thread mutating playback would reintroduce competing state. |

**Consequences:** COM lifecycle (CoInitializeEx/CoUninitialize/Register/Unregister) is contained in the watcher thread; the core never touches MMDevice outside it. Deterministic reactions are specified: (a) the bound endpoint becoming `Removed`/disabled/unplugged while playing or paused → the pipeline is stopped, an explicit "output device became unavailable" error is surfaced, and recovery is via a fresh Play (we never silently switch to a device the user did not select); (b) the default endpoint changing *while following the default* (device_id == None) and playing or paused → a `SwitchDevice` re-arms the stream on the current default, preserving track position; (c) a default change while an explicit device is pinned, or while idle/stopped, updates no stream (the next open resolves the current default). `DeviceEvent`/`DeviceState` translation is unit-tested.

---

## ADR-017: Exclusive-Mode Negotiation, Policy, and Shared Fallback

**Decision:** Exclusive mode is a real `AUDCLNT_SHAREMODE_EXCLUSIVE` path, not a label. Negotiation is honest and minimal: the device is offered the source rate and channel count verbatim, and only the sample encoding adapts (probe order F32 → S32 → S16, via `IAudioClient::IsFormatSupported`; only an exact `S_OK` counts — `S_FALSE` is "unsupported here"). LUMEN never resamples or remaps channels to satisfy exclusivity, and explicitly fails negotiation when no usable exclusive format exists. For `channels > 2` only layouts with a defined standard speaker mask (1/2/4/6/8) are attempted. The conversion note states exactly what happened (e.g. "f32 44100/2ch → S16 44100/2ch, sample-format conversion, no resampling") — and explicitly never claims resampling was avoided "and therefore bit-perfect" beyond the facts. If exclusive negotiation fails at stream-open time, the pipeline falls back to shared mode, and the result is reported truthfully via `StreamInfo { requested_mode: Exclusive, output_mode: Shared, conversion }`. Buffer sizing uses the device default period (>= 30 ms floor).

**Alternatives considered:**

| Option | Notes |
| --- | --- |
| Resample on the LUMEN side to match any exclusive format | Rejected: silently degrading source quality in the name of "exclusive" is the opposite of the honesty rule; resampling remains an explicit later decision (ADR-015). |
| Force-fail when exclusive formats are unavailable | Rejected as the default: frustration without benefit. The pipeline surfaces the fallback in diagnostics, and a future strict option is a small policy change. |
| Claim "bit-perfect" on a successful exclusive open | Rejected: bit-perfect requires the whole path (decoder word width, no dither, no volume) to be verified; LUMEN only truthfully reports "no resampling on this path". |

**Consequences:** Exclusive availability is honest on every machine (this dev hardware currently fails post-negotiation open on both active devices — reported, not hidden). Negotiation ordering/failure message are unit-testable in isolation; `requested_mode` vs `output_mode` is how the UI conveys the real outcome.

---

## ADR-018: Library Browse & Collection UX (Genres, Favorites, Playlists, History)

**Decision:** Genres, Favorites, Playlists, and History are surfaced by querying the existing v1 schema (`genres`/`track_genres`, `tracks.favorited_at`, `playlists`/`playlist_entries`, `history`) rather than introducing new tables. Favorites reuse the existing `tracks.favorited_at` nullable timestamp column: `NULL` = not favorited, set = favorited and when. A separate `favorites` join table was never added — it would have created a second source of truth for a fact the schema already models. Playlists' `(playlist_id, position)` uniqueness is respected by two-phase position rewriting (park negatives, then write the final contiguous sequence), and `remove` deletes only the first matching occurrence so a playlist may legitimately list the same track twice. History appends on each engine `stream-started` event (the first open marks `listened_ms = 0`; listen-durations can be backfilled per session later)) and is deduplicated on consecutive repeats of the same track. Album/artist queue actions reuse the existing `tracks_by_album` / `albums_by_artist` query layer.

**Consequences:** No schema migration was required (still v2); `TrackRow` now exposes `isFavorite` for the heart toggle. Track tables are renderable in Library, Favorites, and Playlist detail through one shared renderer, with per-row heart/playlist/reorder controls stopping propagation so row-click playback never fires when a control is used.

---

## Open Items Requiring Owner Approval

1. **UI stack interpretation** (ADR-001): Tauri means a WebView2 frontend. If "native-first" must mean native widgets, the alternative is a Rust-native toolkit with materially higher UI development cost. Recorded here as a conscious, reversible-early decision.
2. **Code-signing certificate** purchase before public release (later phase).
3. ~~Visual Studio Build Tools not installed~~ — **RESOLVED.** Build Tools 2022 (17.14.41, VCTools workload) installed with owner approval. `stable-x86_64-pc-windows-msvc` is now the default toolchain and the full gate (fmt, 34/34 tests, clippy `-D warnings`, build, window launch) passes on MSVC. The GNU + MinGW toolchain remains available as a secondary check.

## Addendum: Toolchain Notes from Phase 0 Verification

- Initial Phase 0 verification ran on `stable-x86_64-pc-windows-gnu` + MinGW-w64 while Build Tools were absent; all gates passed on both toolchains.
- The project checkout path contains a space (`...\claude setup\LUMEN`). GNU `windres` cannot compile resources in such paths, so the GNU cross-check needs a space-free `CARGO_TARGET_DIR` (documented in ARCHITECTURE.md §23b). The default MSVC toolchain has no such limitation and builds in place.
- **Build artifacts live in the project.** `.cargo/config.toml` sets `build.target-dir = "target"`, so LUMEN builds and tests with no external build directory (ARCHITECTURE.md §23b). Owner's decision, replacing the earlier external `C:\dev` target dirs, which are removed. Building inside OneDrive remains a known tradeoff (sync churn, slower builds), accepted to keep the checkout self-contained.
