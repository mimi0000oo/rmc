//! rmc-server binary (integrator-owned): wires all subsystem modules.

use rmc::backend::{
    AdapterError, AdapterResult, ControlAction, MediaSource, PlaybackState, PlaybackStatus,
    SourceKind, SourceRegistry, TrackInfo,
};
use rmc::webserver::{broadcast_state, snapshot, spawn_sweeper, AppState};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::{broadcast, RwLock};

/// First-run seeds so the UI is never blank. Owned HERE, not by the domain
/// crate: live adapters (MPRIS watch, tab reports) upsert the same ids.
fn demo_sources() -> Vec<MediaSource> {
    let now = chrono::Utc::now();
    vec![
        MediaSource {
            id: "spotify:local".into(),
            kind: SourceKind::Spotify,
            name: "Spotify Desktop".into(),
            origin: "demo-seed".into(),
            state: PlaybackState {
                status: PlaybackStatus::Paused,
                track: TrackInfo {
                    title: Some("Demo Track".into()),
                    artist: Some("Demo Artist".into()),
                    album: Some("Demo Album".into()),
                    duration_ms: Some(210_000),
                    art_url: None,
                },
                position_ms: 42_000,
                volume: 70,
                updated_at: now,
            },
            last_seen: now,
            actuator_error: None,
        },
        MediaSource {
            id: "browser:tab-1".into(),
            kind: SourceKind::Browser,
            name: "Browser — lofi mix".into(),
            origin: "demo-seed".into(),
            state: PlaybackState {
                status: PlaybackStatus::Stopped,
                track: TrackInfo::default(),
                position_ms: 0,
                volume: 80,
                updated_at: now,
            },
            last_seen: now,
            actuator_error: None,
        },
    ]
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let port: u16 = std::env::var("RMC_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8973);
    let frontend_dir = std::env::var("RMC_FRONTEND").unwrap_or_else(|_| "frontend".to_string());

    // Bind address: bare IP or CIDR subnet (RMC_BIND=10.42.0.0/24 listens only
    // on our address inside that subnet instead of the whole LAN).
    let bind_spec = std::env::var("RMC_BIND").unwrap_or_else(|_| "0.0.0.0".to_string());
    let bind_ip = match resolve_bind(&bind_spec) {
        Ok(ip) => ip,
        Err(e) => {
            eprintln!("RMC_BIND error: {e}");
            eprintln!("local addresses: {:?}", local_ipv4_addrs());
            std::process::exit(1);
        }
    };

    let (tx, _) = broadcast::channel::<rmc::protocol::ServerMsg>(64);
    // Demo seeds are OPT-IN (RMC_DEMO=1): default boot shows honest emptiness,
    // never a fake Spotify. Gate-mandated: fake presence is a ship-blocker.
    let demo = std::env::var("RMC_DEMO").is_ok_and(|v| v != "0" && !v.is_empty());
    let mut reg = SourceRegistry::new();
    if demo {
        for s in demo_sources() {
            reg.register(s);
        }
    }
    let registry = Arc::new(RwLock::new(reg));

    // Persisted settings (tokens, modes): file is truth across restarts,
    // .env + env vars seed it once (real env wins). Secrets never leave masked.
    let settings_path = rmc::settings::ServerSettings::path();
    let mut settings_data = rmc::settings::ServerSettings::load(&settings_path);
    let dotenv_path = rmc::settings::ServerSettings::dotenv_path();
    if let Ok(text) = std::fs::read_to_string(&dotenv_path) {
        let file_vars = rmc::settings::parse_dotenv(&text);
        if settings_data.seed_from_map(&file_vars, false) {
            println!("seeded settings from {}", dotenv_path.display());
        }
    }
    if settings_data.seed_from_env() {
        if let Err(e) = settings_data.save(&settings_path) {
            tracing::warn!("could not persist seeded settings: {e}");
        } else {
            println!("saved credentials from env to {}", settings_path.display());
        }
    }
    // Legacy convenience: a ./spotify file with "id: ...", "secret: ...",
    // "token: ..." lines imports once into settings (then delete the file).
    if settings_data.spotify_id.is_none() {
        if let Ok(text) = std::fs::read_to_string("spotify") {
            let mut got = false;
            for line in text.lines() {
                let line = line.trim();
                for (slot, key) in [
                    (&mut settings_data.spotify_id, "id:"),
                    (&mut settings_data.spotify_secret, "secret:"),
                    (&mut settings_data.spotify_token, "token:"),
                ] {
                    if let Some(v) = line.strip_prefix(key) {
                        let v = v.trim().to_string();
                        if !v.is_empty() && slot.is_none() {
                            *slot = Some(v);
                            got = true;
                        }
                    }
                }
            }
            if got {
                match settings_data.save(&settings_path) {
                    Ok(()) => println!(
                        "imported Spotify credentials from ./spotify into {} (you can delete ./spotify now)",
                        settings_path.display()
                    ),
                    Err(e) => tracing::warn!("could not persist imported settings: {e}"),
                }
            }
        }
    }
    if !settings_data.has_search_creds() {
        println!("hint: open the Settings panel in the UI (or set RMC_SPOTIFY_ID + RMC_SPOTIFY_SECRET) for /api/search (see README)");
    }
    // Remembered browser volumes + settings Arc, shared by hook/watch/API.
    let pending = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let pending_hook = pending.clone();
    let settings = Arc::new(RwLock::new(settings_data));
    let settings_hook = settings.clone();
    // Desktop player watch: ALL MPRIS players (Spotify, browsers, VLC...).
    // Soft-fails without a session bus. Applies remembered volumes on start.
    tokio::spawn(rmc::mpris::run_watch(
        registry.clone(),
        pending.clone(),
        tx.clone(),
        2,
    ));
    // Side-effect hook: actuate any MPRIS player from the source's origin
    // (`mpris:{bus}`). Runs on the webserver's blocking thread with a 2s
    // bound; AdapterResult feeds the reconcile path (no silent ok:true).
    let on_control: rmc::webserver::ControlHook = Arc::new(
        move |id: String, origin: Option<String>, action: ControlAction| -> AdapterResult {
            match origin {
                Some(o) if o.starts_with("mpris:") => {
                    let bus = o.trim_start_matches("mpris:").to_string();
                    // Volume mode from settings (blocking lock: hook runs on a
                    // blocking thread, so this is safe).
                    let mode = settings_hook.blocking_read().volume_mode;
                    let system_volume = |slug: &str, level: u8| -> Result<(), String> {
                        match rmc::sysaudio::set_app_volume(slug, level) {
                            Ok(_) => Ok(()),
                            Err(e) if e.contains("no live audio streams") => {
                                if let Ok(mut g) = pending_hook.lock() {
                                    g.insert(slug.to_string(), level);
                                }
                                tracing::info!("volume remembered for {id} (applies on playback)");
                                Ok(())
                            }
                            Err(e) => Err(format!("system volume failed: {e}")),
                        }
                    };
                    let is_browser_vol = id.starts_with("browser:")
                        && matches!(&action, ControlAction::Volume { .. });
                    // System mode skips MPRIS entirely for browser volume.
                    if is_browser_vol && mode == rmc::settings::VolumeMode::System {
                        let ControlAction::Volume { level } = action else {
                            unreachable!()
                        };
                        let slug = id.trim_start_matches("browser:").to_string();
                        let r = system_volume(&slug, level);
                        if let Err(e) = &r {
                            tracing::warn!("system volume failed for {id}: {e}");
                        }
                        return r.map_err(AdapterError::Transport);
                    }
                    let r = rmc::mpris::control_bus(&bus, &action);
                    // Auto mode: Chromium ignores MPRIS volume, so fall back
                    // to whole-app system volume (Mpris mode: honest error).
                    let r = match (&r, &action) {
                        (Err(_), ControlAction::Volume { level })
                            if is_browser_vol && mode == rmc::settings::VolumeMode::Auto =>
                        {
                            let slug = id.trim_start_matches("browser:").to_string();
                            system_volume(&slug, *level)
                        }
                        _ => r,
                    };
                    match &r {
                        Ok(()) => tracing::info!("mpris actuated {id}"),
                        Err(e) => tracing::warn!("mpris actuation failed for {id}: {e}"),
                    }
                    r.map_err(AdapterError::Transport)
                }
                _ => Ok(()),
            }
        },
    );

    let tls_port: u16 = std::env::var("RMC_TLS_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8974);
    let tls_off = std::env::var("RMC_TLS").is_ok_and(|v| v == "0");
    let tls_dir = std::env::var("RMC_TLS_DIR").unwrap_or_else(|_| "./.rmc-tls".to_string());
    // HTTPS twin (self-signed): browsers block http subresources on https
    // media pages, and the UI is fully usable over either port.
    let tls_cfg = if tls_off {
        None
    } else {
        ensure_tls(&tls_dir, &bind_ip.to_string()).await
    };
    if tls_cfg.is_some() {
        println!(
            "RMC https on https://{bind_ip}:{tls_port}/ (self-signed; accept once in browser)"
        );
    } else if !tls_off {
        println!("TLS disabled: cert setup failed (see warnings); http only");
    }

    let state = AppState {
        registry: registry.clone(),
        tx: tx.clone(),
        tabs: Arc::new(RwLock::new(std::collections::HashMap::new())),
        on_control: Some(on_control),
        settings: settings.clone(),
        settings_path: settings_path.clone(),
        token_cache: Arc::new(tokio::sync::Mutex::new(None)),
        art_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        tls_port: tls_cfg.as_ref().map(|_| tls_port),
        frontend_dir: frontend_dir.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    // Tab ghost sweeper (webserver-owned, broadcasts on eviction).
    spawn_sweeper(&state);

    let snap = snapshot(&state).await;
    broadcast_state(&state, &snap);

    let app = rmc::webserver::build_router(state);
    // Reachable bind: the resolved RMC_BIND address on both twins.
    let addr = SocketAddr::from((std::net::IpAddr::V4(bind_ip), port));
    println!("RMC server on http://{bind_ip}:{port}/  (frontend: {frontend_dir})");
    if demo {
        println!("RMC_DEMO=1: demo seeds registered (fake presence for screenshots)");
    }
    if !settings.read().await.has_search_creds() {
        println!("hint: open the Settings panel in the UI (or set RMC_SPOTIFY_ID + RMC_SPOTIFY_SECRET) for /api/search (see README)");
    }
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let http = axum::serve(listener, app.clone());

    match tls_cfg {
        Some(config) => {
            let tls_addr = SocketAddr::from((std::net::IpAddr::V4(bind_ip), tls_port));
            let https = axum_server::bind_rustls(tls_addr, config).serve(app.into_make_service());
            tokio::join!(async { http.await.unwrap() }, async {
                https.await.unwrap()
            });
        }
        None => {
            http.await.unwrap();
        }
    }
}

/// Ensure a self-signed cert covering localhost + LAN IP + the bound address.
/// Returns a rustls config, regenerating when the address set changed.
async fn ensure_tls(dir: &str, bound: &str) -> Option<axum_server::tls_rustls::RustlsConfig> {
    let lan = lan_ip();
    let meta = format!("{dir}/lan.txt");
    let cert_p = format!("{dir}/cert.pem");
    let key_p = format!("{dir}/key.pem");
    // Freshness covers both the default-route LAN IP and the bound address:
    // either changing (hotspot hop, RMC_BIND change) regenerates the cert.
    let want_meta = if lan == bound || lan.is_empty() {
        bound.to_string()
    } else {
        format!("{lan} {bound}")
    };
    let fresh = std::fs::read_to_string(&meta)
        .ok()
        .is_some_and(|m| m.trim() == want_meta)
        && std::path::Path::new(&cert_p).exists()
        && std::path::Path::new(&key_p).exists();
    if !fresh {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("tls dir failed: {e}");
            return None;
        }
        let mut sans = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ];
        if !lan.is_empty() && lan != "127.0.0.1" && !sans.contains(&lan) {
            sans.push(lan.clone());
        }
        if !sans.contains(&bound.to_string()) {
            sans.push(bound.to_string());
        }
        let key = match rcgen::generate_simple_self_signed(sans) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("cert generate failed: {e}");
                return None;
            }
        };
        if std::fs::write(&cert_p, key.cert.pem()).is_err()
            || std::fs::write(&key_p, key.signing_key.serialize_der()).is_err()
            || std::fs::write(&meta, &want_meta).is_err()
        {
            tracing::warn!("cert store failed");
            return None;
        }
        println!("generated self-signed cert for {lan} in {dir}/");
    }
    let (cert, key) = (std::fs::read(&cert_p).ok()?, std::fs::read(&key_p).ok()?);
    // axum-server from_pem wants PEM for both; convert DER key.
    let key_pem = if key.starts_with(b"-----BEGIN") {
        key
    } else {
        der_to_pem(&key, "PRIVATE KEY").into_bytes()
    };
    match axum_server::tls_rustls::RustlsConfig::from_pem(cert, key_pem).await {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!("tls config failed: {e}");
            None
        }
    }
}

fn der_to_pem(der: &[u8], label: &str) -> String {
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::from("-----BEGIN ");
    out.push_str(label);
    out.push_str("-----\n");
    let mut bits: u32 = 0;
    let mut acc: u32 = 0;
    let mut col = 0;
    for &b in der {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(B64[((acc >> bits) & 63) as usize] as char);
            col += 1;
            if col == 64 {
                out.push('\n');
                col = 0;
            }
        }
    }
    if bits > 0 {
        out.push(B64[((acc << (6 - bits)) & 63) as usize] as char);
    }
    if col != 0 {
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out
}

/// Bind-address resolution: `RMC_BIND` accepts a bare IP (`127.0.0.1`,
/// `10.42.0.165`) or a CIDR subnet (`10.42.0.0/24`, `0.0.0.0/0`).
/// A CIDR resolves to the machine's own address inside that subnet, so the
/// server listens only there instead of the whole LAN.
fn parse_ip_or_cidr(spec: &str) -> Result<(std::net::Ipv4Addr, u8), String> {
    let spec = spec.trim();
    if let Some((addr, prefix)) = spec.split_once('/') {
        let ip: std::net::Ipv4Addr = addr
            .trim()
            .parse()
            .map_err(|_| format!("bad IPv4 address in '{spec}'"))?;
        let prefix: u8 = prefix
            .trim()
            .parse()
            .map_err(|_| format!("bad prefix length in '{spec}' (want 0-32)"))?;
        if prefix > 32 {
            return Err(format!("bad prefix length in '{spec}' (want 0-32)"));
        }
        Ok((ip, prefix))
    } else {
        let ip: std::net::Ipv4Addr = spec.parse().map_err(|_| {
            format!("bad bind address '{spec}' (want an IPv4 address or CIDR like 10.42.0.0/24)")
        })?;
        Ok((ip, 32))
    }
}

fn subnet_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

fn in_subnet(ip: std::net::Ipv4Addr, net: std::net::Ipv4Addr, prefix: u8) -> bool {
    u32::from(ip) & subnet_mask(prefix) == u32::from(net) & subnet_mask(prefix)
}

/// All local IPv4 addresses: `ip` command first, `/proc/net/fib_trie` fallback.
fn local_ipv4_addrs() -> Vec<std::net::Ipv4Addr> {
    if let Ok(out) = std::process::Command::new("ip")
        .args(["-o", "-4", "addr", "show"])
        .output()
    {
        if out.status.success() {
            // Tokens look like: <iface> <flags> <name> inet <addr>/<plen> brd <bcast> ...
            // Skip broadcast addresses (the token after `brd`).
            let text = String::from_utf8_lossy(&out.stdout);
            let mut addrs: Vec<std::net::Ipv4Addr> = vec![];
            let mut skip_next = false;
            for tok in text.split_whitespace() {
                if skip_next {
                    skip_next = false;
                    continue;
                }
                if tok == "brd" {
                    skip_next = true;
                    continue;
                }
                if let Some(ip) = tok
                    .split('/')
                    .next()
                    .and_then(|a| a.parse::<std::net::Ipv4Addr>().ok())
                {
                    if !addrs.contains(&ip) {
                        addrs.push(ip);
                    }
                }
            }
            if !addrs.is_empty() {
                return addrs;
            }
        }
    }
    // Fallback: kernel FIB trie, `|-- <ip>` lines followed by `host LOCAL`.
    let mut addrs = vec![];
    if let Ok(text) = std::fs::read_to_string("/proc/net/fib_trie") {
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("|-- ") {
                if let Ok(ip) = rest.trim().parse::<std::net::Ipv4Addr>() {
                    let next = lines.peek().copied().unwrap_or("");
                    if next.contains("host LOCAL") && !addrs.contains(&ip) {
                        addrs.push(ip);
                    }
                }
            }
        }
    }
    addrs
}

fn resolve_bind(spec: &str) -> Result<std::net::Ipv4Addr, String> {
    let (net, prefix) = parse_ip_or_cidr(spec)?;
    if prefix == 32 {
        return Ok(net);
    }
    let addrs = local_ipv4_addrs();
    addrs
        .into_iter()
        .find(|ip| in_subnet(*ip, net, prefix))
        .ok_or_else(|| format!("no local address inside {spec}"))
}

/// Primary LAN IP via the UDP-socket trick (no packets sent).
fn lan_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_parsing() {
        let ip: std::net::Ipv4Addr = "10.42.0.165".parse().unwrap();
        assert_eq!(parse_ip_or_cidr("10.42.0.165").unwrap(), (ip, 32));
        let net: std::net::Ipv4Addr = "10.42.0.0".parse().unwrap();
        assert_eq!(parse_ip_or_cidr("10.42.0.0/24").unwrap(), (net, 24));
        assert_eq!(parse_ip_or_cidr("  0.0.0.0/0 ").unwrap().1, 0);
        assert!(parse_ip_or_cidr("nope").is_err());
        assert!(parse_ip_or_cidr("10.0.0.0/33").is_err());
        assert!(parse_ip_or_cidr("10.0.0.0/abc").is_err());
    }

    #[test]
    fn subnet_membership() {
        let net: std::net::Ipv4Addr = "10.42.0.0".parse().unwrap();
        let inside: std::net::Ipv4Addr = "10.42.0.165".parse().unwrap();
        let outside: std::net::Ipv4Addr = "192.168.1.5".parse().unwrap();
        assert!(in_subnet(inside, net, 24));
        assert!(!in_subnet(outside, net, 24));
        assert!(in_subnet(outside, net, 0));
        assert!(in_subnet(inside, inside, 32));
        assert!(!in_subnet(outside, inside, 32));
    }

    #[test]
    fn resolve_bare_ip_is_identity() {
        let ip: std::net::Ipv4Addr = "127.0.0.1".parse().unwrap();
        assert_eq!(resolve_bind("127.0.0.1").unwrap(), ip);
        assert!(resolve_bind("240.0.0.0/4").is_err());
    }
}
