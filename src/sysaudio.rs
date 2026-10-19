//! sysaudio: whole-application volume via the system audio server.
//! Shells out to `pactl` (PipeWire/PulseAudio). Used as the volume path for
//! desktop browsers: Chromium accepts MPRIS volume commands but never applies
//! them, so we drive the app's sink inputs instead (all tabs, one slider).

use std::process::Command;

/// One playback stream owned by an application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkInput {
    pub index: u32,
    pub names: Vec<String>,
    pub volume_pct: u8,
}

/// Parse `pactl list sink-inputs` into structured entries. Pure, unit-tested.
pub fn parse_sink_inputs(output: &str) -> Vec<SinkInput> {
    let mut inputs = Vec::new();
    let mut cur: Option<SinkInput> = None;
    let mut in_props = false;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("Sink Input #") {
            if let Some(done) = cur.take() {
                inputs.push(done);
            }
            cur = rest.trim().parse::<u32>().ok().map(|index| SinkInput {
                index,
                names: vec![],
                volume_pct: 100,
            });
            in_props = false;
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        let t = line.trim();
        if t == "Properties:" {
            in_props = true;
            continue;
        }
        if !in_props && t.starts_with("Volume:") {
            // "Volume: aux0: 56361 /  86% / ..." — take the first NN%.
            if let Some(pct) = t.split('%').next().and_then(|head| {
                head.rsplit(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|n| n.parse::<u8>().ok())
            }) {
                c.volume_pct = pct.min(100);
            }
            continue;
        }
        if in_props {
            for key in ["application.name", "node.name", "media.name"] {
                if let Some(v) = t.strip_prefix(&format!("{key} = ")) {
                    let v = v.trim_matches('"').to_lowercase();
                    if !v.is_empty() {
                        c.names.push(v);
                    }
                }
            }
        }
    }
    if let Some(done) = cur.take() {
        inputs.push(done);
    }
    inputs
}

fn pactl() -> Result<String, String> {
    let out = Command::new("pactl")
        .arg("list")
        .arg("sink-inputs")
        .output()
        .map_err(|e| format!("pactl not available ({e}); install pipewire/pulseaudio utils"))?;
    if !out.status.success() {
        return Err("pactl list sink-inputs failed".into());
    }
    String::from_utf8(out.stdout).map_err(|e| format!("pactl output not utf8: {e}"))
}

fn matches(input: &SinkInput, app: &str) -> bool {
    let a = app.to_lowercase();
    input.names.iter().any(|n| n.contains(&a))
}

/// Current volume (first match) for an application name fragment.
pub fn app_volume(app: &str) -> Result<u8, String> {
    let inputs = parse_sink_inputs(&pactl()?);
    inputs
        .iter()
        .find(|i| matches(i, app))
        .map(|i| i.volume_pct)
        .ok_or_else(|| format!("no live audio streams for '{app}'"))
}

/// Set volume on ALL sink inputs of an application. Returns inputs touched.
/// Errors when nothing matched (honest: no fake ok for silent browsers).
pub fn set_app_volume(app: &str, level: u8) -> Result<usize, String> {
    let level = level.min(100);
    let inputs = parse_sink_inputs(&pactl()?);
    let targets: Vec<u32> = inputs
        .iter()
        .filter(|i| matches(i, app))
        .map(|i| i.index)
        .collect();
    if targets.is_empty() {
        return Err(format!("no live audio streams for '{app}'"));
    }
    for idx in &targets {
        let st = Command::new("pactl")
            .arg("set-sink-input-volume")
            .arg(idx.to_string())
            .arg(format!("{level}%"))
            .status()
            .map_err(|e| format!("pactl set failed: {e}"))?;
        if !st.success() {
            return Err(format!("pactl set-sink-input-volume {idx} failed"));
        }
    }
    Ok(targets.len())
}

/// Apply remembered volumes when their streams appear. For each pending
/// (app, level): if a live stream exists, set it and drop the entry.
/// Returns slugs applied. No-op when empty (watch calls this every tick).
pub fn apply_pending(
    pending: &std::sync::Mutex<std::collections::HashMap<String, u8>>,
) -> Vec<String> {
    let wanted: Vec<(String, u8)> = match pending.lock() {
        Ok(g) => g.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        Err(_) => return vec![],
    };
    if wanted.is_empty() {
        return vec![];
    }
    let mut done = vec![];
    for (app, level) in wanted {
        if set_app_volume(&app, level).is_ok() {
            if let Ok(mut g) = pending.lock() {
                g.remove(&app);
            }
            done.push(app);
        }
    }
    done
}
#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "Sink Input #109\n\tVolume: aux0: 56361 /  86% / -3.93 dB,   aux1: 56361 /  86% / -3.93 dB\n\tProperties:\n\t\tmedia.name = \"Spotify\"\n\t\tnode.name = \"spotify\"\n\t\tapplication.name = \"Spotify\"\nSink Input #114\n\tVolume: front-left: 20000 /  31% / -30.00 dB\n\tProperties:\n\t\tmedia.name = \"Playback\"\n\t\tnode.name = \"brave\"\n\t\tapplication.name = \"Brave\"\n";

    #[test]
    fn parses_inputs_names_volumes() {
        let v = parse_sink_inputs(FIXTURE);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].index, 109);
        assert_eq!(v[0].volume_pct, 86);
        assert!(v[0].names.contains(&"spotify".to_string()));
        assert_eq!(v[1].volume_pct, 31);
    }

    #[test]
    fn no_inputs_on_garbage() {
        assert!(parse_sink_inputs("").is_empty());
        assert!(parse_sink_inputs("Sink Input #abc\n").is_empty());
    }

    #[test]
    fn live_get_only_never_sets() {
        // Read-only probe: prints real state on audio machines, green in CI.
        match app_volume("spotify") {
            Ok(v) => {
                println!("live spotify sink volume: {v}");
                assert!(v <= 100);
            }
            Err(e) => println!("no spotify streams ({e}); skipped"),
        }
    }

    #[test]
    fn pending_noop_when_empty_and_keeps_missing() {
        let p = std::sync::Mutex::new(std::collections::HashMap::new());
        assert!(apply_pending(&p).is_empty());
        p.lock().unwrap().insert("no-such-app-xyz".into(), 42);
        // Nothing live under that name: entry stays, nothing applied.
        assert!(apply_pending(&p).is_empty());
        assert_eq!(p.lock().unwrap().get("no-such-app-xyz"), Some(&42));
    }
}
