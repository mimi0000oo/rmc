/** RMC client — canonical WS client. Implements client-connection/src/lib.rs envelopes.
 * Plain script (no modules): exposes window.RmcClient.
 * PROTOCOL_VERSION must match server; mismatch surfaces via onError, never silent.
 */
class RmcClient {
  constructor(url, opts = {}) {
    this.url = url;
    this.tabId = opts.tabId || null;
    // Cross-origin mode (bookmarklet): talk to the RMC server, not this page.
    this.base = (opts.base || "").replace(/\/$/, "");
    this.ws = null;
    this.state = { active_id: null, sources: [], revision: 0 };
    this.conn = "offline"; // offline | connecting | connected | reconnecting
    this.stateListeners = new Set();
    this.connListeners = new Set();
    this.errorListeners = new Set();
    this.ackListeners = new Set();
    this.tabControlListeners = new Set();
    this.baseDelayMs = 1000;
    this.maxDelayMs = 15000;
    this.attempt = 0;
    this.wantClose = false;
    this.PROTOCOL_VERSION = 2;
  }
  onState(fn) { this.stateListeners.add(fn); return () => this.stateListeners.delete(fn); }
  onConn(fn) { this.connListeners.add(fn); return () => this.connListeners.delete(fn); }
  onError(fn) { this.errorListeners.add(fn); return () => this.errorListeners.delete(fn); }
  onAck(fn) { this.ackListeners.add(fn); return () => this.ackListeners.delete(fn); }
  onTabControl(fn) { this.tabControlListeners.add(fn); return () => this.tabControlListeners.delete(fn); }
  _setConn(c) { this.conn = c; for (const fn of this.connListeners) { try { fn(c); } catch (e) { console.error(e); } } }
  _emitState() { for (const fn of this.stateListeners) { try { fn(this.state); } catch (e) { console.error(e); } } }
  _emitError(err) { for (const fn of this.errorListeners) { try { fn(err); } catch (e) { console.error(e); } } }
  _delay() {
    // 1s doubling to 15s cap + jitter. Mirrors reconnect_delay_ms in Rust.
    const d = Math.min(this.baseDelayMs * 2 ** Math.min(this.attempt, 4), this.maxDelayMs);
    return d + Math.floor(Math.random() * 500);
  }
  connect() {
    this.wantClose = false;
    this.attempt = 0;
    this._open();
  }
  _open() {
    this._setConn(this.attempt === 0 ? "connecting" : "reconnecting");
    let ws;
    try { ws = new WebSocket(this.url); } catch (e) { return this._retry(); }
    this.ws = ws;
    ws.onopen = () => {
      this.attempt = 0;
      this._setConn("connected");
      this.send({ type: "hello", client: "web-ui", protocol_version: this.PROTOCOL_VERSION, tab_id: this.tabId });
    };
    ws.onmessage = (ev) => {
      let msg;
      try { msg = JSON.parse(ev.data); } catch (e) { console.error("rmc: bad frame", e); return; }
      this._dispatch(msg);
    };
    ws.onclose = () => { if (!this.wantClose) this._retry(); else this._setConn("offline"); };
    ws.onerror = () => { try { ws.close(); } catch {} };
  }
  _retry() {
    this._setConn("reconnecting");
    const d = this._delay();
    this.attempt += 1;
    setTimeout(() => { if (!this.wantClose) this._open(); }, d);
  }
  /** Dispatch one parsed server frame. Split out for node smoke tests. */
  _dispatch(msg) {
    if (!msg || typeof msg.type !== "string") return;
    if (msg.type === "state") {
      if (typeof msg.revision === "number" && msg.revision < this.state.revision) return; // stale guard
      this.state = { active_id: msg.active_id ?? null, sources: msg.sources ?? [], revision: msg.revision ?? 0 };
      if (msg.protocol_version !== undefined && msg.protocol_version !== this.PROTOCOL_VERSION) {
        this._emitError({ code: "version_mismatch", message: `protocol v${msg.protocol_version} vs client v${this.PROTOCOL_VERSION}` });
      }
      this._emitState();
    } else if (msg.type === "error") {
      this._emitError(msg);
    } else if (msg.type === "ack") {
      for (const fn of this.ackListeners) { try { fn(msg); } catch (e) { console.error(e); } }
    } else if (msg.type === "tab_control") {
      for (const fn of this.tabControlListeners) { try { fn(msg); } catch (e) { console.error(e); } }
    }
  }
  disconnect() { this.wantClose = true; try { this.ws?.close(); } catch {} this._setConn("offline"); }
  get isOpen() { return !!this.ws && this.ws.readyState === WebSocket.OPEN; }
  send(obj) { if (this.isOpen) { this.ws.send(JSON.stringify(obj)); return true; } return false; }
  _req() { return Math.random().toString(36).slice(2) + Date.now().toString(36); }
  async _rest(path, body) {
    const r = await fetch(this.base + path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
    const j = await r.json().catch(() => ({}));
    if (!r.ok || j.ok === false) this._emitError({ code: j.code || "internal", message: j.error || `HTTP ${r.status}` });
    return j;
  }
  switchTo(id) {
    const body = { id, request_id: this._req() };
    // Single transport: WS when live, REST only as offline fallback. Never both.
    if (!this.send({ type: "switch", ...body })) this._rest("/api/active", body).catch((e) => this._emitError({ code: "internal", message: String(e) }));
  }
  control(id, action, extra = {}) {
    const body = { id, action, request_id: this._req(), ...extra };
    if (!this.send({ type: "control", ...body })) this._rest("/api/control", body).catch((e) => this._emitError({ code: "internal", message: String(e) }));
  }
  report(tab) {
    const body = { tab, request_id: this._req() };
    if (!this.send({ type: "report", ...body })) this._rest("/api/report", body).catch((e) => this._emitError({ code: "internal", message: String(e && e.message || e) }));
  }
}
if (typeof window !== "undefined") window.RmcClient = RmcClient;
if (typeof module !== "undefined") module.exports = { RmcClient };
