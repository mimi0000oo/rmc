//! settings: persisted server configuration (tokens, modes).
//! File is the source of truth across restarts; env vars seed it on boot.
//! Secrets are NEVER returned by the API — only presence flags.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VolumeMode {
    /// MPRIS first, whole-app system volume on ignored-change, remember when idle.
    #[default]
    Auto,
    /// MPRIS only (honest errors when the player ignores it).
    Mpris,
    /// System sink volume only (whole browser, all tabs).
    System,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerSettings {
    #[serde(default)]
    pub spotify_id: Option<String>,
    #[serde(default)]
    pub spotify_secret: Option<String>,
    #[serde(default)]
    pub spotify_token: Option<String>,
    #[serde(default)]
    pub volume_mode: VolumeMode,
}

impl ServerSettings {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("RMC_SETTINGS_FILE") {
            if !p.trim().is_empty() {
                return PathBuf::from(p);
            }
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".config")
            });
        base.join("rmc").join("settings.json")
    }

    pub fn load(path: &PathBuf) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save with owner-only permissions (0600): this file holds secrets.
    pub fn save(&self, path: &PathBuf) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("settings dir: {e}"))?;
        }
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("settings encode: {e}"))?;
        std::fs::write(path, text).map_err(|e| format!("settings write: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Env vars seed the file (export once, persist forever).
    pub fn seed_from_env(&mut self) -> bool {
        self.seed_from_map(&env_map(), true)
    }

    /// Seed from explicit pairs (real env, then `.env` file — real env wins).
    /// With `overwrite`, differing values are replaced; otherwise only empty
    /// slots are filled (so a stale `.env` never clobbers UI-saved settings).
    pub fn seed_from_map(
        &mut self,
        vars: &std::collections::HashMap<String, String>,
        overwrite: bool,
    ) -> bool {
        let mut changed = false;
        for (slot, var) in [
            (&mut self.spotify_id, "RMC_SPOTIFY_ID"),
            (&mut self.spotify_secret, "RMC_SPOTIFY_SECRET"),
            (&mut self.spotify_token, "RMC_SPOTIFY_TOKEN"),
        ] {
            if let Some(v) = vars.get(var) {
                let v = v.trim().to_string();
                if v.is_empty() {
                    continue;
                }
                if slot.is_none() || (overwrite && slot.as_deref() != Some(v.as_str())) {
                    *slot = Some(v);
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn has_search_creds(&self) -> bool {
        self.spotify_token
            .as_ref()
            .is_some_and(|t| !t.trim().is_empty())
            || (self
                .spotify_id
                .as_ref()
                .is_some_and(|t| !t.trim().is_empty())
                && self
                    .spotify_secret
                    .as_ref()
                    .is_some_and(|t| !t.trim().is_empty()))
    }

    /// Path of the `.env` file: `RMC_ENV_FILE` or `./.env` next to the cwd.
    pub fn dotenv_path() -> PathBuf {
        std::env::var("RMC_ENV_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(".env"))
    }
}

/// Minimal `.env` parser: `KEY=value` lines, `#` comments, optional quotes.
/// Only `RMC_*` keys are picked up; everything else is ignored.
pub fn parse_dotenv(text: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = match line.strip_prefix("export ") {
            Some(rest) => rest.trim(),
            None => line,
        };
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim().to_string();
        if !k.starts_with("RMC_") {
            continue;
        }
        let mut v = v.trim().to_string();
        if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\'')))
        {
            v = v[1..v.len() - 1].to_string();
        }
        // strip trailing inline comment for unquoted values
        if let Some(hash) = v.find(" #") {
            v.truncate(hash);
            v = v.trim_end().to_string();
        }
        out.insert(k, v);
    }
    out
}

/// Real process env as a map (for seed precedence: env beats `.env` file).
pub fn env_map() -> std::collections::HashMap<String, String> {
    std::env::vars().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_perms() {
        let dir = std::env::temp_dir().join(format!("rmc-test-{}", std::process::id()));
        let p = dir.join("settings.json");
        let s = ServerSettings {
            spotify_id: Some("id123".into()),
            volume_mode: VolumeMode::System,
            ..ServerSettings::default()
        };
        s.save(&p).unwrap();
        let back = ServerSettings::load(&p);
        assert_eq!(back.spotify_id.as_deref(), Some("id123"));
        assert_eq!(back.volume_mode, VolumeMode::System);
        assert!(back.spotify_token.is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_default() {
        let p = PathBuf::from("/nonexistent-dir-xyz/settings.json");
        let s = ServerSettings::load(&p);
        assert_eq!(s.volume_mode, VolumeMode::Auto);
        assert!(!s.has_search_creds());
    }

    #[test]
    fn dotenv_parses_and_env_wins() {
        let m = parse_dotenv(
            "# comment\nexport RMC_SPOTIFY_ID=abc\nRMC_SPOTIFY_SECRET=\"s e c\"\nOTHER=1\nNOEQUALS\nRMC_EMPTY=\n",
        );
        assert_eq!(m.get("RMC_SPOTIFY_ID").map(String::as_str), Some("abc"));
        assert_eq!(
            m.get("RMC_SPOTIFY_SECRET").map(String::as_str),
            Some("s e c")
        );
        assert!(!m.contains_key("OTHER"));
        assert!(!m.contains_key("NOEQUALS"));
        assert_eq!(m.get("RMC_EMPTY").map(String::as_str), Some(""));

        // seed_from_map skips empty values but sets the rest
        let mut s = ServerSettings::default();
        assert!(s.seed_from_map(&m, true));
        assert_eq!(s.spotify_id.as_deref(), Some("abc"));
        assert_eq!(s.spotify_secret.as_deref(), Some("s e c"));

        // real env beats .env content
        let mut env = std::collections::HashMap::new();
        env.insert("RMC_SPOTIFY_ID".into(), "from-env".into());
        assert!(s.seed_from_map(&env, true));
        assert_eq!(s.spotify_id.as_deref(), Some("from-env"));

        // fill-gaps mode never clobbers saved values
        let mut env2 = std::collections::HashMap::new();
        env2.insert("RMC_SPOTIFY_ID".into(), "stale".into());
        assert!(!s.seed_from_map(&env2, false));
        assert_eq!(s.spotify_id.as_deref(), Some("from-env"));
    }
}
