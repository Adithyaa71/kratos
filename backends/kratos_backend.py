"""Connects the existing Python perception and actuation to the broker.

The UIA walk, the OCR fallback and the OS-level input in ``D:\\Jev`` are
working, measured code -- a document-anchored accessibility walk that found 198
elements in a Gmail tab in 715ms where a naive root walk took 25 seconds and
still truncated. None of that is rewritten here. This file is a protocol
adapter: it speaks the kratos wire format on a named pipe and calls
those modules.

Run it alongside the broker::

    python kratos_backend.py                 # registers as the "uia" backend
    python kratos_backend.py --source pixel  # forces the OCR-over-window path

What it deliberately does *not* provide is ``container`` or ``selector``. UI
Automation on a Chromium window reports a flat list of controls with a centre
point and no enclosing-dialog name, which is precisely why two fields labelled
"To" on a Gmail compose are indistinguishable through it, and why the browser
extension is the preferred backend for browser windows. This backend is the
fallback that works everywhere, not the one that works best.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import threading
import time
from typing import Any

# The perception and actuation packages ship beside this file, so a clone of
# the repo runs without anything else on disk. KRATOS_PYROOT still wins when
# it is set, for pointing at a separate working copy of those modules.
PY_ROOT = os.environ.get("KRATOS_PYROOT", os.path.dirname(os.path.abspath(__file__)))
if PY_ROOT not in sys.path:
    sys.path.insert(0, PY_ROOT)

PROTOCOL_VERSION = 1
BUILD = "kratos_backend.py 0.1.0"
MAX_LINE = 8 * 1024 * 1024

#: UIA reports a centre point, not a rectangle. Actions only need the point;
#: the broker's region fingerprint needs an area to watch, so one is
#: synthesised around the point. It is approximate on purpose -- an honest
#: approximation beats a precise-looking number that was invented.
ASSUMED_W = 120
ASSUMED_H = 28

#: UIA control type names to the roles the protocol uses.
ROLES = {
    "ButtonControl": "button",
    "HyperlinkControl": "link",
    "EditControl": "textbox",
    "DocumentControl": "document",
    "ComboBoxControl": "combobox",
    "ListItemControl": "option",
    "MenuItemControl": "menuitem",
    "CheckBoxControl": "checkbox",
    "RadioButtonControl": "radio",
    "TabItemControl": "tab",
    "TextControl": "text",
    "ImageControl": "image",
    "TreeItemControl": "treeitem",
    "SliderControl": "slider",
    "SpinnerControl": "spinbutton",
    "OcrText": "text",
}


def role_of(kind: str) -> str:
    return ROLES.get(kind, (kind or "generic").replace("Control", "").lower() or "generic")


# ---------------------------------------------------------------------------
# Pipe transport
# ---------------------------------------------------------------------------


def current_user_sid() -> str:
    """The SID the pipe name is suffixed with.

    pywin32 when it is there, ``whoami`` when it is not -- the backend should
    not fail to start over a missing optional dependency.
    """
    try:
        import win32api  # type: ignore
        import win32security  # type: ignore

        token = win32security.OpenProcessToken(
            win32api.GetCurrentProcess(), win32security.TOKEN_QUERY
        )
        sid = win32security.GetTokenInformation(token, win32security.TokenUser)[0]
        return win32security.ConvertSidToStringSid(sid)
    except Exception:
        out = subprocess.run(
            ["whoami", "/user", "/fo", "csv", "/nh"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        return out.strip().strip('"').split('","')[-1]


class PipeConn:
    """NDJSON over a Windows named pipe, framed exactly as the broker frames it."""

    def __init__(self, path: str):
        # buffering=0 gives one ReadFile per read, so a line arrives as soon as
        # the broker writes it rather than when some buffer happens to fill.
        self.f = open(path, "r+b", buffering=0)
        self.buf = b""
        self.lock = threading.Lock()

    def read_frame(self) -> dict[str, Any] | None:
        while b"\n" not in self.buf:
            chunk = self.f.read(65536)
            if not chunk:
                return None
            self.buf += chunk
            if len(self.buf) > MAX_LINE:
                raise RuntimeError("broker sent an oversized line")
        line, _, self.buf = self.buf.partition(b"\n")
        line = line.rstrip(b"\r")
        if not line:
            return self.read_frame()
        return json.loads(line)

    def write_frame(self, obj: dict[str, Any]) -> None:
        payload = json.dumps(obj, ensure_ascii=False).encode("utf-8") + b"\n"
        with self.lock:
            self.f.write(payload)
            self.f.flush()

    def close(self) -> None:
        try:
            self.f.close()
        except Exception:
            pass


def ok(frame_id: str, payload: dict[str, Any]) -> dict[str, Any]:
    return {"v": PROTOCOL_VERSION, "t": "res", "id": frame_id, "ok": True, "payload": payload}


def err(frame_id: str, code: str, message: str) -> dict[str, Any]:
    return {
        "v": PROTOCOL_VERSION,
        "t": "res",
        "id": frame_id,
        "ok": False,
        "error": {"code": code, "message": message, "detail": {}},
    }


# ---------------------------------------------------------------------------
# Perception
# ---------------------------------------------------------------------------


def observe(payload: dict[str, Any], source: str) -> dict[str, Any]:
    from perception import windows as win_perception

    want = payload.get("window")
    if want:
        # The broker already resolved which window it means; bringing it to the
        # front is what makes the foreground-based walk below look at the right
        # one.
        try:
            from actuation import actions

            actions.switch_to_window(want)
            time.sleep(0.15)
        except Exception:
            pass

    hwnd = 0
    try:
        hwnd = win_perception.foreground_hwnd() or 0
    except Exception:
        pass

    # `pixel` and `ocr` force the OCR path by asking the tree for nothing, so
    # the existing sparse-window fallback takes over.
    if source in ("pixel", "ocr"):
        title, elements = win_perception.list_elements(
            max_depth=1, max_count=255, timeout_s=0.2, allow_ocr_fallback=True
        )
    else:
        title, elements = win_perception.list_elements(
            max_depth=20, max_count=255, timeout_s=8.0, allow_ocr_fallback=True
        )

    out = []
    for e in elements:
        out.append(
            {
                "id": f"{source}:{hwnd:x}:{e.index}",
                "role": role_of(getattr(e, "kind", "")),
                "label": getattr(e, "name", "") or "",
                "value": None,
                "enabled": True,
                # Centre point widened into a watchable area. See ASSUMED_W.
                "bbox": [
                    int(e.x) - ASSUMED_W // 2,
                    int(e.y) - ASSUMED_H // 2,
                    ASSUMED_W,
                    ASSUMED_H,
                ],
                "container": None,
                "selector": None,
                "sensitive": looks_sensitive(getattr(e, "name", "") or ""),
            }
        )

    return {
        "window": title,
        "url": None,  # UIA does not expose the address bar's value reliably
        "elements": out,
        "truncated": len(out) >= 255,
    }


SENSITIVE_HINTS = ("password", "passcode", "pin", "cvv", "cvc", "secret", "otp", "card number")


def looks_sensitive(label: str) -> bool:
    low = label.lower()
    return any(h in low for h in SENSITIVE_HINTS)


# ---------------------------------------------------------------------------
# Actuation
# ---------------------------------------------------------------------------


def centre(target: dict[str, Any] | None) -> tuple[int, int] | None:
    if not target:
        return None
    bbox = target.get("bbox")
    if not bbox or len(bbox) != 4:
        return None
    x, y, w, h = bbox
    return int(x + w // 2), int(y + h // 2)


def act(payload: dict[str, Any]) -> dict[str, Any]:
    from actuation import actions
    from perception import windows as win_perception

    kind = payload.get("kind")
    target = payload.get("target")
    started = time.time()

    def done(applied: bool, readback: Any = None, reason: str | None = None) -> dict[str, Any]:
        result = {
            "applied": bool(applied),
            "readback": readback,
            "took_ms": int((time.time() - started) * 1000),
        }
        if reason:
            result["reason"] = reason
        return result

    if kind in ("wait", "none"):
        return done(True)

    if kind == "navigate":
        url = payload.get("url") or ""
        if not url:
            return done(False, reason="no url")
        from perception import apps as app_index

        actions.open_app_or_url(url, app_index.discover() if hasattr(app_index, "discover") else {})
        return done(True)

    if kind in ("open", "focus"):
        name = payload.get("text") or ""
        if not name:
            return done(False, reason="no application named")
        if kind == "focus":
            return done(actions.switch_to_window(name))
        from perception import apps as app_index

        index = app_index.discover() if hasattr(app_index, "discover") else {}
        actions.open_app_or_url(name, index)
        return done(True)

    if kind == "scroll" and not target:
        actions.scroll(int(payload.get("amount") or 3))
        return done(True)

    if kind == "key":
        # Handled before the position check: a keypress goes to whatever has
        # focus, so it needs no coordinates. Requiring them is what made
        # `press Enter` fail after a successful type.
        key_name = (payload.get("key") or "").strip()
        if not key_name:
            return done(False, reason="no key named")
        spot = centre(target)
        if spot is not None:
            # A target was supplied, so put focus there first.
            actions.click(*spot)
            time.sleep(0.12)
        if "+" in key_name:
            actions.hotkey(*[k for k in key_name.split("+") if k])
        else:
            actions.press_key(key_name)
        return done(True)

    point = centre(target)
    if point is None:
        return done(False, reason="no target position; this backend acts by position")
    x, y = point

    if kind == "click":
        actions.click(x, y)
        return done(True)

    if kind == "scroll":
        actions.click(x, y)
        actions.scroll(int(payload.get("amount") or 3))
        return done(True)

    if kind == "type":
        text = payload.get("text") or ""
        # Try the accessibility value pattern first: it writes the whole string
        # atomically, so a field that re-renders mid-keystroke cannot end up
        # holding half of it.
        placed = False
        try:
            placed = bool(win_perception.try_set_value_at(x, y, text))
        except Exception:
            placed = False
        if not placed:
            actions.click(x, y)
            time.sleep(0.12)
            actions.type_text(text)
        time.sleep(0.12)
        readback = None
        try:
            readback = win_perception.read_value_at(x, y)
        except Exception:
            readback = None
        # `applied: False` only on a *readable* mismatch. Plenty of controls
        # expose no value at all, and treating "unreadable" as failure would
        # make the broker fall back after every successful keystroke.
        if readback is not None and text[:40] not in readback:
            return done(False, readback, "the field did not take the text")
        return done(True, readback)

    if kind == "select":
        actions.click(x, y)
        wanted = payload.get("text")
        if wanted:
            time.sleep(0.2)
            actions.type_text(wanted)
            actions.press_key("enter")
        return done(True)

    return done(False, reason=f"unsupported kind {kind}")


# ---------------------------------------------------------------------------
# Main loop
# ---------------------------------------------------------------------------


def serve(source: str, pipe_base: str, reconnect: bool) -> int:
    path = rf"\\.\pipe\{pipe_base}.{current_user_sid()}"
    delay = 1.0

    while True:
        try:
            conn = PipeConn(path)
        except OSError as e:
            if not reconnect:
                print(f"kratos_backend: no broker at {path} ({e})", file=sys.stderr)
                return 1
            print(f"kratos_backend: waiting for the broker ({e}); retrying in {delay:.0f}s",
                  file=sys.stderr)
            time.sleep(delay)
            delay = min(delay * 2, 30.0)
            continue

        delay = 1.0
        try:
            run_session(conn, source)
        except Exception as e:
            print(f"kratos_backend: session ended: {e}", file=sys.stderr)
        finally:
            conn.close()

        if not reconnect:
            return 0
        print("kratos_backend: broker gone; waiting to reconnect", file=sys.stderr)
        time.sleep(1.0)


def run_session(conn: PipeConn, source: str) -> None:
    conn.write_frame(
        {
            "v": PROTOCOL_VERSION,
            "t": "req",
            "id": "1",
            "op": "hello",
            "payload": {
                "role": "backend",
                "name": f"python-{source}",
                "build": BUILD,
                "v_min": PROTOCOL_VERSION,
                "v_max": PROTOCOL_VERSION,
                "source": source,
                "handles": ["native", "browser"],
            },
        }
    )
    ack = conn.read_frame()
    if not ack or not ack.get("ok"):
        raise RuntimeError(f"handshake refused: {ack}")
    print(
        f"kratos_backend: registered as {source} with {ack['payload'].get('broker')} "
        f"(peer {ack['payload'].get('peer_id')})",
        file=sys.stderr,
    )

    # COM lives on whichever thread touches UI Automation, and every request is
    # handled on this one, so initialise once here. Getting this wrong is what
    # made an entire screen read as "(no window)" with zero elements.
    try:
        from perception.windows import _ensure_com

        _ensure_com()
    except Exception:
        pass

    while True:
        frame = conn.read_frame()
        if frame is None:
            return
        if frame.get("t") != "req":
            continue  # events are ignorable; replies are never addressed to us

        frame_id = frame.get("id") or "0"
        op = frame.get("op")
        payload = frame.get("payload") or {}

        try:
            if op == "backend.observe":
                conn.write_frame(ok(frame_id, observe(payload, source)))
            elif op == "backend.act":
                conn.write_frame(ok(frame_id, act(payload)))
            else:
                conn.write_frame(err(frame_id, "bad_request", f"unknown op {op}"))
        except Exception as e:
            # Always answer. Silence would surface as a backend timeout, which
            # the broker refuses to retry for a mutating action -- so a crash
            # here would strand the agent rather than falling back.
            conn.write_frame(err(frame_id, "backend_error", f"{type(e).__name__}: {e}"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--source",
        default="uia",
        choices=["uia", "ocr", "pixel"],
        help="which backend to register as (default: uia)",
    )
    parser.add_argument(
        "--pipe",
        default=os.environ.get("KRATOS_PIPE", "kratos.v1"),
        help="pipe base name, without the user SID suffix",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="exit when the broker goes away instead of waiting for it to return",
    )
    args = parser.parse_args()
    try:
        return serve(args.source, args.pipe, reconnect=not args.once)
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
