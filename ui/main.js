// LUMEN frontend. The Rust core owns every fact; this file only renders it and
// forwards user intent as typed commands. No authoritative state is kept here.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

/* ------------------------------------------------------------------ helpers */

function formatDuration(ms) {
  if (!ms && ms !== 0) return "—";
  const total = Math.round(ms / 1000);
  const m = Math.floor(total / 60);
  const s = String(total % 60).padStart(2, "0");
  return `${m}:${s}`;
}

/** A library's total runtime, read as prose rather than as a long clock. */
function formatSpan(ms) {
  if (!ms) return "0s";
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest ? `${hours}h ${rest}m` : `${hours}h`;
}

/** Sample rates are spoken in kHz, so show them that way. */
function formatRate(hz) {
  if (!hz) return "—";
  return `${(hz / 1000).toFixed(hz % 1000 ? 1 : 0)} kHz`;
}

function text(el, value) {
  if (el) el.textContent = value ?? "—";
}

function fillRange(input, ratio) {
  const pct = Math.max(0, Math.min(100, ratio * 100));
  input.style.setProperty("--fill", `${pct}%`);
}

function monogram(name) {
  const first = (name || "?").trim().charAt(0).toUpperCase();
  return first || "?";
}

async function loadArtwork(hash) {
  if (!hash) return null;
  try {
    return await invoke("get_artwork", { hash });
  } catch (error) {
    console.warn("artwork unavailable", error);
    return null;
  }
}

/* -------------------------------------------------------------------- state */

const view = {
  name: "library",
  tracks: [],
  albums: [],
  currentTrackId: null,
  focusAlbumId: null,
  stats: null,
  window: null,
  devices: [],
  playlistId: null,
  lastHistoryId: null,
  seeking: false,
  durationMs: null,
  positionMs: 0,
  playback: null,
  lightKey: null,
  playlistCount: 0,
  sheetTracks: [],
  currentRow: null,
  // Navigation history for Alt+Left / Alt+Right, and the mute memory.
  nav: { stack: ["library"], index: 0, locked: false },
  muted: false,
  lastVolume: 1,
  shortcutReturn: null,
};

/**
 * Decoded sleeve cache, most-recently-used.
 *
 * Artwork arrives as a base64 data URL. A real cover is several hundred
 * kilobytes, so as a string it is roughly a third more than the file - and the
 * browser keeps its own decoded copy for every `<img>` that ever pointed at it.
 * An unbounded Map therefore grows with the library and never gives anything
 * back, so it is capped. 64 sleeves is far more than any view shows at once and
 * keeps a large library from pinning tens of megabytes for the whole session.
 */
const ARTWORK_CACHE_MAX = 64;
const artworkCache = new Map();


/* The welcome flourish gets roughly two seconds, then fades; a click sends it
   early, and it is removed from the DOM once faded. */
(function dismissWelcome() {
  const welcome = $("welcome");
  if (!welcome) return;
  const leave = () => welcome.classList.add("is-leaving");
  setTimeout(leave, 2400);
  welcome.addEventListener("click", leave);
  welcome.addEventListener(
    "transitionend",
    () => {
      if (welcome.classList.contains("is-leaving")) welcome.remove();
    },
    { once: true }
  );
})();

async function artworkFor(hash) {
  if (!hash) return null;
  if (artworkCache.has(hash)) {
    // Re-insert so the Map's iteration order doubles as least-recently-used.
    const url = artworkCache.get(hash);
    artworkCache.delete(hash);
    artworkCache.set(hash, url);
    return url;
  }
  const url = await loadArtwork(hash);
  artworkCache.set(hash, url);
  // Evict the oldest entries once the cache is over its ceiling.
  while (artworkCache.size > ARTWORK_CACHE_MAX) {
    const oldest = artworkCache.keys().next().value;
    artworkCache.delete(oldest);
  }
  return url;
}

/* A playlist picture is kept beside the app, not in the library database: it is
   a picture of the playlist, not a fact about any file in it. Keyed by playlist
   id so two playlists never share one. */
const COVER_KEY = (id) => `lumen.playlist.cover.${id}`;

function readPlaylistCover(id) {
  if (id == null) return null;
  try {
    return localStorage.getItem(COVER_KEY(id));
  } catch {
    return null;
  }
}

function writePlaylistCover(id, dataUrl) {
  try {
    localStorage.setItem(COVER_KEY(id), dataUrl);
    return true;
  } catch {
    // Storage is finite; a large photo is the usual culprit.
    return false;
  }
}

function clearPlaylistCover(id) {
  try {
    localStorage.removeItem(COVER_KEY(id));
  } catch {
    /* nothing to do */
  }
}

/**
 * Downscale a picked image to a square data URL.
 *
 * A playlist card is small and there may be many of them, so the full-resolution
 * original is wasted weight — and local storage is finite. Redrawing through a
 * canvas keeps the picture crisp at card size without the megabytes.
 */
async function shrinkImageToDataUrl(file, maxEdge) {
  const source = await decodeImage(file);
  const width = source.width;
  const height = source.height;
  const scale = Math.min(1, maxEdge / Math.max(width, height));

  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, Math.round(width * scale));
  canvas.height = Math.max(1, Math.round(height * scale));

  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("no 2d context available");
  ctx.drawImage(source, 0, 0, canvas.width, canvas.height);
  if (typeof source.close === "function") source.close();

  // WebP keeps a card-sized picture tiny. If this engine cannot encode it the
  // canvas silently hands back PNG, which is still correct — just heavier.
  return canvas.toDataURL("image/webp", 0.86);
}

/**
 * Turn a picked file into something drawable.
 *
 * Deliberately no `URL.createObjectURL`: the app's Content Security Policy is
 * `img-src 'self' data:`, which does not admit `blob:`, so an object URL is
 * blocked before it can decode and the read fails for every format. Decoding
 * the `Blob` itself is a DOM call and no policy reaches it. The fallback stays
 * on `data:`, which the policy does allow.
 */
function decodeImage(file) {
  if (typeof createImageBitmap === "function") {
    return createImageBitmap(file).catch(() => decodeImageViaDataUrl(file));
  }
  return decodeImageViaDataUrl(file);
}

function decodeImageViaDataUrl(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.addEventListener("load", () => {
      const img = new Image();
      img.addEventListener("load", () => resolve(img));
      img.addEventListener("error", () => reject(new Error("not a readable image")));
      img.src = String(reader.result);
    });
    reader.addEventListener("error", () => reject(new Error("could not read the file")));
    reader.readAsDataURL(file);
  });
}

function applyArt(imgId, emptyId, url, fallbackLabel) {
  const img = $(imgId);
  const empty = $(emptyId);
  if (!img || !empty) return;
  if (url) {
    // Same reasoning as setBloom: re-assigning an unchanged src re-decodes it.
    if (img.getAttribute("src") !== url) img.src = url;
    if (img.hidden) img.hidden = false;
    if (!empty.hidden) empty.hidden = true;
  } else {
    if (img.getAttribute("src")) {
      img.removeAttribute("src");
      img.hidden = true;
    }
    if (empty.hidden) empty.hidden = false;
    const label = empty.querySelector(".art-monogram");
    if (label && fallbackLabel) label.textContent = monogram(fallbackLabel);
  }
}

/* --------------------------------------------------------------- room light

   LUMEN takes its accent colour from the artwork of whatever is playing. The
   hue is sampled here and handed to CSS, which crossfades it into the rail,
   the buttons, the console lip and the glow behind the sleeve.

   Only the hue and saturation come from the image. Lightness is pinned, so a
   bright pop sleeve and a black metal sleeve produce the same text contrast —
   the room changes colour without ever changing how hard it is to read.
*/

const GOLD_LIGHT = { h: 41, s: 70, l: 64 };
const LIGHT_LIGHTNESS = 64;

const lightCache = new Map();

function rgbToHsl(r, g, b) {
  r /= 255;
  g /= 255;
  b /= 255;
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const l = (max + min) / 2;
  const d = max - min;
  if (!d) return [0, 0, l];
  const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
  let h;
  if (max === r) h = (g - b) / d + (g < b ? 6 : 0);
  else if (max === g) h = (b - r) / d + 2;
  else h = (r - g) / d + 4;
  return [h * 60, s, l];
}

/**
 * Pick the colour a listener would say the sleeve "is".
 *
 * Pixels are binned by hue and weighted by saturation and by how near they
 * are to mid-lightness, because that is where a sleeve's identity lives —
 * shadows and blown highlights say nothing about it. Greys are skipped
 * entirely, so a monochrome sleeve returns null and the room stays gold.
 */
function dominantLight(data) {
  const BUCKETS = 24;
  const weight = new Float64Array(BUCKETS);
  const vectorX = new Float64Array(BUCKETS);
  const vectorY = new Float64Array(BUCKETS);
  const satSum = new Float64Array(BUCKETS);

  for (let i = 0; i < data.length; i += 4) {
    if (data[i + 3] < 128) continue;
    const [h, s, l] = rgbToHsl(data[i], data[i + 1], data[i + 2]);
    if (s < 0.14 || l < 0.08 || l > 0.94) continue;
    const bucket = Math.min(BUCKETS - 1, Math.floor(h / (360 / BUCKETS)));
    const w = s * (1 - Math.abs(l - 0.5) * 1.2);
    if (w <= 0) continue;
    const rad = (h * Math.PI) / 180;
    weight[bucket] += w;
    vectorX[bucket] += Math.cos(rad) * w;
    vectorY[bucket] += Math.sin(rad) * w;
    satSum[bucket] += s * w;
  }

  let best = -1;
  let bestWeight = 0;
  for (let b = 0; b < BUCKETS; b += 1) {
    if (weight[b] > bestWeight) {
      bestWeight = weight[b];
      best = b;
    }
  }
  if (best < 0) return null;

  // Average the winning bin as a circular mean, so hues either side of 0°
  // do not average into their opposite.
  let hue = (Math.atan2(vectorY[best], vectorX[best]) * 180) / Math.PI;
  if (hue < 0) hue += 360;
  const saturation = satSum[best] / weight[best];

  return {
    h: Math.round(hue),
    s: Math.round(Math.min(0.78, Math.max(0.4, saturation * 1.15)) * 100),
    l: LIGHT_LIGHTNESS,
  };
}

/** Sample a data-URL sleeve. Never throws: a failure just keeps the gold. */
function extractLight(url) {
  if (!url) return Promise.resolve(null);
  if (lightCache.has(url)) return Promise.resolve(lightCache.get(url));

  return new Promise((resolve) => {
    const image = new Image();
    image.onload = () => {
      let light = null;
      try {
        const size = 32;
        const canvas = document.createElement("canvas");
        canvas.width = size;
        canvas.height = size;
        const ctx = canvas.getContext("2d", { willReadFrequently: true });
        ctx.drawImage(image, 0, 0, size, size);
        light = dominantLight(ctx.getImageData(0, 0, size, size).data);
      } catch (error) {
        console.warn("could not sample artwork colour", error);
      }
      lightCache.set(url, light);
      resolve(light);
    };
    image.onerror = () => {
      lightCache.set(url, null);
      resolve(null);
    };
    image.src = url;
  });
}

/**
 * Push a room colour to the document.
 *
 * `--live-h/s/l` are registered with `inherits: true`, so writing them on the
 * root invalidates the computed style of *every* element that mentions them -
 * which, in this interface, is most of it. That was fine when it happened once
 * per track, but the room cycle ran it twenty times a second, so every tick was
 * a whole-document style recalculation plus a repaint of the blurred banner.
 * Measured, that alone drove the renderer from ~850 MB to over 2 GB in half a
 * minute.
 *
 * So the cycle no longer touches these at all. It writes one `opacity` and one
 * `background-color` onto a single overlay element, which confines the repaint
 * to that one layer. Same look, two orders of magnitude less work.
 */
function applyRoomLight(light) {
  const { h, s, l } = light ?? GOLD_LIGHT;
  const root = document.documentElement.style;
  root.setProperty("--live-h", String(h));
  root.setProperty("--live-s", `${s}%`);
  root.setProperty("--live-l", `${l}%`);
}

/**
 * Paint the cycling colour into the console's own background.
 *
 * Deliberately not `applyRoomLight`: those custom properties inherit, so writing
 * them on the root invalidates every rule that mentions them, and the cycle used
 * to do exactly that twenty times a second - which alone drove the renderer from
 * ~850 MB past 2 GB in half a minute. Repainting one element's background is a
 * fraction of that.
 *
 * The lightness stays very low on purpose. The console is black with light
 * moving through it, not a coloured panel: an earlier version laid a flat
 * screen-blended wash over the whole bar, which turned it olive and buried the
 * lamp gradients underneath. Here the hue is only a tint in the gradient, so
 * the bar stays dark and the gradients keep reading.
 */
function applyCycleLight(light) {
  const console_ = $("player");
  if (!console_) return;
  const { h, s } = light;
  console_.style.background =
    `linear-gradient(180deg, hsl(${h} ${s}% 10% / 0.96), hsl(${h} ${s}% 5% / 0.98))`;
}

/**
 * The room's light source, in three modes.
 *
 *   album  — the sleeve's own colour (LUMEN's default trick)
 *   cycle  — red → purple → pink → red, forever, like an RGB wheel with the
 *            green channel left out
 *   candle — one fixed warm gold, the room before anything plays
 *
 * The whole interface is already painted from `--live-h/s/l`, so a mode is a
 * single number per frame — every lamp, beam, glow and rail follows for free.
 */
const ROOM_MODES = ["album", "cycle", "candle"];
const ROOM_CYCLE = {
  // Hue ramp: blood red → deep purple → pink → back to red. The stops are
  // unevenly spaced so the colour dwells on each and does not rush past it.
  stops: [
    { at: 0, h: 4, s: 76, l: 62 },
    { at: 0.42, h: 286, s: 60, l: 66 },
    { at: 0.72, h: 330, s: 72, l: 67 },
    { at: 1, h: 364, s: 76, l: 62 },
  ],
  periodMs: 26000,
  // 100ms over a 26s sweep is 260 steps: visually continuous, and a tenth of
  // the repaints the old 50ms tick caused. The cost of this loop scales
  // directly with how much of the document it invalidates.
  tickMs: 100,
};

function roomMode() {
  try {
    const stored = localStorage.getItem("lumen.room-colour");
    return ROOM_MODES.includes(stored) ? stored : "cycle";
  } catch {
    return "cycle";
  }
}

/** Position along the ramp, 0..1, interpolated with a soft ease per segment. */
function cycleLight(t) {
  const { stops } = ROOM_CYCLE;
  let a = stops[0];
  let b = stops[stops.length - 1];
  for (let i = 0; i < stops.length - 1; i += 1) {
    if (t >= stops[i].at && t <= stops[i + 1].at) {
      a = stops[i];
      b = stops[i + 1];
      break;
    }
  }
  const span = b.at - a.at || 1;
  const local = (t - a.at) / span;
  const eased = local * local * (3 - 2 * local); // smoothstep: no corner at a stop
  return {
    h: a.h + (b.h - a.h) * eased,
    s: a.s + (b.s - a.s) * eased,
    l: a.l + (b.l - a.l) * eased,
  };
}

let roomTimer = null;

function startRoomCycle() {
  stopRoomCycle();
  if (roomMode() !== "cycle") return;
  // Reduced motion: hold one colour instead of cycling. A slow hue sweep is
  // motion, and the request for reduced motion is not conditional.
  if (reducedMotion.matches) {
    applyCycleLight(cycleLight(0));
    return;
  }
  const origin = performance.now();
  roomTimer = setInterval(() => {
    // A hidden window paints nothing, so there is nothing to animate. This is
    // the single biggest saving: a minimised LUMEN costs zero cycles.
    if (document.hidden) return;
    const t = ((performance.now() - origin) % ROOM_CYCLE.periodMs) / ROOM_CYCLE.periodMs;
    // The wash carries its own CSS transition, which smooths these steps into
    // one slow sweep, so the room never visibly ticks.
    applyCycleLight(cycleLight(t));
  }, ROOM_CYCLE.tickMs);

  // Returning to a minimised window should not replay the elapsed time as a
  // jump; rebase so the sweep continues from where it would have been.
  document.addEventListener(
    "visibilitychange",
    () => {
      if (!document.hidden && roomMode() === "cycle" && !reducedMotion.matches) {
        startRoomCycle();
      }
    },
    { passive: true }
  );
}

function stopRoomCycle() {
  if (roomTimer != null) {
    clearInterval(roomTimer);
    roomTimer = null;
  }
}

/** Light the room from a sleeve, honouring the chosen mode. */
function lightRoomFrom(url) {
  const key = url ?? "";
  if (view.lightKey === key) return;
  view.lightKey = key;

  const mode = roomMode();
  if (mode === "cycle") return; // the ramp owns the room while it runs
  if (!url) {
    applyRoomLight(mode === "candle" ? GOLD_LIGHT : null);
    return;
  }
  if (mode === "candle") {
    applyRoomLight(GOLD_LIGHT);
    return;
  }
  extractLight(url).then((light) => {
    // A later track may have won the race while we were sampling.
    if (view.lightKey === key) applyRoomLight(light);
  });
}

/* ------------------------------------------------------------------- router */

const VIEW_META = {
  library: ["Library", "Recently played releases, and your playlists, one click from playback."],
  artists: ["Artists", "Everyone performing across your collection."],
  albums: ["Albums", "Pick a release to load it into the plate and filter the ledger."],
  
  playlists: ["Playlists", "Saved collections, in the order you set them."],
  favorites: ["Favorites", "Tracks you have marked."],
  history: ["History", "What you played, most recent first."],
  settings: ["Settings", "Output, library sources, and diagnostics."],
};

async function showView(name) {
  if (!VIEW_META[name]) return;
  view.name = name;

  for (const button of document.querySelectorAll(".nav-item")) {
    const active = button.dataset.view === name;
    button.classList.toggle("is-active", active);
    if (active) button.setAttribute("aria-current", "page");
    else button.removeAttribute("aria-current");
  }
  for (const section of document.querySelectorAll(".view")) {
    const active = section.id === `view-${name}`;
    section.hidden = !active;
    section.classList.toggle("is-active", active);
  }

  // Alt+Left/Right walk this stack. A jump from history is a move, not a new
  // place, so it does not push; anything else does, and truncates any forward
  // branch the way a browser's history does.
  if (!view.nav.locked) {
    const nav = view.nav;
    if (nav.stack[nav.index] !== name) {
      nav.stack.length = nav.index + 1;
      nav.stack.push(name);
      nav.index = nav.stack.length - 1;
    }
  }

  const [title, sub] = VIEW_META[name];
  text($("view-title"), title);
  text($("view-sub"), sub);
  // Search filters the Library ledger only. Every other view carries its own
  // scope, so a global field there would promise a query that does not exist.
  $("search-slot").hidden = name !== "library";

  if (name === "artists") await loadArtists();
  if (name === "albums") await loadAlbums();
  
  if (name === "playlists") await loadPlaylists();
  if (name === "favorites") await loadFavorites();
  if (name === "history") await loadHistory();
  if (name === "settings") {
    await Promise.all([loadDevices(), renderAbout()]);
  }
  if (name === "library") {
    closeAlbumSheet();
    await loadRecent();
  }
  $("content").scrollTop = 0;
}

/* ----------------------------------------------------------------- library */

/**
 * The ledger strip above the track table.
 *
 * The first three figures describe the *collection*. The runtime used to be the
 * collection's total length too, which is why it read "1h 7m" next to a wall
 * that had never played anything - it was the size of the shelf, not what you
 * listened to. Runtime now comes from the listening window instead, and says
 * which window it is, so the two scopes can never be confused.
 */
async function loadStats() {
  const stats = await invoke("get_library_stats");
  view.stats = stats;
  const strip = $("library-stats");
  strip.replaceChildren();

  const entries = [
    [stats.tracks, "tracks"],
    [stats.albums, "albums"],
    [stats.artists, "artists"],
  ];

  try {
    const listened = await invoke("history_summary");
    view.window = listened;
    entries.push([
      listened.listenedMs > 0 ? formatSpan(listened.listenedMs) : "none yet",
      `listened in ${listened.windowHours}h`,
    ]);
  } catch (error) {
    // The strip is decoration; a missing runtime must not hide the collection
    // counts beside it.
    console.warn("history summary unavailable", error);
  }

  // Only surfaced when it is non-zero: "0 missing" is noise on a healthy
  // library, but a real number the moment something is wrong.
  if (stats.missingTracks) entries.push([stats.missingTracks, "missing"]);

  for (const [value, label] of entries) {
    const span = document.createElement("span");
    const b = document.createElement("b");
    b.textContent = String(value);
    span.append(b, document.createTextNode(` ${label}`));
    strip.append(span);
  }
}

/**
 * The row for whatever the engine has open.
 *
 * The loaded list is checked first (free), then the database. The fetch
 * matters: a track started from an album sheet, a playlist, or a search result
 * is often not in the list the console is showing, and without it the console
 * can only ever say "Nothing playing" no matter what is audible.
 */
function currentTrackRow() {
  const id = view.currentTrackId;
  if (id == null) return null;
  return view.tracks.find((t) => t.id === id) ?? view.currentRow ?? null;
}

/** Keep `view.currentRow` in step with the engine's current track. */
async function syncCurrentRow(id) {
  if (id == null) {
    view.currentRow = null;
    return;
  }
  if (view.currentRow?.id === id) return;
  const loaded = view.tracks.find((t) => t.id === id);
  if (loaded) {
    view.currentRow = loaded;
    return;
  }
  try {
    view.currentRow = (await invoke("get_track", { trackId: id })) ?? null;
  } catch {
    view.currentRow = null;
  }
  if (view.currentTrackId === id) renderNowPlaying();
}

/**
 * Render one track table. Shared by Library, Favorites, and Playlist detail so
 * row markup, favorite toggles, and play-on-click behave identically.
 *
 * `columns` selects the layout: "library" adds Album + Source, "favorites"
 * drops Source, "playlist" drops Album and adds per-row order controls.
 */
function renderTrackTable(tbodyId, tracks, columns, onChanged, emptyMessage) {
  const tbody = $(tbodyId);
  tbody.replaceChildren();

  if (!tracks.length) {
    const row = document.createElement("tr");
    row.className = "empty-row";
    const cell = document.createElement("td");
    cell.colSpan = columns === "playlist" ? 5 : columns === "library" ? 7 : 6;
    cell.textContent = emptyMessage ?? "Nothing here yet.";
    row.append(cell);
    tbody.append(row);
    return;
  }

  tracks.forEach((track, index) => {
    const row = document.createElement("tr");
    row.dataset.id = String(track.id);
    if (track.id === view.currentTrackId) row.classList.add("is-current");

    // Clicking a row plays that track from this list. Controls in the last
    // cell stop propagation so they never start playback by accident.
    row.addEventListener("click", () =>
      invoke("play_tracks", {
        tracks: tracks.map((t) => t.id),
        startIndex: index,
      }).catch(showError)
    );

    // The title cell carries the sleeve when the track has one. A track with no
    // artwork is left exactly as it was: no placeholder, no empty box.
    const titleCell = document.createElement("td");
    titleCell.className = "title-cell";
    if (track.artworkHash) {
      titleCell.classList.add("has-art");
      artworkFor(track.artworkHash).then((url) => {
        if (!url) {
          titleCell.classList.remove("has-art");
          return;
        }
        const img = document.createElement("img");
        img.className = "row-art";
        img.src = url;
        img.alt = "";
        img.loading = "lazy";
        titleCell.prepend(img);
      });
    }
    const titleText = document.createElement("span");
    titleText.className = "title-text";
    titleText.textContent = track.title || "—";
    titleCell.append(titleText);

    row.append(gutterCell(index + 1), titleCell, textCell(track.artistName ?? "—"));
    if (columns !== "playlist") row.append(textCell(track.albumTitle ?? "—"));
    row.append(
      Object.assign(document.createElement("td"), {
        className: "c-num",
        textContent: formatDuration(track.durationMs),
      })
    );

    if (columns === "library") {
      row.append(
        Object.assign(document.createElement("td"), {
          className: "c-num",
          textContent: track.codec
            ? `${track.codec}${track.sampleRateHz ? ` ${track.sampleRateHz}Hz` : ""}`
            : "—",
        })
      );
    }

    const action = document.createElement("td");
    action.className = "c-act";
    if (columns === "playlist") {
      const playlistId = view.playlistId;
      action.append(
        iconButton("▲", `Move ${track.title} up`, () => onReorder(index, index - 1)),
        iconButton("▼", `Move ${track.title} down`, () => onReorder(index, index + 1)),
        iconButton("✕", `Remove ${track.title} from playlist`, () =>
          invoke("remove_from_playlist", { playlistId, trackId: track.id })
            .then(() => {
              // Removing drops the count by one, so the same sync runs here.
              return syncPlaylistViews(playlistId);
            })
            .catch(showError)
        )
      );
    } else {
      const heart = iconButton(track.isFavorite ? "♥" : "♡", track.isFavorite
        ? `Remove ${track.title} from favorites`
        : `Add ${track.title} to favorites`, () => toggleFavorite(track, onChanged));
      if (track.isFavorite) heart.classList.add("is-on");
      action.append(
        heart,
        iconButton("+", `Add ${track.title} to a playlist`, () => addTrackToPlaylist(track))
      );
    }
    row.append(action);
    tbody.append(row);
  });
}

function textCell(value) {
  const cell = document.createElement("td");
  cell.textContent = value;
  return cell;
}

const SVG_NS = "http://www.w3.org/2000/svg";

/**
 * The number gutter, which holds three states in one slot: the track number
 * at rest, a play mark under the pointer, and a level indicator on whichever
 * row the engine actually has open.
 */
function gutterCell(position) {
  const cell = document.createElement("td");
  cell.className = "c-idx";

  const gutter = document.createElement("span");
  gutter.className = "row-idx";

  const number = document.createElement("span");
  number.className = "row-n";
  number.textContent = String(position);

  const play = document.createElementNS(SVG_NS, "svg");
  play.setAttribute("class", "row-play");
  play.setAttribute("viewBox", "0 0 12 12");
  play.setAttribute("aria-hidden", "true");
  const triangle = document.createElementNS(SVG_NS, "path");
  triangle.setAttribute("d", "M3 2 10 6l-7 4z");
  play.append(triangle);

  const meter = document.createElement("span");
  meter.className = "row-eq";
  meter.setAttribute("aria-hidden", "true");
  meter.append(
    document.createElement("i"),
    document.createElement("i"),
    document.createElement("i")
  );

  gutter.append(number, play, meter);
  cell.append(gutter);
  return cell;
}

function iconButton(glyph, label, onClick) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "row-btn";
  button.textContent = glyph;
  button.title = label;
  button.setAttribute("aria-label", label);
  button.addEventListener("click", (event) => {
    event.stopPropagation();
    onClick();
  });
  return button;
}

/** Favorite/unfavorite a track, then refresh whichever views are on screen. */
async function toggleFavorite(track, onChanged) {
  try {
    const next = await invoke("set_favorite", {
      trackId: track.id,
      favorite: !track.isFavorite,
    });
    track.isFavorite = next;
    onChanged?.();
    if (view.name === "favorites") await loadFavorites();
    await loadStats();
    renderTracks(view.tracks);
  } catch (error) {
    showError(error);
  }
}

/**
 * Draw the Library's track ledger, then refresh the Recently Played wall —
 * which maps history entries back to albums, so it needs the fresh list.
 */
function renderTracks(tracks) {
  view.tracks = tracks;
  const rows = visibleTracks();
  const needle = $("search-input").value.trim();

  text($("section-tracks-title"), "All Tracks");
  // The count is only worth saying when the ledger is showing a subset.
  const scoped = Boolean(needle) || rows.length !== view.tracks.length;
  text($("track-count"), scoped && rows.length ? `${rows.length} shown` : "");

  renderTrackTable(
    "track-rows",
    rows,
    "library",
    null,
    needle
      ? `No tracks match “${needle}”.`
      : "Your library is empty. Add a folder in Settings, then scan it."
  );
  loadRecent().catch(() => {});
}

/* --------------------------------------------------------- recent wall */

/** Hide the track sheet and put the Recently Played wall back. */
function closeAlbumSheet() {
  const sheet = $("album-detail");
  if (sheet) sheet.hidden = true;
  for (const id of ["recent-head", "recent-grid"]) {
    const el = $(id);
    if (el) el.hidden = false;
  }
  const pl = $("recent-playlists-head");
  if (pl && view.playlistCount) pl.hidden = false;
  const pg = $("recent-playlist-grid");
  if (pg) pg.hidden = false;
}

/**
 * The Library's doorstep: the releases you played most recently as cards, and
 * your playlists as cards under them. A card opens straight into its tracks.
 */
async function loadRecent() {
  const grid = $("recent-grid");
  if (!grid) return;
  grid.replaceChildren();

  const entries = await invoke("list_history");
  const needle = $("search-input").value.trim().toLowerCase();

  // Dedupe by album: the most recent play of a release speaks for the album.
  const seen = new Map();
  for (const entry of entries) {
    const track = view.tracks.find((t) => t.id === entry.trackId);
    const key = track?.albumId ?? `track:${entry.trackId}`;
    if (seen.has(key)) continue;
    seen.set(key, { entry, track });
    if (seen.size >= 24) break;
  }

  const recents = [...seen.values()].filter(({ entry, track }) => {
    if (!needle) return true;
    const hay = [track?.albumTitle, track?.artistName, track?.title, entry.title, entry.artistName]
      .filter(Boolean)
      .join(" ")
      .toLowerCase();
    return hay.includes(needle);
  });

  text($("recent-count"), recents.length ? `${recents.length} release${recents.length === 1 ? "" : "s"}` : "");
  if (!recents.length) {
    renderEmptyState(
      grid,
      needle
        ? `Nothing recent matches “${needle}”.`
        : view.tracks.length
          ? "Nothing played in the last day yet. Start a track and its sleeve lands here."
          : "Your library is empty. Add a folder in Settings, then scan it.",
      libraryEmptyActions()
    );
  } else {
    grid.classList.remove("is-empty");
    delete grid.dataset.empty;
  }

  for (const { entry, track } of recents) {
    const card = document.createElement("button");
    card.type = "button";
    card.className = "card";

    const art = document.createElement("div");
    art.className = "wall-art";
    const url = await artworkFor(track?.artworkHash ?? entry.artworkHash);
    if (url) {
      const img = document.createElement("img");
      img.src = url;
      img.alt = `${track?.albumTitle ?? entry.title ?? "Album"} cover`;
      art.append(img);
    } else {
      const mono = document.createElement("span");
      mono.className = "card-monogram";
      mono.textContent = monogram(track?.albumTitle ?? entry.title);
      art.append(mono);
    }

    const name = document.createElement("span");
    name.className = "card-name";
    name.textContent = track?.albumTitle ?? entry.title ?? "Unknown album";
    const meta = document.createElement("span");
    meta.className = "card-meta";
    meta.textContent = [track?.artistName ?? entry.artistName, playedAgo(entry.playedAt)]
      .filter(Boolean)
      .join(" · ");

    card.append(art, name, meta);
    card.addEventListener("click", () => {
      if (track?.albumId != null) {
        const album = {
          id: track.albumId,
          title: track.albumTitle,
          artistName: track.artistName,
          year: track.year,
          artworkHash: track.artworkHash,
        };
        openAlbum(album);
      } else {
        openTrackSheet({
          title: entry.title ?? "Unknown track",
          meta: entry.artistName ?? "",
          tracks: [track].filter(Boolean),
        }).catch(showError);
      }
    });
    grid.append(card);
  }

  // Playlists under the albums: same card language, one click into the list.
  // With none to show, the whole section goes rather than leaving an orphaned
  // "Your Playlists" heading hovering over nothing.
  const plGrid = $("recent-playlist-grid");
  const plHead = $("recent-playlists-head");
  if (plGrid && plHead) {
    plGrid.replaceChildren();
    const playlists = await invoke("list_playlists");
    view.playlistCount = playlists.length;
    const shown = needle
      ? playlists.filter((p) => (p.name ?? "").toLowerCase().includes(needle))
      : playlists;
    plHead.hidden = shown.length === 0;
    plGrid.hidden = shown.length === 0;
    plGrid.classList.remove("is-empty");
    delete plGrid.dataset.empty;
    for (const playlist of shown) {
      const card = document.createElement("button");
      card.type = "button";
      card.className = "card is-playlist";

      const art = document.createElement("div");
      art.className = "wall-art";
      const mono = document.createElement("span");
      mono.className = "card-monogram";
      mono.textContent = monogram(playlist.name);
      art.append(mono);

      const name = document.createElement("span");
      name.className = "card-name";
      name.textContent = playlist.name;
      const meta = document.createElement("span");
      meta.className = "card-meta";
      meta.textContent = `${playlist.trackCount} track${playlist.trackCount === 1 ? "" : "s"}`;

      card.append(art, name, meta);
      card.addEventListener("click", async () => {
        await showView("playlists");
        openPlaylist(playlist);
      });
      plGrid.append(card);
    }
  }
}

/** Open a collection of tracks in the Library's sheet: an album or an artist. */
async function openTrackSheet({ title, meta, tracks }) {
  await showView("library");
  view.sheetTracks = tracks;
  text($("album-detail-title"), title);
  text($("album-detail-meta"), [meta, tracks.length ? `${tracks.length} track${tracks.length === 1 ? "" : "s"}` : ""].filter(Boolean).join(" · "));
  for (const id of ["recent-head", "recent-grid", "recent-playlists-head", "recent-playlist-grid"]) {
    const el = $(id);
    if (el) el.hidden = true;
  }
  const sheet = $("album-detail");
  if (sheet) sheet.hidden = false;
  renderTrackTable(
    "album-track-rows",
    tracks,
    "library",
    null,
    "No tracks found for this collection."
  );
}

/** Friendly relative timestamp for the wall: "2h ago", "Yesterday", ... */
function playedAgo(epochSeconds) {
  if (!epochSeconds) return "";
  const delta = Math.max(0, Math.floor(Date.now() / 1000 - epochSeconds));
  if (delta < 60) return "Just now";
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  const days = Math.floor(delta / 86400);
  return days === 1 ? "Yesterday" : `${days}d ago`;
}

/* ------------------------------------------------------------ empty states */

/**
 * Draw a wall's empty state in place of its cards.
 *
 * The CSS-only `::after` placeholder said "nothing here" and left the user
 * stranded. An empty section is a dead end unless it offers the one action that
 * fills it, so the state carries a button that goes exactly there.
 *
 * `actions` is a list of `{ label, run }`; the first is styled as primary.
 */
function renderEmptyState(grid, message, actions = []) {
  grid.replaceChildren();
  grid.classList.add("is-empty");
  delete grid.dataset.empty;

  const box = document.createElement("div");
  box.className = "empty-state";

  const line = document.createElement("p");
  line.className = "empty-line";
  line.textContent = message;
  box.append(line);

  if (actions.length) {
    const row = document.createElement("div");
    row.className = "empty-actions";
    actions.forEach((action, index) => {
      const button = document.createElement("button");
      button.type = "button";
      button.className = index === 0 ? "btn-primary" : "btn-quiet";
      button.textContent = action.label;
      button.addEventListener("click", () => {
        Promise.resolve(action.run()).catch(showError);
      });
      row.append(button);
    });
    box.append(row);
  }

  grid.append(box);
}

/** The actions that make sense for an empty Library wall, in priority order. */
function libraryEmptyActions() {
  const actions = [];
  if (view.tracks.length) {
    actions.push({
      label: "Browse all tracks",
      run: () => $("tracks-head")?.scrollIntoView({ behavior: "smooth", block: "start" }),
    });
  }
  if (!view.stats?.tracks) {
    actions.push({
      label: "Add a library folder",
      run: () => showView("settings"),
    });
  }
  return actions;
}

/** Move a playlist entry and re-render from the database's stored order. */
async function onReorder(from, to) {
  if (to < 0 || view.playlistId == null) return;
  try {
    await invoke("reorder_playlist", { playlistId: view.playlistId, from, to });
    await loadPlaylistTracks();
    await loadPlaylists();
  } catch (error) {
    showError(error);
  }
}

/** Add a track to a chosen playlist, appending to the end. */
async function addTrackToPlaylist(track) {
  const playlists = await invoke("list_playlists");
  if (!playlists.length) {
    appendEvent({
      type: "note",
      message: "create a playlist first, then add tracks to it",
    });
    await showView("playlists");
    return;
  }
  // Default to the first playlist; the picker in Settings can create more.
  const playlist = playlists[0];
  try {
    await invoke("add_to_playlist", { playlistId: playlist.id, trackId: track.id });
    showToast(`Added to ${playlist.name}`);
    // The tally moves the moment the row lands, not on the next navigation.
    await syncPlaylistViews(playlist.id);
  } catch (error) {
    const msg = String(error?.message ?? error);
    if (msg.includes("already in playlist") || msg.includes("duplicate")) {
      showToast(`Already in ${playlist.name}`, "info");
    } else {
      showError(error);
    }
  }
}

/** A small success toast that rises from the top-right, then vanishes after 3 seconds. */
function showToast(message, variant = "success") {
  let stack = document.getElementById("toast-stack");
  if (!stack) {
    stack = document.createElement("div");
    stack.id = "toast-stack";
    stack.setAttribute("aria-live", "polite");
    document.body.appendChild(stack);
  }
  const toast = document.createElement("div");
  toast.className = `toast toast--${variant}`;
  toast.textContent = message;
  stack.appendChild(toast);
  // A timeout rather than requestAnimationFrame: rAF is throttled to zero in a
  // backgrounded or occluded window, and a failure toast that never fades in is
  // worse than one that fades in without a transition.
  setTimeout(() => toast.classList.add("is-visible"), 16);
  // A failure usually needs reading, not just noticing, so it lingers.
  const life = variant === "error" ? 7000 : 3000;
  setTimeout(() => {
    toast.classList.remove("is-visible");
    setTimeout(() => toast.remove(), 350);
  }, life);
}

/**
 * A small confirmation dialog that renders card-by-card. Replaces the
 * native browser-window confirm, which leaks "tauri.localhost" into the
 * title bar and looks unlike the rest of the app.
 */
function confirmDelete(message) {
  return new Promise((resolve) => {
    let overlay = document.getElementById("confirm-overlay");
    if (overlay) overlay.remove();

    overlay = document.createElement("div");
    overlay.id = "confirm-overlay";
    overlay.className = "dialog-overlay";

    const card = document.createElement("div");
    card.className = "dialog-card";

    const body = document.createElement("p");
    body.className = "dialog-body";
    body.textContent = message;

    const actions = document.createElement("div");
    actions.className = "dialog-actions";

    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.className = "btn-quiet";
    cancel.textContent = "Cancel";

    const ok = document.createElement("button");
    ok.type = "button";
    ok.className = "btn-primary";
    ok.textContent = "Delete";

    actions.append(cancel, ok);
    card.append(body, actions);
    overlay.append(card);
    document.body.append(overlay);

    requestAnimationFrame(() => overlay.classList.add("is-open"));

    const done = (result) => {
      overlay.classList.remove("is-open");
      setTimeout(() => overlay.remove(), 200);
      resolve(result);
    };
    cancel.addEventListener("click", () => done(false));
    ok.addEventListener("click", () => done(true));
    overlay.addEventListener("click", (event) => {
      if (event.target === overlay) done(false);
    });
  });
}

async function loadTracks() {
  const needle = $("search-input").value.trim();
  const tracks = needle
    ? await invoke("search_tracks", { needle, limit: 100 })
    : await invoke("list_tracks", { limit: 100, offset: 0 });
  // A search is a deliberate widening of scope: drop the album focus.
  if (needle) view.focusAlbumId = null;
  renderTracks(tracks);
  renderNowPlaying();
  markCurrentRow();
}

/** Rows shown in the track table: the focused album, or everything loaded. */
function visibleTracks() {
  if (view.focusAlbumId == null) return view.tracks;
  const scoped = view.tracks.filter((t) => t.albumId === view.focusAlbumId);
  return scoped.length ? scoped : view.tracks;
}

/** Focus an album: the hero and the rail follow the playback context. */
function focusAlbumById(albumId) {
  view.focusAlbumId = albumId ?? null;
  renderNowPlaying();
}

function markCurrentRow() {
  for (const row of document.querySelectorAll("tbody tr")) {
    row.classList.toggle("is-current", Number(row.dataset.id) === view.currentTrackId);
  }
}

/* ------------------------------------------------------- artists and albums */

async function loadArtists() {
  const grid = $("artist-grid");
  grid.replaceChildren();
  const artists = await invoke("list_artists");
  grid.classList.toggle("is-empty", artists.length === 0);
  if (!artists.length) {
    renderEmptyState(grid, "No artists yet.", [
      { label: "Add a library folder", run: () => showView("settings") },
    ]);
    return;
  }
  delete grid.dataset.empty;

  for (const artist of artists.slice(0, 200)) {
    const card = document.createElement("button");
    // No artist artwork exists in the schema, so the card is set as a
    // monogram plate rather than showing an invented image.
    card.className = "card is-artist";
    card.type = "button";
    card.title = `${artist.trackCount} tracks`;

    const art = document.createElement("div");
    art.className = "wall-art";
    const mono = document.createElement("span");
    mono.className = "card-monogram";
    mono.textContent = monogram(artist.name);
    art.append(mono);

    const name = document.createElement("span");
    name.className = "card-name";
    name.textContent = artist.name ?? "Unknown artist";
    const meta = document.createElement("span");
    meta.className = "card-meta";
    meta.textContent = `${artist.trackCount} track${artist.trackCount === 1 ? "" : "s"}`;

    card.append(art, name, meta);
    card.addEventListener("click", () => {
      const tracks = view.tracks.filter((t) => t.artistName === artist.name);
      openTrackSheet({
        title: artist.name ?? "Unknown artist",
        meta: `${artist.trackCount} track${artist.trackCount === 1 ? "" : "s"}`,
        tracks,
      }).catch(showError);
    });
    grid.append(card);
  }
}

async function loadAlbums() {
  const grid = $("album-grid");
  grid.replaceChildren();
  const albums = await invoke("list_albums");
  view.albums = albums;
  grid.classList.toggle("is-empty", albums.length === 0);
  if (!albums.length) {
    renderEmptyState(grid, "No albums yet.", [
      { label: "Add a library folder", run: () => showView("settings") },
    ]);
    return;
  }
  delete grid.dataset.empty;

  for (const album of albums.slice(0, 200)) {
    const card = document.createElement("button");
    card.className = "card";
    card.type = "button";

    const art = document.createElement("div");
    art.className = "wall-art";
    const url = await artworkFor(album.artworkHash);
    if (url) {
      const img = document.createElement("img");
      img.src = url;
      img.alt = `${album.title ?? "Album"} cover`;
      art.append(img);
    } else {
      const mono = document.createElement("span");
      mono.className = "card-monogram";
      mono.textContent = monogram(album.title);
      art.append(mono);
    }

    const name = document.createElement("span");
    name.className = "card-name";
    name.textContent = album.title ?? "Unknown album";
    const meta = document.createElement("span");
    meta.className = "card-meta";
    meta.textContent = [album.artistName, album.year].filter(Boolean).join(" · ");

    card.append(art, name, meta);
    card.addEventListener("click", () => openAlbum(album));
    grid.append(card);
  }
}

/** Open a release: load it into the plate context and show its track sheet. */
async function openAlbum(album) {
  focusAlbumById(album.id);
  const tracks = view.tracks.filter((t) => t.albumId === album.id);
  await openTrackSheet({
    title: album.title ?? "Unknown album",
    meta: [album.artistName, album.year].filter(Boolean).join(" · "),
    tracks,
  });
}

/* ---------------------------------------------------------------- favorites */

async function loadFavorites() {
  const tracks = await invoke("list_favorites");
  text($("favorites-count"), tracks.length ? `${tracks.length} marked` : "");
  renderTrackTable(
    "favorites-rows",
    tracks,
    "favorites",
    null,
    "Nothing marked yet. Use the heart beside any track to keep it here."
  );
}

/* ---------------------------------------------------------------- playlists */

async function loadPlaylists() {
  const grid = $("playlist-grid");
  grid.replaceChildren();
  const playlists = await invoke("list_playlists");

  if (!playlists.length) {
    renderEmptyState(grid, "No playlists yet. Name one above to start it.", [
      { label: "Focus the name field", run: () => $("playlist-name-input")?.focus() },
      {
        label: "Add tracks from Library",
        run: () => showView("library"),
      },
    ]);
    return;
  }
  grid.classList.remove("is-empty");

  for (const playlist of playlists) {
    const card = document.createElement("button");
    card.type = "button";
    card.className = "card is-playlist";
    card.dataset.playlistId = String(playlist.id);

    // A chosen picture stands in for the initial. The cover is opaque content,
    // not atmosphere: full bleed, aspect preserved, no tint laid over it.
    const art = document.createElement("div");
    art.className = "wall-art";
    const cover = readPlaylistCover(playlist.id);
    if (cover) {
      const img = document.createElement("img");
      img.src = cover;
      img.alt = "";
      art.classList.add("has-cover");
      art.append(img);
    } else {
      const mono = document.createElement("span");
      mono.className = "card-monogram";
      mono.textContent = monogram(playlist.name);
      art.append(mono);
    }

    const name = document.createElement("span");
    name.className = "card-name";
    name.textContent = playlist.name;

    const meta = document.createElement("span");
    meta.className = "card-meta";
    meta.textContent = [
      `${playlist.trackCount} track${playlist.trackCount === 1 ? "" : "s"}`,
      formatDuration(playlist.durationMs),
    ]
      .filter((v) => v && v !== "—")
      .join(" · ");

    card.append(art, name, meta);
    card.addEventListener("click", () => openPlaylist(playlist));
    grid.append(card);
  }

  if (view.playlistId != null) await loadPlaylistTracks();
}

/**
 * Bring every playlist count on screen back in line with the database.
 *
 * Adding a track changes three numbers at once — the card's tally, the sheet's
 * heading, and the row list under it — and they live in three different places.
 * One call refreshes all of them together so nothing is left showing a stale
 * count while the rest already moved.
 */
async function syncPlaylistViews(playlistId) {
  if (view.name === "playlists") await loadPlaylists();

  if (view.playlistId == null || view.playlistId !== playlistId) return;

  const meta = $("playlist-detail-meta");
  if (!meta) return;
  const tracks = await invoke("list_playlist_tracks", { playlistId }).catch(() => null);
  if (!tracks) return;
  const duration = tracks.reduce((sum, t) => sum + (t.durationMs ?? 0), 0);
  text(meta, `${tracks.length} track${tracks.length === 1 ? "" : "s"} · ${formatDuration(duration)}`);
}

async function openPlaylist(playlist) {
  view.playlistId = playlist.id;
  text($("playlist-detail-title"), playlist.name);
  text(
    $("playlist-detail-meta"),
    `${playlist.trackCount} track${playlist.trackCount === 1 ? "" : "s"} · ${formatDuration(playlist.durationMs)}`
  );
  $("playlist-detail").hidden = false;
  applyPlaylistCover(playlist);
  $("playlist-close").focus();
  await loadPlaylistTracks();
}

/** Paint the open playlist's picture, or its initial when there isn't one. */
function applyPlaylistCover(playlist) {
  const img = $("playlist-cover-img");
  const empty = $("playlist-cover-empty");
  const src = readPlaylistCover(playlist?.id);
  if (src) {
    img.src = src;
    img.hidden = false;
    empty.hidden = true;
    empty.textContent = "";
  } else {
    img.removeAttribute("src");
    img.hidden = true;
    empty.hidden = false;
    empty.textContent = monogram(playlist?.name);
  }
  const pick = $("playlist-cover-pick");
  if (pick) {
    pick.textContent = src ? "Change picture" : "Set picture";
    pick.classList.toggle("is-on", Boolean(src));
  }
}

async function loadPlaylistTracks() {
  if (view.playlistId == null) return;
  const tracks = await invoke("list_playlist_tracks", { playlistId: view.playlistId });
  renderTrackTable(
    "playlist-rows",
    tracks,
    "playlist",
    loadPlaylists,
    "This playlist is empty. Add tracks with the + button in the library."
  );
}

/* ------------------------------------------------------------------ history */

async function loadHistory() {
  const list = $("history-list");
  list.replaceChildren();
  const entries = await invoke("list_history");

  text($("history-count"), entries.length ? `${entries.length} play${entries.length === 1 ? "" : "s"}` : "");
  $("clear-history").disabled = entries.length === 0;

  if (!entries.length) {
    const li = document.createElement("li");
    li.className = "tape-empty";
    li.textContent = "Nothing played in the last day yet. Start a track and it lands here.";
    list.append(li);
    return;
  }

  for (const [index, entry] of entries.entries()) {
    const li = document.createElement("li");
    li.className = "tape-item";

    const ordinal = document.createElement("span");
    ordinal.className = "tape-index";
    ordinal.textContent = String(index + 1).padStart(2, "0");

    const art = document.createElement("span");
    art.className = "tape-art";
    artworkFor(entry.artworkHash).then((url) => {
      if (url) {
        const img = document.createElement("img");
        img.src = url;
        img.alt = "";
        art.replaceChildren(img);
      } else {
        art.textContent = monogram(entry.title);
      }
    });
    if (!entry.artworkHash) art.textContent = monogram(entry.title);

    const body = document.createElement("span");
    body.className = "tape-body";
    const title = document.createElement("span");
    title.className = "tape-title";
    title.textContent = entry.title;
    const meta = document.createElement("span");
    meta.className = "tape-meta";
    meta.textContent = [
      entry.artistName,
      new Date(entry.playedAt * 1000).toLocaleString(undefined, {
        month: "short",
        day: "numeric",
        hour: "numeric",
        minute: "2-digit",
      }),
      entry.playCount > 1 ? `played ${entry.playCount} times` : null,
    ]
      .filter(Boolean)
      .join(" · ");
    body.append(title, meta);

    const play = document.createElement("button");
    play.type = "button";
    play.className = "row-btn";
    play.textContent = "▶";
    play.title = `Play ${entry.title}`;
    play.setAttribute("aria-label", `Play ${entry.title}`);
    play.addEventListener("click", () =>
      invoke("play_tracks", { tracks: [entry.trackId], startIndex: 0 }).catch(showError)
    );

    li.append(ordinal, art, body, play);
    list.append(li);
  }
}

/* ------------------------------------------------------------------ now playing */

/**
 * Record a play for the track the engine just opened.
 *
 * Called from the `stream-started` event, not from a click: a track can also
 * start because the queue advanced or the app restored state, and history
 * should reflect what actually played. `listened_ms` is 0 here — the duration
 * of the *previous* track is not yet known when the next one opens.
 */
async function noteHistory() {
  // `currentTrackId` is refreshed by the snapshot fetch that follows this
  // event, so ask for it here rather than trusting a stale value.
  let id = null;
  try {
    id = (await invoke("get_playback_state"))?.current ?? null;
  } catch {
    return; // history is never worth breaking playback over
  }

  // Credit the track that just finished before recording the new one. At the
  // moment a play is *recorded* nothing is known about how long it will be
  // heard, which is why listened time used to be permanently zero and the
  // runtime figure had to be faked from the collection's total length.
  const previous = view.lastHistoryId;
  if (previous != null && previous !== id) {
    const listened = view.positionMs ?? 0;
    // Below a few seconds is a skip, not a listen.
    if (listened > 5000) {
      invoke("add_listened_time", { trackId: previous, listenedMs: Math.round(listened) }).catch(
        () => {}
      );
    }
  }

  if (id == null || id === view.lastHistoryId) return;
  view.lastHistoryId = id;
  try {
    await invoke("record_play", { trackId: id, listenedMs: 0 });
    if (view.name === "history") await loadHistory();
    if (view.name === "library") await loadRecent();
    await loadStats();
  } catch (error) {
    console.warn("history write failed", error);
  }
}

/** Tracks that belong to the album currently in focus (hero context). */
function focusTracks() {
  if (view.focusAlbumId == null) return view.tracks;
  const scoped = view.tracks.filter((t) => t.albumId === view.focusAlbumId);
  return scoped.length ? scoped : view.tracks;
}

function focusAlbum() {
  if (view.focusAlbumId == null) return null;
  return view.albums.find((a) => a.id === view.focusAlbumId) ?? null;
}

const STATE_LABEL = {
  playing: "Now playing",
  paused: "Paused",
  loading: "Opening",
  seeking: "Seeking",
  buffering: "Buffering",
  stopped: "Ready",
  finished: "Finished",
  error: "Playback error",
};

const HERO_PLAY = "M4.5 3.2 12.5 8l-8 4.8z";
const HERO_PAUSE = "M4.8 3.4h2.4v9.2H4.8zM8.8 3.4h2.4v9.2H8.8z";
const PLAY_PATH = "M6.5 4.2 15 10l-8.5 5.8z";
const PAUSE_PATH = "M6.4 4.4h2.9v11.2H6.4zM10.7 4.4h2.9v11.2h-2.9z";
/* Three repeat states, told apart by the glyph itself: a struck-through loop
   (off), a loop with two arrowheads (whole playlist), and a loop with a single
   arrowhead and a "1" (this song). Legible at 16px. */
const REPEAT_PATH =
  "M12.5 3.4H5.2a2.4 2.4 0 0 0-2.4 2.4v3.6M3.5 12.6h7.3a2.4 2.4 0 0 0 2.4-2.4V6.6";
const REPEAT_HEAD_TOP = "M12.9 1.4l2.1 2-2.1 2";
const REPEAT_HEAD_BOTTOM = "M11.4 9.6l2.1 2-2.1 2";
const REPEAT_ONE_GLYPH = "M6.1 6.4h1.5v3.6M6.1 10h1.9";
const REPEAT_OFF_PATH = "M13.6 2.6 2.9 13.3";

/**
 * A plain-language verdict on the signal path, built only from what the
 * engine reports. "Bit-perfect" is claimed in exactly one case: exclusive
 * mode, with the device accepting the file's own rate, channels and encoding
 * and nothing converted on the way.
 */
function describePath(info) {
  if (!info) {
    return {
      label: "No stream open",
      note: "Start a track to see the live signal path.",
      kind: "",
    };
  }

  const untouched = (info.conversion ?? "").startsWith("none");
  const refused = info.requestedMode && info.requestedMode !== info.outputMode;
  const note = refused
    ? `Exclusive mode was refused by the device. ${info.conversion}`
    : info.conversion;

  if (info.outputMode === "exclusive") {
    return untouched
      ? { label: "Bit-perfect", note, kind: "is-pure" }
      : { label: "Exclusive, no resampling", note, kind: "is-clean" };
  }
  return untouched
    ? { label: "Shared, nothing converted", note, kind: "is-clean" }
    : { label: "Windows mixer in the path", note, kind: "" };
}

/** The halo behind the sleeve is the sleeve itself, scaled and blurred. */
/**
 * The halo behind the sleeve is the sleeve itself, scaled and blurred.
 *
 * Assigning `src` is not free even when the value is unchanged: the engine tears
 * down the previous decoded bitmap and decodes the new one. Doing that on every
 * 400 ms poll kept a full-size album image re-decoding two and a half times a
 * second, and the surfaces queued up faster than they were collected - which is
 * what drove the renderer past a gigabyte. So this is a no-op unless the
 * artwork genuinely changed.
 */
function setBloom(url) {
  const bloom = $("now-bloom");
  if (!bloom) return;
  if (url) {
    if (bloom.getAttribute("src") === url) return;
    bloom.src = url;
    bloom.hidden = false;
  } else {
    if (!bloom.getAttribute("src") && bloom.hidden) return;
    bloom.removeAttribute("src");
    bloom.hidden = true;
  }
}

function renderNowPlaying() {
  const snapshot = view.playback;
  const track = currentTrackRow();
  const info = snapshot?.stream ?? null;
  const hasTrack = Boolean(track);

  // The plate shows a focused album when there is one, otherwise whatever the
  // engine has open, otherwise an invitation to start something.
  const album = focusAlbum();
  const scope = focusTracks();

  if (album) {
    const count = album.trackCount ?? scope.length;
    const parts = [
      `${count} track${count === 1 ? "" : "s"}`,
      formatDuration(scope.reduce((sum, t) => sum + (t.durationMs ?? 0), 0)),
    ];
    if (album.year) parts.push(`released ${album.year}`);

    text($("now-eyebrow"), "Album");
    text($("now-title"), album.title ?? "Unknown album");
    text($("now-artist"), album.artistName ?? "Unknown artist");
    text($("now-meta"), parts.join(", "));
  } else if (hasTrack) {
    text($("now-eyebrow"), STATE_LABEL[snapshot?.state] ?? "Ready");
    text($("now-title"), track.title || "Untitled");
    text($("now-artist"), track.artistName ?? "");
    text($("now-meta"), track.albumTitle ?? "");
  } else {
    text($("now-eyebrow"), "Ready");
    text($("now-title"), "Nothing playing");
    text($("now-artist"), "");
    // Say something true and useful instead of a generic invitation: the counts
    // are already loaded, so the idle plate can describe the room it is in.
    text(
      $("now-meta"),
      view.stats?.tracks
        ? `${view.stats.tracks} track${view.stats.tracks === 1 ? "" : "s"} across ${
            view.stats.albums ?? 0
          } album${view.stats.albums === 1 ? "" : "s"} — pick one below, or press Space to shuffle.`
        : "Add a library folder in Settings, then scan it."
    );
  }

  const playing = snapshot?.state === "playing";
  document.body.classList.toggle("is-playing", playing);

  // Idle means: nothing open and nothing focused. That is the state in which
  // the plate used to spend a third of the viewport on a placeholder and an
  // invitation, so it is marked here and styled down in CSS. The body class is
  // the single source of truth, so it cannot drift from what is on screen.
  const idle = !hasTrack && !album;
  document.body.classList.toggle("is-idle", idle);

  text($("hero-play-label"), playing ? "Pause" : album ? "Play album" : "Play");
  $("hero-play-btn").disabled = !scope.length;
  $("hero-shuffle-btn").disabled = scope.length < 2;
  $("hero-play-path").setAttribute("d", playing ? HERO_PAUSE : HERO_PLAY);

  // The signal row only appears when there is a real stream to describe.
  const path = describePath(info);
  const readout = $("now-readout");
  readout.hidden = !info;
  if (info) {
    const verdict = $("rd-verdict");
    verdict.className = `verdict ${path.kind}`.trim();
    verdict.title = path.note;
    text($("rd-verdict-text"), path.label);
    text($("rd-rate"), formatRate(info.outputSampleRateHz));
    text($("rd-chan"), `${info.outputChannels} ch`);
    text($("rd-depth"), info.outputSampleFormat ?? "—");
    text($("rd-device"), info.outputDevice ?? "—");
  }

  // Settings mirrors the same facts, from the same StreamInfo.
  $("out-lamp").className = `verdict-lamp ${path.kind}`.trim();
  text($("out-verdict"), path.label);
  text($("out-conversion"), path.note);
  text($("out-rate"), info ? formatRate(info.outputSampleRateHz) : "—");
  text($("out-channels"), info ? String(info.outputChannels) : "—");
  text($("out-format"), info?.outputSampleFormat ?? "—");
  text(
    $("out-mix"),
    info?.mixSampleRateHz
      ? `${formatRate(info.mixSampleRateHz)} / ${info.mixChannels} ch`
      : "—"
  );
  text($("out-requested"), info?.requestedMode ?? "—");
  text($("out-actual"), info?.outputMode ?? "—");
  text($("out-underruns"), snapshot ? String(snapshot.underrunFrames ?? 0) : "—");
  text($("out-device"), info?.outputDevice ?? "—");
  text($("out-endpoint"), info?.outputEndpointId ?? "—");
  const selected = view.devices.find((d) => d.id === view.selectedDeviceId);
  text($("out-state"), selected?.state ?? "—");

  // The console.
  text($("chip-mode"), info ? info.outputMode : "idle");
  $("chip-mode").classList.toggle("is-exclusive", info?.outputMode === "exclusive");
  text($("chip-device"), info?.outputDevice ?? "No stream");
  text(
    $("chip-format"),
    info
      ? `${formatRate(info.outputSampleRateHz)} / ${info.outputChannels} ch / ${
          info.outputSampleFormat ?? "—"
        }`
      : ""
  );

  // The console names what is audible even when the loaded list does not hold
  // the row (album sheet, playlist, search) — the path is the honest fallback.
  const streamPath = info?.path ?? view.playback?.currentPath ?? "";
  const streamTitle = streamPath ? streamPath.split(/[\\/]/).filter(Boolean).pop() : "";
  text($("player-title"), hasTrack ? track.title : streamTitle || "Nothing playing");
  text($("player-artist"), hasTrack ? track.artistName ?? "" : info?.codec ?? "");

  // The status word, so "playing" is stated rather than implied by a glyph.
  const state = snapshot?.state ?? "stopped";
  const stateEl = $("player-state");
  if (stateEl) {
    stateEl.textContent = STATE_LABEL[state] ?? state;
    stateEl.dataset.state = state;
  }

  // Artwork, and the light the whole room takes from it.
  const artHash = album?.artworkHash ?? track?.artworkHash ?? null;
  const label = album?.title ?? track?.title;
  if (artHash) {
    artworkFor(artHash).then((url) => {
      applyArt("now-art", "now-art-empty", url, label);
      applyArt("player-art", "player-art-empty", url, label);
      setBloom(url);
      lightRoomFrom(url);
    });
  } else {
    applyArt("now-art", "now-art-empty", null, label);
    applyArt("player-art", "player-art-empty", null, label);
    setBloom(null);
    lightRoomFrom(null);
  }

  renderMoreByArtist(album?.artistName ?? track?.artistName ?? null);
}

/**
 * Right-hand rail: other releases by the same artist, from real album rows.
 *
 * The single-release case is a real state, not an error: when an artist has
 * only the album in the hero, the rail says so plainly instead of showing an
 * empty box. When nothing is loaded at all, it hides entirely.
 */
function renderMoreByArtist(artistName) {
  const list = $("more-by-list");
  const side = document.querySelector(".plate-rail");
  if (!list) return;
  list.replaceChildren();

  if (!artistName) {
    if (side) side.hidden = true;
    return;
  }

  const all = view.albums.filter((a) => (a.artistName ?? "") === artistName);
  const others = all.filter((a) => a.id !== view.focusAlbumId);
  text($("more-by-note"), others.length
    ? `${others.length} other release${others.length === 1 ? "" : "s"}`
    : `${all.length} release${all.length === 1 ? "" : "s"}`);

  if (!others.length) {
    const li = document.createElement("li");
    li.className = "rail-note";
    li.textContent = all.length
      ? `“${artistName}” has ${all.length === 1 ? "one release" : "only these releases"} in the library.`
      : `No other releases by ${artistName} in the library.`;
    list.append(li);
    if (side) side.hidden = false;
    return;
  }
  if (side) side.hidden = false;

  for (const other of others.slice(0, 8)) {
    const li = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.className = "more-item";

    const art = document.createElement("span");
    art.className = "more-art";
    artworkFor(other.artworkHash).then((url) => {
      if (url) {
        const img = document.createElement("img");
        img.src = url;
        img.alt = "";
        art.replaceChildren(img);
      } else {
        art.textContent = monogram(other.title);
      }
    });
    if (!other.artworkHash) art.textContent = monogram(other.title);

    const text_ = document.createElement("span");
    text_.className = "more-text";
    const name = document.createElement("span");
    name.className = "more-name";
    name.textContent = other.title ?? "Unknown album";
    const meta = document.createElement("span");
    meta.className = "more-meta";
    meta.textContent = [other.year, `${other.trackCount} tracks`].filter(Boolean).join(" · ");
    text_.append(name, meta);

    button.append(art, text_);
    button.addEventListener("click", () => {
      view.focusAlbumId = other.id;
      renderNowPlaying();
      showView("library");
    });
    li.append(button);
    list.append(li);
  }
}

/* ------------------------------------------------------------------ playback */

function renderRepeatButton() {
  const btn = $("repeat-btn");
  if (!btn) return;
  const mode = String(view.playback?.repeat ?? "off");
  btn.classList.toggle("is-on", mode !== "off");
  btn.dataset.repeat = mode;

  const labels = {
    off: "No loop — play the queue once",
    all: "Loop playlist — repeat every track",
    one: "Loop song — repeat this track",
  };
  btn.title = labels[mode] ?? labels.off;
  btn.setAttribute("aria-label", btn.title);

  const loop = $("repeat-path");
  const headTop = $("repeat-head-top");
  const headBottom = $("repeat-head-bottom");
  const one = $("repeat-one");
  const off = $("repeat-off");
  if (loop) loop.setAttribute("d", REPEAT_PATH);
  if (headTop) headTop.setAttribute("d", REPEAT_HEAD_TOP);
  // Two arrowheads mean "the whole list"; one means "this track", so the
  // bottom head is dropped and a 1 takes its place.
  if (headBottom) headBottom.setAttribute("d", mode === "one" ? "" : REPEAT_HEAD_BOTTOM);
  if (one) one.setAttribute("d", mode === "one" ? REPEAT_ONE_GLYPH : "");
  if (off) off.setAttribute("d", mode === "off" ? REPEAT_OFF_PATH : "");
}

/**
 * The transport's own view of the world: the big glyph, its label, and the
 * status word under the title. Driven by the engine's state, so a click that
 * the engine refuses shows up here instead of being swallowed.
 */
const CONSOLE_STATE_LABEL = {
  playing: "Now playing",
  paused: "Paused",
  loading: "Opening",
  seeking: "Seeking",
  buffering: "Buffering",
  stopped: "Stopped",
  finished: "Finished",
  error: "Playback error",
};

function renderTransportState(state) {
  const playing = state === "playing";
  const loading = state === "loading" || state === "buffering";
  $("play-glyph")?.setAttribute("d", playing ? PAUSE_PATH : PLAY_PATH);
  text($("play-label"), playing ? "Pause" : loading ? "Opening" : "Play");
  $("play-pause-btn")?.setAttribute("aria-pressed", playing ? "true" : "false");
  document.body.classList.toggle("is-playing", playing);

  const stateEl = $("player-state");
  if (stateEl) {
    stateEl.textContent = CONSOLE_STATE_LABEL[state] ?? state;
    stateEl.dataset.state = state;
  }
}

function renderPlayback(snapshot) {
  view.playback = snapshot;
  const currentId = snapshot.current ?? null;
  if (currentId !== view.currentTrackId) {
    view.currentTrackId = currentId;
    view.currentRow = null;
    syncCurrentRow(currentId).catch(() => {});
  }
  view.durationMs = snapshot.durationMs ?? view.durationMs;

  if (!view.seeking) {
    view.positionMs = snapshot.positionMs ?? 0;
    renderPosition();
  }

  // The transport glyph follows the engine, not the button: a click sends
  // pause/play and the next snapshot says what actually happened.
  const playing = snapshot.state === "playing";
  const loading = snapshot.state === "loading" || snapshot.state === "buffering";
  renderTransportState(snapshot.state);
  $("seek-slider").disabled = !snapshot.durationMs;

  const volume = Math.round(snapshot.volume * 100);
  const slider = $("volume-slider");
  if (document.activeElement !== slider) slider.value = String(volume);
  fillRange(slider, snapshot.volume ?? 1);
  // Keep the mute memory current from the engine, so Ctrl+Shift+Down restores
  // the real level even if the slider was never touched this session.
  if ((snapshot.volume ?? 0) > 0) view.lastVolume = snapshot.volume;

  markCurrentRow();
  renderNowPlaying();
  renderRepeatButton();
}

function renderPosition() {
  const duration = view.durationMs ?? 0;
  const position = view.positionMs ?? 0;
  const ratio = duration ? position / duration : 0;

  text($("time-current"), formatDuration(position));
  text($("time-total"), duration ? formatDuration(duration) : "0:00");

  // The console's lit lip carries the same ratio, so position is readable
  // without looking at the clock.
  $("console-progress").style.width = `${Math.min(100, ratio * 100)}%`;

  const slider = $("seek-slider");
  slider.disabled = !duration;
  if (!view.seeking) {
    slider.value = String(Math.round(ratio * 1000));
    fillRange(slider, ratio);
  }
}

/* ---------------------------------------------------------------- motion */

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

// For users asking for reduced motion, take the atmosphere offline entirely
// (the GIF would still animate; hiding it is the respectful choice, and the
// room is fully intact with the static gradients behind it).
if (reducedMotion.matches) {
  document.getElementById("gothic-bg")?.remove();
}

/* ------------------------------------------------------------------- folders */

async function loadFolders() {
  const config = await invoke("get_config");
  view.selectedDeviceId = config.output_device_id ?? null;
  view.config = config;

  const gapless = $("gapless-toggle");
  if (gapless) gapless.checked = config.gapless !== false;

  const list = $("folder-list");
  list.replaceChildren();
  if (!config.library_folders.length) {
    const li = document.createElement("li");
    li.className = "dim";
    // Tracks already in the database stay playable, but a scan has nothing to
    // walk — say so, because "Scan library" otherwise looks broken.
    const stats = view.stats;
    li.textContent = stats?.tracks
      ? `No library folders configured, so a scan has nothing to walk. The ${stats.tracks} track${stats.tracks === 1 ? "" : "s"} already in your library still play.`
      : "No library folders configured. Add one above, then scan it.";
    list.append(li);
  } else {
    for (const folder of config.library_folders) {
      const li = document.createElement("li");
      const path = document.createElement("span");
      path.textContent = folder;
      const remove = document.createElement("button");
      remove.type = "button";
      remove.textContent = "Remove";
      remove.addEventListener("click", async () => {
        await invoke("remove_library_folder", { path: folder });
        await loadFolders();
      });
      li.append(path, remove);
      list.append(li);
    }
  }

  // The saved mode may still say `exclusive` from before the picker was
  // removed. Pin it to shared once, so the file the app writes matches what
  // the app can actually do.
  if (config.output_mode && config.output_mode !== "shared") {
    await invoke("set_output_mode", { mode: "shared" }).catch(() => {});
    config.output_mode = "shared";
  }
  text($("mode-readout"), "Shared");
  await loadDevices();
}

async function loadDevices() {
  const select = $("output-device-select");
  try {
    view.devices = await invoke("list_output_devices");
  } catch (error) {
    // Never fail silently: the output panel is technical by design.
    view.devices = [];
    appendEvent({
      type: "warn",
      message: `device list failed — ${error.message ?? error}`,
    });
  }
  const config = await invoke("get_config").catch(() => null);
  const stored = config?.output_device_id ?? view.selectedDeviceId ?? null;

  select.replaceChildren();
  const follow = document.createElement("option");
  follow.value = "";
  follow.textContent = "System default (auto)";
  select.append(follow);

  // Connected devices first, disconnected ones in their own group. This is the
  // whole point of the panel: you should see what you can actually hear on
  // right now, not a flat wall of phantom endpoints.
  const connected = view.devices.filter((d) => d.state === "active");
  const absent = view.devices.filter((d) => d.state !== "active");

  for (const device of connected) {
    const option = document.createElement("option");
    option.value = device.id;
    option.textContent = device.isDefault ? `${device.name} — default` : device.name;
    select.append(option);
  }

  if (absent.length) {
    const group = document.createElement("optgroup");
    group.label = "Not connected";
    for (const device of absent) {
      const option = document.createElement("option");
      option.value = device.id;
      option.textContent = `${device.name} (${device.state})`;
      // Disabled so an unplugged device cannot be picked by accident; a stored
      // selection on one is ignored below rather than silently opening a
      // stream that cannot play.
      option.disabled = true;
      group.append(option);
    }
    select.append(group);
  }

  // Autodetect: honour a stored device only while it is still connected,
  // otherwise fall back to the system default (empty value = follow Windows).
  const storedIsLive =
    stored && view.devices.some((d) => d.id === stored && d.state === "active");
  const selected = storedIsLive ? stored : "";
  if (stored && !storedIsLive) {
    view.selectedDeviceId = null;
    try {
      await invoke("set_output_device", { deviceId: null });
    } catch (error) {
      console.warn("could not clear a stale device selection", error);
    }
  }
  select.value = selected;
  renderDeviceNote();
  renderNowPlaying();
}

/** Plain-language line under the device picker. */
function renderDeviceNote() {
  const note = $("device-note");
  if (!note) return;
  const pinned = view.selectedDeviceId;
  if (pinned) {
    const device = view.devices.find((d) => d.id === pinned);
    note.textContent = `Playing to ${device?.name ?? "a specific device"}.`;
    return;
  }
  const system = view.devices.find((d) => d.isDefault && d.state === "active");
  note.textContent = system
    ? `Following the system default: ${system.name}.`
    : "Following the system default. No output device is connected.";
}

async function renderAbout() {
  const [info, db] = await Promise.all([
    invoke("get_app_info"),
    invoke("get_db_info"),
  ]);
  const list = $("app-info");
  list.replaceChildren();
  const rows = [
    ["Name", info.name],
    ["Version", info.version],
    ["Profile", info.profile],
    ["Platform", info.platform],
    ["Database", `schema v${db.schemaVersion}`],
    ["Database path", db.path],
  ];
  for (const [term, value] of rows) {
    const dt = document.createElement("dt");
    dt.textContent = term;
    const dd = document.createElement("dd");
    dd.textContent = value;
    list.append(dt, dd);
  }
}

/* ------------------------------------------------------- figure panel */

/** Show/hide the Library's right-side figure and load the stored image. */
function applyFigure() {
  const wrap = $("figure-wrap");
  if (!wrap) return;
  const enabled = localStorage.getItem("lumen.figure.enabled") === "1";
  const src = localStorage.getItem("lumen.figure.data");
  wrap.hidden = !enabled;
  const img = $("figure-img");
  const empty = $("figure-empty");
  if (img) {
    if (src) {
      img.src = src;
      img.hidden = false;
    } else {
      img.removeAttribute("src");
      img.hidden = true;
    }
  }
  if (empty) empty.hidden = Boolean(src);
}

const ROOM_NOTES = {
  album: "The room takes the colour of whatever is playing, sampled from its sleeve.",
  cycle: "The room sweeps red → purple → pink and loops, the way an RGB wheel would if you left the green out.",
  candle: "One warm gold, fixed. Nothing moves.",
};

function renderRoomModes() {
  const mode = roomMode();
  for (const button of document.querySelectorAll("#room-modes button")) {
    button.classList.toggle("is-on", button.dataset.room === mode);
  }
  text($("room-note"), ROOM_NOTES[mode]);
}

function wireRoomModes() {
  renderRoomModes();
  for (const button of document.querySelectorAll("#room-modes button")) {
    button.addEventListener("click", () => {
      const mode = button.dataset.room;
      try {
        localStorage.setItem("lumen.room-colour", mode);
      } catch {
        /* private mode: the choice just will not persist */
      }
      startRoomCycle();
      // Re-apply from the artwork the moment the cycle lets go of the room.
      view.lightKey = null;
      const artHash = view.currentRow?.artworkHash ?? focusAlbum()?.artworkHash ?? null;
      if (artHash) {
        artworkFor(artHash).then((url) => lightRoomFrom(url));
      } else {
        lightRoomFrom(null);
      }
      renderRoomModes();
    });
  }
}

function wireFigurePanel() {
  const toggle = $("figure-toggle");
  const note = $("figure-note");
  if (toggle) {
    toggle.checked = localStorage.getItem("lumen.figure.enabled") === "1";
    toggle.addEventListener("change", () => {
      localStorage.setItem("lumen.figure.enabled", toggle.checked ? "1" : "");
      applyFigure();
    });
  }
  const pick = $("figure-pick");
  const input = $("figure-file");
  pick?.addEventListener("click", () => input?.click());
  input?.addEventListener("change", () => {
    const file = input.files?.[0];
    if (!file) return;
    const reader = new FileReader();
    reader.addEventListener("load", () => {
      try {
        localStorage.setItem("lumen.figure.data", String(reader.result));
        localStorage.setItem("lumen.figure.name", file.name);
        localStorage.setItem("lumen.figure.enabled", "1");
        if (toggle) toggle.checked = true;
        if (note) note.textContent = file.name;
        applyFigure();
      } catch (error) {
        if (note) note.textContent = "That image is too large to keep; pick a smaller one.";
        console.warn("could not store the figure", error);
      }
    });
    reader.readAsDataURL(file);
    input.value = "";
  });
  $("figure-clear")?.addEventListener("click", () => {
    localStorage.removeItem("lumen.figure.data");
    localStorage.removeItem("lumen.figure.name");
    if (note) note.textContent = "";
    applyFigure();
  });
  if (note) {
    const name = localStorage.getItem("lumen.figure.name");
    if (name) note.textContent = name;
  }
  applyFigure();
}

/* -------------------------------------------------------------- gapless */

/**
 * The gapless switch, seeded from config and mirrored from the engine.
 *
 * The engine is the authority: `get_playback_state` reports what is actually in
 * force, so the checkbox reflects reality rather than the last thing the UI
 * hoped it set. Persisting happens in the command.
 */
function wireGapless() {
  const toggle = $("gapless-toggle");
  if (!toggle) return;

  toggle.addEventListener("change", async () => {
    const enabled = toggle.checked;
    try {
      await invoke("set_gapless", { enabled });
      showToast(enabled ? "Gapless playback on" : "Gapless playback off", "info");
    } catch (error) {
      // Put the switch back where it was: the command failed, so the engine did
      // not change, and a checkbox that lies is worse than no checkbox.
      toggle.checked = !enabled;
      showError(error);
    }
  });
}

/* ---------------------------------------------------------------- scan events */

function setScanning(active) {
  $("start-scan").disabled = active;
  $("cancel-scan").disabled = !active;
}

async function handleScanEvent(payload) {
  const progress = $("scan-progress");
  switch (payload.scan) {
    case "started":
      setScanning(true);
      progress.textContent = `Scanning ${payload.roots} folder(s)…`;
      break;
    case "progress":
      progress.textContent =
        `found ${payload.discovered} · written ${payload.processed} · ` +
        `skipped ${payload.skipped} · failed ${payload.failed}`;
      break;
    case "completed": {
      setScanning(false);
      const s = payload.summary;
      progress.textContent =
        `${s.elapsedMs}ms — ${s.insertedOrUpdated} written, ${s.skippedUnchanged} skipped, ` +
        `${s.failed} failed, ${s.relinkedMoved} moved, ${s.markedMissing} missing.`;

      // Rejections are the answer to "why wasn't my folder imported?", so they
      // are named rather than reduced to a count.
      const failures = s.failures ?? [];
      if (failures.length) {
        const byKind = new Map();
        for (const failure of failures) {
          const list = byKind.get(failure.kind) ?? [];
          list.push(failure);
          byKind.set(failure.kind, list);
        }
        for (const [kind, list] of byKind) {
          const sample = list
            .slice(0, 3)
            .map((f) => `${(f.path ?? "").split(/[\\/]/).pop()}: ${f.message}`)
            .join("; ");
          appendEvent({
            type: "warn",
            message:
              `scan rejected ${list.length} file(s) at "${kind}" — ` +
              (list.length > 3 ? `${sample}; …` : sample),
          });
        }
        progress.textContent += " Rejected files are listed in Engine events.";
      }

      await invoke("clear_finished_scan");
      await Promise.all([loadStats(), loadTracks(), loadAlbums()]);
      break;
    }
    case "canceled":
      setScanning(false);
      progress.textContent = "Scan canceled.";
      await invoke("clear_finished_scan");
      await loadStats();
      break;
    case "failed":
      setScanning(false);
      progress.textContent = `Scan failed: ${payload.message}`;
      break;
  }
}

/* -------------------------------------------------------------------- events */

function appendEvent(payload) {
  const log = $("event-log");
  const placeholder = log.querySelector(".dim");
  if (placeholder) placeholder.remove();
  const item = document.createElement("li");
  const label = payload.event
    ? payload.event.replace(/-/g, " ")
    : payload.type
      ? payload.type
      : "note";
  item.textContent = `${new Date().toLocaleTimeString()} — ${label}${
    payload.message ? ` — ${payload.message}` : ""
  }`;
  log.prepend(item);
  while (log.children.length > 120) log.lastElementChild.remove();
}

/**
 * Report a failure.
 *
 * The event log is the permanent record and it lives in Settings, which is
 * exactly where you are *not* when something goes wrong. So an error also
 * raises a toast: the log keeps the history, the toast interrupts the moment.
 * The engine repeats some errors on every poll, so identical text inside a few
 * seconds is swallowed rather than stacking into a wall of toasts.
 */
let lastErrorShownAt = 0;
let lastErrorText = "";

function showError(error) {
  const message = error?.message ?? String(error);
  appendEvent({ type: "error", message });

  const now = Date.now();
  if (message === lastErrorText && now - lastErrorShownAt < 4000) return;
  lastErrorText = message;
  lastErrorShownAt = now;
  showToast(message, "error");
}

/* ============================================================== shortcuts
   One table drives both the key handling and the overlay, so the help card
   can never drift from what the app actually does.
   ------------------------------------------------------------------------ */

/* Keys are written the way `KeyboardEvent.key` actually reports them —
   lowercase letters, and "ArrowRight" rather than an arrow glyph — because a
   table that reads prettily and matches nothing is worse than no table. The
   overlay renders the friendly form through KEY_DISPLAY. */
const KEY_DISPLAY = {
  ArrowRight: "→",
  ArrowLeft: "←",
  ArrowUp: "↑",
  ArrowDown: "↓",
  " ": "Space",
  r: "R",
  s: "S",
  l: "L",
  k: "K",
  f: "F",
  p: "P",
  n: "N",
};

const SHORTCUT_GROUPS = [
  {
    title: "Play",
    items: [
      { keys: [" "], label: "Play / pause", run: () => togglePlay() },
      { keys: ["Ctrl", "ArrowRight"], label: "Next track", run: () => invoke("next_track") },
      { keys: ["Ctrl", "ArrowLeft"], label: "Previous track", run: () => invoke("previous_track") },
      { keys: ["Ctrl", "r"], label: "Cycle repeat", run: () => cycleRepeat() },
      { keys: ["Ctrl", "s"], label: "Shuffle the list", run: () => playScope(true) },
    ],
  },
  {
    title: "Volume",
    items: [
      { keys: ["Ctrl", "ArrowUp"], label: "Volume up", run: () => nudgeVolume(0.05) },
      { keys: ["Ctrl", "ArrowDown"], label: "Volume down", run: () => nudgeVolume(-0.05) },
      { keys: ["Ctrl", "Shift", "ArrowDown"], label: "Mute / unmute", run: () => toggleMute() },
      { keys: ["Ctrl", "Shift", "ArrowUp"], label: "Maximum volume", run: () => setVolume(1) },
    ],
  },
  {
    title: "Go",
    items: [
      { keys: ["Ctrl", "l"], label: "Search", also: ["Ctrl", "k"], run: () => focusSearch() },
      { keys: ["Ctrl", "f"], label: "Filter the library", run: () => focusSearch() },
      { keys: ["Alt", "ArrowLeft"], label: "Back", run: () => navMove(-1) },
      { keys: ["Alt", "ArrowRight"], label: "Forward", run: () => navMove(1) },
      { keys: ["Ctrl", "p"], label: "Settings", run: () => showView("settings") },
      { keys: ["Ctrl", "n"], label: "New playlist", run: () => startNewPlaylist() },
      { keys: ["Ctrl", "Shift", "/"], label: "This list", run: () => toggleShortcuts() },
    ],
  },
];

/** True when the key event came from somewhere that eats its own keys. */
function isTyping(target) {
  if (!target) return false;
  const tag = target.tagName;
  return (
    tag === "INPUT" ||
    tag === "TEXTAREA" ||
    tag === "SELECT" ||
    target.isContentEditable === true
  );
}

function isButton(target) {
  return target?.tagName === "BUTTON" || target?.closest?.("button") != null;
}

/** Ctrl+K is listed as an alias for Ctrl+L; both land on the same action. */
function matchesShortcut(event, keys) {
  const ctrl = event.ctrlKey || event.metaKey;
  const shift = event.shiftKey;
  const alt = event.altKey;

  // Modifiers must match exactly. Ctrl+Shift+Arrow (mute / max) is a
  // different gesture from Ctrl+Arrow (next / previous) and must not be
  // swallowed by the looser binding.
  if (keys.includes("Ctrl") !== ctrl) return false;
  if (keys.includes("Shift") !== shift) return false;
  if (keys.includes("Alt") !== alt) return false;

  // A binding with only modifiers (Ctrl+Shift+/) matches on those alone.
  const named = keys.find((k) => !["Ctrl", "Shift", "Alt"].includes(k));
  if (named === undefined) return true;
  // Shift+/ reports "?" on a US layout, so the two are the same gesture.
  if (named === "/" && event.key === "?") return true;
  // Letters arrive lowercase unless Shift is held, and no binding here uses a
  // shifted letter, so compare case-insensitively rather than trapping on it.
  return named.length === 1 ? named.toLowerCase() === event.key.toLowerCase() : named === event.key;
}

function wireShortcuts(event) {
  // Escape is the way out of everything, so it is handled before any chord.
  if (event.key === "Escape") {
    if ($("shortcut-overlay")?.classList.contains("is-open")) {
      event.preventDefault();
      toggleShortcuts(false);
      return;
    }
    if (view.focusAlbumId != null) {
      event.preventDefault();
      focusAlbumById(null);
      return;
    }
    if (!isTyping(event.target) && $("album-detail") && !$("album-detail").hidden) {
      event.preventDefault();
      closeAlbumSheet();
      return;
    }
    const search = $("search-input");
    if (search.value) {
      event.preventDefault();
      search.value = "";
      loadTracks().catch(() => {});
      return;
    }
    if (document.activeElement && !isTyping(document.activeElement)) {
      document.activeElement.blur();
    }
    return;
  }

  // Space is play/pause everywhere except where it means something else:
  // typing a space, or activating a focused button, must not start a track.
  if (event.key === " " && !event.ctrlKey && !event.metaKey && !event.altKey) {
    if (isTyping(event.target) || isButton(event.target)) return;
    event.preventDefault();
    togglePlay();
    return;
  }

  for (const group of SHORTCUT_GROUPS) {
    for (const item of group.items) {
      if (matchesShortcut(event, item.keys)) {
        event.preventDefault();
        item.run();
        return;
      }
      if (item.also && matchesShortcut(event, item.also)) {
        event.preventDefault();
        item.run();
        return;
      }
    }
  }
}

/** Search lives only on the Library, so the shortcut takes you there first. */
async function focusSearch() {
  if (view.name !== "library") {
    await showView("library");
  }
  const search = $("search-input");
  if (search) {
    search.focus();
    search.select();
  }
}

/** Walk the view history for Alt+Left / Alt+Right. */
function navMove(direction) {
  const nav = view.nav;
  const next = nav.index + direction;
  if (next < 0 || next >= nav.stack.length) return;
  nav.index = next;
  nav.locked = true;
  showView(nav.stack[next]).finally(() => {
    nav.locked = false;
  });
}

/** Ctrl+N: land on Playlists with the name field focused, ready to type. */
async function startNewPlaylist() {
  if (view.name !== "playlists") {
    await showView("playlists");
  }
  const input = $("playlist-name-input");
  if (input) {
    input.focus();
    input.select();
  }
}

/** Build the overlay from the same table the key handler uses. */
function renderShortcutOverlay() {
  const list = $("shortcut-list");
  if (!list) return;
  list.replaceChildren();
  for (const group of SHORTCUT_GROUPS) {
    const section = document.createElement("section");
    section.className = "shortcut-group";

    const heading = document.createElement("h3");
    heading.textContent = group.title;
    section.append(heading);

    for (const item of group.items) {
      const row = document.createElement("div");
      row.className = "shortcut-row";

      const keys = document.createElement("span");
      keys.className = "shortcut-keys";
      for (const key of item.keys) {
        const kbd = document.createElement("kbd");
        kbd.textContent = KEY_DISPLAY[key] ?? key.toUpperCase();
        keys.append(kbd);
      }
      if (item.also) {
        const or = document.createElement("span");
        or.className = "shortcut-or";
        or.textContent = "or";
        keys.append(or);
        for (const key of item.also) {
          const kbd = document.createElement("kbd");
          kbd.textContent = KEY_DISPLAY[key] ?? key.toUpperCase();
          keys.append(kbd);
        }
      }

      const label = document.createElement("span");
      label.className = "shortcut-label";
      label.textContent = item.label;

      row.append(keys, label);
      section.append(row);
    }
    list.append(section);
  }
}

function toggleShortcuts(force) {
  const overlay = $("shortcut-overlay");
  if (!overlay) return;
  const open = force ?? !overlay.classList.contains("is-open");
  overlay.classList.toggle("is-open", open);
  overlay.setAttribute("aria-hidden", String(!open));
  if (open) {
    // Remember who opened it so focus can go back there on close.
    view.shortcutReturn = document.activeElement;
    renderShortcutOverlay();
    // Focus goes to the card so the keyboard user is inside the dialog, and
    // Tab cycles within it rather than wandering the app behind.
    overlay.querySelector(".shortcut-card")?.setAttribute("tabindex", "-1");
    overlay.querySelector(".shortcut-card")?.focus();
  } else {
    // Return focus to whatever opened it.
    view.shortcutReturn?.focus?.();
    view.shortcutReturn = null;
  }
}

function wireShortcutOverlay() {
  const overlay = $("shortcut-overlay");
  if (!overlay) return;
  overlay.addEventListener("click", (event) => {
    // A click on the dimmed field, not on the card, dismisses.
    if (event.target === overlay) toggleShortcuts(false);
  });
  $("shortcut-close")?.addEventListener("click", () => toggleShortcuts(false));
}

/* -------------------------------------------------------------------- boot */

/* -------------------------------------------------- transport, as actions
   Every control is a named function so a button and a keyboard key can call
   exactly the same thing. There is no behaviour that exists only on the
   keyboard, and none that exists only on a button.
   ------------------------------------------------------------------------ */

function togglePlay() {
  const cmd = view.playback?.state === "playing" ? "pause" : "play";
  return invoke(cmd).catch(showError);
}

/** Play the current scope (focused album, or the loaded track list). */
function playScope(shuffle) {
  const rows = visibleTracks();
  if (!rows.length) return;
  const order = rows.map((t) => t.id);
  let startIndex = 0;
  if (shuffle && order.length > 1) {
    for (let i = order.length - 1; i > 0; i--) {
      const j = Math.floor(Math.random() * (i + 1));
      [order[i], order[j]] = [order[j], order[i]];
    }
  } else {
    startIndex = Math.max(
      0,
      rows.findIndex((t) => t.id === view.currentTrackId)
    );
  }
  return invoke("play_tracks", { tracks: order, startIndex }).catch(showError);
}

async function cycleRepeat() {
  if (!view.playback) return;
  const order = ["off", "all", "one"];
  const cur = String(view.playback.repeat ?? "off");
  const next = order[(order.indexOf(cur) + 1) % order.length];
  try {
    await invoke("set_repeat", { mode: next });
    view.playback.repeat = next;
    renderRepeatButton();
  } catch (error) {
    showError(error);
  }
}

/** Absolute volume, which also keeps the mute memory honest. */
function setVolume(value) {
  const clamped = Math.max(0, Math.min(1, value));
  if (clamped > 0) {
    view.muted = false;
    view.lastVolume = clamped;
  } else {
    view.muted = true;
  }
  const slider = $("volume-slider");
  if (slider) {
    slider.value = String(Math.round(clamped * 100));
    fillRange(slider, clamped);
  }
  return invoke("set_volume", { volume: clamped }).catch(showError);
}

function nudgeVolume(delta) {
  const current = view.playback?.volume ?? view.lastVolume ?? 1;
  // Nudging up while muted restores the level that was there, rather than
  // jumping to 5% — which is what every other player does.
  const from = view.muted ? view.lastVolume : current;
  return setVolume(from + delta);
}

function toggleMute() {
  const current = view.playback?.volume ?? 1;
  if (view.muted || current === 0) {
    return setVolume(view.lastVolume > 0 ? view.lastVolume : 0.6);
  }
  view.lastVolume = current;
  return setVolume(0);
}

function wireTransport() {
  $("play-pause-btn").addEventListener("click", togglePlay);
  $("next-btn").addEventListener("click", () => invoke("next_track").catch(showError));
  $("prev-btn").addEventListener("click", () => invoke("previous_track").catch(showError));
  $("repeat-btn").addEventListener("click", cycleRepeat);

  $("hero-play-btn").addEventListener("click", () => {
    if (view.playback?.state === "playing") {
      invoke("pause").catch(showError);
    } else {
      playScope(false);
    }
  });
  $("hero-shuffle-btn").addEventListener("click", () => playScope(true));
  $("shuffle-btn").addEventListener("click", () => playScope(true));

  const slider = $("seek-slider");
  slider.addEventListener("pointerdown", () => {
    view.seeking = true;
  });
  slider.addEventListener("input", () => {
    view.seeking = true;
    const duration = view.durationMs ?? 0;
    const position = Math.round((slider.value / 1000) * duration);
    view.positionMs = position;
    renderPosition();
  });
  const commit = () => {
    if (!view.seeking) return;
    view.seeking = false;
    const duration = view.durationMs ?? 0;
    invoke("seek_to", { positionMs: Math.round((slider.value / 1000) * duration) }).catch(
      showError
    );
  };
  slider.addEventListener("change", commit);
  slider.addEventListener("pointerup", commit);

  let volumeTimer = null;
  const volume = $("volume-slider");
  volume.addEventListener("input", (event) => {
    clearTimeout(volumeTimer);
    const value = Number(event.target.value) / 100;
    fillRange(volume, value);
    // Moving the slider by hand is an explicit volume, so it clears mute and
    // updates the memory that Ctrl+Shift+Down restores later.
    if (value > 0) {
      view.muted = false;
      view.lastVolume = value;
    } else {
      view.muted = true;
    }
    volumeTimer = setTimeout(() => invoke("set_volume", { volume: value }), 120);
  });

  for (const button of document.querySelectorAll(".nav-item")) {
    button.addEventListener("click", () => showView(button.dataset.view));
  }

  $("collapse-btn").addEventListener("click", (event) => {
    const app = $("app");
    const collapsed = app.classList.toggle("is-collapsed");
    event.currentTarget.setAttribute("aria-pressed", String(collapsed));
    try {
      localStorage.setItem("lumen.sidebar-collapsed", collapsed ? "1" : "0");
    } catch {
      /* storage unavailable: layout still works */
    }
  });

  document.addEventListener("keydown", wireShortcuts);

  let searchTimer = null;
  $("search-input").addEventListener("input", () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => loadTracks().catch(() => {}), 200);
  });

  // ------------------------------------------------------------- collections
  $("create-playlist").addEventListener("click", async () => {
    const input = $("playlist-name-input");
    const name = input.value.trim();
    if (!name) {
      input.focus();
      return;
    }
    try {
      const id = await invoke("create_playlist", { name });
      input.value = "";
      await loadPlaylists();
      await loadStats();
      const created = { id, name, trackCount: 0, durationMs: 0 };
      await openPlaylist(created);
    } catch (error) {
      showError(error);
    }
  });

  $("playlist-name-input").addEventListener("keydown", (event) => {
    if (event.key === "Enter") $("create-playlist").click();
  });

  $("playlist-close").addEventListener("click", () => {
    $("playlist-detail").hidden = true;
    view.playlistId = null;
  });

  $("album-close").addEventListener("click", () => closeAlbumSheet());

  $("album-play").addEventListener("click", () => {
    const tracks = view.sheetTracks ?? [];
    if (!tracks.length) return;
    invoke("play_tracks", { tracks: tracks.map((t) => t.id), startIndex: 0 }).catch(showError);
  });

  $("playlist-play").addEventListener("click", async () => {
    if (view.playlistId == null) return;
    const tracks = await invoke("list_playlist_tracks", { playlistId: view.playlistId });
    if (!tracks.length) {
      appendEvent({ type: "note", message: "playlist is empty — add tracks first" });
      return;
    }
    invoke("play_tracks", { tracks: tracks.map((t) => t.id), startIndex: 0 }).catch(showError);
  });

  $("playlist-rename").addEventListener("click", async () => {
    if (view.playlistId == null) return;
    const current = $("playlist-detail-title").textContent ?? "";
    const name = window.prompt("Rename playlist", current);
    if (name === null || !name.trim()) return;
    try {
      await invoke("rename_playlist", { playlistId: view.playlistId, name });
      text($("playlist-detail-title"), name.trim());
      applyPlaylistCover({ id: view.playlistId, name: name.trim() });
      await loadPlaylists();
    } catch (error) {
      showError(error);
    }
  });

  // A playlist picture: pick a file, keep it beside the app, redraw the card.
  // Right-click the picture to take it away again.
  $("playlist-cover-pick").addEventListener("click", () => $("playlist-cover-file").click());
  $("playlist-cover-file").addEventListener("change", async () => {
    const file = $("playlist-cover-file").files?.[0];
    if (!file || view.playlistId == null) return;
    try {
      const dataUrl = await shrinkImageToDataUrl(file, 512);
      if (!writePlaylistCover(view.playlistId, dataUrl)) {
        showToast("That picture is too large to keep. Try a smaller one.", "info");
        return;
      }
      const name = $("playlist-detail-title").textContent ?? "";
      applyPlaylistCover({ id: view.playlistId, name });
      await loadPlaylists();
      showToast("Picture set");
    } catch (error) {
      console.warn("could not store the playlist picture", error);
      showToast("That file could not be read as an image.", "info");
    } finally {
      $("playlist-cover-file").value = "";
    }
  });
  $("playlist-detail-cover").addEventListener("contextmenu", async (event) => {
    event.preventDefault();
    if (view.playlistId == null || !readPlaylistCover(view.playlistId)) return;
    clearPlaylistCover(view.playlistId);
    applyPlaylistCover({
      id: view.playlistId,
      name: $("playlist-detail-title").textContent ?? "",
    });
    await loadPlaylists();
    showToast("Picture removed");
  });

  $("playlist-delete").addEventListener("click", async () => {
    if (view.playlistId == null) return;
    const name = $("playlist-detail-title").textContent ?? "";
    const confirmed = await confirmDelete(`Delete playlist "${name}"? This cannot be undone.`);
    if (!confirmed) return;
    try {
      await invoke("delete_playlist", { playlistId: view.playlistId });
      // The picture goes with the playlist, or the next one by that name would
      // inherit it.
      clearPlaylistCover(view.playlistId);
      $("playlist-detail").hidden = true;
      view.playlistId = null;
      await loadPlaylists();
      await loadStats();
    } catch (error) {
      showError(error);
    }
  });

  $("clear-history").addEventListener("click", async () => {
    if (!(await confirmDelete("Clear your entire listening history?"))) return;
    try {
      await invoke("clear_history");
      await loadHistory();
    } catch (error) {
      showError(error);
    }
  });

  $("output-device-select").addEventListener("change", async (event) => {
    const deviceId = event.target.value === "" ? null : event.target.value;
    view.selectedDeviceId = deviceId;
    renderDeviceNote();
    try {
      await invoke("set_output_device", { deviceId });
    } catch (error) {
      showError(error);
    }
  });

  // The device panel no longer has a mode picker; nothing to wire here.

  $("add-folder").addEventListener("click", async () => {
    const input = $("folder-input");
    const path = input.value.trim();
    if (!path) return;
    try {
      await invoke("add_library_folder", { path });
      input.value = "";
      await loadFolders();
    } catch (error) {
      $("scan-progress").textContent = error.message ?? String(error);
    }
  });

  $("folder-input").addEventListener("keydown", (event) => {
    if (event.key === "Enter") $("add-folder").click();
  });

  $("start-scan").addEventListener("click", async () => {
    try {
      await invoke("start_scan");
      setScanning(true);
    } catch (error) {
      $("scan-progress").textContent = error.message ?? String(error);
    }
  });

  $("cancel-scan").addEventListener("click", async () => {
    try {
      await invoke("cancel_scan");
    } catch (error) {
      $("scan-progress").textContent = error.message ?? String(error);
    }
  });
}

document.addEventListener("DOMContentLoaded", async () => {
  // One arrival: the rail, stage and console settle in once. The class is
  // dropped afterwards so nothing re-animates when a view changes.
  if (!reducedMotion.matches) {
    $("app").classList.add("is-booting");
    setTimeout(() => $("app").classList.remove("is-booting"), 1200);
  }

  try {
    localStorage.getItem("lumen.sidebar-collapsed") === "1" &&
      ($("app").classList.add("is-collapsed"),
      $("collapse-btn").setAttribute("aria-pressed", "true"));
  } catch {
    /* ignore */
  }

  try {
    const [snapshot] = await Promise.all([
      invoke("get_playback_state"),
      loadStats(),
      loadTracks(),
      loadFolders(),
      renderAbout(),
    ]);
    renderPlayback(snapshot);
  } catch (error) {
    console.error("failed to load initial state", error);
    showError(error);
  }

  wireTransport();
  wireFigurePanel();
  wireGapless();
  wireShortcutOverlay();
  wireRoomModes();
  startRoomCycle();
  fillRange($("seek-slider"), 0);
  fillRange($("volume-slider"), 1);

  // The console polls the engine as well as listening to it. Events are the
  // fast path; this is the guarantee. Without it a single dropped event leaves
  // the player showing "Nothing playing" while the music keeps running, and
  // there is no way for the user to tell that apart from a broken player.
  //
  // The cadence follows the state: while audio is moving the position has to
  // look live, so it polls fast. Stopped or paused there is nothing to keep in
  // step, so it eases right off - an idle player should not be re-rendering
  // eight times a second.
  const POLL_LIVE_MS = 400;
  const POLL_IDLE_MS = 2000;
  let pollTimer = null;

  function pollDelay() {
    const state = view.playback?.state;
    return state === "playing" || state === "seeking" || state === "loading"
      ? POLL_LIVE_MS
      : POLL_IDLE_MS;
  }

  function schedulePoll() {
    if (pollTimer != null) clearTimeout(pollTimer);
    pollTimer = setTimeout(async () => {
      try {
        invoke("get_playback_state").then(renderPlayback).catch(() => {});
      } finally {
        schedulePoll();
      }
    }, pollDelay());
  }

  schedulePoll();

  // Nothing is moving, and nothing will, while the window is hidden. A hidden
  // window paints nothing, so neither should the poll.
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) schedulePoll();
  });

  await listen("audio-event", (event) => {
    const payload = event.payload;
    switch (payload.event) {
      case "position-update":
        if (!view.seeking) {
          view.positionMs = payload.positionMs ?? 0;
          view.durationMs = payload.durationMs ?? view.durationMs;
          renderPosition();
        }
        break;
      case "stream-started":
        // A new stream began: this is the honest moment to record history,
        // because the engine has told us a track really opened. The stream info
        // arrives here, so the console names the song without waiting for the
        // next snapshot round-trip.
        view.playback = {
          ...(view.playback ?? {}),
          state: "playing",
          stream: payload.info ?? view.playback?.stream ?? null,
          currentPath: payload.info?.path ?? view.playback?.currentPath ?? null,
        };
        renderNowPlaying();
        renderTransportState("playing");
        noteHistory();
        break;
      case "state-changed":
        renderTransportState(payload.state ?? view.playback?.state);
        break;
      case "error":
        appendEvent({ type: "error", message: payload.message });
        renderTransportState("error");
        break;
      default:
        appendEvent(payload);
    }
    invoke("get_playback_state").then(renderPlayback).catch(() => {});
  });

  await listen("library-event", (event) => handleScanEvent(event.payload));
});
