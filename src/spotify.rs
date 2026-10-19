//! spotify integration: owns the Spotify source adapter.
//! Linux: real MPRIS D-Bus observation + control of the Spotify desktop client.
//! Web API: token config reserved; HTTP path lands when OAuth is provided.

use crate::backend::{
    AdapterError, AdapterResult, ControlAction, MediaAdapter, MediaSource, PlaybackState,
    PlaybackStatus, SourceKind, TrackInfo,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Stable registry id for the local Spotify client.
pub const SPOTIFY_SOURCE_ID: &str = "spotify:local";

/// Config for future Spotify Web API use. Token optional — server runs without it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotifyConfig {
    pub client_id: Option<String>,
    pub access_token: Option<String>,
    /// MPRIS bus name to look for, default `org.mpris.MediaPlayer2.spotify`.
    pub mpris_bus_name: String,
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            client_id: None,
            access_token: None,
            mpris_bus_name: "org.mpris.MediaPlayer2.spotify".into(),
        }
    }
}

pub struct SpotifyAdapter {
    config: SpotifyConfig,
}

impl Default for SpotifyAdapter {
    fn default() -> Self {
        Self::new(SpotifyConfig::default())
    }
}

impl SpotifyAdapter {
    pub fn new(config: SpotifyConfig) -> Self {
        Self { config }
    }

    pub fn bus_name(&self) -> &str {
        &self.config.mpris_bus_name
    }

    pub fn has_web_token(&self) -> bool {
        self.config
            .access_token
            .as_ref()
            .is_some_and(|t| !t.is_empty())
    }

    /// Build a source snapshot for a detected local Spotify player.
    /// Live data comes from `poll_once`, which builds on this constructor so
    /// the two paths cannot diverge (id/kind/origin set here, enriched there).
    pub fn local_source(
        &self,
        status: PlaybackStatus,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> MediaSource {
        MediaSource {
            id: SPOTIFY_SOURCE_ID.into(),
            kind: SourceKind::Spotify,
            name: "Spotify Desktop".into(),
            origin: format!("mpris:{}", self.config.mpris_bus_name),
            state: PlaybackState {
                status,
                track: TrackInfo {
                    title: title.map(str::to_string),
                    artist: artist.map(str::to_string),
                    album: None,
                    duration_ms: None,
                    art_url: None,
                },
                ..PlaybackState::default()
            },
            last_seen: chrono::Utc::now(),
            actuator_error: None,
        }
    }
}

/// Find the player by well-known bus name. NOTE: mpris `find_by_name`
/// compares against Identity ("Spotify"), not the bus name — using it with a
/// bus name silently never matches. We match the bus directly instead,
/// tolerating MPRIS instance suffixes (`...spotify.instance1234`).
/// Bus-name predicate (pure, unit-tested): exact match or instance suffix.
fn bus_matches(bus: &str, configured: &str) -> bool {
    bus == configured || bus.starts_with(&format!("{configured}."))
}
fn find_player(finder: &mpris::PlayerFinder, bus_name: &str) -> Result<mpris::Player, String> {
    let players = finder
        .find_all()
        .map_err(|e| format!("player scan failed: {e}"))?;
    players
        .into_iter()
        .find(|p| bus_matches(p.bus_name(), bus_name))
        .ok_or_else(|| format!("spotify player not on bus: {bus_name}"))
}
/// Blocking MPRIS actuation. Call from a blocking thread (see `control_blocking`).
pub struct MprisSpotify {
    bus_name: String,
}

impl MprisSpotify {
    pub fn new(bus_name: impl Into<String>) -> Self {
        Self {
            bus_name: bus_name.into(),
        }
    }

    fn player(&self) -> Result<mpris::Player, String> {
        let finder = mpris::PlayerFinder::new().map_err(|e| format!("dbus unavailable: {e}"))?;
        find_player(&finder, self.bus_name.as_str())
    }

    pub fn control_blocking(&self, action: &ControlAction) -> Result<(), String> {
        // Volume goes through read-back verification (its own error type).
        if let ControlAction::Volume { level } = action {
            return self.set_volume_verified(*level);
        }
        let p = self.player()?;
        let r = match action {
            ControlAction::Play => p.play(),
            ControlAction::Pause => p.pause(),
            ControlAction::Toggle => p.play_pause(),
            ControlAction::Next => p.next(),
            ControlAction::Prev => p.previous(),
            ControlAction::Seek { position_ms } => {
                // MPRIS Seek is relative; compute offset from current position.
                let now = p.get_position().unwrap_or_default();
                let target = std::time::Duration::from_millis(*position_ms);
                if target >= now {
                    p.seek_forwards(&(target - now))
                } else {
                    p.seek_backwards(&(now - target))
                }
            }
            ControlAction::Volume { .. } => unreachable!("handled above"),
            ControlAction::OpenUri { uri } => {
                drop(p);
                return open_uri_blocking(&self.bus_name, uri);
            }
        };
        r.map_err(|e| format!("mpris call failed: {e}"))
    }

    /// Set volume, then read it back (with settle retries). Slow appliers
    /// (Spotify applies async — immediate read still shows the old value)
    /// converge within a few hundred ms; players that never apply (Chromium)
    /// exhaust the retries and report loudly instead of a silent no-effect ok.
    fn set_volume_verified(&self, level: u8) -> Result<(), String> {
        let want = level.min(100);
        let p = self.player()?;
        p.set_volume(want as f64 / 100.0)
            .map_err(|e| format!("mpris call failed: {e}"))?;
        for _ in 0..5 {
            let got = (p.get_volume().unwrap_or(-1.0) * 100.0).round() as i16;
            if got >= 0 && (got - want as i16).abs() <= 5 {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        let got = (p.get_volume().unwrap_or(-1.0) * 100.0).round() as i16;
        Err(format!(
            "player ignored volume change (wanted {want}, still {got}): volume is not remotely settable on this player — browser sources use whole-app system volume instead (see Settings → Browser volume)"
        ))
    }
}

impl MediaAdapter for MprisSpotify {
    fn source_id(&self) -> &str {
        SPOTIFY_SOURCE_ID
    }
    fn play(&self) -> AdapterResult {
        self.control_blocking(&ControlAction::Play)
            .map_err(AdapterError::Transport)
    }
    fn pause(&self) -> AdapterResult {
        self.control_blocking(&ControlAction::Pause)
            .map_err(AdapterError::Transport)
    }
    fn toggle(&self) -> AdapterResult {
        self.control_blocking(&ControlAction::Toggle)
            .map_err(AdapterError::Transport)
    }
    fn next(&self) -> AdapterResult {
        self.control_blocking(&ControlAction::Next)
            .map_err(AdapterError::Transport)
    }
    fn prev(&self) -> AdapterResult {
        self.control_blocking(&ControlAction::Prev)
            .map_err(AdapterError::Transport)
    }
    fn seek(&self, position_ms: u64) -> AdapterResult {
        self.control_blocking(&ControlAction::Seek { position_ms })
            .map_err(AdapterError::Transport)
    }
    fn set_volume(&self, level: u8) -> AdapterResult {
        self.control_blocking(&ControlAction::Volume { level })
            .map_err(AdapterError::Transport)
    }
    fn open_uri(&self, uri: &str) -> AdapterResult {
        open_uri_blocking(&self.bus_name, uri).map_err(AdapterError::Transport)
    }
}

/// MPRIS OpenUri via a raw blocking D-Bus call (the `mpris` crate exposes no
/// high-level wrapper). Used for play-by-URI (e.g. spotify:track:...).
pub fn open_uri_blocking(bus_name: &str, uri: &str) -> Result<(), String> {
    let conn =
        dbus::blocking::Connection::new_session().map_err(|e| format!("dbus unavailable: {e}"))?;
    let proxy = conn.with_proxy(
        bus_name,
        "/org/mpris/MediaPlayer2",
        std::time::Duration::from_secs(5),
    );
    proxy
        .method_call::<(), _, _, _>(
            "org.mpris.MediaPlayer2.Player",
            "OpenUri",
            (uri.to_string(),),
        )
        .map_err(|e| format!("open uri failed: {e}"))?;
    Ok(())
}

/// App credentials for Spotify client-credentials flow (public catalog:
/// search works without a user login). Preferred over pasting hourly tokens.
#[derive(Debug, Clone)]
pub struct SpotifyCreds {
    pub id: String,
    pub secret: String,
}

/// Mint a client-credentials token. Returns (token, expires_in_secs).
pub fn client_credentials_token(creds: &SpotifyCreds) -> Result<(String, u64), String> {
    if creds.id.trim().is_empty() || creds.secret.trim().is_empty() {
        return Err("empty Spotify app credentials".into());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("http client failed: {e}"))?;
    let resp = client
        .post("https://accounts.spotify.com/api/token")
        .basic_auth(creds.id.trim(), Some(creds.secret.trim()))
        .form(&[("grant_type", "client_credentials")])
        .send()
        .map_err(|e| format!("token request failed (offline?): {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "Spotify token HTTP {} (check id/secret)",
            resp.status()
        ));
    }
    let v: serde_json::Value = resp
        .json()
        .map_err(|e| format!("token parse failed: {e}"))?;
    let token = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "token response missing access_token".to_string())?
        .to_string();
    let expires = v.get("expires_in").and_then(|e| e.as_u64()).unwrap_or(3600);
    Ok((token, expires))
}

/// A track from Spotify Web API search. Needs a token: explicit
/// (`RMC_SPOTIFY_TOKEN`) or minted from app credentials — never fakes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebTrack {
    pub name: String,
    pub artists: String,
    pub album: String,
    pub uri: String,
    pub art_url: Option<String>,
}

/// Search tracks via api.spotify.com. Blocking (call from spawn_blocking).
/// Token how-to: open https://developer.spotify.com/console/get-search-item/ ,
/// press "Get Token" (any logged-in Spotify account), paste as RMC_SPOTIFY_TOKEN.
pub fn search_tracks(token: &str, query: &str) -> Result<Vec<WebTrack>, String> {
    if token.trim().is_empty() {
        return Err("no Spotify token: set RMC_SPOTIFY_TOKEN (see README)".into());
    }
    if query.trim().is_empty() {
        return Err("empty query".into());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("http client failed: {e}"))?;
    let resp = client
        .get("https://api.spotify.com/v1/search")
        .bearer_auth(token.trim())
        .query(&[("q", query.trim()), ("type", "track"), ("limit", "10")])
        .send()
        .map_err(|e| format!("search request failed (offline?): {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Spotify token invalid/expired: refresh RMC_SPOTIFY_TOKEN".into());
    }
    if !resp.status().is_success() {
        return Err(format!("Spotify search HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp
        .json()
        .map_err(|e| format!("search parse failed: {e}"))?;
    let items = v
        .pointer("/tracks/items")
        .and_then(|i| i.as_array())
        .ok_or_else(|| "unexpected search response shape".to_string())?;
    Ok(items
        .iter()
        .filter_map(|t| {
            Some(WebTrack {
                name: t.get("name")?.as_str()?.to_string(),
                artists: t
                    .get("artists")?
                    .as_array()?
                    .iter()
                    .filter_map(|a| a.get("name")?.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                album: t
                    .get("album")?
                    .get("name")?
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                uri: t.get("uri")?.as_str()?.to_string(),
                art_url: t
                    .pointer("/album/images/0/url")
                    .and_then(|u| u.as_str())
                    .map(str::to_string),
            })
        })
        .collect())
}
/// One blocking poll of the bus: Found(source) or Gone (player absent / bus down).
pub enum MonitorOutcome {
    Found(Box<MediaSource>),
    Gone,
}

pub fn poll_once(bus_name: &str) -> MonitorOutcome {
    let finder = match mpris::PlayerFinder::new() {
        Ok(f) => f,
        Err(_) => return MonitorOutcome::Gone, // no session bus (CI/headless): soft-gone
    };
    let player = match find_player(&finder, bus_name) {
        Ok(p) => p,
        Err(_) => return MonitorOutcome::Gone,
    };
    let status = match player.get_playback_status() {
        Ok(mpris::PlaybackStatus::Playing) => PlaybackStatus::Playing,
        Ok(mpris::PlaybackStatus::Paused) => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    };
    let md = player.get_metadata().ok();
    let title = md.as_ref().and_then(|m| m.title());
    let artist: Option<String> = md
        .as_ref()
        .and_then(|m| m.artists())
        .and_then(|a| a.first().map(|s| s.to_string()));
    // Single constructor path: base identity from local_source, enriched live.
    let adapter = SpotifyAdapter::new(SpotifyConfig {
        mpris_bus_name: bus_name.to_string(),
        ..SpotifyConfig::default()
    });
    let mut src = adapter.local_source(status, title, artist.as_deref());
    src.name = player.identity().to_string();
    src.state.track.album = md.as_ref().and_then(|m| m.album_name()).map(str::to_string);
    src.state.track.art_url = md.as_ref().and_then(|m| m.art_url()).map(str::to_string);
    src.state.track.duration_ms = md
        .as_ref()
        .and_then(|m| m.length())
        .map(|d| d.as_millis() as u64);
    src.state.position_ms = player
        .get_position()
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    src.state.volume = (player.get_volume().unwrap_or(0.7) * 100.0).round() as u8;
    src.state.volume = src.state.volume.min(100);
    src.state.updated_at = chrono::Utc::now();
    src.last_seen = chrono::Utc::now();
    MonitorOutcome::Found(Box::new(src))
}

/// Background presence/metadata loop. Upserts the registry while Spotify is
/// present; removes the seed/stale entry when the player is gone so the UI
/// never shows a ghost. Soft-fails without a bus.
pub async fn run_monitor(
    registry: Arc<RwLock<crate::backend::SourceRegistry>>,
    bus_name: String,
    interval_secs: u64,
) {
    let mut missing = 0u32;
    let mut seen = false;
    loop {
        let name = bus_name.clone();
        let outcome = tokio::task::spawn_blocking(move || poll_once(&name))
            .await
            .unwrap_or(MonitorOutcome::Gone);
        {
            let mut reg = registry.write().await;
            match outcome {
                MonitorOutcome::Found(src) => {
                    missing = 0;
                    seen = true;
                    let id = src.id.clone();
                    let state = src.state.clone();
                    if reg.get(&id).is_some() {
                        let _ = reg.update_state(&id, state);
                    } else {
                        reg.register(*src);
                    }
                }
                MonitorOutcome::Gone => {
                    missing += 1;
                    // Evict only a player we actually saw (presence latch): a
                    // quit Spotify vanishes, but demo seeds and never-present
                    // players are left alone — no fake presence either way.
                    if seen && missing >= 3 {
                        reg.remove(SPOTIFY_SOURCE_ID);
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_source_has_spotify_kind() {
        let a = SpotifyAdapter::default();
        let s = a.local_source(PlaybackStatus::Playing, Some("T"), Some("A"));
        assert_eq!(s.kind, SourceKind::Spotify);
        assert_eq!(s.id, SPOTIFY_SOURCE_ID);
    }

    #[test]
    fn adapter_dispatch_routes_all_actions_without_bus() {
        // Without a session bus every call must fail as Transport, never panic.
        let m = MprisSpotify::new("org.mpris.MediaPlayer2.spotify-testid-absent");
        for a in [
            ControlAction::Play,
            ControlAction::Pause,
            ControlAction::Toggle,
            ControlAction::Next,
            ControlAction::Prev,
            ControlAction::Seek { position_ms: 1000 },
            ControlAction::Volume { level: 50 },
            ControlAction::OpenUri {
                uri: "spotify:track:xxx".into(),
            },
        ] {
            assert!(
                m.dispatch(&a).is_err(),
                "expected transport error for {a:?}"
            );
        }
        assert_eq!(m.source_id(), SPOTIFY_SOURCE_ID);
    }

    #[test]
    fn poll_without_player_is_gone_not_panic() {
        match poll_once("org.mpris.MediaPlayer2.spotify-testid-absent") {
            MonitorOutcome::Gone => {}
            MonitorOutcome::Found(_) => panic!("unexpected player on test bus name"),
        }
    }

    #[test]
    fn bus_name_predicate() {
        assert!(bus_matches(
            "org.mpris.MediaPlayer2.spotify",
            "org.mpris.MediaPlayer2.spotify"
        ));
        assert!(bus_matches(
            "org.mpris.MediaPlayer2.spotify.instance123",
            "org.mpris.MediaPlayer2.spotify"
        ));
        assert!(!bus_matches(
            "org.mpris.MediaPlayer2.spotifyfoo",
            "org.mpris.MediaPlayer2.spotify"
        ));
        assert!(!bus_matches(
            "org.mpris.MediaPlayer2.brave",
            "org.mpris.MediaPlayer2.spotify"
        ));
    }

    #[test]
    fn poll_live_bus_reports_honest_invariants() {
        // Happy path on dev machines with Spotify running; vacuously green in CI.
        match poll_once("org.mpris.MediaPlayer2.spotify") {
            MonitorOutcome::Gone => println!("no live player on bus; skipped"),
            MonitorOutcome::Found(s) => {
                assert_eq!(s.id, SPOTIFY_SOURCE_ID);
                assert_eq!(s.kind, SourceKind::Spotify);
                assert!(s.origin.starts_with("mpris:"));
                assert!(!s.name.trim().is_empty());
                assert!(s.state.volume <= 100);
                println!("live: {} ({})", s.name, s.id);
            }
        }
    }
}
