# 🕯️ LUMEN

A **dark, gothic hi-fi music player** for Windows, built with 🦀 **Rust + Tauri v2**.
Local files in, beautiful playback out. No terminal, no cloud dependency.

---

## ✨ Highlights

- 🎧 **Gapless playback** — no silence between tracks; the next decoder is opened while the current stream is still rolling.
- 🎚️ **Bit-perfect-ish local playback** — WASAPI (shared mode), hardware through the real device.
- 📜 **Lossless library** — scans local folders, probes format/codec/depth, indexes albums/artists/genres/playlists.
- 🎨 **Artwork-driven theming** — a sleeve's colour bleeds into the console, library, modals and the room.
- 🖼️ **Playlists with covers** — set your own picture per playlist; they're stored locally.
- ⭐ **Favourites & 24 h listening history** — recent plays with listened-time, and durable full history retained on disk.
- ⏱️ **Gapless, repeat, shuffle, seek** — full transport surface; the launch screen shows the live signal path when a stream is open.
- ⌨️ **Keyboard-first** — shortcuts sheet for everything, `Space` to play/pause (context-aware, never hijacks typing).
- 🪟 **No terminal at launch** — release build is a proper Windows GUI app: `Run LUMEN.cmd` and you're in.

---

## 🚀 Quick start

```
Run LUMEN.cmd
```

That builds the release binary if needed (first launch ≈ a few minutes) and starts the app with no console window.

From source, the usual dev loop:

```
cargo check --workspace
cargo test --workspace
cargo build --release --workspace
```

> 🛠️ Debug builds keep a console window because of `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`. The release build has **no console**, which is the fix you wanted.

---

## 📂 Where your data lives

| Thing | Location |
|---|---|
| Library & listening history DB | `%APPDATA%\LUMEN\lumen.db` |
| Config / library folders | `%APPDATA%\LUMEN\config.toml` |
| Processed artwork | `%APPDATA%\LUMEN\artwork\` |

> ⚠️ The actual DB is **outside the repo**, so it never gets committed. `_target/`, `.devkit/`, scratch harnesses, and the isolated test `APPDATA` are all git-ignored.

---

## 🧩 Stack

| Layer | Tech |
|---|---|
| UI | React-free frontend: `index.html` + `main.js` + `style.css` served by Tauri |
| Desktop shell | Tauri v2 (`src-tauri/`) |
| Audio engine | Rust core (`crates/core/src/audio/`) — WASAPI output, Symphonia decode, gapless pipeline |
| Library | Rust core (`crates/core/src/library/`) — scanner, metadata, SQLite |
| DB | SQLite via `rusqlite` (`crates/core/src/db/`) |

---

## ⌨️ Controls (few worth remembering)

| Key | Action |
|---|---|
| `Space` | Play / pause (skipped while typing or a button is focused) |
| `Ctrl+Shift+/` / `?` | Shortcuts overlay |
| `←` / `→` | Previous / next |
| More | Press `?` in-app for the full sheet |

---

## 🧪 Project health

All of these are expected to be green at every change:

```
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

---

## 🖥️ Runtime profile notes

- **Release build** is a GUI-subsystem Windows binary (~10 MB). No console, instant launch after build.
- Memory was hunted down in the dev loop: the largest offender was the room-colour wash painting the root on a 50 ms tick — that's gone. The single remaining big allocation is the WebView2 renderer, which is normal for Tauri.
- The app launches from a single exe; there is no terminal process attached.

---

## 📖 Docs

- `docs/ARCHITECTURE.md` — why the audio engine owns playback state, and why it matters.
- `docs/DECISIONS.md` — the little trade-offs that shape this project.

---

## 📜 License

No license file yet — treat this repo as personal/work-in-progress until a `LICENSE` is added.

🕯️ *Lumen is light. Dark gothic hi-fi only: no skulls, no crosses, no neon.*
