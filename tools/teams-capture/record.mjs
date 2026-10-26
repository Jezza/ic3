#!/usr/bin/env node
// Passive network recorder for a Teams client running with
// `--remote-debugging-port` (teams-for-linux, Chrome, Edge). It only observes:
// it sends DevTools commands to the local browser and never sends a request to
// Microsoft itself.
//
// Usage: node tools/teams-capture/record.mjs [--port 9222] [--out <dir>]
//
// Writes `<dir>/events.jsonl` (default `captures/live-<YYYYMMDD-HHMMSS>/`), one
// JSON object per line: `req`, `res`, `body`, `ws-open`, `ws-in`, `ws-out`,
// `attach`, `mark`, `timeout`, `error`, `ready`, `closed`. Authorization
// headers and token-like headers are redacted, and sign-in request/response
// bodies are omitted, but bodies still hold real chat content: the output
// belongs under the git-ignored `captures/` and stays private.
//
// Start it before or after Teams: it waits for the debugging port. New pages
// and workers are attached paused and resumed once recording is on for them,
// so a recorder started first sees the very first requests (useful after
// clearing Teams' cache to capture a full sync). It exits when Teams closes.
import { appendFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const opt = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
};
const stamp = new Date().toISOString().replace(/[-:]/g, "").replace("T", "-").slice(0, 15);
const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const port = opt("--port", "9222");
const dir = resolve(opt("--out", join(repo, "captures", `live-${stamp}`)));
mkdirSync(dir, { recursive: true });
const out = join(dir, "events.jsonl");
const write = (o) => appendFileSync(out, JSON.stringify({ t: Date.now(), ...o }) + "\n");

const SECRET_HEADERS =
  /^(authorization|x-skypetoken|cookie|set-cookie|x-ms-.*token.*|.*authtoken.*|x-ms-session-id)$/i;
const NO_BODY = /login\.microsoftonline|login\.live|aadcdn|msauth|\/oauth2\//i;
const SKIP =
  /\.(js|mjs|css|png|jpe?g|gif|svg|woff2?|ttf|ico|webp|map|wasm|mp3|wav)(\?|$)|\/(static|assets|precompiled)\//i;
const COMMAND_TIMEOUT_MS = 4000;

// An Authorization scheme ("Bearer", "skype_token", …) is not secret and tells which token a
// service takes, so it is kept; everything after it is not.
const AUTH_SCHEME = /^([A-Za-z_-]{1,24}) /;
const redactValue = (k, v) => {
  const scheme = /^authorization$/i.test(k) && AUTH_SCHEME.exec(String(v));
  return scheme ? `${scheme[1]} <redacted>` : "<redacted>";
};
const redact = (h = {}) =>
  Object.fromEntries(Object.entries(h).map(([k, v]) => [k, SECRET_HEADERS.test(k) ? redactValue(k, v) : v]));

let version;
for (;;) {
  try {
    version = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
    break;
  } catch {
    await new Promise((r) => setTimeout(r, 100));
  }
}

const ws = new WebSocket(version.webSocketDebuggerUrl);
let nextId = 1;
const pending = new Map();
// Resolves with the result, or `{ timeout: true }`: some workers never answer
// some commands, and recording must not stall on them.
const send = (method, params = {}, sessionId) =>
  new Promise((resolve) => {
    const id = nextId++;
    pending.set(id, resolve);
    setTimeout(() => {
      if (pending.delete(id)) {
        write({ kind: "timeout", method });
        resolve({ timeout: true });
      }
    }, COMMAND_TIMEOUT_MS);
    ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
  });

const reqs = new Map(); // `${session}:${requestId}` -> url

async function setup(sessionId, info) {
  write({ kind: "attach", type: info.type, url: info.url?.slice(0, 120) });
  await send("Network.enable", { maxPostDataSize: 1 << 20 }, sessionId);
  await send("Target.setAutoAttach", { autoAttach: true, waitForDebuggerOnStart: true, flatten: true }, sessionId);
  await send("Runtime.runIfWaitingForDebugger", {}, sessionId);
}

ws.onmessage = async (ev) => {
  const msg = JSON.parse(ev.data);
  if (msg.id && pending.has(msg.id)) {
    pending.get(msg.id)(msg.result ?? { error: msg.error });
    pending.delete(msg.id);
    return;
  }
  const { method, params: p, sessionId: s } = msg;
  try {
    switch (method) {
      case "Target.attachedToTarget":
        await setup(p.sessionId, p.targetInfo);
        break;
      case "Network.requestWillBeSent": {
        const r = p.request;
        if (SKIP.test(r.url) || r.url.startsWith("data:") || r.url.startsWith("blob:")) break;
        reqs.set(`${s}:${p.requestId}`, r.url);
        let body = r.postData;
        if (!body && r.hasPostData) body = (await send("Network.getRequestPostData", { requestId: p.requestId }, s)).postData;
        if (NO_BODY.test(r.url)) body = body ? "<omitted: auth>" : undefined;
        write({ kind: "req", id: p.requestId, method: r.method, url: r.url, headers: redact(r.headers), body });
        break;
      }
      case "Network.responseReceived": {
        if (!reqs.has(`${s}:${p.requestId}`)) break;
        const r = p.response;
        write({ kind: "res", id: p.requestId, status: r.status, mime: r.mimeType, headers: redact(r.headers) });
        break;
      }
      case "Network.loadingFinished": {
        const url = reqs.get(`${s}:${p.requestId}`);
        if (!url) break;
        reqs.delete(`${s}:${p.requestId}`);
        if (NO_BODY.test(url)) break;
        const b = await send("Network.getResponseBody", { requestId: p.requestId }, s);
        if (b.body !== undefined && !b.base64Encoded) write({ kind: "body", id: p.requestId, url, body: b.body.slice(0, 4 << 20) });
        break;
      }
      case "Network.webSocketCreated":
        write({ kind: "ws-open", id: p.requestId, url: p.url.split("?")[0] });
        break;
      case "Network.webSocketFrameSent":
      case "Network.webSocketFrameReceived":
        write({
          kind: method.endsWith("Sent") ? "ws-out" : "ws-in",
          id: p.requestId,
          data: String(p.response.payloadData).slice(0, 1 << 20),
        });
        break;
    }
  } catch (e) {
    write({ kind: "error", method, error: String(e) });
  }
};

ws.onopen = async () => {
  // Targets created from now on start paused until setup() has enabled Network.
  await send("Target.setAutoAttach", { autoAttach: true, waitForDebuggerOnStart: true, flatten: true });
  const { targetInfos = [] } = await send("Target.getTargets");
  for (const t of targetInfos) {
    if (!/^(page|shared_worker|service_worker|worker)$/.test(t.type)) continue;
    if (t.url.startsWith("file:") || t.url.startsWith("devtools:")) continue;
    const { sessionId } = await send("Target.attachToTarget", { targetId: t.targetId, flatten: true });
    if (sessionId) await setup(sessionId, t);
  }
  write({ kind: "ready" });
  console.log("recording to", out);
};
ws.onclose = () => {
  write({ kind: "closed" });
  process.exit(0);
};
