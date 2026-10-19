# RMC — Remote Media Control

Rust + web UI remote media controller. Control multiple media sources
(Spotify app, browser tabs, YouTube Music) from any device on your LAN:
list streams, tell them apart, switch the active one, and drive playback
from your phone.

## Building with cargo

Prerequisites: a recent stable Rust toolchain (`rustup`), pkg-config +
a C compiler (vendored D-Bus/TLS build), and on the *run* machine:
`pactl` (PipeWire/PulseAudio utils) for whole-app browser volume.

```bash
cargo build                    # debug binary -> target/debug/rmc-server
cargo build --release          # optimized binary -> target/release/rmc-server
cargo run --bin rmc-server     # build + run (http :8973, https :8974)
cargo test                     # 48 Rust tests (unit + live-bus probes)
node frontend/js/smoke.mjs     # 13 JS wire-client checks (no deps)
cargo clippy -- -D warnings    # lint-clean guarantee
cargo fmt --check              # formatting gate
```

The build emits **exactly one binary** (`rmc-server`); the web UI is
embedded at compile time, so the binary runs from any directory with no
sibling files. The build is warning-free (`0 warnings` on `cargo build`
and `cargo test --no-run`).

## Binding to a subnet

By default the server listens on `0.0.0.0` (every interface). To restrict it
to one network, set `RMC_BIND` to a bare IP or a CIDR subnet — a CIDR
resolves to the machine's own address inside it:

```bash
RMC_BIND=10.42.0.0/24 cargo run --bin rmc-server
# listens on 10.42.0.165:8973 (+:8974 https) — docker0, wg tunnels etc. excluded
```

Invalid specs (or a subnet with no local address) fail fast at startup and
list your actual addresses. The HTTPS cert regenerates to include the bound
address. `RMC_BIND=127.0.0.1` makes it localhost-only.

## Quickstart

```bash
cargo run --bin rmc-server
# open http://127.0.0.1:8973/ — or http://<server-lan-ip>:8973/ from your phone
```

Demo mode (fake Spotify + browser seeds, for screenshots only):

```bash
RMC_DEMO=1 cargo run --bin rmc-server
```

Without `RMC_DEMO=1` the server boots honestly empty — no fake players.

## Configuration (env)

| Var | Default | Meaning |
|---|---|---|
| `RMC_PORT` | `8973` | Port to bind (on the `RMC_BIND` address) |
| `RMC_BIND` | `0.0.0.0` | Bind address: bare IP (`127.0.0.1`) or CIDR subnet (`10.42.0.0/24` → our address inside it). Restricts which LAN can reach the server |
| `RMC_FRONTEND` | `frontend` | Directory of static web assets to serve |
| `RMC_DEMO` | off | `1` registers demo seeds (fake presence, screenshots only) |
| `RMC_SPOTIFY_BUS` | `org.mpris.MediaPlayer2.spotify` | Legacy: MPRIS watch now covers all players (kept for compat) |
| `RMC_SPOTIFY_TOKEN` | — | Explicit user OAuth token (override; hourly paste) |
| `RMC_SPOTIFY_ID` + `RMC_SPOTIFY_SECRET` | — | Spotify app credentials; tokens auto-minted + refreshed (recommended) |
| `RMC_TLS` | on | `0` disables the HTTPS twin |
| `RMC_TLS_PORT` | `8974` | HTTPS twin port (self-signed cert) |
| `RMC_TLS_DIR` | `./.rmc-tls` | Cert storage (regenerated when your LAN IP changes) |

## Single binary

`cargo build` produces exactly one binary: `target/debug/rmc-server`
(`cargo build --release` for the optimized one). The web UI is embedded at
compile time — copy that one file anywhere and run it, no `frontend/`
directory needed (verified: runs from an empty cwd, serves UI + API).

## Spotify search — step by step

Search uses Spotify's public catalog, which needs app credentials (no user
login, no redirect dance). You do this **once**; the server mints and refreshes
tokens itself.

1. Go to <https://developer.spotify.com/dashboard> and log in with any Spotify account.
2. Click **Create app**. Fill in:
   - App name: `RMC` (anything works)
   - App description: anything
   - Redirect URI: `http://127.0.0.1:3000` (required field; RMC doesn't use it)
   - Tick the API checkbox "Web API" and accept the terms, then **Save**.
3. Open your new app → **Settings**. Copy the **Client ID**. Click
   **View client secret** and copy the **Client secret**.
4. Export both and (re)start the server:
   ```bash
   export RMC_SPOTIFY_ID=<your-client-id>
   export RMC_SPOTIFY_SECRET=<your-client-secret>
   cargo run -p rmc-server
   ```
   Startup no longer prints the search hint — credentials were picked up.
5. Use the **Search Spotify** panel in the UI. Results show art + artist +
   album; ▶ plays the track on the active stream.

Notes:
- Tokens minted this way last ~1 hour; the server refreshes automatically.
  (The old copy-paste-a-token flow still works via `RMC_SPOTIFY_TOKEN` as an
  override, but you don't need it.)
- **Easiest: skip env vars entirely** — paste the Client ID + secret into the
  **Settings** panel in the UI (below). Same effect, saved securely.
- If search answers `token_failed`, re-check the id/secret for typos.
- Client-credentials search covers the public catalog (tracks/artists/albums).
  Personalized endpoints (your playlists etc.) are out of scope.

## Settings (saved, survives restarts)

The **Settings** panel in the UI manages everything that used to need env
vars and restarts:

- Spotify Client ID / secret / token override (password fields; the API only
  ever reports `*_set` flags, never values)
- Browser volume mode: **Auto** (MPRIS → whole-app system → remember),
  **MPRIS only**, or **System only**

Saved to `~/.config/rmc/settings.json` (owner-only `0600`; override path via
`RMC_SETTINGS_FILE`). Tokens and volume mode apply instantly; ports/TLS still
need a restart. Env vars (`RMC_SPOTIFY_ID/SECRET/TOKEN`) and a `./spotify`
file (`id:`/`secret:`/`token:` lines) seed the file once on first boot.

## Browser volume (whole app)

Chromium-based browsers (Brave/Chrome/Edge) accept MPRIS volume commands but
silently ignore them (verified: set succeeds, read-back unchanged) — so RMC
drives the **whole application's system volume** instead (`pactl` sink
inputs, all tabs at once):

- Drag the volume slider on a `browser:*` source: MPRIS is tried first, then
  the app's sink inputs. It just works — no setup.
- The slider shows the *real* sink volume (polled), not the cosmetic MPRIS one.
- Setting volume while nothing is playing **remembers** the level and applies
  it the moment audio starts.
- **Settings → Browser volume** switches modes: Auto (above), MPRIS only, or
  System only.

Requires `pactl` (PipeWire/PulseAudio utils) on the server machine. Per-tab
volume (rather than whole-app) remains available to tinkerers via
`frontend/ytm-reporter.js` (`?rmc=` + `?tab=` params, see ARCHITECTURE.md).

## How it works

```
[Spotify app] --MPRIS--> [mpris watch] --+
[Browser tab] --MediaSession--> [reporter + ytmusic] --+
[Desktop browsers] --MPRIS--> [mpris watch] ----------+
                                               |
                                               v
                                    [backend registry: sources + active]
                                               |
                                               v
                                    [webserver: REST + WS :8973 (+:8974 https)]
                                               |
                                               v
                                    [frontend UI / any LAN phone]
```

- **`src/backend.rs`** — single source of truth: `SourceRegistry`
  (streams + active id + revision counter), stale-update rejection, TTL
  pruning, `actuator_error` degradation flag, `MediaAdapter` trait.
- **`src/webserver.rs`** — Axum REST + WebSocket transport.
  Optimistic cache update + broadcast, then bounded actuation (2s) with
  reconcile: failed players are marked degraded and the caller gets an
  error — never silent `ok:true`.
- **`src/protocol.rs` + `frontend/js/rmc-client.js`** — the canonical wire
  protocol (Rust types + the single-copy plain-script JS client).
- **`src/spotify.rs`** — MPRIS adapter + control primitives, Spotify Web
  API search (credentials below), play-by-URI via MPRIS `OpenUri`.
- **`src/mpris.rs`** — desktop player watch: polls *all* MPRIS players
  (Spotify, Brave/Chrome/Firefox, VLC), so browser playback is detected with
  no reporter. Skips proxy buses (`playerctld`) to avoid duplicates.
- **`src/ytmusic.rs`** — tab ingestion mapping + kind detection.
  Live reporting comes from `frontend/ytm-reporter.js` (MediaSession read,
  heartbeat reports, `TabControl` actuation, `pagehide` unreport beacon).
- **`src/sysaudio.rs`** — whole-app volume via `pactl`: browsers ignore MPRIS
  volume, so the server drives the app's sink inputs (all tabs) and remembers
  the level when nothing is live yet. UI slider shows the real sink volume.
- **`src/settings.rs`** — persisted settings (tokens, volume mode) in
  owner-only JSON; secrets are only ever exposed as presence flags.
- **`frontend/`** — no-build vanilla HTML/CSS/JS. Keyed rendering, seek +
  volume, Spotify search, settings panel, connection
  machine, error toasts, degraded badges.

## API

Base `http://<host>:8973`. Protocol version: `2` (see `src/protocol.rs`).

| Endpoint | Purpose |
|---|---|
| `GET /` | Web UI (never blank: honest empty state when sourceless) |
| `GET /api/health` | `{status, version, protocol_version}` |
| `GET /api/sources` | `{type:"state", protocol_version, revision, active_id, sources:[...]}` |
| `POST /api/active {id}` | Switch active stream (`400`/`404`/`409` typed errors) |
| `POST /api/control {id?, action, position_ms?, level?, uri?}` | `play\|pause\|toggle\|next\|prev\|seek\|volume\|open_uri` — strict: seek needs `position_ms`, volume needs `level` 0–100, open_uri needs `uri` |
| `POST /api/search {q}` | Spotify catalog search (needs token, else `503` + instructions) |
| `POST /api/play {id?, uri}` | Play a URI (e.g. a search result) on the target stream |
| `POST /api/report {tab:{...}}` | Tab heartbeat ingestion (creates/refreshes `ytmusic:*` / `browser:*`) |
| `POST /api/unreport {tab_id}` | Explicit tab close (removes its sources) |
| `GET /api/art?src=<id>` | Artwork bytes for a known source (`file://` thumbnails proxied) |
| `GET /api/settings` / `POST /api/settings` | Masked flags / save tokens + volume mode (live) |
| `WS /ws` | `hello` (binds optional `tab_id` for targeted downlink) → live `state` pushes with `revision`; every rejection returns `{type:"error", code, message, request_id?}`; successes with a `request_id` get `{type:"ack"}`; tabs receive `{type:"tab_control"}` |

Source kinds for stream differentiation: `spotify`, `browser`,
`youtubemusic` (+ `kind_label`, `origin`, per-source status/track).

## Reporting tabs (YouTube Music / browser media)

Open the media page with the reporter loaded (or `?tab=name` to pin an id).
It reports MediaSession state every 2s, actuates `TabControl` frames on the
local `<audio>/<video>` element, and unreports on close. Each page load gets
a unique tab id (stable base + load nonce), so cloned tabs never collapse
onto one source. Sources silent for >60s are swept (with broadcast).

## Verification

```bash
cargo test                                   # 45 Rust tests
node frontend/js/smoke.mjs                   # 13 JS wire-client checks
node --check frontend/app.js frontend/ytm-reporter.js frontend/js/rmc-client.js
```

Live checks used during development: health/state shape, `400` on
missing/out-of-range params, `ok:false` + `actuator_error` when actuating a
dead player, report→unreport round-trip, WS hello→state / toggle→ack /
bad-switch→typed-error.

## Project layout

```
ARCHITECTURE.md  authoritative design + subsystem map + wire spec
docs/STATUS.json gauntlet scores, per-round issues, verification log
src/            backend webserver protocol spotify mpris ytmusic sysaudio settings
frontend/        UI + tab reporter + canonical JS client
```

Module rule: each `src/*.rs` owns its area — cross-module changes go
through the integrator (`src/main.rs`, `ARCHITECTURE.md`).

## Known gaps (from the final architecture gate)

- No rate limiting / auth on LAN-open endpoints (trusted-network assumption).
- Tab control is queue-to-live-tab (no end-to-end ack); `next`/`prev` on bare
  media elements are no-ops.
- Spotify search needs a manually refreshed user token (~1h lifetime); no
  OAuth refresh flow. No Spotify Web API playback, only catalog search.
- `file://` artwork never renders in-browser (https art works); no art proxy.
- Tab ids churn on page reload (nonce identity); no DOM tests.
- `RMC_DEMO=1` seeds are fake presence — screenshots only, never parties.
