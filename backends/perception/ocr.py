"""OCR fallback perception, for windows that expose almost nothing through
the accessibility tree -- canvas-drawn UIs, games, some editors, remote
desktop. This is the documented weak spot in RESEARCH.md ("observe will
return very few interactive elements and say so") and in skills/jev-interface
/SKILL.md. windows.list_elements() falls back to this automatically when it
returns too few candidates; it is not used as the primary path because the
accessibility tree, where available, is exact (real widget types, real
labels) and OCR is not (misreads, no notion of "button" vs "label").
"""
from __future__ import annotations

import os

import pytesseract
from PIL import ImageGrab

from perception.windows import Element

_TESSERACT_PATHS = [
    r"C:\Program Files\Tesseract-OCR\tesseract.exe",
    r"C:\Program Files (x86)\Tesseract-OCR\tesseract.exe",
]


def _ensure_tesseract() -> bool:
    if pytesseract.pytesseract.tesseract_cmd and os.path.exists(pytesseract.pytesseract.tesseract_cmd):
        return True
    for p in _TESSERACT_PATHS:
        if os.path.exists(p):
            pytesseract.pytesseract.tesseract_cmd = p
            return True
    return False


def ocr_elements(bbox: tuple[int, int, int, int], min_confidence: int = 40,
                  max_count: int = 254) -> list[Element]:
    """OCR everything inside `bbox` (left, top, right, bottom in screen
    coordinates) and return it as text candidates with click centers.
    `kind` is "OcrText" so callers (and the resolver's instruction wording)
    can tell these apart from real, typed accessibility elements -- an OCR
    read is a text match, not a confirmed interactive control."""
    if not _ensure_tesseract():
        return []
    try:
        image = ImageGrab.grab(bbox=bbox)
    except Exception:
        return []

    try:
        data = pytesseract.image_to_data(image, output_type=pytesseract.Output.DICT)
    except Exception:
        return []

    left_off, top_off = bbox[0], bbox[1]
    elements: list[Element] = []
    seen: set[str] = set()
    n = len(data.get("text", []))
    for i in range(n):
        if len(elements) >= max_count:
            break
        text = (data["text"][i] or "").strip()
        if not text or len(text) < 2:
            continue
        try:
            conf = int(float(data["conf"][i]))
        except (ValueError, KeyError):
            conf = -1
        if conf < min_confidence:
            continue
        key = text.lower()
        if key in seen:
            continue
        seen.add(key)
        x = left_off + data["left"][i] + data["width"][i] // 2
        y = top_off + data["top"][i] + data["height"][i] // 2
        elements.append(Element(len(elements), text, "OcrText", x, y))
    return elements
