//! webserver: Axum REST + WebSocket transport over backend::SourceRegistry.
//! Owns HTTP/WS only. Wire types come from rmc-client-connection (canonical).
//! Tab-targeted controls route to the WS connection that registered the tab.

use crate::backend::{AdapterResult, ControlAction, MediaSource, SourceRegistry};
use crate::protocol::{
    ClientMsg, ErrorCode, ServerMsg, SourceSnapshot, TabReport, PROTOCOL_VERSION,
};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{StatusCode, Uri},
    response::{Html, IntoResponse, Json},
    routing::{get, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{broadcast, mpsc, RwLock};

#[derive(rust_embed::RustEmbed)]
#[folder = "frontend/"]
struct EmbeddedFrontend;

/// Side-effect hook set by the server binary: actuate real players
/// (e.g. MPRIS, resolved from origin `mpris:{bus}`). Must be non-blocking-fast;
/// it runs on a blocking thread with a 2s bound and its Result decides
/// success/failure (no silent ok:true).
pub type ControlHook =
    Arc<dyn Fn(String, Option<String>, ControlAction) -> AdapterResult + Send + Sync>;

/// Artwork bytes cache (url -> bytes, content type, fetched-at).
pub type ArtCache =
    Arc<std::sync::Mutex<std::collections::HashMap<String, (Vec<u8>, String, std::time::Instant)>>>;

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<RwLock<SourceRegistry>>,
    pub tx: broadcast::Sender<ServerMsg>,
    /// tab_id -> direct sender for targeted downlink (TabControl frames).
    pub tabs: Arc<RwLock<HashMap<String, mpsc::UnboundedSender<ServerMsg>>>>,
    pub on_control: Option<ControlHook>,
    /// Persisted server settings (tokens, modes). Secrets never leave masked.
    pub settings: Arc<RwLock<crate::settings::ServerSettings>>,
    pub settings_path: std::path::PathBuf,
    /// HTTPS port when the TLS twin is up (None = http only).
    pub tls_port: Option<u16>,
    /// Cached minted token + expiry.
    pub token_cache: Arc<tokio::sync::Mutex<Option<(String, std::time::Instant)>>>,
    /// Artwork bytes cache. Id-scoped proxy only.
    pub art_cache: ArtCache,
    pub frontend_dir: String,
    pub version: String,
}

pub async fn snapshot(state: &AppState) -> ServerMsg {
    let reg = state.registry.read().await;
    ServerMsg::State {
        protocol_version: PROTOCOL_VERSION,
        revision: reg.revision(),
        active_id: reg.active_id().map(str::to_string),
        sources: reg
            .list()
            .into_iter()
            .map(SourceSnapshot::from_source)
            .collect(),
    }
}

pub fn broadcast_state(state: &AppState, push: &ServerMsg) {
    let _ = state.tx.send(push.clone());
}

fn err_msg(code: ErrorCode, message: String, request_id: Option<String>) -> ServerMsg {
    ServerMsg::Error {
        code,
        message,
        request_id,
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ActiveBody {
    pub id: String,
    pub request_id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ControlBody {
    pub id: Option<String>,
    pub action: String,
    pub position_ms: Option<u64>,
    pub level: Option<u8>,
    pub uri: Option<String>,
    pub request_id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ReportBody {
    pub tab: TabReport,
}

#[derive(Debug, serde::Deserialize)]
pub struct ArtQuery {
    pub src: String,
}

/// Artwork proxy, id-scoped (never an open relay): resolves the CURRENT art
/// of a known source and serves the bytes. Handles `file://` (Chromium dumps
/// thumbnails to /tmp — browsers can't load file:// from http pages) and
/// http(s), with size/type caps. Browsers cache by URL; art URLs are per-track.
async fn get_art(
    State(s): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ArtQuery>,
) -> impl IntoResponse {
    let url = {
        let reg = s.registry.read().await;
        match reg.get(q.src.trim()) {
            Some(src) => src.state.track.art_url.clone().unwrap_or_default(),
            None => {
                return (StatusCode::NOT_FOUND, "unknown source").into_response();
            }
        }
    };
    if url.is_empty() {
        return (StatusCode::NOT_FOUND, "no artwork").into_response();
    }
    // Cache (10 min TTL, 50 entries): track art is immutable per URL.
    if let Ok(g) = s.art_cache.lock() {
        if let Some((bytes, ctype, at)) = g.get(&url) {
            if at.elapsed() < std::time::Duration::from_secs(600) {
                return (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, ctype.clone())],
                    bytes.clone(),
                )
                    .into_response();
            }
        }
    }
    let fetched: Option<(Vec<u8>, String)> = if url.starts_with("file://") {
        let path = url.trim_start_matches("file://");
        std::fs::read(path).ok().map(|bytes| {
            let ct = sniff_image(
                &bytes,
                mime_guess::from_path(path)
                    .first_or_octet_stream()
                    .to_string(),
            );
            (bytes, ct)
        })
    } else if url.starts_with("http://") || url.starts_with("https://") {
        fetch_art_http(&url).await
    } else {
        None
    };
    match fetched {
        Some((bytes, ctype)) if bytes.len() <= 2 * 1024 * 1024 => {
            if let Ok(mut g) = s.art_cache.lock() {
                if g.len() > 50 {
                    g.clear();
                }
                g.insert(
                    url,
                    (bytes.clone(), ctype.clone(), std::time::Instant::now()),
                );
            }
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, ctype)],
                bytes,
            )
                .into_response()
        }
        Some(_) => (StatusCode::PAYLOAD_TOO_LARGE, "art too large").into_response(),
        None => (StatusCode::NOT_FOUND, "art unreachable").into_response(),
    }
}

/// Sniff image type from magic bytes (extensionless tmp thumbnails serve
/// as octet-stream otherwise, which some browsers refuse to render as <img>).
fn sniff_image(bytes: &[u8], fallback: String) -> String {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png".to_string()
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg".to_string()
    } else if bytes.starts_with(b"GIF8") {
        "image/gif".to_string()
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp".to_string()
    } else if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        "image/webm".to_string()
    } else {
        fallback
    }
}

async fn fetch_art_http(url: &str) -> Option<(Vec<u8>, String)> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;
        let resp = client.get(&url).send().ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        if !ctype.starts_with("image/") {
            return None;
        }
        let bytes = resp.bytes().ok()?.to_vec();
        Some((bytes, ctype))
    })
    .await
    .ok()?
}

#[derive(Debug, serde::Deserialize)]
pub struct SettingsBody {
    pub spotify_id: Option<String>,
    pub spotify_secret: Option<String>,
    pub spotify_token: Option<String>,
    pub volume_mode: Option<crate::settings::VolumeMode>,
    /// When true, wipe all three Spotify secrets.
    pub clear_tokens: Option<bool>,
}

fn settings_view(s: &crate::settings::ServerSettings) -> serde_json::Value {
    serde_json::json!({
        "spotify_id_set": s.spotify_id.as_ref().is_some_and(|v| !v.trim().is_empty()),
        "spotify_secret_set": s.spotify_secret.as_ref().is_some_and(|v| !v.trim().is_empty()),
        "spotify_token_set": s.spotify_token.as_ref().is_some_and(|v| !v.trim().is_empty()),
        "search_ready": s.has_search_creds(),
        "volume_mode": s.volume_mode,
    })
}

/// Secrets are never returned — only presence flags.
async fn get_settings(State(s): State<AppState>) -> impl IntoResponse {
    let st = s.settings.read().await;
    let mut v = settings_view(&st);
    v["settings_path"] = serde_json::json!(s.settings_path.display().to_string());
    Json(v)
}

async fn post_settings(
    State(s): State<AppState>,
    Json(b): Json<SettingsBody>,
) -> impl IntoResponse {
    let mut st = s.settings.write().await;
    if b.clear_tokens.unwrap_or(false) {
        st.spotify_id = None;
        st.spotify_secret = None;
        st.spotify_token = None;
    } else {
        // Some(non-empty) = set, Some(empty) = clear, None = keep.
        if let Some(v) = b.spotify_id {
            let v = v.trim().to_string();
            st.spotify_id = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = b.spotify_secret {
            let v = v.trim().to_string();
            st.spotify_secret = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = b.spotify_token {
            let v = v.trim().to_string();
            st.spotify_token = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(m) = b.volume_mode {
            st.volume_mode = m;
        }
    }
    // Any credential change invalidates the minted-token cache.
    *s.token_cache.lock().await = None;
    match st.save(&s.settings_path) {
        Ok(()) => (StatusCode::OK, Json(settings_view(&st))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "code": "internal", "error": e })),
        )
            .into_response(),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct SearchBody {
    pub q: String,
}

/// Spotify catalog search. Token sources, in order: explicit
/// RMC_SPOTIFY_TOKEN, then minted+cached from RMC_SPOTIFY_ID/SECRET.
/// Without either (or on failure) returns an honest error — never fakes.
async fn post_search(State(s): State<AppState>, Json(b): Json<SearchBody>) -> impl IntoResponse {
    let setup_error = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "ok": false,
            "code": "no_token",
            "error": "Spotify search needs credentials: set RMC_SPOTIFY_ID + RMC_SPOTIFY_SECRET (auto-refresh, recommended) or RMC_SPOTIFY_TOKEN. See README.",
        })),
    );
    let token: Option<String> = {
        let st = s.settings.read().await;
        if let Some(t) = st.spotify_token.clone().filter(|t| !t.trim().is_empty()) {
            Some(t)
        } else if st.spotify_id.clone().is_some_and(|v| !v.trim().is_empty())
            && st
                .spotify_secret
                .clone()
                .is_some_and(|v| !v.trim().is_empty())
        {
            let creds = crate::spotify::SpotifyCreds {
                id: st.spotify_id.clone().unwrap(),
                secret: st.spotify_secret.clone().unwrap(),
            };
            drop(st);
            // Cached minted token with 60s expiry skew; refresh blocking.
            let cached = s.token_cache.lock().await.clone();
            match cached {
                Some((tok, exp))
                    if exp > std::time::Instant::now() + std::time::Duration::from_secs(60) =>
                {
                    Some(tok)
                }
                _ => {
                    let out = tokio::task::spawn_blocking(move || {
                        crate::spotify::client_credentials_token(&creds)
                    })
                    .await;
                    match out {
                        Ok(Ok((tok, expires))) => {
                            let exp =
                                std::time::Instant::now() + std::time::Duration::from_secs(expires);
                            *s.token_cache.lock().await = Some((tok.clone(), exp));
                            Some(tok)
                        }
                        Ok(Err(e)) => {
                            return (
                                StatusCode::BAD_GATEWAY,
                                Json(
                                    serde_json::json!({ "ok": false, "code": "token_failed", "error": e }),
                                ),
                            )
                        }
                        Err(e) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(
                                    serde_json::json!({ "ok": false, "code": "internal", "error": format!("token task failed: {e}") }),
                                ),
                            )
                        }
                    }
                }
            }
        } else {
            None
        }
    };
    let Some(token) = token else {
        return setup_error;
    };
    let q = b.q.clone();
    let out = tokio::task::spawn_blocking(move || crate::spotify::search_tracks(&token, &q)).await;
    match out {
        Ok(Ok(tracks)) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "tracks": tracks })),
        ),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "ok": false, "code": "search_failed", "error": e })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                serde_json::json!({ "ok": false, "code": "internal", "error": format!("search task failed: {e}") }),
            ),
        ),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct PlayBody {
    pub id: Option<String>,
    pub uri: String,
    pub request_id: Option<String>,
}

/// Play a URI (e.g. from search) on the target (default: active) source.
/// Flows through the normal honest control path (OpenUri + reconcile).
async fn post_play(State(s): State<AppState>, Json(b): Json<PlayBody>) -> impl IntoResponse {
    if b.uri.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "code": "bad_param", "error": "uri required" })),
        );
    }
    match apply_control_to(&s, b.id, ControlAction::OpenUri { uri: b.uri }).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "request_id": b.request_id })),
        ),
        Err((code, msg)) => {
            let status = match code {
                ErrorCode::UnknownSource | ErrorCode::NoActiveSource => StatusCode::NOT_FOUND,
                _ => StatusCode::BAD_REQUEST,
            };
            (
                status,
                Json(
                    serde_json::json!({ "ok": false, "code": code_str(&code), "error": msg, "request_id": b.request_id }),
                ),
            )
        }
    }
}

/// Strict parsing shared by REST and WS: unknown action or missing/out-of-range
/// params are errors, never silent defaults (no seek-to-0 / volume-to-70).
fn parse_action(
    action: &str,
    position_ms: Option<u64>,
    level: Option<u8>,
    uri: Option<String>,
) -> Result<ControlAction, (ErrorCode, String)> {
    match action {
        "play" => Ok(ControlAction::Play),
        "pause" => Ok(ControlAction::Pause),
        "toggle" => Ok(ControlAction::Toggle),
        "next" => Ok(ControlAction::Next),
        "prev" => Ok(ControlAction::Prev),
        "seek" => match position_ms {
            Some(p) => Ok(ControlAction::Seek { position_ms: p }),
            None => Err((ErrorCode::BadParam, "seek requires position_ms".into())),
        },
        "volume" => match level {
            Some(l) if l <= 100 => Ok(ControlAction::Volume { level: l }),
            Some(l) => Err((
                ErrorCode::BadParam,
                format!("volume out of range 0-100: {l}"),
            )),
            None => Err((ErrorCode::BadParam, "volume requires level 0-100".into())),
        },
        "open_uri" => match uri {
            Some(u) if !u.trim().is_empty() => Ok(ControlAction::OpenUri { uri: u }),
            _ => Err((ErrorCode::BadParam, "open_uri requires uri".into())),
        },
        other => Err((ErrorCode::UnknownAction, format!("unknown action: {other}"))),
    }
}

/// Map registry errors to wire codes in one place (REST and WS share it).
fn reg_err(e: &crate::backend::RegistryError) -> (ErrorCode, String) {
    let code = match e {
        crate::backend::RegistryError::NotFound(_) => ErrorCode::UnknownSource,
        crate::backend::RegistryError::Empty => ErrorCode::NoActiveSource,
        crate::backend::RegistryError::Stale { .. } => ErrorCode::Stale,
        crate::backend::RegistryError::InvalidId => ErrorCode::BadParam,
    };
    (code, e.to_string())
}

/// Build a state push from a locked registry. Caller holds the write guard.
fn push_locked(reg: &SourceRegistry) -> ServerMsg {
    ServerMsg::State {
        protocol_version: PROTOCOL_VERSION,
        revision: reg.revision(),
        active_id: reg.active_id().map(str::to_string),
        sources: reg
            .list()
            .into_iter()
            .map(SourceSnapshot::from_source)
            .collect(),
    }
}

/// Mark a source degraded and rebroadcast (reconcile path for failed actuation).
async fn mark_and_rebroadcast(s: &AppState, tid: &str, message: String) {
    let mut reg = s.registry.write().await;
    reg.mark_actuator_error(tid, message);
    let push = push_locked(&reg);
    drop(reg);
    broadcast_state(s, &push);
}
/// Core control application used by REST, WS, and tab reports alike:
/// optimistic cache mutate + broadcast, then bounded actuation with reconcile.
/// A failed actuation marks the source degraded, rebroadcasts, and returns
/// Err — callers never report ok:true for a dead player.
async fn apply_control_to(
    s: &AppState,
    target: Option<String>,
    action: ControlAction,
) -> Result<(), (ErrorCode, String)> {
    let mut reg = s.registry.write().await;
    let tid = match target {
        Some(t) => t,
        None => reg
            .active_id()
            .map(str::to_string)
            .ok_or((ErrorCode::NoActiveSource, "no active source".into()))?,
    };
    if reg.get(&tid).is_none() {
        return Err((ErrorCode::UnknownSource, format!("source not found: {tid}")));
    }
    // Map lookup with owned key.
    let origin = reg.get(&tid).map(|src| src.origin.clone());
    reg.apply_control(&tid, &action).map_err(|e| reg_err(&e))?;
    let push = push_locked(&reg);
    drop(reg);
    broadcast_state(s, &push);
    let is_tab_origin = origin.as_ref().is_some_and(|o| o.starts_with("tab:"));
    // Reporter tabs: targeted downlink to the owning connection. No binding
    // means no actuating endpoint: honest failure.
    if is_tab_origin {
        let tab = tid
            .split_once(':')
            .map(|(_, t)| t.to_string())
            .unwrap_or_default();
        let frame = {
            let tabs = s.tabs.read().await;
            tabs.get(&tab).map(|tx| {
                let (action_str, position_ms, level) = match &action {
                    ControlAction::Play => ("play", None, None),
                    ControlAction::Pause => ("pause", None, None),
                    ControlAction::Toggle => ("toggle", None, None),
                    ControlAction::Next => ("next", None, None),
                    ControlAction::Prev => ("prev", None, None),
                    ControlAction::Seek { position_ms } => ("seek", Some(*position_ms), None),
                    ControlAction::Volume { level } => ("volume", None, Some(*level)),
                    ControlAction::OpenUri { .. } => ("open_uri", None, None),
                };
                (
                    tx.clone(),
                    ServerMsg::TabControl {
                        tab_id: tab.clone(),
                        action: action_str.into(),
                        position_ms,
                        level,
                    },
                )
            })
        };
        match frame {
            Some((tx, f)) => {
                // Queued to a live tab; its next report reconciles truth.
                let _ = tx.send(f);
                return Ok(());
            }
            None => {
                let msg = format!("no live tab connection for {tid}");
                mark_and_rebroadcast(s, &tid, msg.clone()).await;
                return Err((ErrorCode::Internal, msg));
            }
        }
    }
    // Native actuation (MPRIS etc.) off the transport task with a 2s bound.
    if let Some(hook) = s.on_control.clone() {
        let tid2 = tid.clone();
        let action2 = action.clone();
        let origin2 = origin.clone();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::task::spawn_blocking(move || hook(tid2, origin2, action2)),
        )
        .await;
        let failure: Option<String> = match outcome {
            Ok(Ok(Ok(()))) => None,
            Ok(Ok(Err(e))) => Some(e.to_string()),
            Ok(Err(join)) => Some(format!("actuator task failed: {join}")),
            Err(_) => Some("actuator timed out after 2s".into()),
        };
        if let Some(msg) = failure {
            mark_and_rebroadcast(s, &tid, msg.clone()).await;
            return Err((ErrorCode::Internal, msg));
        }
    }
    Ok(())
}

/// Upsert a tab report into the registry (create or refresh), then broadcast.
async fn ingest_report(s: &AppState, tab: TabReport) -> ServerMsg {
    if tab.tab_id.trim().is_empty() {
        return err_msg(ErrorCode::BadParam, "tab_id required".into(), None);
    }
    let source: MediaSource = crate::ytmusic::YouTubeMusicAdapter::source_for_tab(&tab);
    let mut reg = s.registry.write().await;
    let id = source.id.clone();
    let state = source.state.clone();
    if reg.get(&id).is_some() {
        // Refresh path honors staleness: out-of-order heartbeats can't regress.
        let _ = reg.update_state(&id, state);
        // Touch name in case the page title changed (stable prefix kept).
        if let Some(existing) = reg.get(&id) {
            let mut merged = existing.clone();
            merged.name = source.name;
            merged.last_seen = chrono::Utc::now();
            reg.register(merged);
        }
    } else {
        reg.register(source);
    }
    let push = ServerMsg::State {
        protocol_version: PROTOCOL_VERSION,
        revision: reg.revision(),
        active_id: reg.active_id().map(str::to_string),
        sources: reg
            .list()
            .into_iter()
            .map(SourceSnapshot::from_source)
            .collect(),
    };
    drop(reg);
    broadcast_state(s, &push);
    push
}

async fn health(State(s): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "version": s.version,
        "protocol_version": PROTOCOL_VERSION,
        "tls": s.tls_port.is_some(),
        "tls_port": s.tls_port,
    }))
}

async fn get_sources(State(s): State<AppState>) -> impl IntoResponse {
    Json(snapshot(&s).await)
}

/// Wire code string via serde so REST and WS agree (never Debug+lowercase).
fn code_str(code: &ErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "internal".into())
}

async fn post_active(State(s): State<AppState>, Json(b): Json<ActiveBody>) -> impl IntoResponse {
    let id = b.id.trim().to_string();
    if id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "code": "bad_param", "error": "id required" })),
        );
    }
    let mut reg = s.registry.write().await;
    match reg.set_active(&id) {
        Ok(()) => {
            let push = push_locked(&reg);
            drop(reg);
            broadcast_state(&s, &push);
            (
                StatusCode::OK,
                Json(
                    serde_json::json!({ "ok": true, "active_id": id, "request_id": b.request_id }),
                ),
            )
        }
        Err(e) => {
            let (code, msg) = reg_err(&e);
            let status = match code {
                ErrorCode::UnknownSource | ErrorCode::NoActiveSource => StatusCode::NOT_FOUND,
                ErrorCode::Stale => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            (
                status,
                Json(
                    serde_json::json!({ "ok": false, "code": code_str(&code), "error": msg, "request_id": b.request_id }),
                ),
            )
        }
    }
}

async fn post_control(State(s): State<AppState>, Json(b): Json<ControlBody>) -> impl IntoResponse {
    let action = match parse_action(&b.action, b.position_ms, b.level, b.uri.clone()) {
        Ok(a) => a,
        Err((code, msg)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({ "ok": false, "code": code_str(&code), "error": msg, "request_id": b.request_id }),
                ),
            )
        }
    };
    match apply_control_to(&s, b.id, action).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "request_id": b.request_id })),
        ),
        Err((code, msg)) => {
            let status = match code {
                ErrorCode::UnknownSource | ErrorCode::NoActiveSource => StatusCode::NOT_FOUND,
                _ => StatusCode::BAD_REQUEST,
            };
            (
                status,
                Json(
                    serde_json::json!({ "ok": false, "code": code_str(&code), "error": msg, "request_id": b.request_id }),
                ),
            )
        }
    }
}

async fn post_report(State(s): State<AppState>, Json(b): Json<ReportBody>) -> impl IntoResponse {
    let push = ingest_report(&s, b.tab).await;
    if matches!(push, ServerMsg::Error { .. }) {
        (StatusCode::BAD_REQUEST, Json(push)).into_response()
    } else {
        (StatusCode::OK, Json(push)).into_response()
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct UnreportBody {
    pub tab_id: String,
}

/// Explicit tab close (pagehide beacon). Removes both kind spellings for the
/// tab; the 60s heartbeat-expiry sweeper backstops crashed tabs.
async fn post_unreport(
    State(s): State<AppState>,
    Json(b): Json<UnreportBody>,
) -> impl IntoResponse {
    let tab = b.tab_id.trim().to_string();
    if tab.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({ "ok": false, "code": code_str(&ErrorCode::BadParam), "error": "tab_id required" }),
            ),
        );
    }
    // Drop any stale direct-send binding for this tab too.
    s.tabs.write().await.remove(&tab);
    let mut reg = s.registry.write().await;
    let mut gone = 0;
    for id in [format!("ytmusic:{tab}"), format!("browser:{tab}")] {
        if reg.remove(&id) {
            gone += 1;
        }
    }
    if gone == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(
                serde_json::json!({ "ok": false, "code": code_str(&ErrorCode::UnknownSource), "error": "no such tab" }),
            ),
        );
    }
    let push = ServerMsg::State {
        protocol_version: PROTOCOL_VERSION,
        revision: reg.revision(),
        active_id: reg.active_id().map(str::to_string),
        sources: reg
            .list()
            .into_iter()
            .map(SourceSnapshot::from_source)
            .collect(),
    };
    drop(reg);
    broadcast_state(&s, &push);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "removed": gone })),
    )
}

async fn ws_handler(State(s): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_loop(socket, s))
}

async fn send_frame(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: &ServerMsg,
) -> bool {
    match serde_json::to_string(msg) {
        Ok(text) => sender.send(Message::Text(text)).await.is_ok(),
        Err(_) => true,
    }
}

async fn ws_loop(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    // Keepalive: server pings every 30s; connections idle >90s are dropped.
    // Slow broadcast consumers that lag get a full resnapshot (never stale).
    let snap = snapshot(&state).await;
    if !send_frame(&mut sender, &snap).await {
        return;
    }
    let mut rx = state.tx.subscribe();
    // Direct queue for this connection's targeted tab frames.
    let (direct_tx, mut direct_rx) = mpsc::unbounded_channel::<ServerMsg>();
    let mut owned_tab: Option<String> = None;
    let mut ping_tick = tokio::time::interval(std::time::Duration::from_secs(30));
    ping_tick.tick().await; // consume immediate first tick
    let mut last_active = tokio::time::Instant::now();

    loop {
        tokio::select! {
            _ = ping_tick.tick() => {
                if last_active.elapsed() > std::time::Duration::from_secs(90) {
                    break; // dead peer: no frames in 3 ping windows
                }
                if sender.send(Message::Ping(b"rmc".to_vec())).await.is_err() {
                    break;
                }
            }
            msg = receiver.next() => {
                let Some(Ok(msg)) = msg else { break };
                last_active = tokio::time::Instant::now();
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => break,
                    Message::Ping(d) => {
                        let _ = sender.send(Message::Pong(d)).await;
                        continue;
                    }
                    Message::Pong(_) => continue, // peer answers our pings
                    _ => continue,
                };
                let input: Result<ClientMsg, _> = serde_json::from_str(&text);
                let input = match input {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = send_frame(&mut sender, &err_msg(ErrorCode::BadParam, format!("malformed frame: {e}"), None)).await;
                        continue;
                    }
                };
                match input {
                    ClientMsg::Hello { protocol_version, tab_id, .. } => {
                        if let Some(v) = protocol_version {
                            if v != PROTOCOL_VERSION {
                                let _ = send_frame(&mut sender, &err_msg(ErrorCode::VersionMismatch,
                                    format!("server speaks protocol {PROTOCOL_VERSION}, client sent {v}"), None)).await;
                            }
                        }
                        if let Some(tab) = tab_id {
                            // Bind this connection for targeted downlink.
                            state.tabs.write().await.insert(tab.clone(), direct_tx.clone());
                            owned_tab = Some(tab);
                        }
                        let snap = snapshot(&state).await;
                        if !send_frame(&mut sender, &snap).await { break; }
                    }
                    ClientMsg::Switch { id, request_id } => {
                        let id = id.trim().to_string();
                        if id.is_empty() {
                            let _ = send_frame(&mut sender, &err_msg(ErrorCode::BadParam, "id required".into(), request_id)).await;
                            continue;
                        }
                        let mut reg = state.registry.write().await;
                        match reg.set_active(&id) {
                            Ok(()) => {
                                let push = push_locked(&reg);
                                drop(reg);
                                broadcast_state(&state, &push);
                                if let Some(rid) = request_id {
                                    let _ = send_frame(&mut sender, &ServerMsg::Ack { request_id: rid }).await;
                                }
                            }
                            Err(e) => {
                                let (code, msg) = reg_err(&e);
                                drop(reg);
                                let _ = send_frame(&mut sender, &err_msg(code, msg, request_id)).await;
                            }
                        }
                    }
                    ClientMsg::Control { id, action, position_ms, level, uri, request_id } => {
                        match parse_action(&action, position_ms, level, uri) {
                            Ok(a) => {
                                if let Err((code, msg)) = apply_control_to(&state, id, a).await {
                                    let _ = send_frame(&mut sender, &err_msg(code, msg, request_id.clone())).await;
                                } else if let Some(rid) = request_id {
                                    let _ = send_frame(&mut sender, &ServerMsg::Ack { request_id: rid }).await;
                                }
                            }
                            Err((code, msg)) => {
                                let _ = send_frame(&mut sender, &err_msg(code, msg, request_id)).await;
                            }
                        }
                    }
                    ClientMsg::Report { tab, .. } => {
                        let push = ingest_report(&state, tab).await;
                        // Echo the resulting state only to the reporter (broadcast already went out).
                        if matches!(push, ServerMsg::Error { .. }) {
                            let _ = send_frame(&mut sender, &push).await;
                        }
                    }
                }
            }
            push = rx.recv() => {
                match push {
                    Ok(p) => {
                        if !send_frame(&mut sender, &p).await { break; }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Slow client missed frames: resnapshot full truth, never stay stale.
                        let snap = snapshot(&state).await;
                        if !send_frame(&mut sender, &snap).await { break; }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            direct = direct_rx.recv() => {
                if let Some(frame) = direct {
                    if !send_frame(&mut sender, &frame).await { break; }
                } else { break; }
            }
        }
    }
    // Release tab binding owned by this connection, and unreport its sources:
    // a clean WS close means the tab is gone (beacon miss backstop is the sweeper).
    if let Some(tab) = owned_tab {
        let mut tabs = state.tabs.write().await;
        // Only remove if it still points at our sender (a newer conn may have replaced it).
        let stale = tabs.get(&tab).is_some_and(|tx| tx.same_channel(&direct_tx));
        if stale {
            tabs.remove(&tab);
        }
        drop(tabs);
        if stale {
            let mut reg = state.registry.write().await;
            let mut gone = false;
            for id in [format!("ytmusic:{tab}"), format!("browser:{tab}")] {
                gone |= reg.remove(&id);
            }
            if gone {
                let push = push_locked(&reg);
                drop(reg);
                broadcast_state(&state, &push);
            }
        }
    }
}

/// Heartbeat-expiry sweeper, owned by this crate (not the binary): every 15s,
/// drop tab-originated sources silent for >60s and broadcast the new truth.
pub fn spawn_sweeper(state: &AppState) {
    let s = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            let mut reg = s.registry.write().await;
            let now = chrono::Utc::now();
            let dead: Vec<String> = reg
                .list()
                .into_iter()
                .filter(|src| {
                    src.origin.starts_with("tab:") && (now - src.last_seen).num_seconds() > 60
                })
                .map(|src| src.id.clone())
                .collect();
            if dead.is_empty() {
                continue;
            }
            for id in dead {
                reg.remove(&id);
            }
            let push = push_locked(&reg);
            drop(reg);
            broadcast_state(&s, &push);
        }
    });
}

fn mime_for(path: &str) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}

async fn index(State(s): State<AppState>) -> impl IntoResponse {
    // Dev override: serve from disk when the directory exists (single-binary
    // builds serve the compile-time embedded copy instead).
    let path = format!("{}/index.html", s.frontend_dir);
    match tokio::fs::read_to_string(&path).await {
        Ok(html) => Html(html).into_response(),
        Err(_) => match EmbeddedFrontend::get("index.html") {
            Some(f) => (
                StatusCode::OK,
                [("content-type", mime_for("index.html"))],
                f.data.to_vec(),
            )
                .into_response(),
            None => (
                StatusCode::OK,
                Html("<h1>RMC</h1><p>frontend/ missing — API at /api/sources</p>".to_string()),
            )
                .into_response(),
        },
    }
}

/// Fallback static serving. Prefers the on-disk dir (dev), else embedded
/// assets (single binary). Unknown paths 404.
async fn static_fallback(State(s): State<AppState>, uri: Uri) -> impl IntoResponse {
    let raw = uri.path().trim_start_matches('/');
    let rel = if raw.is_empty() { "index.html" } else { raw };
    if rel.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    }
    let disk = format!("{}/{}", s.frontend_dir, rel);
    if let Ok(bytes) = tokio::fs::read(&disk).await {
        return (StatusCode::OK, [("content-type", mime_for(rel))], bytes).into_response();
    }
    match EmbeddedFrontend::get(rel) {
        Some(f) => (
            StatusCode::OK,
            [("content-type", mime_for(rel))],
            f.data.to_vec(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/sources", get(get_sources))
        .route("/api/active", post(post_active))
        .route("/api/control", post(post_control))
        .route("/api/search", post(post_search))
        .route("/api/play", post(post_play))
        .route("/api/report", post(post_report))
        .route("/api/unreport", post(post_unreport))
        .route("/api/art", get(get_art))
        .route(
            "/api/settings",
            get(get_settings).merge(post(post_settings)),
        )
        .route("/ws", get(ws_handler))
        .fallback(static_fallback)
        // LAN-open control endpoint: cap request bodies (report/control JSON is tiny).
        .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
        .with_state(state)
}

pub fn new_state(frontend_dir: String, version: String) -> AppState {
    let (tx, _) = broadcast::channel(64);
    AppState {
        registry: Arc::new(RwLock::new(SourceRegistry::new())),
        tx,
        tabs: Arc::new(RwLock::new(HashMap::new())),
        on_control: None,
        settings: Arc::new(RwLock::new(crate::settings::ServerSettings::default())),
        settings_path: std::path::PathBuf::from("settings.json"),
        tls_port: None,
        token_cache: Arc::new(tokio::sync::Mutex::new(None)),
        art_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        frontend_dir,
        version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_action_matrix() {
        assert_eq!(
            parse_action("play", None, None, None).unwrap(),
            ControlAction::Play
        );
        assert_eq!(
            parse_action("pause", None, None, None).unwrap(),
            ControlAction::Pause
        );
        assert_eq!(
            parse_action("toggle", None, None, None).unwrap(),
            ControlAction::Toggle
        );
        assert_eq!(
            parse_action("next", None, None, None).unwrap(),
            ControlAction::Next
        );
        assert_eq!(
            parse_action("prev", None, None, None).unwrap(),
            ControlAction::Prev
        );
        assert_eq!(
            parse_action("seek", Some(5000), None, None).unwrap(),
            ControlAction::Seek { position_ms: 5000 }
        );
        assert_eq!(
            parse_action("volume", None, Some(33), None).unwrap(),
            ControlAction::Volume { level: 33 }
        );
        assert_eq!(
            parse_action("open_uri", None, None, Some("spotify:track:x".into())).unwrap(),
            ControlAction::OpenUri {
                uri: "spotify:track:x".into()
            }
        );
    }

    #[test]
    fn parse_action_rejects_strict() {
        // Missing params are BadParam, never silent defaults.
        assert!(matches!(
            parse_action("seek", None, None, None),
            Err((ErrorCode::BadParam, _))
        ));
        assert!(matches!(
            parse_action("volume", None, None, None),
            Err((ErrorCode::BadParam, _))
        ));
        assert!(matches!(
            parse_action("volume", None, Some(250), None),
            Err((ErrorCode::BadParam, _))
        ));
        assert!(matches!(
            parse_action("dance", None, None, None),
            Err((ErrorCode::UnknownAction, _))
        ));
        assert!(matches!(
            parse_action("open_uri", None, None, None),
            Err((ErrorCode::BadParam, _))
        ));
    }

    #[test]
    fn rest_codes_match_wire_snake_case() {
        assert_eq!(code_str(&ErrorCode::UnknownSource), "unknown_source");
        assert_eq!(code_str(&ErrorCode::BadParam), "bad_param");
        assert_eq!(code_str(&ErrorCode::UnknownAction), "unknown_action");
        assert_eq!(code_str(&ErrorCode::NoActiveSource), "no_active_source");
    }

    fn test_source(id: &str) -> MediaSource {
        MediaSource {
            id: id.into(),
            kind: crate::backend::SourceKind::Spotify,
            name: id.into(),
            origin: "test".into(),
            state: crate::backend::PlaybackState::default(),
            last_seen: chrono::Utc::now(),
            actuator_error: None,
        }
    }

    #[tokio::test]
    async fn failed_actuation_marks_degraded_and_errors() {
        // Repro gate: dead player must NOT yield ok:true with a clean cache.
        let mut state = new_state("frontend".into(), "test".into());
        {
            let mut reg = state.registry.write().await;
            reg.register(test_source("spotify:local"));
        }
        let failing: ControlHook =
            Arc::new(|_, _, _| Err(crate::backend::AdapterError::Unavailable("bus down".into())));
        state.on_control = Some(failing);
        let r = apply_control_to(&state, Some("spotify:local".into()), ControlAction::Toggle).await;
        assert!(matches!(r, Err((ErrorCode::Internal, _))));
        let reg = state.registry.read().await;
        assert!(reg.get("spotify:local").unwrap().actuator_error.is_some());
    }

    #[tokio::test]
    async fn successful_actuation_stays_clean() {
        let mut state = new_state("frontend".into(), "test".into());
        {
            let mut reg = state.registry.write().await;
            reg.register(test_source("spotify:local"));
        }
        let ok: ControlHook = Arc::new(|_, _, _| Ok(()));
        state.on_control = Some(ok);
        let r = apply_control_to(&state, Some("spotify:local".into()), ControlAction::Toggle).await;
        assert!(r.is_ok());
        let reg = state.registry.read().await;
        assert_eq!(reg.get("spotify:local").unwrap().actuator_error, None);
    }

    #[test]
    fn sniff_magic_bytes() {
        assert_eq!(
            sniff_image(&[0x89, b'P', b'N', b'G', 0], "x".into()),
            "image/png"
        );
        assert_eq!(
            sniff_image(&[0xFF, 0xD8, 0xFF, 0], "x".into()),
            "image/jpeg"
        );
        assert_eq!(sniff_image(b"GIF89a....", "x".into()), "image/gif");
        assert_eq!(sniff_image(b"RIFF1234WEBP", "x".into()), "image/webp");
        assert_eq!(sniff_image(b"hello", "fallback/x".into()), "fallback/x");
    }

    #[tokio::test]
    async fn art_proxy_rejects_unknown_and_missing() {
        use axum::extract::Query;
        let state = new_state("frontend".into(), "test".into());
        {
            let mut reg = state.registry.write().await;
            let mut s = test_source("spotify:local");
            s.state.track.art_url = Some("file:///nonexistent-art-xyz.png".into());
            reg.register(s);
        }
        let r = get_art(
            State(state.clone()),
            Query(crate::webserver::ArtQuery { src: "nope".into() }),
        )
        .await
        .into_response();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = get_art(
            State(state.clone()),
            Query(crate::webserver::ArtQuery {
                src: "spotify:local".into(),
            }),
        )
        .await
        .into_response();
        assert_eq!(r.status(), StatusCode::NOT_FOUND); // missing file, not a leak
    }

    #[tokio::test]
    async fn control_to_bindingless_tab_fails_honest() {
        let state = new_state("frontend".into(), "test".into());
        {
            let mut reg = state.registry.write().await;
            let mut s = test_source("ytmusic:lonely");
            s.kind = crate::backend::SourceKind::YouTubeMusic;
            s.origin = "tab:lonely".into();
            reg.register(s);
        }
        let r = apply_control_to(&state, Some("ytmusic:lonely".into()), ControlAction::Play).await;
        assert!(r.is_err());
    }
}
