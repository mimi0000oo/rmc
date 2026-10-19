//! client-connection: THE canonical wire protocol.
//! `js/rmc-client.js` is the canonical browser implementation of these envelopes.
//! The webserver MUST use these types (not local duplicates) so a struct change
//! breaks compile instead of silently desyncing clients.

use crate::backend::{MediaSource, PlaybackStatus};
use serde::{Deserialize, Serialize};

/// Bump when the envelope shape changes. Sent in every hello/state; mismatch
/// must surface to the user, never silently misparse.
pub const PROTOCOL_VERSION: u32 = 2;

/// Messages the client can send over WS (REST mirrors control/switch/report).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        client: String,
        #[serde(default)]
        protocol_version: Option<u32>,
        /// Set by tab reporters so the server can route targeted controls.
        #[serde(default)]
        tab_id: Option<String>,
    },
    Switch {
        id: String,
        #[serde(default)]
        request_id: Option<String>,
    },
    /// Flat shape (action as string + optional params) so WS and REST bodies
    /// parse identically. Golden JSON vectors in tests lock this.
    Control {
        id: Option<String>,
        action: String,
        #[serde(default)]
        position_ms: Option<u64>,
        #[serde(default)]
        level: Option<u8>,
        /// URI for open_uri (e.g. spotify:track:...).
        #[serde(default)]
        uri: Option<String>,
        #[serde(default)]
        request_id: Option<String>,
    },
    Report {
        tab: TabReport,
        #[serde(default)]
        request_id: Option<String>,
    },
}

/// State pushed by a browser tab / MediaSession reporter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabReport {
    pub tab_id: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub position_ms: Option<u64>,
    pub volume: Option<u8>,
    /// Cover art URL (MediaSession artwork[0]).
    #[serde(default)]
    pub artwork_url: Option<String>,
    /// "playing" | "paused" | "stopped"
    pub status: Option<String>,
    pub page_url: Option<String>,
}

impl TabReport {
    /// music.youtube.com (and common YTM wrappers) count as YouTube Music.
    pub fn is_ytmusic(&self) -> bool {
        self.page_url.as_ref().is_some_and(|u| {
            let u = u.to_lowercase();
            u.contains("music.youtube.com") || u.contains("youtube-music") || u.contains("ytmusic")
        })
    }

    pub fn playback_status(&self) -> PlaybackStatus {
        match self.status.as_deref() {
            Some("playing") => PlaybackStatus::Playing,
            Some("stopped") => PlaybackStatus::Stopped,
            _ => PlaybackStatus::Paused,
        }
    }
}

/// Typed server errors — every rejection on REST and WS uses these codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    UnknownSource,
    NoActiveSource,
    UnknownAction,
    BadParam,
    Stale,
    VersionMismatch,
    Internal,
}

/// Messages the server sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    State {
        protocol_version: u32,
        revision: u64,
        active_id: Option<String>,
        sources: Vec<SourceSnapshot>,
    },
    Error {
        code: ErrorCode,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// Targeted downlink: only the WS connection that registered `tab_id`
    /// receives this. Tabs actuate local media, then report back.
    TabControl {
        tab_id: String,
        action: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        position_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<u8>,
    },
    /// Success acknowledgement echoing the client's request_id so controls
    /// can be correlated (broadcast state follows separately).
    Ack { request_id: String },
}

/// Lossless projection of backend::MediaSource for the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub id: String,
    pub kind: String,
    pub kind_label: String,
    pub name: String,
    pub origin: String,
    pub status: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub position_ms: u64,
    pub volume: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art_url: Option<String>,
    /// Present when the last actuation for this source failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actuator_error: Option<String>,
}

impl SourceSnapshot {
    pub fn from_source(s: &MediaSource) -> Self {
        Self {
            id: s.id.clone(),
            kind: format!("{:?}", s.kind).to_lowercase(),
            kind_label: s.kind.label().to_string(),
            name: s.name.clone(),
            origin: s.origin.clone(),
            status: match s.state.status {
                PlaybackStatus::Playing => "playing",
                PlaybackStatus::Paused => "paused",
                PlaybackStatus::Stopped => "stopped",
            }
            .to_string(),
            title: s.state.track.title.clone(),
            artist: s.state.track.artist.clone(),
            album: s.state.track.album.clone(),
            duration_ms: s.state.track.duration_ms,
            position_ms: s.state.position_ms,
            volume: s.state.volume,
            art_url: s.state.track.art_url.clone(),
            actuator_error: s.actuator_error.clone(),
        }
    }
}

/// Pure backoff helper both JS and Rust clients implement identically:
/// base 1s, doubling, capped at 15s. Jitter is added client-side.
pub fn reconnect_delay_ms(attempt: u32) -> u64 {
    (1_000u64.saturating_mul(1 << attempt.min(4))).min(15_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vectors: these exact JSON strings must also parse in
    /// js/rmc-client.js (see js/interop test note). Change shape -> change both.
    #[test]
    fn golden_control_toggle_flat() {
        let m: ClientMsg =
            serde_json::from_str(r#"{"type":"control","id":"spotify:local","action":"toggle"}"#)
                .unwrap();
        assert!(matches!(m, ClientMsg::Control { .. }));
    }

    #[test]
    fn golden_control_volume_flat_with_params() {
        let m: ClientMsg =
            serde_json::from_str(r#"{"type":"control","id":null,"action":"volume","level":42}"#)
                .unwrap();
        match m {
            ClientMsg::Control { action, level, .. } => {
                assert_eq!(action, "volume");
                assert_eq!(level, Some(42));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn golden_control_seek_flat_with_params() {
        let m: ClientMsg =
            serde_json::from_str(r#"{"type":"control","action":"seek","position_ms":90000}"#)
                .unwrap();
        match m {
            ClientMsg::Control { position_ms, .. } => assert_eq!(position_ms, Some(90000)),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn golden_hello_carries_version_and_tab() {
        let m: ClientMsg = serde_json::from_str(
            r#"{"type":"hello","client":"web-ui","protocol_version":2,"tab_id":"tab-9"}"#,
        )
        .unwrap();
        match m {
            ClientMsg::Hello {
                protocol_version,
                tab_id,
                ..
            } => {
                assert_eq!(protocol_version, Some(2));
                assert_eq!(tab_id.as_deref(), Some("tab-9"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn golden_report_shape() {
        let m: ClientMsg = serde_json::from_str(
            r#"{"type":"report","tab":{"tab_id":"t1","title":"lofi","artist":null,"album":null,"duration_ms":null,"position_ms":null,"volume":null,"status":"playing","page_url":"https://music.youtube.com/watch?v=x"}}"#,
        )
        .unwrap();
        match m {
            ClientMsg::Report { tab, .. } => {
                assert!(tab.is_ytmusic());
                assert_eq!(tab.playback_status(), PlaybackStatus::Playing);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn snapshot_roundtrip_lossless() {
        let src = crate::backend::MediaSource {
            id: "ytmusic:tab-2".into(),
            kind: crate::backend::SourceKind::YouTubeMusic,
            name: "YouTube Music (tab tab-2)".into(),
            origin: "tab:tab-2".into(),
            state: crate::backend::PlaybackState::default(),
            last_seen: chrono::Utc::now(),
            actuator_error: None,
        };
        let snap = SourceSnapshot::from_source(&src);
        assert_eq!(snap.kind, "youtubemusic");
        assert_eq!(snap.kind_label, "YouTube Music");
        let s = serde_json::to_string(&snap).unwrap();
        let back: SourceSnapshot = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, snap.id);
    }

    #[test]
    fn backoff_caps() {
        assert_eq!(reconnect_delay_ms(0), 1_000);
        assert_eq!(reconnect_delay_ms(1), 2_000);
        assert_eq!(reconnect_delay_ms(10), 15_000);
    }

    #[test]
    fn ack_echoes_request_id() {
        let m = ServerMsg::Ack {
            request_id: "abc".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""type":"ack""#));
        let back: ServerMsg = serde_json::from_str(&s).unwrap();
        match back {
            ServerMsg::Ack { request_id } => assert_eq!(request_id, "abc"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn error_codes_serialize_snake_case() {
        // REST and WS must agree: codes go over the wire via serde, never Debug.
        for (code, want) in [
            (ErrorCode::UnknownSource, "unknown_source"),
            (ErrorCode::NoActiveSource, "no_active_source"),
            (ErrorCode::UnknownAction, "unknown_action"),
            (ErrorCode::BadParam, "bad_param"),
            (ErrorCode::Stale, "stale"),
            (ErrorCode::VersionMismatch, "version_mismatch"),
            (ErrorCode::Internal, "internal"),
        ] {
            let v = serde_json::to_value(code).unwrap();
            assert_eq!(v, serde_json::json!(want), "code {code:?}");
        }
    }
}
