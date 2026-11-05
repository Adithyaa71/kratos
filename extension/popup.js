// Popup: shows whether the bridge is connected and what it can see here.
// Read-only -- nothing in this window can drive the page.

function setState(connected, reason) {
  const el = document.getElementById("state");
  el.textContent = connected ? "connected" : "not connected";
  el.className = `val dot ${connected ? "on" : "off"}`;

  const why = document.getElementById("why");
  const retry = document.getElementById("retry");
  if (connected) {
    why.hidden = true;
    retry.hidden = true;
    return;
  }
  retry.hidden = false;
  why.hidden = false;
  // Chromium's own wording, verbatim. "not found" means the registry key or
  // the manifest is missing; "forbidden" means the manifest names a different
  // extension id than this one; "exited" means the host itself failed.
  why.textContent = reason || "the service worker has not tried yet";
}

async function refresh() {
  // The badge is the service worker's own view of the port, which is the one
  // that matters; reading it avoids a second source of truth.
  // Ask the worker directly: opening this popup wakes it, and its own view of
  // the port is the only one that is not a guess.
  let state = null;
  try {
    state = await chrome.runtime.sendMessage({ op: "bridge.status" });
  } catch (_) {
    /* worker did not answer */
  }
  if (state) {
    setState(state.connected, state.reason);
  } else {
    let text = "";
    try {
      text = await chrome.action.getBadgeText({});
    } catch (_) {
      /* no badge yet */
    }
    setState(text === "on", "the service worker is not responding");
  }

  document.getElementById("id").textContent = chrome.runtime.id;

  const ua = navigator.userAgent || "";
  let brand = "chrome";
  try {
    if (navigator.brave && (await navigator.brave.isBrave())) brand = "brave";
    else if (ua.includes("Edg/")) brand = "edge";
    else if (ua.includes("OPR/")) brand = "opera";
    else if (ua.includes("Vivaldi")) brand = "vivaldi";
  } catch (_) {
    /* leave as chrome */
  }
  document.getElementById("brand").textContent = brand;

  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab) return;
  chrome.tabs.sendMessage(tab.id, { op: "scan" }, { frameId: 0 }, (response) => {
    const out = document.getElementById("elements");
    if (chrome.runtime.lastError || !response || !response.ok) {
      out.textContent = "not readable here";
      return;
    }
    const n = (response.result.elements || []).length;
    out.textContent = `${n} element${n === 1 ? "" : "s"}`;
  });
}

refresh();

document.getElementById("retry").addEventListener("click", async () => {
  try {
    const state = await chrome.runtime.sendMessage({ op: "bridge.status", retry: true });
    setState(state && state.connected, state && state.reason);
  } catch (_) {
    /* worker did not answer */
  }
  setTimeout(refresh, 400);
});
