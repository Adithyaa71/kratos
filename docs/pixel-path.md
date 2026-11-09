# The pixel path

For windows that declare nothing: a canvas app, a remote desktop, a game, a
toolbar of unlabelled icons. UI Automation reports a window and nothing in it,
OCR finds the words but not the controls, and this finds the controls.

It is **off unless you turn it on**, and turning it on means two files that
this repository does not ship.

---

## Why it is not included

**The weights are AGPL-3.0.** OmniParser v2's icon detector derives from
YOLOv8, and its licence is not this project's to pass on. Shipping it would
make the licence of everything it is shipped with somebody's problem, so the
detector is a thing you add rather than a thing you have.

**The runtime is 16MB of somebody else's binary.** `ort`'s default features
download an onnxruntime build during `cargo build`. A network fetch inside a
build, producing a binary nobody audited, is not a dependency this takes on:
kratos loads an onnxruntime that is already on the machine instead.

A machine with neither has exactly the perception it had before any of this
existed — the accessibility tree and OCR — and says so plainly rather than
failing.

---

## Turning it on

### 1. The weights

```bash
# 77MB, AGPL-3.0
curl -L -o models/omniparser/icon_detect.onnx \
  https://huggingface.co/onnx-community/OmniParser-v2.0_icon_detect/resolve/main/onnx/model.onnx
```

Put the file at `models/omniparser/icon_detect.onnx`, beside the executable or
at the root of a development checkout; `KRATOS_ICON_MODEL` overrides the path.
`models/` is gitignored.

The model is a single-class YOLOv8 head: input `images [1,3,640,640]`, output
`output0 [1,5,8400]` — centre x, centre y, width, height, score.

### 2. The runtime

Put `onnxruntime.dll` beside `kratos-native.exe`, or point `ORT_DYLIB_PATH` at
one. Any 1.x onnxruntime does; a Python `onnxruntime` install already has one
at `site-packages/onnxruntime/capi/onnxruntime.dll`.

If it is built with a GPU provider, that provider is used. This project's
measurements are CPU, because the machine they were taken on has no GPU
provider.

### 3. The switch

`backends.enabled` in `config.json` decides which sources the broker routes
to. Take `"pixel"` out of that list and nothing reaches the detector however
many weights are on disk:

```json
"backends": { "enabled": ["dom-ext", "uia", "ocr"] }
```

So there are two independent off switches — the file not being there, and the
source not being routed to — and both are the operator's.

---

## What it does

Three sources are reconciled into one list, and **UI Automation leads**:

| source | knows | does not know |
|---|---|---|
| `uia` | what a thing *is*: role, name, container, value, enabled | anything drawn rather than declared |
| `ocr` | where the words are | what they belong to |
| `pixel` | where the controls are | what they do |

* A detector box over a UIA control **is** that control: the box is dropped,
  because the tree's answer has a role, a name and a container, and reporting
  both would hand the decision model two candidates that are the same thing.
  Two candidates that score alike is the exact failure the broker's
  `ambiguous` refusal exists to report; manufacturing it here would be
  self-inflicted.
* A line of text **inside** an element is that element's own label, not a
  separate thing. Containment rather than overlap: a "Send" label inside a
  Send button covers about 0.29 of it, so an overlap test alone reports the
  word twice.
* A word inside a detector box becomes that box's label — the detector found
  the control, OCR found what it is called, and together they are one usable
  element rather than two halves. A word merely *near* a box is not its name:
  guessing from proximity is how a Delete icon gets labelled Save.
* Overlap of more than **0.5** means one element. Generous on purpose: a box
  drawn tight around an icon and a UIA rectangle that includes its padding
  overlap by about that much, and treating them as different would double
  every toolbar.

Every element carries `source` — `uia`, `ocr`, `pixel`, or `pixel+ocr` — and a
`confidence` only where something measured one. UI Automation reports no
confidence, because a control either is in the tree or is not, and inventing
`1.0` would put a number next to something that was never measured.

**A sparse tree runs the pixel path without being asked.** Two elements or
fewer is not a tree: a canvas app reports its own window and perhaps a scroll
bar. Taking "there is nothing here" at face value is how an agent concludes a
screen is empty when it is full.

---

## What it costs

Measured numbers, including the split between the resize this code does and
the network onnxruntime runs, are in
[native-benchmarks.md](native-benchmarks.md) and in
[benchmarks.md](benchmarks.md).

The short version on a six-core CPU with no GPU provider: the letterbox costs
4–6ms, the network costs **360–700ms**, and the same frame again costs
nothing — the detector keeps the last frame's result against a hash of its
pixels, as the OCR engine does.

That is far above the 60ms the design document asks for, and the design
document asks for it **on a GPU**. On this hardware it is not met and is not
claimed.

---

## What is not here

* **Captions for unlabelled icons.** Florence-2 on demand, cached by crop
  hash, is in the design and not built. An icon with no word on it comes back
  with an empty label, which is honest: nobody knows what it is yet.
* **Accuracy numbers.** Recall, label exactness and bbox IoU against a golden
  element list need a corpus that can be committed, and a UIA dump or a
  screenshot of this machine is a picture of somebody's screen. What exists is
  a structural check against a locally generated corpus and the per-source
  element counts in the benchmark table.
