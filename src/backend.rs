//! backend: single source of truth for media sources and active-stream switching.
//! Integrator rule: other crates may depend on this crate, but must not edit this folder.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Stable identifier for a stream, e.g. `spotify:local`, `browser:tab-3`, `ytmusic:tab-1`.
pub type SourceId = String;

/// What kind of stream this is — used by the UI to differentiate streams.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Spotify,
    Browser,
    YouTubeMusic,
    System,
}

impl SourceKind {
    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::Spotify => "Spotify",
            SourceKind::Browser => "Browser",
            SourceKind::YouTubeMusic => "YouTube Music",
            SourceKind::System => "System",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TrackInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    /// Cover art / thumbnail URL (MPRIS artUrl, MediaSession artwork).
    #[serde(default)]
    pub art_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackState {
    pub status: PlaybackStatus,
    pub track: TrackInfo,
    pub position_ms: u64,
    pub volume: u8, // 0..=100
    pub updated_at: DateTime<Utc>,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            track: TrackInfo::default(),
            position_ms: 0,
            volume: 70,
            updated_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaSource {
    pub id: SourceId,
    pub kind: SourceKind,
    /// Human name, e.g. "Spotify Desktop", "Chrome — Tab: lofi mix".
    pub name: String,
    /// Where it comes from, e.g. "mpris:spotify", "tab:42", "demo-seed".
    pub origin: String,
    pub state: PlaybackState,
    pub last_seen: DateTime<Utc>,
    /// Set when the last actuation attempt for this source failed (optimistic
    /// cache was applied but the player rejected it). Cleared by the next
    /// successful poll/report. Surfaced in snapshots so remotes stay honest.
    #[serde(default)]
    pub actuator_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    Play,
    Pause,
    Toggle,
    Next,
    Prev,
    Seek {
        position_ms: u64,
    },
    Volume {
        level: u8,
    },
    /// Play a URI (e.g. spotify:track:...) on players that support OpenUri.
    OpenUri {
        uri: String,
    },
}

/// Failure modes for adapter actuation (real players fail; the cache never lies about it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    Unavailable(String),
    Rejected(String),
    Transport(String),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterError::Unavailable(m) => write!(f, "player unavailable: {m}"),
            AdapterError::Rejected(m) => write!(f, "player rejected command: {m}"),
            AdapterError::Transport(m) => write!(f, "transport error: {m}"),
        }
    }
}

impl std::error::Error for AdapterError {}

pub type AdapterResult = Result<(), AdapterError>;

/// Trait every integration implements to actuate a real player.
/// Sync by design: callers run it on a blocking thread; the registry cache
/// update stays separate so a dead player can never corrupt registry truth.
pub trait MediaAdapter: Send + Sync {
    fn source_id(&self) -> &str;
    fn play(&self) -> AdapterResult;
    fn pause(&self) -> AdapterResult;
    fn toggle(&self) -> AdapterResult;
    fn next(&self) -> AdapterResult;
    fn prev(&self) -> AdapterResult;
    fn seek(&self, position_ms: u64) -> AdapterResult;
    fn set_volume(&self, level: u8) -> AdapterResult;
    fn open_uri(&self, uri: &str) -> AdapterResult {
        let _ = uri;
        Err(AdapterError::Rejected("open_uri not supported".into()))
    }

    fn dispatch(&self, action: &ControlAction) -> AdapterResult {
        match action {
            ControlAction::Play => self.play(),
            ControlAction::Pause => self.pause(),
            ControlAction::Toggle => self.toggle(),
            ControlAction::Next => self.next(),
            ControlAction::Prev => self.prev(),
            ControlAction::Seek { position_ms } => self.seek(*position_ms),
            ControlAction::Volume { level } => self.set_volume(*level),
            ControlAction::OpenUri { uri } => self.open_uri(uri),
        }
    }
}

/// Apply a control action to a playback state (pure, testable core of switching/control).
pub fn apply_action(mut state: PlaybackState, action: &ControlAction) -> PlaybackState {
    match action {
        ControlAction::Play => state.status = PlaybackStatus::Playing,
        ControlAction::Pause => state.status = PlaybackStatus::Paused,
        ControlAction::Toggle => {
            state.status = match state.status {
                PlaybackStatus::Playing => PlaybackStatus::Paused,
                _ => PlaybackStatus::Playing,
            }
        }
        ControlAction::Next | ControlAction::Prev => {
            // Advance marker: keep current track info (queue is player-side),
            // reset position so remotes show progress restarting.
            state.position_ms = 0;
        }
        ControlAction::Seek { position_ms } => {
            let clamped = match state.track.duration_ms {
                Some(d) => (*position_ms).min(d),
                None => *position_ms,
            };
            state.position_ms = clamped;
        }
        ControlAction::Volume { level } => state.volume = (*level).min(100),
        ControlAction::OpenUri { .. } => {
            // Player resolves the URI; optimistically show playback starting.
            state.status = PlaybackStatus::Playing;
            state.position_ms = 0;
        }
    }
    state.updated_at = Utc::now();
    state
}

/// Validate a source id: non-empty, no whitespace-only, bounded length.
pub fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 128
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegistryError {
    NotFound(SourceId),
    Empty,
    Stale { id: SourceId },
    InvalidId,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::NotFound(id) => write!(f, "source not found: {id}"),
            RegistryError::Empty => write!(f, "no sources registered"),
            RegistryError::Stale { id } => write!(f, "stale update rejected for: {id}"),
            RegistryError::InvalidId => write!(f, "invalid source id"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// Registry: owns all streams + which one is active.
/// Mutation counter (`revision`) lets WS subscribers resync after lag instead
/// of guessing; `last_seen` + `prune_expired` enforce liveness.
#[derive(Debug, Default)]
pub struct SourceRegistry {
    sources: HashMap<SourceId, MediaSource>,
    active_id: Option<SourceId>,
    revision: u64,
    /// When true, a newly-playing source steals active status. Default off.
    pub auto_switch_on_play: bool,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            active_id: None,
            revision: 0,
            auto_switch_on_play: false,
        }
    }

    /// Monotonic mutation counter. Every register/update/remove/switch/control bumps it.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Register or refresh a source. First source becomes active automatically.
    /// Returns false (and ignores) when id is invalid or when kind/origin
    /// collide on an existing id with a different kind (reconnect vs new-tab guard).
    pub fn register(&mut self, source: MediaSource) -> bool {
        if !valid_id(&source.id) || source.name.trim().is_empty() {
            return false;
        }
        if let Some(existing) = self.sources.get(&source.id) {
            if existing.kind != source.kind {
                return false;
            }
        }
        let id = source.id.clone();
        let was_playing_new = source.state.status == PlaybackStatus::Playing;
        self.sources.insert(id.clone(), source);
        if self.active_id.is_none() || (self.auto_switch_on_play && was_playing_new) {
            self.active_id = Some(id);
        }
        self.bump();
        true
    }

    /// Apply a polled/reported state. Rejects stale updates whose timestamp
    /// predates the cached one so concurrent pollers can't clobber newer state.
    pub fn update_state(&mut self, id: &str, state: PlaybackState) -> Result<(), RegistryError> {
        let s = self
            .sources
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound(id.to_string()))?;
        if state.updated_at < s.state.updated_at {
            return Err(RegistryError::Stale { id: id.to_string() });
        }
        let became_playing =
            s.state.status != PlaybackStatus::Playing && state.status == PlaybackStatus::Playing;
        s.state = state;
        s.last_seen = Utc::now();
        s.actuator_error = None; // fresh poll/report is truth: clear degradation
        if became_playing && self.auto_switch_on_play {
            self.active_id = Some(id.to_string());
        }
        self.bump();
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let removed = self.sources.remove(id).is_some();
        if removed {
            if self.active_id.as_deref() == Some(id) {
                // Fall back to any remaining source (deterministic: smallest id).
                let mut ids: Vec<&SourceId> = self.sources.keys().collect();
                ids.sort();
                self.active_id = ids.first().map(|s| (*s).clone());
            }
            self.bump();
        }
        removed
    }

    /// Drop sources not seen for longer than `max_age_secs`. Returns evicted ids.
    pub fn prune_expired(&mut self, max_age_secs: i64) -> Vec<SourceId> {
        let now = Utc::now();
        let stale: Vec<SourceId> = self
            .sources
            .iter()
            .filter(|(_, s)| (now - s.last_seen).num_seconds() > max_age_secs)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &stale {
            self.sources.remove(id);
        }
        if !stale.is_empty() {
            if self
                .active_id
                .as_ref()
                .is_some_and(|a| stale.iter().any(|s| s == a))
            {
                let mut ids: Vec<&SourceId> = self.sources.keys().collect();
                ids.sort();
                self.active_id = ids.first().map(|s| (*s).clone());
            }
            self.bump();
        }
        stale
    }

    /// Explicit user switch — always wins. Returns error if id unknown or empty.
    pub fn set_active(&mut self, id: &str) -> Result<(), RegistryError> {
        if self.sources.is_empty() {
            return Err(RegistryError::Empty);
        }
        if !self.sources.contains_key(id) {
            return Err(RegistryError::NotFound(id.to_string()));
        }
        if self.active_id.as_deref() != Some(id) {
            self.active_id = Some(id.to_string());
            self.bump();
        }
        Ok(())
    }

    pub fn active(&self) -> Option<&MediaSource> {
        self.active_id.as_ref().and_then(|id| self.sources.get(id))
    }

    pub fn active_id(&self) -> Option<&str> {
        self.active_id.as_deref()
    }

    pub fn get(&self, id: &str) -> Option<&MediaSource> {
        self.sources.get(id)
    }

    pub fn list(&self) -> Vec<&MediaSource> {
        let mut v: Vec<&MediaSource> = self.sources.values().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    pub fn apply_control(&mut self, id: &str, action: &ControlAction) -> Result<(), RegistryError> {
        let s = self
            .sources
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound(id.to_string()))?;
        s.state = apply_action(s.state.clone(), action);
        s.last_seen = Utc::now();
        self.bump();
        Ok(())
    }

    /// Record a failed actuation (optimistic cache stands, but flagged).
    /// Returns false when the source is unknown.
    pub fn mark_actuator_error(&mut self, id: &str, message: impl Into<String>) -> bool {
        match self.sources.get_mut(id) {
            Some(s) => {
                s.actuator_error = Some(message.into());
                s.last_seen = Utc::now();
                self.bump();
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn src(id: &str) -> MediaSource {
        MediaSource {
            id: id.into(),
            kind: SourceKind::Browser,
            name: id.into(),
            origin: "test".into(),
            state: PlaybackState::default(),
            last_seen: Utc::now(),
            actuator_error: None,
        }
    }

    #[test]
    fn first_register_becomes_active() {
        let mut r = SourceRegistry::new();
        assert!(r.register(src("a")));
        assert_eq!(r.active_id(), Some("a"));
    }

    #[test]
    fn explicit_switch_wins_and_unknown_fails() {
        let mut r = SourceRegistry::new();
        r.register(src("a"));
        r.register(src("b"));
        assert!(r.set_active("b").is_ok());
        assert_eq!(r.active_id(), Some("b"));
        assert!(r.set_active("nope").is_err());
    }

    #[test]
    fn remove_active_falls_back_deterministically() {
        let mut r = SourceRegistry::new();
        r.register(src("b"));
        r.register(src("a"));
        r.set_active("b").unwrap();
        assert!(r.remove("b"));
        assert_eq!(r.active_id(), Some("a"));
    }

    #[test]
    fn toggle_flips_playing_paused() {
        let playing = PlaybackState {
            status: PlaybackStatus::Playing,
            ..PlaybackState::default()
        };
        let paused = apply_action(playing, &ControlAction::Toggle);
        assert_eq!(paused.status, PlaybackStatus::Paused);
        let playing2 = apply_action(paused, &ControlAction::Toggle);
        assert_eq!(playing2.status, PlaybackStatus::Playing);
    }

    #[test]
    fn volume_clamps() {
        let s = apply_action(
            PlaybackState::default(),
            &ControlAction::Volume { level: 250 },
        );
        assert_eq!(s.volume, 100);
    }

    #[test]
    fn revision_bumps_on_mutation() {
        let mut r = SourceRegistry::new();
        assert_eq!(r.revision(), 0);
        r.register(src("a"));
        let r1 = r.revision();
        assert!(r1 > 0);
        r.set_active("a").unwrap(); // no change -> no bump required
        r.apply_control("a", &ControlAction::Toggle).unwrap();
        assert!(r.revision() > r1);
    }

    #[test]
    fn rejects_invalid_ids_and_kind_collision() {
        let mut r = SourceRegistry::new();
        let mut bad = src("   ");
        assert!(!r.register(bad.clone()));
        bad.id = "x".into();
        bad.name = "".into();
        assert!(!r.register(bad));
        assert!(r.register(src("x")));
        let mut evil = src("x");
        evil.kind = SourceKind::Spotify;
        assert!(!r.register(evil));
        assert_eq!(r.get("x").unwrap().kind, SourceKind::Browser);
    }

    #[test]
    fn rejects_stale_updates() {
        let mut r = SourceRegistry::new();
        r.register(src("a"));
        let mut fresh = PlaybackState {
            status: PlaybackStatus::Playing,
            ..PlaybackState::default()
        };
        fresh.updated_at = Utc::now() + Duration::seconds(10);
        r.update_state("a", fresh).unwrap();
        let stale = PlaybackState::default(); // older timestamp
        assert_eq!(
            r.update_state("a", stale),
            Err(RegistryError::Stale { id: "a".into() })
        );
        assert_eq!(r.get("a").unwrap().state.status, PlaybackStatus::Playing);
    }

    #[test]
    fn prune_expired_evicts_ghosts() {
        let mut r = SourceRegistry::new();
        let mut old = src("ghost");
        old.last_seen = Utc::now() - Duration::seconds(3600);
        r.register(old);
        r.register(src("live"));
        let evicted = r.prune_expired(60);
        assert_eq!(evicted, vec!["ghost".to_string()]);
        assert!(r.get("ghost").is_none());
    }

    #[test]
    fn seek_clamps_to_duration() {
        let mut st = PlaybackState::default();
        st.track.duration_ms = Some(100_000);
        let s = apply_action(
            st,
            &ControlAction::Seek {
                position_ms: 500_000,
            },
        );
        assert_eq!(s.position_ms, 100_000);
    }

    #[test]
    fn next_keeps_track_resets_position() {
        let mut st = PlaybackState::default();
        st.track.title = Some("T".into());
        st.position_ms = 999;
        let s = apply_action(st, &ControlAction::Next);
        assert_eq!(s.position_ms, 0);
        assert_eq!(s.track.title.as_deref(), Some("T"));
    }

    #[test]
    fn auto_switch_policy() {
        let mut r = SourceRegistry::new();
        r.auto_switch_on_play = true;
        r.register(src("a"));
        let mut b = src("b");
        b.state.status = PlaybackStatus::Playing;
        r.register(b);
        assert_eq!(r.active_id(), Some("b"));
    }

    #[test]
    fn actuator_error_marks_and_clears_on_truth() {
        let mut r = SourceRegistry::new();
        r.register(src("a"));
        assert!(r.mark_actuator_error("a", "player down"));
        assert_eq!(
            r.get("a").unwrap().actuator_error.as_deref(),
            Some("player down")
        );
        assert!(!r.mark_actuator_error("ghost", "x"));
        // Next successful poll clears the degradation flag.
        r.update_state("a", PlaybackState::default()).unwrap();
        assert_eq!(r.get("a").unwrap().actuator_error, None);
    }
}
