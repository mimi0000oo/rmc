# RMC — Remote Media Control

Rust + Web UI remote media controller. Standard single-crate layout
(`src/*.rs` modules, one `rmc-server` binary). Controls multiple media
sources (Spotify app, browser media, YouTube Music), differentiates streams,
lets user switch active stream and control playback remotely (phone/laptop on LAN).

## Assumptions (auto-decided, no questions asked)

1. Linux first (MPRIS D-Bus for native players, `pactl` for app volumes). Windows/macOS via platform shim later.
2. Single Rust package, one binary `rmc-server`. Axum + Tokio WebSocket on one port (default 8973), HTTPS twin (default 8974).
3. Frontend is no-build vanilla HTML/CSS/JS, embedded in the binary — always loadable, screenshot-safe.
4. Spotify: MPRIS-backed observation/control + Web API catalog search (app credentials, auto-refresh). Full librespot/Spotify Web Playback deferred.
5. YouTube Music: browser tab is the player — controlled via MediaSession reporter + server-side registry. No unofficial YTM API dependency.
6. Module rule: each `src/*.rs` module owns its area; `src/main.rs` (integrator) wires everything.

## Modules (one file per subsystem, all in `src/`)

```
rmc/
├── ARCHITECTURE.md
├── Cargo.toml                  # single package (integrator-owned)
├── src/
│   ├── main.rs                 # binary: wiring, hook, TLS, settings boot
│   ├── backend.rs              # core domain: sources, sessions, switching
│   ├── webserver.rs            # Axum REST + WS transport
│   ├── protocol.rs             # canonical wire protocol (Rust types)
│   ├── spotify.rs              # Spotify adapter + Web API search + OpenUri
│   ├── mpris.rs                # desktop player watch (ALL MPRIS players)
│   ├── ytmusic.rs              # YT Music tab adapter
│   ├── sysaudio.rs             # whole-app volume via pactl + pending queue
│   └── settings.rs             # persisted settings (0600 JSON)
├── frontend/                   # static UI (embedded): index.html, app.js, styles.css
│   └── js/rmc-client.js        # canonical WS client (plain script)
│   └── js/smoke.mjs            # node wire-client checks
├── docs/STATUS.json            # gauntlet scores + open issues (source of resume truth)
```

## Data & control flow

```
[Spotify app] --MPRIS--> [mpris watch] --+
[Browser tab] --MediaSession--> [frontend reporter + ytmusic] --+
[Desktop browsers] --MPRIS--> [mpris watch] ---------------+--> [backend registry] --> [webserver: REST+WS] <--> [UI / phone]
                                                     [protocol module in the middle]
```

- `backend::SourceRegistry` is single source of truth: `HashMap<SourceId, MediaSource> + active_id`.
- Integrations only push `PlaybackState` deltas and implement `MediaAdapter` (play/pause/next/prev/seek/volume). They never touch WS/HTTP.
- Actuation path: `webserver` mutates the cache, routes tab downlink (only for
  `tab:` origins), then fires the `on_control(id, origin, action)` hook (wired
  by the binary to MPRIS via the source's `mpris:{bus}` origin). Adapters never
  block transport; registry cache is updated first so remotes stay responsive
  even if a player is slow. Failures mark `actuator_error` + rebroadcast.
- Desktop players: `mpris` discovers every MPRIS bus name and subscribes to
  `PropertiesChanged` signals (Noctalia-style live updates) + `NameOwnerChanged`
  for presence, with the periodic poll as backstop (position, safety). Track
  changes trigger a 700ms confirmation re-read so staged metadata converges fast.
- Browser volume: MPRIS volume is cosmetic in Chromium, so `browser:*` volume
  goes MPRIS → whole-app system volume (`sysaudio` via `pactl`) → remembered
  when idle, applied on playback start. Mode persisted in settings
  (auto/mpris/system). Volume read-back verification catches ignored changes.
- Artwork: `mpris:artUrl` + MediaSession artwork flow into snapshots; the UI
  loads `https` directly and `file://` (Chromium tmp thumbnails) through the
  id-scoped `GET /api/art?src=` proxy (magic-byte sniffing, caps, cache).
- Settings: `GET/POST /api/settings` (masked flags only, never values),
  persisted owner-only JSON; env/file seed once.
- Single binary: `server/rmc-server` is the only binary target; the frontend
  is embedded at compile time (rust-embed) with the on-disk dir as dev
  override. It runs from any cwd with no sibling files.
- `webserver` translates registry into `GET /api/sources`, `POST /api/active`, `POST /api/control`, `WS /ws`.
- `protocol` (+ `frontend/js/rmc-client.js`) defines wire JSON (`hello/state/control/report/error/tab_control` envelopes + `PROTOCOL_VERSION`). Both Rust types and `js/rmc-client.js` must match; the JS file lives at `frontend/js/rmc-client.js` (single copy, plain script). The webserver imports these types — duplicates are banned.
- `frontend` renders source list, differentiates streams (app icon + kind badge + origin), switches active, sends controls, reconnects.

## Wire protocol (v2, canonical: `protocol` + `frontend/js/`)

- `GET /api/health` → `{status, version, protocol_version}`
- `GET /api/sources` → `{type:"state", protocol_version, revision, active_id, sources:[{id,kind,kind_label,name,origin,status,title,artist,album,art_url,duration_ms,position_ms,volume,actuator_error?}]}` (flat shape — frontend parses this, plus nested `{state:{...}}` defensively)
- `POST /api/active {id}` → switch active stream (`400` empty id, `404` unknown)
- `POST /api/control {id?, action, position_ms?, level?, uri?}` → `play|pause|toggle|next|prev|seek|volume|open_uri`; strict: seek requires `position_ms`, volume requires `level` 0–100, open_uri requires `uri`, else `400 {ok:false, code, error}`
- `POST /api/search {q}` → Spotify catalog search (needs `RMC_SPOTIFY_TOKEN`, else `503` with setup instructions); returns `{ok, tracks:[{name,artists,album,uri,art_url}]}`
- `POST /api/play {id?, uri}` → play a URI on target (default active) via the honest control path (`OpenUri` + reconcile)
- `POST /api/report {tab:{tab_id,title?,artist?,album?,artwork_url?,duration_ms?,position_ms?,volume?,status?,page_url?}}` → tab ingestion (YTM iff `page_url` matches `music.youtube.com`); heartbeat via repeat reports, expiry after 60s silence
- `POST /api/unreport {tab_id}` → explicit tab close (removes its sources)
- `GET /api/art?src=<id>` → artwork bytes for a known source (http(s) fetch or `file://` read, id-scoped, capped, cached)
- `GET /api/settings` → masked presence flags + volume mode (never secrets); `POST /api/settings` → save (applies live)
- `WS /ws` → `hello {client, protocol_version?, tab_id?}` binds optional tab for targeted downlink; server pushes `state` on change (with `revision`; lagged clients get full resnapshot); every rejection returns `{type:"error", code, message, request_id?}`; tab-targeted `{type:"tab_control", tab_id, action, ...}` goes only to the owning connection.
- Single transport per action: WS when open, REST fallback only when WS is down. Never both.

## Stream differentiation & switching

- `SourceKind`: `Spotify | Browser | YouTubeMusic | System`.
- Identity = `SourceId` (stable string, e.g. `spotify:local`, `browser:tab-3`).
- Active-switch policy lives in `backend`: explicit user switch wins; auto-switch on new playing source is opt-in flag (default off to avoid fights).
- Remote = any device opening `http://<server-ip>:8973/`.

## Build / run contract (keep loadable at all times)

- `cargo run --bin rmc-server` → serves `http://127.0.0.1:8973/` (+ https twin).
- Frontend must render with zero sources (empty state) — never blank.
- Dev server kept running in background during iterations.

## Gauntlet process

- After each builder round, brutal critic (writes no code) scores module 0–10 against ultimate product.
- Builder fixes ranked issues, up to 4 rounds per module.
- Scores + open issues persist in `docs/STATUS.json`; resume from weakest module.
- Final gate: whole-system critic judges everything.
