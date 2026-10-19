//! youtube-music integration: owns browser-tab YTM sources.
//! Ingestion: tabs POST /api/report or send WS {type:"report"} (TabReport);
//! this crate maps reports to registry sources. Control downlink reaches tabs
//! via the webserver's per-connection tab binding (TabControl frames).

use crate::backend::{MediaSource, PlaybackState, SourceKind};
use crate::protocol::TabReport;

pub struct YouTubeMusicAdapter;

impl YouTubeMusicAdapter {
    /// Canonical mapping: rich TabReport -> full MediaSource.
    /// Name is stable per tab (never churns with track); track goes in state.
    pub fn source_for_report(report: &TabReport) -> MediaSource {
        let state = PlaybackState {
            status: report.playback_status(),
            track: crate::backend::TrackInfo {
                title: report.title.clone(),
                artist: report.artist.clone(),
                album: report.album.clone(),
                duration_ms: report.duration_ms,
                art_url: report.artwork_url.clone(),
            },
            position_ms: match (report.position_ms, report.duration_ms) {
                (Some(p), Some(d)) => p.min(d),
                (Some(p), None) => p,
                _ => 0,
            },
            volume: report.volume.unwrap_or(70).min(100),
            ..PlaybackState::default()
        };
        MediaSource {
            id: format!("ytmusic:{}", report.tab_id),
            kind: SourceKind::YouTubeMusic,
            name: format!("YouTube Music (tab {})", report.tab_id),
            origin: format!("tab:{}", report.tab_id),
            state,
            last_seen: chrono::Utc::now(),
            actuator_error: None,
        }
    }

    pub fn generic_browser_source(report: &TabReport) -> MediaSource {
        let state = PlaybackState {
            status: report.playback_status(),
            track: crate::backend::TrackInfo {
                title: report.title.clone(),
                artist: report.artist.clone(),
                ..Default::default()
            },
            position_ms: report.position_ms.unwrap_or(0),
            volume: report.volume.unwrap_or(70).min(100),
            ..PlaybackState::default()
        };
        MediaSource {
            id: format!("browser:{}", report.tab_id),
            kind: SourceKind::Browser,
            name: format!("Browser (tab {})", report.tab_id),
            origin: format!("tab:{}", report.tab_id),
            state,
            last_seen: chrono::Utc::now(),
            actuator_error: None,
        }
    }

    /// One source per physical tab: YTM kind wins on YTM URLs.
    pub fn source_for_tab(report: &TabReport) -> MediaSource {
        if report.is_ytmusic() {
            Self::source_for_report(report)
        } else {
            Self::generic_browser_source(report)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(tab: &str, url: &str) -> TabReport {
        TabReport {
            tab_id: tab.into(),
            title: Some("lofi".into()),
            artist: Some("artist".into()),
            album: Some("album".into()),
            duration_ms: Some(200_000),
            position_ms: Some(50_000),
            volume: Some(90),
            status: Some("playing".into()),
            page_url: Some(url.into()),
            artwork_url: None,
        }
    }

    #[test]
    fn tab_id_is_stable_and_rich() {
        let s = YouTubeMusicAdapter::source_for_tab(&report(
            "tab-3",
            "https://music.youtube.com/watch?v=x",
        ));
        assert_eq!(s.id, "ytmusic:tab-3");
        assert_eq!(s.kind, SourceKind::YouTubeMusic);
        assert_eq!(s.name, "YouTube Music (tab tab-3)");
        assert_eq!(s.state.track.duration_ms, Some(200_000));
        assert_eq!(s.state.position_ms, 50_000);
        assert_eq!(s.state.volume, 90);
    }

    #[test]
    fn non_ytm_url_maps_to_browser() {
        let s = YouTubeMusicAdapter::source_for_tab(&report("t9", "https://example.com/video"));
        assert_eq!(s.id, "browser:t9");
        assert_eq!(s.kind, SourceKind::Browser);
    }

    #[test]
    fn position_clamps_to_duration() {
        let mut r = report("t1", "https://music.youtube.com/");
        r.position_ms = Some(999_999);
        let s = YouTubeMusicAdapter::source_for_tab(&r);
        assert_eq!(s.state.position_ms, 200_000);
    }

    #[test]
    fn stopped_maps_to_stopped() {
        let mut r = report("t1", "https://music.youtube.com/");
        r.status = Some("stopped".into());
        let s = YouTubeMusicAdapter::source_for_tab(&r);
        assert_eq!(s.state.status, crate::backend::PlaybackStatus::Stopped);
    }
}
