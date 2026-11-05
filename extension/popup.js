// Popup: shows whether the bridge is connected and what it can see here.
// Read-only -- nothing in this window can drive the page.

function setState(connected) {
  const el = document.getElementById("state");
  el.textContent = connected ? "connected" : "not running";
  el.className = `val dot ${connected ? "on" : "off"}`;
}

async function refresh() {
  // The badge is the service worker's own view of the port, which is the one
  // that matters; reading it avoids a second source of truth.
  let text = "";
  try {
    text = await chrome.action.getBadgeText({});
  } catch (_) {
    /* no badge yet */
  }
  setState(text === "on");

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
