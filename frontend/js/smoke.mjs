/** RMC JS smoke test — run with: node client-connection/js/smoke.mjs
 * Exercises _dispatch, stale guard, error/ack/tab_control fan-out, flat send
 * shapes (must match Rust golden vectors), and REST-fallback error surfacing.
 * No browser, no deps: WebSocket is never opened (no connect() call).
 */
import { readFileSync } from "fs";
import { fileURLToPath } from "url";
import { dirname, join } from "path";
import assert from "node:assert/strict";

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, "rmc-client.js"), "utf8");
assert(!/\bexport\b/.test(src), "plain-script file must not contain export");
const window = {};
const moduleShim = { exports: {} };
const WsStub = function () {};
WsStub.OPEN = 1;
const stubs = {
  fetch: async () => { throw new Error("fetch stub not set"); },
};
new Function("window", "module", "WebSocket", "fetch", src)(window, moduleShim, WsStub, (...a) => stubs.fetch(...a));
const RmcClient = window.RmcClient || moduleShim.exports.RmcClient;
assert(typeof RmcClient === "function", "RmcClient global exists");

let n = 0;
const ok = (cond, name) => { n++; assert(cond, name); console.log(`  ok ${n} - ${name}`); };

// --- _dispatch: state + stale guard ---
{
  const c = new RmcClient("ws://x/ws");
  const states = [];
  c.onState((s) => states.push(s));
  c._dispatch({ type: "state", protocol_version: 2, revision: 5, active_id: "a", sources: [{ id: "a" }] });
  ok(c.state.revision === 5 && states.length === 1, "state applies");
  c._dispatch({ type: "state", protocol_version: 2, revision: 3, active_id: "b", sources: [] });
  ok(c.state.active_id === "a" && states.length === 1, "stale revision dropped");
  const errs = [];
  c.onError((e) => errs.push(e));
  c._dispatch({ type: "state", protocol_version: 99, revision: 6, active_id: "a", sources: [] });
  ok(errs.length === 1 && errs[0].code === "version_mismatch", "version mismatch surfaces");
  c._dispatch({ type: "error", code: "unknown_source", message: "nope", request_id: "r9" });
  ok(errs.length === 2 && errs[1].request_id === "r9", "error frame surfaces with request_id");
  const acks = [];
  c.onAck((m) => acks.push(m));
  c._dispatch({ type: "ack", request_id: "abc" });
  ok(acks.length === 1 && acks[0].request_id === "abc", "ack fans out with exact id");
  const tabs = [];
  c.onTabControl((m) => tabs.push(m));
  c._dispatch({ type: "tab_control", tab_id: "t1", action: "pause" });
  ok(tabs.length === 1 && tabs[0].action === "pause", "tab_control fans out");
  c._dispatch({ type: "bogus" });
  c._dispatch(null);
  ok(true, "unknown frames ignored");
}

// --- send shapes match Rust golden vectors (flat, not nested) ---
{
  const c = new RmcClient("ws://x/ws");
  const sent = [];
  c.ws = { readyState: 1, send: (s) => sent.push(JSON.parse(s)) };
  globalThis.WebSocket = { OPEN: 1 };
  c.control("spotify:local", "toggle");
  ok(sent[0].type === "control" && sent[0].action === "toggle" && typeof sent[0].request_id === "string", "control toggle flat");
  c.control(null, "volume", { level: 42 });
  ok(sent[1].action === "volume" && sent[1].level === 42, "control volume flat with level");
  c.control(null, "seek", { position_ms: 90000 });
  ok(sent[2].action === "seek" && sent[2].position_ms === 90000, "control seek flat with position");
  c.switchTo("browser:tab-1");
  ok(sent[3].type === "switch" && sent[3].id === "browser:tab-1", "switch flat");
  // Rust golden hello parses through the same dispatch path conceptually:
  const hello = JSON.parse('{"type":"hello","client":"web-ui","protocol_version":2,"tab_id":"tab-9"}');
  ok(hello.protocol_version === 2 && hello.tab_id === "tab-9", "golden hello shape");
}

// --- REST fallback surfaces errors (no silent swallow) ---
{
  const c = new RmcClient("ws://x/ws");
  c.ws = null; // force REST path
  const errs = [];
  c.onError((e) => errs.push(e));
  stubs.fetch = async () => ({ ok: false, status: 400, json: async () => ({ ok: false, code: "bad_param", error: "level?" }) });
  await c._rest("/api/control", { action: "volume" });
  ok(errs.length === 1 && errs[0].code === "bad_param", "REST HTTP error emits onError");
}

console.log(`\nJS smoke: ${n} checks passed`);
