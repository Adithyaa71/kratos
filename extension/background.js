// Service worker: the extension's end of the native-messaging port.
//
// It owns one connection to kratos-host and routes broker requests to the
// right frame of the right tab. It makes no decisions and holds no policy --
// the broker does that, and a copy of the rules living here would be a second
// set of rules to disagree with the first.
//
// Ids are namespaced `brand:window:tab:element` so two browsers, two windows
// and two tabs can be driven at once without an id from one landing in
// another.

const HOST = "ai.kratos.bridge";

/// Chromium stops a service worker after 30s idle. A native port resets that
/// timer on each message, but a quiet period with no actions would still end
/// it and drop the broker connection, so an alarm keeps it warm while
/// connected. Nothing runs on the alarm itself.
///
/// 0.5 is Chromium's documented floor. The old 0.4 was accepted by the API and
/// then clamped, which is the worst way to be wrong: the alarm existed, read
/// back at the value asked for, and did not fire when expected.
const KEEPALIVE_MINUTES = 0.5;

let port = null;
let brand = "chrome";
let reconnectDelay = 1000;
/// Why the port is not open, in Chromium's own words. The service worker's
/// console is asleep exactly when you want to read it, so this is kept here
/// and shown in the popup, which is where someone looks first.
let lastReason = "";
/// connectNative returns a port synchronously even when there is no host to
/// run -- the failure arrives later, on the disconnect listener. So a port is
/// not evidence of anything; this flag is, and it is set only when the host
/// itself says hello.
let ready = false;

// -- brand ------------------------------------------------------------------

async function detectBrand() {
  try {
    // Brave ships an identical user-agent on purpose, so this is the only
    // reliable signal, and it is a promise.
    if (navigator.brave && typeof navigator.brave.isBrave === "function") {
      if (await navigator.brave.isBrave()) return "brave";
    }
  } catch (_) {
    /* not Brave */
  }
  const ua = navigator.userAgent || "";
  if (ua.includes("Edg/")) return "edge";
  if (ua.includes("OPR/")) return "opera";
  if (ua.includes("Vivaldi")) return "vivaldi";
  return "chrome";
}

// -- native port ------------------------------------------------------------

function connect() {
  if (port) return;
  try {
    port = chrome.runtime.connectNative(HOST);
  } catch (e) {
    schedule(`could not start the native host: ${e}`);
    return;
  }
  // connectNative resolves lazily: a bad id or a missing registration shows up
  // on the disconnect listener below, not here.

  port.onMessage.addListener(onBrokerMessage);
  port.onDisconnect.addListener(() => {
    const why = chrome.runtime.lastError ? chrome.runtime.lastError.message : "closed";
    port = null;
    ready = false;
    schedule(why);
  });
  lastReason = "waiting for the native host to answer";
  // Amber, not green: the host has not spoken yet.
  setBadge("...", "#8a6d3b");
}

function schedule(reason) {
  lastReason = reason;
  console.warn("[kratos]", reason, `retrying in ${reconnectDelay}ms`);
  setBadge("off", "#8b3d3d");
  const delay = reconnectDelay;
  reconnectDelay = Math.min(reconnectDelay * 2, 30000);
  setTimeout(connect, delay);
}

function setBadge(text, colour) {
  try {
    chrome.action.setBadgeText({ text });
    chrome.action.setBadgeBackgroundColor({ color: colour });
  } catch (_) {
    /* no action in this context */
  }
}

function send(message) {
  if (!port) return false;
  try {
    port.postMessage(message);
    return true;
  } catch (e) {
    console.warn("[kratos] send failed", e);
    port = null;
    schedule("send failed");
    return false;
  }
}

// Replies over 768 KiB are chunked, because Chromium's native messaging caps a
// single message at 1 MiB and a full DOM snapshot of a heavy page exceeds it.
const CHUNK_THRESHOLD = 768 * 1024;

function sendFrame(frame) {
  const text = JSON.stringify(frame);
  if (text.length <= CHUNK_THRESHOLD) {
    return send(frame);
  }
  const id = frame.id || "chunked";
  const total = Math.ceil(text.length / CHUNK_THRESHOLD);
  for (let i = 0; i < total; i += 1) {
    send({
      chunk: { id, i, n: total },
      part: text.slice(i * CHUNK_THRESHOLD, (i + 1) * CHUNK_THRESHOLD),
    });
  }
  return true;
}

// -- request routing --------------------------------------------------------

async function onBrokerMessage(frame) {
  if (!frame || frame.t === "evt") {
    if (frame && frame.op === "connected") {
      ready = true;
      lastReason = "";
      // Only a host that actually spoke earns the backoff reset. Resetting it
      // when connectNative merely returned a port turned the retry into a
      // flat one-second loop: 1231 identical warnings in twenty minutes,
      // which is a way of saying nothing very loudly.
      reconnectDelay = 1000;
      setBadge("on", "#3d8b4a");
    }
    if (frame && frame.op === "disconnected") {
      ready = false;
      lastReason = "the host lost the broker";
      setBadge("off", "#8b3d3d");
    }
    return;
  }
  if (frame.t !== "req") return;

  const id = frame.id;
  const payload = frame.payload || {};
  try {
    let result;
    if (frame.op === "backend.observe") {
      result = await observe(payload);
    } else if (frame.op === "backend.act") {
      result = await act(payload);
    } else {
      return sendFrame(reply(id, false, {}, "bad_request", `unknown op ${frame.op}`));
    }
    sendFrame(reply(id, true, result));
  } catch (e) {
    sendFrame(
      reply(id, false, {}, "backend_error", String((e && e.message) || e))
    );
  }
}

function reply(id, ok, payload, code, message) {
  if (ok) return { v: 1, t: "res", id, ok: true, payload };
  return { v: 1, t: "res", id, ok: false, error: { code, message, detail: {} } };
}

async function activeTab(windowHint) {
  const query = windowHint ? { active: true } : { active: true, lastFocusedWindow: true };
  const tabs = await chrome.tabs.query(query);
  if (!tabs.length) return null;
  if (!windowHint) return tabs[0];
  const want = String(windowHint).toLowerCase();
  return (
    tabs.find((t) => (t.title || "").toLowerCase().includes(want)) ||
    tabs.find((t) => (t.url || "").toLowerCase().includes(want)) ||
    null
  );
}

function prefix(tab, frameId) {
  return `${brand}:w${tab.windowId}:t${tab.id}:f${frameId}`;
}

async function observe(payload) {
  const tab = await activeTab(payload.window);
  if (!tab) {
    throw Object.assign(new Error("no active tab in this browser"), { code: "no_target" });
  }

  // Every frame, because the control that matters is often in an iframe -- a
  // Gmail compose box among them. The content script owns the element registry
  // in each frame, so each frame is asked directly rather than re-scanned from
  // here; two scanners would be two things to keep in step.
  const perFrame = await scanAllFrames(tab);

  const elements = [];
  let truncated = false;
  let url = tab.url || "";
  for (const { frameId, result } of perFrame) {
    if (!result) continue;
    truncated = truncated || !!result.truncated;
    if (frameId === 0 && result.url) url = result.url;
    for (const el of result.elements || []) {
      elements.push({ ...el, id: `${prefix(tab, frameId)}:${el.id}` });
    }
  }

  return {
    window: tab.title || "",
    url,
    elements,
    truncated,
  };
}

async function scanAllFrames(tab) {
  const frameIds = await listFrames(tab.id);
  const out = [];
  for (const frameId of frameIds) {
    const result = await ask(tab.id, frameId, { op: "scan" });
    out.push({ frameId, result });
  }
  return out;
}

async function listFrames(tabId) {
  // No webNavigation permission, so frames are discovered by asking the main
  // frame first and letting Chromium's message routing enumerate the rest.
  // chrome.tabs.sendMessage without a frameId reaches every frame, but only
  // the first reply is delivered, so each frame is addressed explicitly.
  try {
    const frames = await chrome.scripting.executeScript({
      target: { tabId, allFrames: true },
      func: () => 1,
    });
    return frames.map((f) => f.frameId);
  } catch (_) {
    return [0];
  }
}

function ask(tabId, frameId, message) {
  return new Promise((resolve) => {
    chrome.tabs.sendMessage(tabId, message, { frameId }, (response) => {
      if (chrome.runtime.lastError || !response || !response.ok) {
        resolve(null);
        return;
      }
      resolve(response.result);
    });
  });
}

async function act(payload) {
  const target = payload.target || null;
  const routed = target && target.id ? parseId(target.id) : null;

  let tabId;
  let frameId;
  if (routed) {
    tabId = routed.tabId;
    frameId = routed.frameId;
  } else {
    const tab = await activeTab(null);
    if (!tab) {
      throw Object.assign(new Error("no active tab"), { code: "no_target" });
    }
    tabId = tab.id;
    frameId = 0;
  }

  const started = Date.now();
  const result = await ask(tabId, frameId, { op: "act", payload });
  if (!result) {
    return { applied: false, readback: null, took_ms: Date.now() - started };
  }
  return {
    applied: !!result.applied,
    readback: result.readback ?? null,
    took_ms: Date.now() - started,
  };
}

/// `brand:wN:tN:fN:eN` -> the tab and frame to address.
function parseId(id) {
  const parts = String(id).split(":");
  if (parts.length < 5) return null;
  const tab = parseInt(parts[2].replace(/^t/, ""), 10);
  const frame = parseInt(parts[3].replace(/^f/, ""), 10);
  if (Number.isNaN(tab) || Number.isNaN(frame)) return null;
  return { tabId: tab, frameId: frame };
}

// -- lifecycle --------------------------------------------------------------

chrome.runtime.onStartup.addListener(start);
chrome.runtime.onInstalled.addListener(start);

async function start() {
  brand = await detectBrand();
  chrome.alarms.create("keepalive", { periodInMinutes: KEEPALIVE_MINUTES });
  connect();
}

chrome.alarms.onAlarm.addListener(() => {
  // The alarm exists to keep this worker resident while a port is open; if the
  // port died while we were asleep, this is also where it comes back.
  if (!port) connect();
});

/// Reconnect on browsing, not only on a timer.
///
/// A worker that is asleep when the broker appears has no way back if an alarm
/// is its only retry: the alarm fires in the worker that is not running. These
/// events wake it as a matter of course, and every one of them is a moment
/// when the bridge is about to be wanted anyway.
///
/// The guard matters as much as the listeners. `connect()` returns immediately
/// when a port exists, so a busy browsing session costs one comparison per
/// event and opens nothing.
function reviveOnActivity() {
  const revive = () => {
    if (!port) connect();
  };
  try {
    chrome.tabs.onUpdated.addListener(revive);
    chrome.tabs.onActivated.addListener(revive);
    chrome.windows.onFocusChanged.addListener(revive);
  } catch (e) {
    console.warn("[kratos] could not listen for activity", e);
  }
}
reviveOnActivity();

// The popup asks for this; nothing else may. It reports state only -- there is
// no message here that drives a page, because the broker is the only thing
// allowed to ask for that.
chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  if (!msg || msg.op !== "bridge.status") return false;
  if (msg.retry) {
    // Someone is watching, so do not make them wait out the backoff.
    reconnectDelay = 1000;
    if (!port) connect();
  }
  reply({ connected: ready, port: !!port, reason: lastReason, brand, host: HOST });
  return false;
});

// Also connect when the worker is spun up by anything else.
start();
