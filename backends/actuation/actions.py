"""Executes typed, already-gated decisions against the real desktop.
Nothing here decides anything -- by the time a function in this module is
called, the resolver has already gated on confidence. This module just does
exactly what it's told, precisely, in code -- no judgment calls.
"""
from __future__ import annotations

import os
import time
import webbrowser

import pyautogui
import win32con
import win32gui

pyautogui.FAILSAFE = False
pyautogui.PAUSE = 0.02


def click(x: int, y: int) -> None:
    pyautogui.moveTo(x, y, duration=0.06)
    pyautogui.click(x, y)


def double_click(x: int, y: int) -> None:
    pyautogui.moveTo(x, y, duration=0.06)
    pyautogui.doubleClick(x, y)


def type_text(text: str) -> None:
    pyautogui.typewrite(text, interval=0.012)


def press_key(key: str) -> None:
    pyautogui.press(key)


def hotkey(*keys: str) -> None:
    pyautogui.hotkey(*keys)


def scroll(amount: int) -> None:
    pyautogui.scroll(amount)


def _snapshot_windows() -> dict:
    """hwnd -> title for every visible top-level window right now."""
    windows: dict = {}

    def _cb(hwnd, _):
        if win32gui.IsWindowVisible(hwnd):
            text = win32gui.GetWindowText(hwnd)
            if text:
                windows[hwnd] = text

    win32gui.EnumWindows(_cb, None)
    return windows


BROWSER_CLASSES = {"Chrome_WidgetWin_1", "MozillaWindowClass"}  # Chromium (Brave/Chrome/Edge), Firefox


def _wait_for_new_window(before: dict, expect_name: str, timeout_s: float = 6.0,
                          allow_browser: bool = False) -> bool:
    """Poll for the window we just launched and focus it. A freshly
    os.startfile'd app (especially a UWP one like the modern Calculator)
    can take seconds to appear and doesn't reliably steal foreground focus
    on its own -- without this, screen_after/verify report a stale window.

    Matches by NAME against what we just launched, not merely "any new
    window" -- an unrelated window appearing during the poll (a browser tab
    finishing a title change, a notification) is a real false positive that
    live testing caught, and focusing the wrong window is worse than
    focusing none."""
    stem = expect_name.lower().strip()
    stem = stem.rsplit(".", 1)[0] if "." in stem else stem  # "calc.exe" -> "calc"
    deadline = time.time() + timeout_s

    while time.time() < deadline:
        after = _snapshot_windows()
        new = {hwnd: title for hwnd, title in after.items() if hwnd not in before}
        for hwnd, title in new.items():
            if not allow_browser and win32gui.GetClassName(hwnd) in BROWSER_CLASSES:
                continue  # a browser window can appear at any moment for unrelated reasons
            if stem and stem in title.lower():
                _focus(hwnd)
                return True
        time.sleep(0.15)

    # Named match never showed up among trustworthy candidates -- report
    # failure rather than guess at focusing something unrelated.
    return False


def _focus(hwnd) -> None:
    try:
        if win32gui.IsIconic(hwnd):
            win32gui.ShowWindow(hwnd, win32con.SW_RESTORE)
        win32gui.SetForegroundWindow(hwnd)
    except Exception:
        pass


def open_app_or_url(target_name: str, apps: dict) -> str:
    # Defense in depth: strip trailing sentence punctuation before judging
    # whether this looks like a URL. A bare "." from STT punctuation
    # ("paint.") must never look like a domain -- confirmed live, it opened
    # a browser to https://paint. instead of launching mspaint.exe.
    low = target_name.lower().strip().rstrip(".!?,;:")
    before = _snapshot_windows()
    first_word = low.split()[0] if low else ""

    if low.startswith("http://") or low.startswith("https://") or (
        "." in first_word and not first_word.endswith(".") and " " not in first_word
        and len(first_word) < 60 and first_word.lower() not in apps
    ):
        url = low if "://" in low else f"https://{low}"
        domain = low.split("/")[0].split("://")[-1].split(".")[0]
        webbrowser.open(url)
        focused = _wait_for_new_window(before, domain or low, allow_browser=True)
        return f"opened url {url}" + ("" if focused else " (opened, but couldn't confirm focus)")

    if low in apps:
        os.startfile(apps[low])  # noqa: S606 -- intentional, gated by the resolver upstream
        focused = _wait_for_new_window(before, target_name)
        return f"launched {target_name} ({apps[low]})" + ("" if focused else " (launched, but couldn't confirm focus)")

    # fuzzy fallback: substring match either direction
    for name, path in apps.items():
        if low in name or name in low:
            os.startfile(path)
            focused = _wait_for_new_window(before, name)
            return (f"launched {name} ({path}) [fuzzy match for {target_name!r}]" +
                    ("" if focused else " (launched, but couldn't confirm focus)"))

    raise FileNotFoundError(f"no known app or url matches {target_name!r}")


def switch_to_window(title_substring: str) -> bool:
    match = {"hwnd": None}

    def _cb(hwnd, _):
        if win32gui.IsWindowVisible(hwnd) and title_substring.lower() in win32gui.GetWindowText(hwnd).lower():
            match["hwnd"] = hwnd

    win32gui.EnumWindows(_cb, None)
    if not match["hwnd"]:
        return False

    if win32gui.IsIconic(match["hwnd"]):
        win32gui.ShowWindow(match["hwnd"], win32con.SW_RESTORE)

    try:
        win32gui.SetForegroundWindow(match["hwnd"])
    except Exception:
        # Windows refuses the foreground to a process that does not currently
        # own it, and pywin32 raises for that -- with error code 0, "no error
        # message is available", which is not a failure anyone can read. The
        # call often half-works anyway (the window is raised but not
        # activated), so swallow it and let the check below decide.
        pass

    time.sleep(0.15)

    # Report what actually happened rather than that the call was made: a
    # refused activation that returns True sends the next click to whatever
    # window is really in front.
    try:
        return win32gui.GetForegroundWindow() == match["hwnd"]
    except Exception:
        return False
