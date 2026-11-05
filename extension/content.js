// Content script: the only thing in kratos that sees a real DOM.
//
// Runs in every frame, including iframes, because a Gmail compose box lives in
// one and a tree that stops at the top document reports an empty screen.
//
// Two things here are load-bearing:
//
//   1. Every element gets a `container` -- the nearest named dialog, form or
//      landmark. Without it, two controls labelled "To" on one page are
//      indistinguishable, a decision model has no basis to choose, and it will
//      still answer confidently. That is how a subject line ended up in a
//      recipient field at confidence 1.00.
//
//   2. Actions resolve by element identity, then by selector -- never by
//      coordinates. A bbox is reported for other backends to use, but nothing
//      here clicks one. Position is the thing that goes wrong when a page
//      reflows between looking and acting.

(() => {
  "use strict";
  if (window.__desktopHandsInstalled) return;
  window.__desktopHandsInstalled = true;

  const MAX_ELEMENTS = 400;
  const MAX_DEPTH = 40;

  // id -> element for the most recent scan. Rebuilt per scan, so an id from a
  // stale scan resolves to nothing rather than to whatever now sits there.
  let registry = new Map();
  let scanSeq = 0;

  const INTERACTIVE = [
    "a[href]", "button", "input", "select", "textarea", "summary",
    "[role=button]", "[role=link]", "[role=textbox]", "[role=searchbox]",
    "[role=combobox]", "[role=checkbox]", "[role=radio]", "[role=switch]",
    "[role=menuitem]", "[role=menuitemcheckbox]", "[role=menuitemradio]",
    "[role=option]", "[role=tab]", "[role=slider]", "[role=spinbutton]",
    "[contenteditable=true]", "[contenteditable=plaintext-only]",
    "[tabindex]:not([tabindex='-1'])",
  ].join(",");

  const CONTAINER_ROLES = {
    dialog: "dialog", alertdialog: "dialog", form: "form", search: "search",
    navigation: "nav", main: "main", region: "region", complementary: "aside",
    banner: "banner", contentinfo: "footer", tabpanel: "tabpanel",
    listbox: "listbox", menu: "menu", table: "table", grid: "grid",
  };

  // -- geometry -------------------------------------------------------------

  function screenRect(el) {
    const r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) return null;
    const dpr = window.devicePixelRatio || 1;
    // screenX/screenY are this frame's viewport origin, so an element in an
    // iframe lands in the right place without the parent having to help.
    // Chromium reports these per frame; where it does not, the bbox is
    // approximate -- which is survivable precisely because this backend never
    // acts on it.
    return [
      Math.round((window.screenX + r.left) * dpr),
      Math.round((window.screenY + r.top) * dpr),
      Math.round(r.width * dpr),
      Math.round(r.height * dpr),
    ];
  }

  function visible(el) {
    const style = window.getComputedStyle(el);
    if (style.visibility === "hidden" || style.display === "none") return false;
    if (parseFloat(style.opacity || "1") < 0.05) return false;
    const r = el.getBoundingClientRect();
    if (r.width <= 1 || r.height <= 1) return false;
    // Off-screen by more than a viewport is not on this screen.
    if (r.bottom < -r.height || r.top > window.innerHeight + r.height) return false;
    if (r.right < -r.width || r.left > window.innerWidth + r.width) return false;
    return true;
  }

  // -- naming ---------------------------------------------------------------

  function labelOf(el) {
    const byAria = el.getAttribute("aria-label");
    if (byAria && byAria.trim()) return byAria.trim();

    const labelledBy = el.getAttribute("aria-labelledby");
    if (labelledBy) {
      const text = labelledBy
        .split(/\s+/)
        .map((id) => (el.ownerDocument.getElementById(id) || {}).textContent || "")
        .join(" ")
        .trim();
      if (text) return text;
    }

    if (el.labels && el.labels.length) {
      const text = Array.from(el.labels).map((l) => l.textContent || "").join(" ").trim();
      if (text) return text;
    }

    for (const attr of ["title", "placeholder", "name", "alt"]) {
      const v = el.getAttribute(attr);
      if (v && v.trim()) return v.trim();
    }

    if (el.tagName === "INPUT" && (el.type === "submit" || el.type === "button")) {
      if (el.value) return el.value;
    }

    // Own text, excluding nested interactive text so a card does not swallow
    // the label of the button inside it.
    const own = Array.from(el.childNodes)
      .filter((n) => n.nodeType === Node.TEXT_NODE)
      .map((n) => n.textContent)
      .join(" ")
      .trim();
    if (own) return own.slice(0, 120);

    const text = (el.textContent || "").trim().replace(/\s+/g, " ");
    return text.slice(0, 120);
  }

  function roleOf(el) {
    const explicit = el.getAttribute("role");
    if (explicit) return explicit;
    const tag = el.tagName.toLowerCase();
    if (tag === "a") return "link";
    if (tag === "button" || tag === "summary") return "button";
    if (tag === "select") return "combobox";
    if (tag === "textarea") return "textbox";
    if (tag === "input") {
      const t = (el.type || "text").toLowerCase();
      if (t === "checkbox" || t === "radio") return t;
      if (t === "submit" || t === "button" || t === "reset") return "button";
      if (t === "range") return "slider";
      if (t === "number") return "spinbutton";
      if (t === "password") return "textbox";
      return t === "search" ? "searchbox" : "textbox";
    }
    if (el.isContentEditable) return "textbox";
    return "generic";
  }

  /// The nearest enclosing named region. This is the field that stops two
  /// identically-labelled controls from being the same option.
  function containerOf(el) {
    let node = el.parentElement || hostOf(el);
    let hops = 0;
    while (node && hops < MAX_DEPTH) {
      const role = (node.getAttribute && node.getAttribute("role")) || "";
      const kind = CONTAINER_ROLES[role] || implicitContainer(node);
      if (kind) {
        const name =
          (node.getAttribute && (node.getAttribute("aria-label") || "")) ||
          headingIn(node) ||
          "";
        return name ? `${kind}:${name.trim().slice(0, 60)}` : kind;
      }
      node = node.parentElement || hostOf(node);
      hops += 1;
    }
    return null;
  }

  function implicitContainer(node) {
    const tag = (node.tagName || "").toLowerCase();
    if (tag === "dialog") return "dialog";
    if (tag === "form") return "form";
    if (tag === "nav") return "nav";
    if (tag === "main") return "main";
    if (tag === "aside") return "aside";
    if (tag === "table") return "table";
    if (tag === "fieldset") return "fieldset";
    return null;
  }

  function headingIn(node) {
    if (!node.querySelector) return "";
    const h = node.querySelector("h1,h2,h3,[role=heading],legend,caption");
    return h ? (h.textContent || "").trim().replace(/\s+/g, " ").slice(0, 60) : "";
  }

  function hostOf(node) {
    const root = node.getRootNode && node.getRootNode();
    return root && root.host ? root.host : null;
  }

  // -- selectors ------------------------------------------------------------

  // A document-unique selector. Shadow boundaries are joined with ">>>",
  // which is not valid CSS and is resolved by `resolve` below -- a plain
  // selector cannot cross a shadow root, and pretending otherwise would hand
  // back a selector that silently matches nothing.
  function selectorFor(el) {
    const segments = [];
    let node = el;
    let guard = 0;
    while (node && guard < MAX_DEPTH) {
      segments.unshift(pathWithin(node));
      const host = hostOf(node);
      if (!host) break;
      node = host;
      guard += 1;
    }
    return segments.join(" >>> ");
  }

  function pathWithin(el) {
    const root = el.getRootNode();
    if (el.id && root.querySelectorAll(`#${cssEscape(el.id)}`).length === 1) {
      return `#${cssEscape(el.id)}`;
    }
    const parts = [];
    let node = el;
    while (node && node.nodeType === Node.ELEMENT_NODE) {
      let part = node.tagName.toLowerCase();
      if (node.id && root.querySelectorAll(`#${cssEscape(node.id)}`).length === 1) {
        parts.unshift(`#${cssEscape(node.id)}`);
        break;
      }
      const parent = node.parentElement;
      if (parent) {
        const siblings = Array.from(parent.children).filter((c) => c.tagName === node.tagName);
        if (siblings.length > 1) {
          part += `:nth-of-type(${siblings.indexOf(node) + 1})`;
        }
      }
      parts.unshift(part);
      node = node.parentElement;
      if (parts.length > 12) break;
    }
    return parts.join(" > ");
  }

  function cssEscape(s) {
    return window.CSS && CSS.escape ? CSS.escape(s) : s.replace(/([^\w-])/g, "\\$1");
  }

  function resolve(selector) {
    if (!selector) return null;
    const hops = selector.split(" >>> ");
    let scope = document;
    let found = null;
    for (let i = 0; i < hops.length; i += 1) {
      try {
        found = scope.querySelector(hops[i]);
      } catch (_) {
        return null;
      }
      if (!found) return null;
      if (i < hops.length - 1) {
        if (!found.shadowRoot) return null;
        scope = found.shadowRoot;
      }
    }
    return found;
  }

  // -- scanning -------------------------------------------------------------

  function valueOf(el) {
    if (el.isContentEditable) return (el.textContent || "").slice(0, 400);
    if (el.type === "password") return el.value ? "•".repeat(el.value.length) : "";
    if (el.type === "checkbox" || el.type === "radio") return el.checked ? "checked" : "unchecked";
    if ("value" in el && typeof el.value === "string") return el.value.slice(0, 400);
    return null;
  }

  function isSensitive(el) {
    if (el.type === "password") return true;
    const hint = `${el.autocomplete || ""} ${el.name || ""} ${el.id || ""}`.toLowerCase();
    return /password|passwd|cc-num|cardnumber|cvc|cvv|ssn|secret|otp|totp/.test(hint);
  }

  function collectRoots() {
    // Every shadow root reachable from this document, so a component library
    // that hides its controls inside one is still visible.
    const roots = [document];
    const queue = [document];
    let guard = 0;
    while (queue.length && guard < 5000) {
      const root = queue.shift();
      guard += 1;
      let hosts;
      try {
        hosts = root.querySelectorAll("*");
      } catch (_) {
        continue;
      }
      for (const el of hosts) {
        if (el.shadowRoot) {
          roots.push(el.shadowRoot);
          queue.push(el.shadowRoot);
        }
      }
    }
    return roots;
  }

  function scan() {
    scanSeq += 1;
    registry = new Map();
    const elements = [];
    const seen = new Set();

    for (const root of collectRoots()) {
      let matches;
      try {
        matches = root.querySelectorAll(INTERACTIVE);
      } catch (_) {
        continue;
      }
      for (const el of matches) {
        if (elements.length >= MAX_ELEMENTS) break;
        if (seen.has(el)) continue;
        seen.add(el);
        if (!visible(el)) continue;

        const localId = `e${elements.length}`;
        registry.set(localId, el);
        elements.push({
          id: localId,
          role: roleOf(el),
          label: labelOf(el),
          value: valueOf(el),
          enabled: !(el.disabled || el.getAttribute("aria-disabled") === "true"),
          bbox: screenRect(el),
          container: containerOf(el),
          selector: selectorFor(el),
          sensitive: isSensitive(el),
        });
      }
    }

    return {
      url: location.href,
      title: document.title,
      elements,
      truncated: elements.length >= MAX_ELEMENTS,
    };
  }

  // -- acting ---------------------------------------------------------------

  function find(target) {
    if (!target) return null;
    // Identity first: an id from the current scan is exact.
    if (target.id) {
      const local = String(target.id).split(":").pop();
      const hit = registry.get(local);
      if (hit && hit.isConnected) return hit;
    }
    // Then the selector, which survives a re-render that changed ids.
    const bySelector = resolve(target.selector);
    if (bySelector) return bySelector;
    // Then the label, which survives a re-render that changed the DOM shape.
    if (target.label) return byLabel(target.label);
    return null;
  }

  function byLabel(label) {
    const want = String(label).trim().toLowerCase();
    if (!want) return null;
    for (const root of collectRoots()) {
      let matches;
      try {
        matches = root.querySelectorAll(INTERACTIVE);
      } catch (_) {
        continue;
      }
      for (const el of matches) {
        if (visible(el) && labelOf(el).trim().toLowerCase() === want) return el;
      }
    }
    return null;
  }

  function setNativeValue(el, text) {
    const proto = Object.getPrototypeOf(el);
    const descriptor = Object.getOwnPropertyDescriptor(proto, "value");
    if (descriptor && descriptor.set) {
      descriptor.set.call(el, text);
    } else {
      el.value = text;
    }
  }

  function typeInto(el, text) {
    el.focus();
    const before = valueOf(el);

    if (el.isContentEditable) {
      const range = document.createRange();
      range.selectNodeContents(el);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      // execCommand is deprecated but is still the only path that produces the
      // beforeinput/input sequence rich editors listen for. A plain textContent
      // assignment is silently ignored by most of them.
      if (!document.execCommand("insertText", false, text)) {
        el.textContent = text;
        el.dispatchEvent(new InputEvent("input", { bubbles: true, data: text }));
      }
    } else {
      el.select && el.select();
      if (!document.execCommand("insertText", false, text)) {
        setNativeValue(el, text);
        el.dispatchEvent(new InputEvent("input", { bubbles: true, data: text }));
        el.dispatchEvent(new Event("change", { bubbles: true }));
      }
    }

    const after = valueOf(el);
    // `applied: false` is the honest answer when the page took the events and
    // did nothing with them -- a controlled React input that rejected the
    // write, for instance. The broker reads it as a cue to fall back to
    // OS-level input rather than to retry the same way.
    const landed = after != null && String(after).includes(text.slice(0, 40));
    return { applied: landed || before !== after, readback: after };
  }

  function clickOn(el) {
    el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
    const r = el.getBoundingClientRect();
    const init = {
      bubbles: true,
      cancelable: true,
      composed: true,
      clientX: Math.round(r.left + r.width / 2),
      clientY: Math.round(r.top + r.height / 2),
      button: 0,
      buttons: 1,
      isTrusted: false,
    };
    // The whole sequence, because plenty of components listen for pointerdown
    // or mousedown and never see a bare click.
    for (const type of ["pointerover", "pointerenter", "pointerdown", "mousedown", "pointerup", "mouseup"]) {
      try {
        el.dispatchEvent(new PointerEvent(type, init));
      } catch (_) {
        el.dispatchEvent(new MouseEvent(type, init));
      }
    }
    el.dispatchEvent(new MouseEvent("click", init));
    if (typeof el.click === "function") {
      try {
        el.click();
      } catch (_) {
        /* already dispatched above */
      }
    }
    return { applied: true, readback: valueOf(el) };
  }

  function act(payload) {
    const kind = payload.kind;

    if (kind === "navigate") {
      location.assign(payload.url);
      return { applied: true, readback: null };
    }
    if (kind === "wait" || kind === "none") {
      return { applied: true, readback: null };
    }
    if (kind === "scroll" && !payload.target) {
      window.scrollBy({ top: (payload.amount || 3) * 100, behavior: "instant" });
      return { applied: true, readback: null };
    }
    if (kind === "key" && !payload.target) {
      // A keypress goes wherever focus is. Demanding an element here is what
      // made `press Enter` fail after typing into a search box: the broker
      // sends no target for a key, correctly, and both backends then refused.
      const focused =
        document.activeElement && document.activeElement !== document.body
          ? document.activeElement
          : document.body || document.documentElement;
      if (!focused) {
        return { applied: false, readback: null, reason: "nothing has focus in this frame" };
      }
      return pressKey(focused, payload.key || "");
    }

    const el = find(payload.target);
    if (!el) {
      return { applied: false, readback: null, reason: "element not found in this frame" };
    }
    if (el.disabled || el.getAttribute("aria-disabled") === "true") {
      return { applied: false, readback: null, reason: "element is disabled" };
    }

    switch (kind) {
      case "click":
        return clickOn(el);
      case "type":
        return typeInto(el, payload.text || "");
      case "key":
        return pressKey(el, payload.key || "");
      case "focus":
        el.focus();
        return { applied: document.activeElement === el, readback: valueOf(el) };
      case "scroll":
        el.scrollBy({ top: (payload.amount || 3) * 100, behavior: "instant" });
        return { applied: true, readback: null };
      case "select":
        return selectOption(el, payload.text);
      default:
        return { applied: false, readback: null, reason: `unsupported kind ${kind}` };
    }
  }

  const KEY_MAP = {
    enter: { key: "Enter", code: "Enter", keyCode: 13 },
    escape: { key: "Escape", code: "Escape", keyCode: 27 },
    tab: { key: "Tab", code: "Tab", keyCode: 9 },
    backspace: { key: "Backspace", code: "Backspace", keyCode: 8 },
    delete: { key: "Delete", code: "Delete", keyCode: 46 },
    space: { key: " ", code: "Space", keyCode: 32 },
    up: { key: "ArrowUp", code: "ArrowUp", keyCode: 38 },
    down: { key: "ArrowDown", code: "ArrowDown", keyCode: 40 },
    left: { key: "ArrowLeft", code: "ArrowLeft", keyCode: 37 },
    right: { key: "ArrowRight", code: "ArrowRight", keyCode: 39 },
    home: { key: "Home", code: "Home", keyCode: 36 },
    end: { key: "End", code: "End", keyCode: 35 },
    pageup: { key: "PageUp", code: "PageUp", keyCode: 33 },
    pagedown: { key: "PageDown", code: "PageDown", keyCode: 34 },
  };

  function pressKey(el, spec) {
    const parts = String(spec).toLowerCase().split("+").filter(Boolean);
    const name = parts.pop() || "";
    const base = KEY_MAP[name] || { key: name, code: `Key${name.toUpperCase()}`, keyCode: 0 };
    const init = {
      bubbles: true,
      cancelable: true,
      composed: true,
      ...base,
      ctrlKey: parts.includes("ctrl") || parts.includes("control"),
      altKey: parts.includes("alt"),
      shiftKey: parts.includes("shift"),
      metaKey: parts.includes("win") || parts.includes("meta"),
    };
    el.focus();
    const down = el.dispatchEvent(new KeyboardEvent("keydown", init));
    el.dispatchEvent(new KeyboardEvent("keyup", init));
    // A synthetic shortcut cannot reach the browser's own handlers, only the
    // page's. Reporting that honestly lets the broker fall back to real OS
    // input for things like Ctrl+T.
    return { applied: down, readback: valueOf(el), reason: down ? undefined : "page cancelled the key" };
  }

  function selectOption(el, wanted) {
    if (el.tagName !== "SELECT") {
      return clickOn(el);
    }
    const want = String(wanted || "").trim().toLowerCase();
    const option = Array.from(el.options).find(
      (o) => (o.label || o.textContent || "").trim().toLowerCase() === want ||
             (o.value || "").toLowerCase() === want
    );
    if (!option) {
      return { applied: false, readback: el.value, reason: `no option matching ${wanted}` };
    }
    el.value = option.value;
    el.dispatchEvent(new Event("change", { bubbles: true }));
    return { applied: el.value === option.value, readback: el.value };
  }

  // -- message plumbing -----------------------------------------------------

  chrome.runtime.onMessage.addListener((message, _sender, respond) => {
    try {
      if (message && message.op === "scan") {
        respond({ ok: true, result: scan() });
      } else if (message && message.op === "act") {
        respond({ ok: true, result: act(message.payload || {}) });
      } else {
        respond({ ok: false, error: "unknown op" });
      }
    } catch (e) {
      // Never leave the background waiting: an exception here would otherwise
      // show up as a backend timeout, which the broker refuses to retry.
      respond({ ok: false, error: String((e && e.message) || e) });
    }
    return true;
  });
})();
