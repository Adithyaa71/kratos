"""Windows UI perception: turn the foreground window into a ranked, budgeted
list of interactive elements -- the only thing Jev is allowed to choose from.
No vision; this reads the accessibility (UI Automation) tree, which is
already text. See RESEARCH.md: perception must be deterministic and produce
clean, minimal, pre-filtered state before a Jev call is made.
"""
from __future__ import annotations

import threading
import time
from dataclasses import dataclass

import uiautomation as auto

_com = threading.local()


def _ensure_com() -> None:
    """Initialise COM on whichever thread is about to touch UI Automation.

    uiautomation loads UIAutomationCore.dll through COM, and COM must be
    initialised per thread. It happens to be initialised on the main thread
    at import, which is why this is invisible from a script -- but the MCP
    server runs tool handlers on an anyio worker thread that has never called
    CoInitialize. There, every UIA call fails with

        [WinError -2147221008] CoInitialize has not been called

    and because the callers below swallow exceptions, the whole screen reads
    as "(no window)" with zero elements. That is not a visible crash; it is
    silent blindness, and it makes the agent fall back to launching its own
    browser because that is the only backend still answering.

    Apartment-threaded, because that is what UI Automation expects. Called at
    the top of every entry point here; cheap after the first time per thread.
    """
    if getattr(_com, "ready", False):
        return
    try:
        import comtypes
        comtypes.CoInitializeEx(comtypes.COINIT_APARTMENTTHREADED)
    except OSError:
        # Already initialised on this thread, possibly in another apartment.
        # Nothing to do, and nothing worth failing over.
        pass
    except Exception:
        pass
    _com.ready = True

INTERACTIVE_TYPES = {
    "ButtonControl", "EditControl", "HyperlinkControl", "MenuItemControl",
    "CheckBoxControl", "ComboBoxControl", "ListItemControl", "TabItemControl",
    "RadioButtonControl", "TreeItemControl", "SplitButtonControl",
}

MAX_CANDIDATES = 254  # Jev's Choice cap is 255; leave room for "none of these"


@dataclass
class Element:
    index: int
    name: str
    kind: str
    x: int
    y: int


def foreground_title() -> str:
    _ensure_com()
    try:
        return auto.GetForegroundControl().Name or "(untitled)"
    except Exception:
        return "(unknown)"


# Limits used once a page document has been found. Relative to the document,
# not the window, so they stay modest: Gmail's deepest controls sit ~9 levels
# under it, and the whole subtree walks in well under a second.
# 20, not 16: Gmail's compose body is an EditControl named "Message Body" at
# depth 17 below the document. At 16 it was one level out of reach, so the
# field simply did not exist as a candidate and every attempt to write the
# message escalated at ~0.6 confidence -- a silent miss that looked like the
# model being unsure rather than the field being invisible.
DOCUMENT_MAX_DEPTH = 20
DOCUMENT_TIMEOUT_S = 8.0

# Browser chrome is shallow; this keeps tabs and the address bar without
# re-descending into the page that was already walked.
CHROME_MAX_DEPTH = 6

# How far down to look for the document itself. Chromium puts it at depth 7
# behind a stack of unnamed panes and a "Chrome Legacy Window".
_DOCUMENT_SEARCH_DEPTH = 9
_DOCUMENT_SEARCH_TIMEOUT_S = 2.0


def _content_document(win):
    """The DocumentControl holding the rendered page, or None for a native app.

    Chromium exposes more than one DocumentControl, and the first one found is
    typically an empty placeholder -- walking it yields nothing at all. The one
    that matters has both a name (the page title) and children, so require both.
    """
    deadline = time.time() + _DOCUMENT_SEARCH_TIMEOUT_S

    def search(ctrl, depth: int):
        if depth > _DOCUMENT_SEARCH_DEPTH or time.time() > deadline:
            return None
        try:
            children = ctrl.GetChildren()
        except Exception:
            return None
        for child in children:
            try:
                if child.ControlTypeName == "DocumentControl" and (child.Name or "").strip():
                    if child.GetChildren():
                        return child
            except Exception:
                pass
            found = search(child, depth + 1)
            if found is not None:
                return found
        return None

    try:
        return search(win, 0)
    except Exception:
        return None


OCR_FALLBACK_THRESHOLD = 2  # at or below this many UIA elements, try OCR instead
# A real page with actual content exposes dozens of elements; a page with
# none still often has a handful of chrome spillover (a notification
# banner's close button, a tab-preview tooltip) below the toolbar line.
# Found live, against Gmail: 5 such elements, zero of them page content.
CONTENT_OCR_THRESHOLD = 8


def list_elements(max_depth: int = 8, max_count: int = MAX_CANDIDATES, timeout_s: float = 3.0,
                   allow_ocr_fallback: bool = True):
    """Returns (window_title, [Element, ...]) for the current foreground window.

    Tries the accessibility tree first -- it's exact: real widget types, real
    labels. Canvas-drawn UIs (games, some editors, remote desktop) expose
    almost nothing through it, so when that returns too few candidates this
    falls back to OCR over the window's screen region (see perception/ocr.py).
    OCR candidates are tagged kind="OcrText" so the resolver and the caller
    can tell a text match from a confirmed interactive control."""
    _ensure_com()
    try:
        win = auto.GetForegroundControl()
    except Exception:
        return "(no window)", []

    title = win.Name or "(untitled)"
    elements: list[Element] = []
    seen_names: set[tuple[str, str]] = set()
    start = time.time()

    # Browser page content sits far below the window root -- in Brave, Gmail's
    # "Compose" is at depth 13, under a DocumentControl at depth 7. A walk from
    # the root with the native-app defaults (depth 8, 3s) never reaches it and
    # returns only browser chrome, so the page reads as empty and OCR takes
    # over. Walking from the document instead skips the whole chrome subtree:
    # measured against this Gmail inbox, 198 page elements in 715ms, where a
    # root walk deep enough to find them took 25s and still truncated.
    doc = _content_document(win)

    # Limits are per-walk: deep for the page, shallow for the chrome around it.
    limit_depth = max_depth
    limit_timeout = timeout_s

    def walk(ctrl, depth: int):
        if depth > limit_depth or len(elements) >= max_count or (time.time() - start) > limit_timeout:
            return
        try:
            children = ctrl.GetChildren()
        except Exception:
            return
        for c in children:
            if len(elements) >= max_count or (time.time() - start) > timeout_s:
                return
            try:
                ctype = c.ControlTypeName
                if ctype in INTERACTIVE_TYPES:
                    name = (c.Name or "").strip()
                    if name:
                        key = (name, ctype)
                        if key not in seen_names:
                            seen_names.add(key)
                            rect = c.BoundingRectangle
                            cx = (rect.left + rect.right) // 2
                            cy = (rect.top + rect.bottom) // 2
                            if rect.right > rect.left and rect.bottom > rect.top:
                                elements.append(Element(len(elements), name, ctype, cx, cy))
            except Exception:
                pass
            walk(c, depth + 1)

    if doc is not None:
        # Page first, so its controls get the low indices and survive the
        # max_count cap -- the page is what the instruction is usually about.
        limit_depth, limit_timeout = DOCUMENT_MAX_DEPTH, DOCUMENT_TIMEOUT_S
        walk(doc, 0)
        # Then the chrome, shallow: tabs, address bar, back/forward are real
        # targets ("switch to the Gmail tab") and sit within a few levels.
        limit_depth, limit_timeout = CHROME_MAX_DEPTH, timeout_s
        walk(win, 0)
    else:
        walk(win, 0)

    if allow_ocr_fallback and len(elements) <= OCR_FALLBACK_THRESHOLD:
        # Whole window is sparse (canvas/custom-drawn UI) -- OCR the lot.
        try:
            rect = win.BoundingRectangle
            bbox = (rect.left, rect.top, rect.right, rect.bottom)
            if bbox[2] > bbox[0] and bbox[3] > bbox[1]:
                from perception.ocr import ocr_elements  # lazy: avoids a circular import
                ocr_found = ocr_elements(bbox, max_count=max_count)
                if ocr_found:
                    elements = ocr_found
        except Exception:
            pass  # UIA's (possibly empty) result stands if OCR can't run
    elif allow_ocr_fallback and elements:
        # A real (non-automation) Chromium browser window without
        # --force-renderer-accessibility shows this pattern: plenty of UIA
        # elements, but ALL of them are toolbar/tab-strip chrome -- the page
        # content area is empty, because the renderer never published its
        # accessibility tree. A flat "total count" threshold misses this
        # entirely (61 chrome elements looks "rich"). Found live, driving
        # the user's actual Gmail: Compose/inbox content was completely
        # invisible via UIA despite 61 real elements being returned.
        try:
            rect = win.BoundingRectangle
            content_top = rect.top + 130  # approx toolbar+bookmarks-bar height
            content_elements = [e for e in elements if e.y > content_top]
            if len(content_elements) <= CONTENT_OCR_THRESHOLD and rect.bottom > content_top:
                from perception.ocr import ocr_elements
                content_bbox = (rect.left, content_top, rect.right, rect.bottom)
                ocr_found = ocr_elements(content_bbox, max_count=max(0, max_count - len(elements)))
                if ocr_found:
                    merged = list(elements)
                    for oe in ocr_found:
                        merged.append(Element(len(merged), oe.name, oe.kind, oe.x, oe.y))
                    elements = merged[:max_count]
        except Exception:
            pass  # UIA's chrome-only result stands if OCR can't run

    return title, elements


def list_windows() -> list[str]:
    """All visible top-level window titles, for switch_window candidates."""
    _ensure_com()
    titles: list[str] = []
    try:
        desktop = auto.GetRootControl()
        for c in desktop.GetChildren():
            try:
                if c.Name and c.ControlTypeName == "WindowControl":
                    titles.append(c.Name)
            except Exception:
                pass
    except Exception:
        pass
    return titles


def try_set_value_at(x: int, y: int, text: str) -> bool:
    """UIA ValuePattern typing: set a field's value directly through the
    accessibility API instead of simulating keystrokes. More reliable (no
    OS keyboard focus race, no layout-dependent character mangling) when the
    control supports it; returns False so the caller can fall back to
    click + clipboard-paste when it doesn't."""
    _ensure_com()
    try:
        ctrl = auto.ControlFromPoint(x, y)
        pattern = ctrl.GetValuePattern()
        if pattern and not pattern.IsReadOnly:
            pattern.SetValue(text)
            return True
    except Exception:
        pass
    return False


def read_value_at(x: int, y: int) -> str | None:
    """The current text of the control under (x, y), or None if it has none.

    Used to confirm that typing actually landed. Dispatching a keystroke is
    not evidence the field received it: a ValuePattern.SetValue can report
    success while the control ignores it, and a clipboard paste goes to
    whatever holds focus, which is not always what was clicked. Found live --
    a Gmail subject line reported typed at 0.99 confidence and was empty.
    """
    _ensure_com()
    try:
        ctrl = auto.ControlFromPoint(x, y)
    except Exception:
        return None

    for getter in ("GetValuePattern", "GetLegacyIAccessiblePattern"):
        try:
            pattern = getattr(ctrl, getter)()
            if pattern is not None:
                value = pattern.Value
                if value is not None:
                    return str(value)
        except Exception:
            continue
    return None


def foreground_hwnd() -> int | None:
    try:
        return auto.GetForegroundControl().NativeWindowHandle
    except Exception:
        return None


def is_elevated_window(hwnd: int) -> bool | None:
    """True if the foreground window's process is running elevated (as
    admin) while we are not -- Windows UIPI silently blocks input from a
    lower-integrity process to a higher one, so clicks/keys would appear to
    do nothing. Returns None, not False, when elevation can't be determined,
    so a caller can tell "known not elevated" from "couldn't check"."""
    _ensure_com()
    try:
        import win32api
        import win32con
        import win32process
        import win32security

        _, pid = win32process.GetWindowThreadProcessId(hwnd)
        h_process = win32api.OpenProcess(win32con.PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        h_token = win32security.OpenProcessToken(h_process, win32security.TOKEN_QUERY)
        target_elevated = bool(win32security.GetTokenInformation(h_token, win32security.TokenElevation))

        our_token = win32security.OpenProcessToken(
            win32api.GetCurrentProcess(), win32security.TOKEN_QUERY)
        we_are_elevated = bool(win32security.GetTokenInformation(our_token, win32security.TokenElevation))

        return target_elevated and not we_are_elevated
    except Exception:
        return None
