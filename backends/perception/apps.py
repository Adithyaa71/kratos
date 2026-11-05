"""App catalogue: discover installed apps from Start Menu shortcuts, so
'open X' resolves against what's actually installed on this machine instead
of a hardcoded guess-list. Rebuilt fresh (not copied) -- same idea as the
"154 apps, not 14 guesses" principle: enumerate, don't hardcode.
"""
from __future__ import annotations

import glob
import os

START_MENU_DIRS = [
    os.path.join(os.environ.get("ProgramData", r"C:\ProgramData"),
                 r"Microsoft\Windows\Start Menu\Programs"),
    os.path.join(os.environ.get("APPDATA", ""),
                 r"Microsoft\Windows\Start Menu\Programs"),
]


def discover_apps() -> dict:
    """Returns {lowercase app name: path to .lnk or .exe}."""
    catalog: dict[str, str] = {}
    for base in START_MENU_DIRS:
        if not base or not os.path.isdir(base):
            continue
        for path in glob.glob(os.path.join(base, "**", "*.lnk"), recursive=True):
            name = os.path.splitext(os.path.basename(path))[0]
            catalog.setdefault(name.lower().strip(), path)
    # A few well-known system apps that don't always have a Start Menu shortcut
    system32 = os.path.join(os.environ.get("WINDIR", r"C:\Windows"), "System32")
    for exe, aliases in {
        "notepad.exe": ["notepad"],
        "calc.exe": ["calculator", "calc"],
        "mspaint.exe": ["paint"],
        "explorer.exe": ["file explorer", "explorer", "files"],
        "taskmgr.exe": ["task manager"],
        "control.exe": ["control panel"],
    }.items():
        full = os.path.join(system32, exe)
        if os.path.exists(full):
            for alias in aliases:
                catalog.setdefault(alias, full)
    return catalog
