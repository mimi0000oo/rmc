//! mpris integration: watches ALL desktop media players on the MPRIS bus
//! (Spotify, Brave/Chrome/Firefox, VLC, ...), not just Spotify.
//! Browser playback is detected here with zero reporter needed; the
//! tab reporter (ytm-reporter.js) remains the path for URL-aware YTM
//! detection and per-tab actuation.

use crate::backend::{ControlAction, MediaSource, PlaybackState, PlaybackStatus, SourceKind};
use crate::protocol::{ServerMsg, SourceSnapshot, PROTOCOL_VERSION};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

const PREFIX: &str = "org.mpris.MediaPlayer2.";

fn slugify(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "x".into()
    } else {
        s
    }
}

/// Classify a bus name to (kind, id-slug). None = skip (proxies like playerctld
/// mirror the active player and would duplicate sources).
fn classify(bus: &str) -> Option<(SourceKind, String)> {
    let rest = bus.strip_prefix(PREFIX)?;
    let part = rest.split('.').next().unwrap_or(rest).to_lowercase();
    if part == "playerctld" {
        return None;
    }
    if part == "spotify" {
        return Some((SourceKind::Spotify, "local".into()));
    }
    let slug = slugify(&part);
    match part.as_str() {
        "chromium" | "chrome" | "brave" | "firefox" | "edge" | "opera" | "vivaldi" => {
            Some((SourceKind::Browser, slug))
        }
        _ if rest.contains("spotify") => Some((SourceKind::Spotify, slug)),
        _ => Some((SourceKind::System, slug)),
    }
}

fn status_of(p: &mpris::Player) -> PlaybackStatus {
    match p.get_playback_status() {
        Ok(mpris::PlaybackStatus::Playing) => PlaybackStatus::Playing,
        Ok(mpris::PlaybackStatus::Paused) => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    }
}

fn source_for(bus: &str, kind: &SourceKind, slug: &str, p: &mpris::Player) -> MediaSource {
    let md = p.get_metadata().ok();
    let title = md.as_ref().and_then(|m| m.title()).map(str::to_string);
    let artist = md
        .as_ref()
        .and_then(|m| m.artists())
        .and_then(|a| a.first().map(|s| s.to_string()));
    let mut volume = (p.get_volume().unwrap_or(0.7) * 100.0).round() as u8;
    volume = volume.min(100);
    // Browsers lie about MPRIS volume (accepted, never applied): report the
    // real whole-app sink volume instead so the UI slider tells the truth.
    if *kind == SourceKind::Browser {
        if let Ok(v) = crate::sysaudio::app_volume(slug) {
            volume = v;
        }
    }
    let mut state = PlaybackState {
        status: status_of(p),
        track: crate::backend::TrackInfo {
            title,
            artist,
            album: md.as_ref().and_then(|m| m.album_name()).map(str::to_string),
            duration_ms: md
                .as_ref()
                .and_then(|m| m.length())
                .map(|d| d.as_millis() as u64),
            art_url: md.as_ref().and_then(|m| m.art_url()).map(str::to_string),
        },
        position_ms: p.get_position().map(|d| d.as_millis() as u64).unwrap_or(0),
        volume,
        ..PlaybackState::default()
    };
    state.updated_at = chrono::Utc::now();
    let (id, name) = match kind {
        SourceKind::Spotify if slug == "local" => {
            ("spotify:local".to_string(), p.identity().to_string())
        }
        SourceKind::Spotify => (format!("spotify:{slug}"), p.identity().to_string()),
        SourceKind::Browser => (
            format!("browser:{slug}"),
            format!("{} ({})", p.identity(), slug),
        ),
        _ => (format!("player:{slug}"), p.identity().to_string()),
    };
    MediaSource {
        id,
        kind: kind.clone(),
        name,
        origin: format!("mpris:{bus}"),
        state,
        last_seen: chrono::Utc::now(),
        actuator_error: None,
    }
}

/// One blocking scan of the bus. Soft-empty without a session bus.
pub fn poll_all() -> Vec<MediaSource> {
    let finder = match mpris::PlayerFinder::new() {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let players = match finder.find_all() {
        Ok(p) => p,
        Err(_) => return vec![],
    };
    players
        .into_iter()
        .filter_map(|p| {
            let bus = p.bus_name().to_string();
            let (kind, slug) = classify(&bus)?;
            Some(source_for(&bus, &kind, &slug, &p))
        })
        .collect()
}

/// Actuate any MPRIS player by bus (read from the source's `mpris:{bus}` origin).
pub fn control_bus(bus: &str, action: &ControlAction) -> Result<(), String> {
    crate::spotify::MprisSpotify::new(bus).control_blocking(action)
}

/// Events from the D-Bus signal thread (plain OS thread, blocking connection).
#[derive(Debug, Clone)]
pub enum BusEvent {
    /// A player property changed (or one appeared): re-poll now.
    Refresh,
    /// A well-known bus name lost its owner: evict its sources now.
    PlayerGone(String),
}

/// Noctalia-style live updates: subscribe to PropertiesChanged on every
/// MPRIS player + NameOwnerChanged for discovery, instead of relying on
/// poll cadence alone. The 2s poll remains as backstop (position, safety).
/// Thread exits silently without a bus; the poll loop covers everything.
pub fn spawn_signal_thread(tx: tokio::sync::mpsc::UnboundedSender<BusEvent>) {
    std::thread::spawn(move || {
        use dbus::arg::PropMap;
        use dbus::blocking::Connection;
        use dbus::message::MatchRule;
        let conn = match Connection::new_session() {
            Ok(c) => c,
            Err(_) => return,
        };
        let tx_prop = tx.clone();
        let _ = conn.add_match(
            MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged")
                .with_path("/org/mpris/MediaPlayer2"),
            move |(iface, _changed, _invalid): (String, PropMap, Vec<String>), _, _| {
                if iface == "org.mpris.MediaPlayer2.Player" {
                    let _ = tx_prop.send(BusEvent::Refresh);
                }
                true
            },
        );
        let _ = conn.add_match(
            MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged"),
            move |(name, _old, new): (String, String, String), _, _| {
                if name.starts_with(PREFIX) {
                    let _ = if new.is_empty() {
                        tx.send(BusEvent::PlayerGone(name))
                    } else {
                        tx.send(BusEvent::Refresh)
                    };
                }
                true
            },
        );
        loop {
            if conn.process(std::time::Duration::from_millis(500)).is_err() {
                return;
            }
        }
    });
}
/// Background watch: upsert every bus player, evict managed ids missing 3
/// consecutive scans (flap tolerance), broadcast via registry revisions.
pub async fn run_watch(
    registry: Arc<RwLock<crate::backend::SourceRegistry>>,
    pending_volumes: Arc<std::sync::Mutex<HashMap<String, u8>>>,
    btx: broadcast::Sender<ServerMsg>,
    interval_secs: u64,
) {
    let mut missing: HashMap<String, u32> = HashMap::new();
    // Track signature per source (title + art). Players sometimes publish a
    // new title before the new artwork; on change we re-read once after a
    // beat so art can't lag a full poll behind (or look "one song behind").
    let mut sig: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    // Signal events (property changes, player gone) trigger immediate
    // re-polls; debounced so bursts collapse into one scan.
    let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<BusEvent>();
    spawn_signal_thread(etx);
    /// Snapshot + broadcast the registry (watch mutations must reach WS clients;
    /// nothing else broadcasts for them).
    async fn broadcast_now(
        registry: &Arc<RwLock<crate::backend::SourceRegistry>>,
        tx: &broadcast::Sender<ServerMsg>,
    ) {
        let reg = registry.read().await;
        let _ = tx.send(ServerMsg::State {
            protocol_version: PROTOCOL_VERSION,
            revision: reg.revision(),
            active_id: reg.active_id().map(str::to_string),
            sources: reg
                .list()
                .into_iter()
                .map(SourceSnapshot::from_source)
                .collect(),
        });
    }
    let mut last_apply = tokio::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(10))
        .unwrap_or_else(tokio::time::Instant::now);
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
    loop {
        enum Wake {
            Tick,
            Gone(String),
        }
        let wake = tokio::select! {
            _ = ticker.tick() => Wake::Tick,
            msg = erx.recv() => match msg {
                Some(BusEvent::Refresh) => {
                    // Debounce bursts (<400ms) into the tick path.
                    if last_apply.elapsed() < std::time::Duration::from_millis(400) {
                        continue;
                    }
                    Wake::Tick
                }
                Some(BusEvent::PlayerGone(bus)) => Wake::Gone(bus),
                None => Wake::Tick,
            },
        };
        if let Wake::Gone(bus) = wake {
            // Immediate evict (no 3-miss wait): the name is gone from the bus.
            let mut reg = registry.write().await;
            let dead: Vec<String> = reg
                .list()
                .into_iter()
                .filter(|s| {
                    s.origin == format!("mpris:{bus}")
                        || s.origin.starts_with(&format!("mpris:{bus}."))
                })
                .map(|s| s.id.clone())
                .collect();
            if dead.is_empty() {
                continue;
            }
            for id in &dead {
                reg.remove(id);
                missing.remove(id);
                sig.remove(id);
            }
            drop(reg);
            // Fall through: the revision check below broadcasts the removal.
        }
        let rev_before = registry.read().await.revision();
        let found = tokio::task::spawn_blocking(poll_all)
            .await
            .unwrap_or_default();
        let mut changed: Vec<String> = vec![];
        {
            let mut reg = registry.write().await;
            let mut seen = HashSet::new();
            for src in found {
                seen.insert(src.id.clone());
                missing.remove(&src.id);
                let id = src.id.clone();
                let state = src.state.clone();
                let cur = (state.track.title.clone(), state.track.art_url.clone());
                if sig.get(&id) != Some(&cur) {
                    sig.insert(id.clone(), cur);
                    if reg.get(&id).is_some() {
                        changed.push(id.clone());
                    }
                }
                if reg.get(&id).is_some() {
                    let _ = reg.update_state(&id, state);
                } else {
                    reg.register(src);
                }
            }
            // Evict only ids we manage (mpris origin) that vanished.
            let managed: Vec<String> = reg
                .list()
                .into_iter()
                .filter(|s| s.origin.starts_with("mpris:") && !seen.contains(&s.id))
                .map(|s| s.id.clone())
                .collect();
            for id in managed {
                let n = missing.get(&id).copied().unwrap_or(0) + 1;
                if n >= 3 {
                    reg.remove(&id);
                    missing.remove(&id);
                    sig.remove(&id);
                } else {
                    missing.insert(id, n);
                }
            }
            // Remembered browser volumes: apply as soon as streams appear.
            let applied = crate::sysaudio::apply_pending(&pending_volumes);
            if !applied.is_empty() {
                tracing::info!("applied remembered volumes for {applied:?}");
            }
            last_apply = tokio::time::Instant::now();
        }
        // Confirmation pass: re-read right away when tracks changed, so staged
        // metadata (title first, art second) converges in <1s, not next tick.
        if !changed.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            let again = tokio::task::spawn_blocking(poll_all)
                .await
                .unwrap_or_default();
            {
                let mut reg = registry.write().await;
                for src in again {
                    if !changed.contains(&src.id) {
                        continue;
                    }
                    let cur = (
                        src.state.track.title.clone(),
                        src.state.track.art_url.clone(),
                    );
                    if sig.get(&src.id) != Some(&cur) {
                        sig.insert(src.id.clone(), cur);
                        let _ = reg.update_state(&src.id, src.state.clone());
                    }
                }
            }
        }
        // Publish: any revision change (upsert/evict/confirm) reaches WS clients.
        if registry.read().await.revision() != rev_before {
            broadcast_now(&registry, &btx).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_table() {
        assert_eq!(
            classify("org.mpris.MediaPlayer2.spotify"),
            Some((SourceKind::Spotify, "local".into()))
        );
        assert_eq!(
            classify("org.mpris.MediaPlayer2.brave.instance10855"),
            Some((SourceKind::Browser, "brave".into()))
        );
        assert_eq!(
            classify("org.mpris.MediaPlayer2.firefox.instance1"),
            Some((SourceKind::Browser, "firefox".into()))
        );
        assert_eq!(classify("org.mpris.MediaPlayer2.playerctld"), None);
        assert_eq!(classify(":1.973"), None);
        assert_eq!(
            classify("org.mpris.MediaPlayer2.vlc"),
            Some((SourceKind::System, "vlc".into()))
        );
    }

    #[test]
    fn poll_all_live_smoke() {
        // Prints what the bus offers on dev machines; green in CI.
        let v = poll_all();
        for s in &v {
            println!("mpris: {} {} [{}]", s.id, s.name, s.origin);
        }
        assert!(v.iter().all(|s| s.origin.starts_with("mpris:")));
    }
}
