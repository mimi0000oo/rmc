/* RMC frontend — vanilla, no build. Keyed render preserves focus across pushes. */
(() => {
  "use strict";
  if (typeof RmcClient === "undefined") {
    document.getElementById("conn").textContent = "client failed to load";
    document.getElementById("sources").innerHTML =
      `<p class="empty">Could not load /js/rmc-client.js. Check the server and reload.</p>`;
    return;
  }
  const wsUrl = (location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/ws";
  const client = new RmcClient(wsUrl, { base: "" });
  const elSources = document.getElementById("sources");
  const elActive = document.getElementById("active");
  const elConn = document.getElementById("conn");
  const elCount = document.getElementById("count");
  const elToast = document.getElementById("toast");
  const vol = document.getElementById("vol");
  const volnum = document.getElementById("volnum");
  const seek = document.getElementById("seek");
  const elPos = document.getElementById("pos");
  const elDur = document.getElementById("dur");
  document.getElementById("lan").textContent = `RMC · LAN: open ${location.protocol}//${location.host}/ on your phone`;

  const KIND_ICON = { spotify: "🟢", browser: "🌐", youtubemusic: "▶️", system: "🔊" };
  const KIND_CLASS = { spotify: "k-spotify", browser: "k-browser", youtubemusic: "k-ytm", system: "k-sys" };

  let lastState = { active_id: null, sources: [] };
  let receivedAt = Date.now();
  const nodes = new Map(); // id -> button element
  let toastTimer = null;

  function toast(msg) {
    elToast.hidden = false;
    elToast.textContent = msg;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { elToast.hidden = true; }, 4000);
  }

  function esc(x) { return String(x ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])); }

  // Defensive: accept flat wire shape (current) and nested {state:{...}} (spec).
  // Unknowns stay null (rendered as "—"), never fabricated into 70%/0:00.
  function norm(s) {
    const st = s.state && typeof s.state === "object" ? s.state : s;
    const vol = st.volume ?? st.track?.volume ?? null;
    const pos = st.position_ms ?? st.track?.position_ms ?? null;
    return {
      id: s.id, kind: String(s.kind || "").toLowerCase(), kind_label: s.kind_label || s.kind || "?",
      name: s.name || s.id, origin: s.origin || "",
      status: st.status || "stopped",
      title: st.title || null, artist: st.artist || null,
      position_ms: pos == null ? null : Number(pos),
      duration_ms: st.duration_ms ?? st.track?.duration_ms ?? null,
      volume: vol == null ? null : Number(vol),
      actuator_error: s.actuator_error || null,
      art_url: st.art_url ?? st.track?.art_url ?? s.art_url ?? null,
    };
  }

  function fmt(ms) {
    if (ms == null) return "—";
    const s = Math.max(0, Math.floor(ms / 1000));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = String(s % 60).padStart(2, "0");
    return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
  }

  function trackLabel(n) {
    if (!n.title && !n.artist) return "no track";
    return (n.title || "unknown") + (n.artist ? " — " + n.artist : "");
  }

  function badge(n) {
    const icon = KIND_ICON[n.kind] || "🎵";
    return `<span class="badge ${KIND_CLASS[n.kind] || ""}">${icon} ${esc(n.kind_label.toUpperCase())}</span>`;
  }

  function art(n, cls) {
    const u = n.art_url;
    if (typeof u !== "string" || !u) return `<span class="${cls} noart" aria-hidden="true">🎵</span>`;
    // http(s) loads directly; anything else (file:// thumbnails from Chromium)
    // goes through the id-scoped server proxy (browsers block file:// subresources).
    const src = /^https?:\/\//.test(u) ? u : `/api/art?src=${encodeURIComponent(n.id)}`;
    return `<img class="${cls}" src="${esc(src)}" alt="" loading="lazy" onerror="this.outerHTML='<span class=&quot;${cls} noart&quot;>🎵</span>'" />`;
  }

  function cardInner(n) {
    const live = n.status === "playing" ? `<span class="live" aria-label="playing">●</span>` : "";
    const deg = n.actuator_error ? `<span class="degraded" title="${esc(n.actuator_error)}">⚠ control failed</span>` : "";
    return `${art(n, "thumb")}<span class="cardtext">${badge(n)}<strong>${esc(n.name)} ${live}</strong>
      <span class="meta">${esc(n.status)} · ${esc(trackLabel(n))}</span>
      <span class="origin">${esc(n.origin)}</span>${deg}</span>`;
  }

  let lastRevision = -1;

  function render(state) {
    // Revision guard: a slow REST snapshot must never clobber newer WS state.
    const rev = typeof state.revision === "number" ? state.revision : lastRevision;
    if (rev < lastRevision) return;
    lastRevision = rev;
    lastState = state;
    receivedAt = Date.now();
    const sources = (state.sources || []).map(norm);
    elCount.textContent = sources.length ? `(${sources.length})` : "";
    if (!sources.length) {
      elSources.innerHTML = `<p class="empty">No sources yet. Play Spotify or a browser tab.</p>`;
      nodes.clear();
      elActive.innerHTML = `<p class="empty">Nothing active.</p>`;
      elActive._html = null;
      return;
    }
    // Keyed update: reuse nodes, never rebuild focused element.
    const seen = new Set();
    for (const n of sources) {
      seen.add(n.id);
      let b = nodes.get(n.id);
      if (!b) {
        b = document.createElement("button");
        b.className = "src";
        b.dataset.id = n.id;
        b.addEventListener("click", () => client.switchTo(n.id));
        nodes.set(n.id, b);
        elSources.appendChild(b);
      }
      const isActive = n.id === state.active_id;
      b.classList.toggle("active", isActive);
      b.setAttribute("aria-pressed", isActive ? "true" : "false");
      b.setAttribute("aria-label", `${n.name}, ${n.status}${isActive ? ", active stream" : ", switch to this stream"}`);
      const html = cardInner(n);
      if (b._html !== html) { b.innerHTML = html; b._html = html; }
    }
    for (const [id, b] of nodes) if (!seen.has(id)) { b.remove(); nodes.delete(id); }
    const empty = elSources.querySelector(".empty");
    if (empty) empty.remove();

    // No fabrication: unknown active_id renders empty, matching the guards.
    const a = sources.find((s) => s.id === state.active_id) || null;
    if (!a) {
      const na = `<p class="empty">Nothing active. Pick a stream.</p>`;
      if (elActive._html !== na) { elActive.innerHTML = na; elActive._html = na; }
      return;
    }
    const ahtml = `${art(a, "bigart")}<span class="cardtext">${badge(a)}<strong>${esc(a.name)}</strong>
      <span class="meta">${esc(trackLabel(a))}</span></span>`;
    if (elActive._html !== ahtml) { elActive.innerHTML = ahtml; elActive._html = ahtml; }
    if (document.activeElement !== vol) {
      if (a.volume == null) { volnum.textContent = "—"; vol.disabled = true; }
      else { vol.disabled = false; vol.value = a.volume; volnum.textContent = `${a.volume}%`; }
    }
    if (document.activeElement !== seek) {
      const hasDur = a.duration_ms != null && a.duration_ms > 0;
      seek.disabled = !hasDur;
      seek.max = hasDur ? a.duration_ms : 0;
      seek.value = a.position_ms == null ? 0 : (hasDur ? Math.min(a.position_ms, a.duration_ms) : 0);
      elPos.textContent = fmt(a.position_ms);
      elDur.textContent = fmt(a.duration_ms);
    }
  }

  // Tick position for the playing source between pushes.
  setInterval(() => {
    const a = (lastState.sources || []).map(norm).find((s) => s.id === lastState.active_id);
    if (!a || a.status !== "playing" || a.position_ms == null || document.activeElement === seek) return;
    const pos = a.position_ms + (Date.now() - receivedAt);
    elPos.textContent = fmt(pos);
    if (a.duration_ms) seek.value = Math.min(pos, a.duration_ms);
  }, 1000);

  function setConn(c) {
    const label = { connecting: "connecting…", connected: "connected", reconnecting: "reconnecting…", offline: "offline" }[c] || c;
    elConn.textContent = label;
    elConn.className = "conn " + (c === "connected" ? "ok" : c === "offline" ? "bad" : "warn");
  }

  client.onState(render);
  client.onConn(setConn);
  client.onError((e) => toast(`Error [${e.code || "unknown"}]: ${e.message || "request failed"}`));
  client.onAck(() => {});
  setConn("connecting");
  client.connect();
  fetch("/api/sources").then((r) => r.json()).then((snap) => {
    if (snap && snap.sources) render({ active_id: snap.active_id, sources: snap.sources, revision: snap.revision || 0 });
  }).catch((e) => { setConn(client.conn); toast(`Could not reach server: ${e}`); });

  function activeNorm() {
    return lastState.sources.map(norm).find((s) => s.id === lastState.active_id) || null;
  }
  // Every press sends immediately — no collapsing (double-next must skip twice).
  document.querySelectorAll("[data-act]").forEach((b) =>
    b.addEventListener("click", () => {
      const a = activeNorm();
      if (!a) { toast("No active source — pick a stream first."); return; }
      client.control(a.id, b.dataset.act);
    }));
  vol.addEventListener("input", () => { volnum.textContent = `${vol.value}%`; });
  vol.addEventListener("change", () => {
    const a = activeNorm();
    if (!a) { toast("No active source — pick a stream first."); return; }
    client.control(a.id, "volume", { level: Number(vol.value) });
  });
  seek.addEventListener("change", () => {
    const a = activeNorm();
    if (a && a.duration_ms) client.control(a.id, "seek", { position_ms: Number(seek.value) });
  });

  // Settings: presence flags only, secrets never come back.
  const setStatus = document.getElementById("setstatus");
  async function loadSettings() {
    try {
      const r = await fetch("/api/settings");
      const j = await r.json();
      document.getElementById("set-id").placeholder = j.spotify_id_set ? "set ✓ (hidden)" : "not set";
      document.getElementById("set-secret").placeholder = j.spotify_secret_set ? "set ✓ (hidden)" : "not set";
      document.getElementById("set-token").placeholder = j.spotify_token_set ? "set ✓ (hidden)" : "not set";
      document.getElementById("set-vol").value = j.volume_mode || "auto";
      setStatus.textContent = j.search_ready ? "· search ready ✓" : "· search off";
    } catch (e) { setStatus.textContent = "· unreachable"; }
  }
  loadSettings();
  document.getElementById("settingsform").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const body = {};
    for (const [key, id] of [["spotify_id", "set-id"], ["spotify_secret", "set-secret"], ["spotify_token", "set-token"]]) {
      const v = document.getElementById(id).value;
      if (v) body[key] = v; // empty = keep existing; API clears only on explicit ""
    }
    body.volume_mode = document.getElementById("set-vol").value;
    const r = await fetch("/api/settings", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
    if (r.ok) {
      toast("Settings saved");
      document.getElementById("set-id").value = "";
      document.getElementById("set-secret").value = "";
      document.getElementById("set-token").value = "";
      loadSettings();
    } else toast("Settings save failed");
  });
  document.getElementById("set-clear").addEventListener("click", async () => {
    await fetch("/api/settings", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ clear_tokens: true }) });
    toast("Tokens cleared");
    loadSettings();
  });

  // Spotify catalog search + play-by-URI.
  const form = document.getElementById("searchform");
  const q = document.getElementById("q");
  const results = document.getElementById("results");
  form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const query = q.value.trim();
    if (!query) return;
    results.innerHTML = `<p class="empty">Searching…</p>`;
    try {
      const r = await fetch("/api/search", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ q: query }) });
      const j = await r.json();
      if (!r.ok || j.ok === false) { results.innerHTML = `<p class="empty">${esc(j.error || ("HTTP " + r.status))}</p>`; return; }
      const tracks = j.tracks || [];
      if (!tracks.length) { results.innerHTML = `<p class="empty">No results.</p>`; return; }
      results.innerHTML = tracks.map((t, i) => `
        <div class="track" data-i="${i}">
          ${typeof t.art_url === "string" && /^https?:\/\//.test(t.art_url) ? `<img class="thumb" src="${esc(t.art_url)}" alt="" loading="lazy" />` : `<span class="thumb noart">🎵</span>`}
          <span class="cardtext"><strong>${esc(t.name)}</strong>
          <span class="meta">${esc(t.artists)} · ${esc(t.album)}</span></span>
          <button data-uri="${esc(t.uri)}" aria-label="Play ${esc(t.name)}">▶</button>
        </div>`).join("");
      results.querySelectorAll("button[data-uri]").forEach((b) =>
        b.addEventListener("click", async () => {
          const a = activeNorm();
          const rr = await fetch("/api/play", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ id: a ? a.id : null, uri: b.dataset.uri }) });
          const jj = await rr.json().catch(() => ({}));
          if (!rr.ok || jj.ok === false) toast(`Play failed: ${jj.error || ("HTTP " + rr.status)}`);
          else toast("Playing on " + (a ? a.name : "active stream"));
        }));
    } catch (e) { results.innerHTML = `<p class="empty">Search failed: ${esc(e)}</p>`; }
  });
})();
