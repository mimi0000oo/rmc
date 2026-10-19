/* RMC tab reporter — runs on any page with media. Reads MediaSession state,
 * reports it to the server (ingestion), and actuates TabControl downlink frames
 * on the local <audio>/<video> element. Dormant when no media and no ?tab=. */
(() => {
  "use strict";
  // Params come from our own <script src> (bookmarklet injection) or the page URL.
  let query = location.search;
  try {
    const self = document.currentScript;
    if (self && self.src && self.src.includes("?")) query = self.src.slice(self.src.indexOf("?"));
  } catch {}
  const params = new URLSearchParams(query);
  // Identity: stable base per tab (sessionStorage) + per-page-load nonce.
  // The nonce makes duplicated/cloned tabs distinct (sessionStorage is copied
  // on clone); the pagehide beacon unreports our full id so reloads don't leak.
  let base = params.get("tab") || null;
  try {
    if (base) sessionStorage.setItem("rmc-tab-base", base);
    else base = sessionStorage.getItem("rmc-tab-base") || null;
  } catch {}
  const nonce = ((crypto.randomUUID && crypto.randomUUID().replace(/-/g, "").slice(0, 8))
    || Math.random().toString(36).slice(2, 10));
  let tabId = base ? `${base}-${nonce}` : `tab-${nonce}`;
  const hasMedia = () =>
    !!(document.querySelector("audio,video") ||
      (navigator.mediaSession && navigator.mediaSession.metadata));

  if (!base && !params.get("tab") && !hasMedia()) return; // dormant on plain UI
  if (typeof RmcClient === "undefined") return; // rmc-client.js must load first

  // Server base override for bookmarklet use (?rmc=https://host:port):
  // the reporter talks to RMC, not to whatever page injected it.
  let BASE = location.origin;
  try {
    const u = new URL(params.get("rmc") || location.origin);
    if (/^https?:$/.test(u.protocol)) BASE = u.origin;
  } catch {}
  const wsUrl = BASE.replace(/^http/, "ws") + "/ws";
  const client = new RmcClient(wsUrl, { tabId, base: BASE });
  client.connect();

  function mediaEl() { return document.querySelector("audio,video"); }

  function snapshot() {
    const md = navigator.mediaSession?.metadata || null;
    const el = mediaEl();
    let status = "stopped";
    if (navigator.mediaSession?.playbackState === "playing" || (el && !el.paused && !el.ended)) status = "playing";
    else if (md || el) status = "paused";
    let artwork = null;
    try {
      const arts = md && md.artwork;
      if (arts && arts.length) artwork = arts[arts.length - 1].src || arts[0].src || null;
    } catch {}
    return {
      tab_id: tabId,
      title: md?.title || document.title || null,
      artist: md?.artist || null,
      album: md?.album || null,
      duration_ms: el && isFinite(el.duration) ? Math.floor(el.duration * 1000) : null,
      position_ms: el && isFinite(el.currentTime) ? Math.floor(el.currentTime * 1000) : null,
      volume: el ? Math.round(el.volume * 100) : null,
      status,
      page_url: location.href,
      artwork_url: artwork,
    };
  }

  let last = "";
  function push() {
    const s = snapshot();
    const key = JSON.stringify(s);
    if (key !== last) { last = key; client.report(s); }
  }

  client.onTabControl((msg) => {
    const el = mediaEl();
    if (!el) return;
    try {
      switch (msg.action) {
        case "play": el.play().catch(() => {}); break;
        case "pause": el.pause(); break;
        case "toggle": el.paused ? el.play().catch(() => {}) : el.pause(); break;
        case "seek": if (msg.position_ms != null) el.currentTime = msg.position_ms / 1000; break;
        case "volume": if (msg.level != null) el.volume = msg.level / 100; break;
        case "next":
        case "prev":
          // No queue concept on a bare element: surface via MediaSession handlers if set.
          break;
      }
    } finally { setTimeout(push, 300); }
  });

  setInterval(push, 2000);
  ["play", "pause", "seeked", "volumechange", "loadedmetadata"].forEach((ev) =>
    document.addEventListener(ev, push, true));
  // Lifecycle: tell the server this tab is gone so its source doesn't leak.
  // (60s heartbeat-expiry sweeper is the backstop for crashed tabs.)
  window.addEventListener("pagehide", () => {
    try {
      const blob = new Blob([JSON.stringify({ tab_id: tabId })], { type: "application/json" });
      navigator.sendBeacon(BASE + "/api/unreport", blob);
    } catch {}
  });
  try {
    if (navigator.mediaSession) {
      navigator.mediaSession.setActionHandler("play", () => mediaEl()?.play().catch(() => {}));
      navigator.mediaSession.setActionHandler("pause", () => mediaEl()?.pause());
    }
  } catch {}
})();
