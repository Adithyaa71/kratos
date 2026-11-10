# Kratos — measurements

Everything in this file was measured on the machine it describes. Where a
number could not be measured, the row says so rather than carrying an
estimate. Where a target in `docs/design-multi-harness.md` was not met, it
says that too.

**The machine.** Windows 11 Pro 10.0.26300, single 1920×1080 display, D: is a
70 GB volume. Brave 157.1.99.55 is the default browser. The decision model is
TypeSafe Jev (`jev-1.14.0`) over HTTPS except where a row says the offline
mock. Release builds throughout.

**How to reproduce.** Both generators overwrite whatever file you point them
at, so send them to a scratch path and copy the tables in here — this document
is the report, not the generator's output.

```bash
cargo run --release -p kratos-bench -- --out /tmp/offline.md
./target/release/kratos-native.exe --bench "<window title>" --out /tmp/native.md
```

**Read the best-case column.** Every figure below is nearest-rank: no
interpolation, so each number printed is a measurement that actually happened.
p50 moves several tens of percent with what else is running on the machine —
the same broker cold start measured 26.5 ms on a quiet desktop and 38.9 ms
with two browsers, a chat client, Explorer and two Settings windows open. The
best-case column is the stable one. Quote p50 for "what a session feels like"
and best for "what the code costs".

---

## 1. The broker's own overhead

What sits between an agent and the screen: the MCP shim, the named pipe, the
broker's handling, the gate, candidate compression and the screen
fingerprints. The decision model here is the offline mock, so no network and
no key are involved and nothing is measuring somebody else's GPU.

| what | p50 ms | p95 ms | best ms | n | |
|---|---:|---:|---:|---:|---|
| mcp `tools/list` | **0.198** | 0.226 | 0.171 | 9 | 8 tools, from a static table |
| mcp cold, spawn included | **12.9** | 17.3 | 12.5 | 9 | process start + initialize + tools/list |
| broker cold start | **26.5** | 26.8 | 26.3 | 5 | spawn to first status reply |
| status round trip | **0.042** | 0.090 | 0.040 | 400 | agent → pipe → broker → agent |
| `log.tail 50` | **0.043** | 0.064 | 0.042 | 100 | journal read over the pipe |
| connect + handshake | **0.115** | 0.176 | 0.090 | 50 | new client, ACL check, hello/ack |
| candidate compression | **0.025** | 0.028 | 0.025 | 200 | 64 elements → options |
| decide (mock, offline) | **0.074** | 0.083 | 0.065 | 200 | two typed questions, 64 options |
| gate verdict | **0.004** | 0.005 | 0.004 | 20000 | block, deny, review, brand, rate |
| screen capture | **32.6** | 35.1 | 23.6 | 20 | 1920×1080 virtual desktop, BitBlt + DIB |
| full fingerprint | **3.30** | 3.71 | 2.75 | 200 | 64×36 greyscale from the grab |
| element fingerprint | **0.302** | 0.390 | 0.284 | 200 | 96×96 crop of one control |

**What these say.** The protocol is not the cost of anything. A status round
trip is 42 microseconds; the gate decides in four. An agent's whole non-visual
overhead per step is well under a millisecond, which means every number that
matters in this file is perception or the decision model, not plumbing.

**The cold start is the one to watch.** 26.5 ms from `CreateProcess` to a
served `status` is what makes "the first tool call starts the broker" a design
rather than an excuse. `spawn_native` is off in this rig deliberately: it
measures the broker, and with it on the cold start was p50 40.6 ms / best
38.4 ms against best 25.2 ms — so **launching the perception backend costs
about 13 ms on the cold path**. That is the honest price of the P7 default.

**Three screen grabs, not eight.** An action's pixel evidence needs a drift
baseline and a before and an after. Two of those come from one frame cropped
twice, which is why the full and element fingerprints are separate rows: 3.3 ms
and 0.3 ms against a 32.6 ms grab. The optimisation is worth 2 grabs per
action and it is measured, not asserted (`three_grabs_cost_less_than_eight`).

---

## 2. The native perception backend

`kratos-native`, measured against real windows on this machine. Not
reproducible the way section 1 is: these depend on which windows are open and
what is in them. They are here because the claim that matters — one cached
subtree fetch per window instead of a call per property — is only meaningful
against real applications.

### 2.1 UI Automation

| window | elements | cold ms | p50 ms | p90 ms | of which the fetch |
|---|---:|---:|---:|---:|---:|
| Brave, a loaded page | 86 | 51.5 | **22.5** | 23.0 | 21 ms |
| File Explorer, a folder view | 205 | 118.9 | **117.9** | 123.2 | 115 ms |

Earlier runs in the same build, for range:

| window | elements | p50 ms | when |
|---|---:|---:|---|
| Notepad | 57 | 31.6 | P4a |
| Notepad | 41 | 39.3 | P7, loaded desktop |
| File Explorer | 206 | 78.0 | P4a |
| File Explorer | 204 | 85.8 | P7 |
| File Explorer | 205 | 117.9 | now |
| Settings | 128 | 42.0 | P4a |

**The spec's target was ~150 elements at p50 ≤ 80 ms. Partly met.** 86
elements in 22.5 ms is comfortably inside it. 205 elements at 117.9 ms is
not, and the three File Explorer figures — 78, 85.8, 117.9 ms for the same
window and the same element count — are the same code on a busier machine
each time. I am not claiming the target as met for trees that size. The
fetch is 97% of the cost in every row, which is the point: it is one
cross-process call, and the walk over the cache afterwards is free.

**Against what it replaced.** The Python backend it retired in P7 managed 24
elements in 223 ms and 168 in 2.1 s on this machine. Per element that is
**roughly 27× slower**, and it also reported a fabricated bounding box —
the centre point widened to a fixed rectangle — where this reports the real
one.

### 2.2 Window capture and OCR

| window | geometry | capture | OCR | lines | second read |
|---|---|---:|---:|---:|---:|
| Brave, a loaded page | 1936×1048 | 25 ms | 112 ms | 61 | **0 ms** |
| File Explorer | 1210×735 | 38 ms | 55 ms | 51 | **0 ms** |
| a WinForms probe window | 520×200 | 27 ms | 30 ms | 10 | **0 ms** |

Range across the session: capture 19–38 ms, OCR 30–112 ms, and **0 ms on an
unchanged frame** every time.

**The spec's target was a window region at p50 ≤ 100 ms. Met**, except on a
1936×1048 full-page capture where OCR alone is 112 ms. The frame hash is what
makes this usable in a loop: a second observation of a screen nobody touched
costs the capture and nothing else, which is why the cached column is zero and
not merely small. Windows.Media.Ocr, offline, against the language packs this
user already has (`en-GB`, `en-US`) — no install, no network.

### 2.3 The icon detector

OmniParser v2 `icon_detect`, ONNX, **CPU only on this machine.**

| window | icons found | resize | network | second run |
|---|---:|---:|---:|---:|
| Brave, a loaded page | 128 | 6 ms | **380 ms** | 0 ms |
| File Explorer | 106 | 5 ms | **627 ms** | 0 ms |
| the settings window (a canvas app) | 29–61 | 5–9 ms | 352–700 ms | 0 ms |

**The spec's target was ≤ 60 ms, and that is a GPU figure. Not met, and not
claimed.** This machine has no GPU execution provider wired up, so every
number here is CPU, 380–700 ms, six to eleven times the target. Two things
make it usable anyway: it is cached by frame hash, so a loop pays it once; and
it only runs on the pixel path, which runs when the accessibility tree has
nothing — a quarter of the cases, by the operator's own estimate.

**GPU is not measured at all.** Not slower, not faster — unmeasured. Anyone
quoting 60 ms for this build is quoting the spec, not a measurement.

### 2.4 Fusion

| window | UIA | OCR lines | detector | fused out |
|---|---:|---:|---:|---:|
| Brave, a loaded page | 86 | 61 | 128 | **147** |
| File Explorer | 205 | 51 | 106 | **121** |

The fused count is not the sum, and that is the whole design: UIA leads, and
OCR text and detector boxes are added only where the tree has nothing.
Containment, not overlap, decides it for text — a "Send" label inside a Send
button covers 0.29 of it, so an IoU test alone reported the button twice and
handed the decision model two options that were the same thing.

---

## 3. The browser path, live

Measured through the broker against Brave started normally — from the taskbar,
no flags, no full quit, no reloading the extension — with the operator's own
tabs open. This is the workflow the tool exists for: a person opens a page and
logs in, an agent is handed the tab.

| page | elements | time | source |
|---|---:|---:|---|
| GitHub, the operator's own tab | 48 | **30 ms** | `dom-ext` |
| example.com | 1 | **4 ms** | `dom-ext` |
| example.com, warm | 1 | 7–8 ms | `dom-ext` |
| fr.wikipedia.org, a long article | 72 | **35 ms** | `dom-ext` |
| de.wikipedia.org, translate prompt on screen | 48 | **30 ms** | `dom-ext` |
| a new tab | 0 | 4 ms | `dom-ext` |

The 1-element and 0-element rows are correct answers, not failures:
example.com has exactly one link, and a new-tab page has nothing scriptable.
Reporting zero honestly is the behaviour under test.

### 3.1 One action, end to end

`click the quick search button`, on the operator's own GitHub tab:

| segment | ms |
|---|---:|
| observe (48 elements, `dom-ext`) | 11 |
| decide — Jev 1.14.0 | 413 |
| gate | < 1 |
| dispatch + settle + prove | ~1340 |
| **wall clock** | **1769** |

Decision: `click`, confidence **0.99** against a threshold of **0.70**.
Result `executed` through `dom-ext`.

**Where the time goes in a real step.** The decision model is 413 ms of it and
the broker is under a millisecond. The remaining 1.3 s is settle-and-prove:
the action is dispatched, the target's own pixels are polled against drift
measured moments earlier, and the screen is re-read to show something changed.
That is the cost of `executed` meaning *earned*, and it is the single largest
item in any real action.

### 3.2 Two lanes at once

| lane | page | observe |
|---|---|---:|
| brave / Default | example.com | 5 ms |
| vivaldi / Default | example.org | 5 ms |

A cross-lane observe is refused by name, not served. Two sessions act in
parallel while a third holds the input lease, because a DOM action goes
through the page's own event system and needs no pointer — which is the whole
reason lanes exist.

### 3.3 Opening a lane

| | |
|---|---|
| cold Brave launch → instance bound → lane ready | **1.8 s** |
| into an already-running browser | process count unchanged |
| bounded recovery | 3 steps, then stops |
| the ladder per lane, not per request | first observe 6.9 s, then 5.2 s, 5.4 s |

---

## 4. Native actuation, live

| action | method | evidence |
|---|---|---|
| type into a text box | `semantic` (ValuePattern) | field read back `someone@example.com` |
| click a button | `semantic` (InvokePattern) | the app's own label said `sent to someone@example.com` |
| toggle a checkbox | `semantic` (TogglePattern) | no Invoke on a checkbox; a toggle is the click |
| press Enter | `physical` (SendInput) | survives a stale element id attached to it |
| click a control only the pixels found | `physical` | 1103 ms, on a window with no accessibility tree |

**Semantic first, physical second, and the result says which.** A click through
`InvokePattern` *is* the button being pressed. A click at a coordinate is a
guess that the button is still where it was. Both happen — most of a Windows
desktop exposes no pattern for most things — and the difference is reported
rather than hidden.

### 4.1 A window with no accessibility tree

The settings window is an egui GL canvas: UIA reports six elements and all six
are title bar. Read through the pixel path instead:

| | |
|---|---|
| observe, default route | 6 elements, all title bar |
| observe, `prefer: pixel` | **55 elements, 461 ms** |
| click a tab, pinned by id | `executed`, `physical`, 1103 ms |

All five tabs were driven this way and their contents read back off the
screen. **This is the only end-to-end proof of the physical path in the
build**, and it is also the case the spec's sparse-tree rule was meant to
catch and does not: six title-bar elements is above the "≤ 2" threshold, so
the pixel path has to be asked for by name on a window like this.

---

## 5. What the fixes were worth

Measured before and after, same machine.

| | before | after |
|---|---|---|
| D: free space | **0.1 GB** of 70 | **19.5 GB** |
| `target/` on disk | 25.5 GB | **4.2 GB** (full debug + release + tests) |
| console window on "Start broker" | one, empty, for as long as the broker ran | none |
| observe with a browser prompt on screen | 15 el, **20050 ms**, `uia`, `'Translate this page?'` | 48 el, **30 ms**, `dom-ext`, the page |
| capture, screen locked | 428–751 ms, **0 lines read** | — |
| capture, screen unlocked | — | 19–39 ms, 10 lines, 3 runs in 3 |
| full-screen grab, locked | **1.7 s** | 32.6 ms unlocked |
| the capture test in the full suite | failed ~1 run in 3 | 428 tests, 0 failures, 2 full runs |

**The disk cap.** `target/debug` was 23.5 GB against 2.0 GB for the entire
release tree — almost all of it debug info, most of that belonging to
dependencies nobody steps through. Dev and test builds now keep line tables,
which is what a backtrace actually uses, and dependencies keep none. A full
build of everything is 4.2 GB.

---

## 6. The matrix

Two fake agents × two fake browser instances × four kill scopes × pause on and
off = **16 cells, one test**, on `brave` and `chrome`.

| scope | stops lane A | stops lane B |
|---|---|---|
| none | — | — |
| `browser:brave` | yes | — |
| `browser:chrome` | — | yes |
| `global` | yes | yes |

Each cell also asserts that a DOM action is never *paused* — it needs no
pointer, so there is nothing for a person at the keyboard to collide with —
and that an agent asking to **clear** a kill switch is refused.

**One branch is not in the matrix and cannot be.** Pause fires on real
keyboard and mouse input, and input this process injects is ignored on purpose
— that is the entire point of the `LLMHF_INJECTED` check — so no test outside
the broker's own process can produce the condition. It is covered by a unit
test in `activity.rs`; the matrix checks that the setting is honoured and
reported.

---

## 7. Correctness, and what is not measured about it

**What is checked.** A fixture corpus of real UIA dumps, asserting the
invariants an observation hands the decision model: ids unique within an
observation, rectangles that are somewhere, roles from one vocabulary,
containers that name something. Those are exactly what a fusion pass can break
quietly, which is why they are pinned.

**What is not.** The spec asks for recall, label exactness and bbox IoU per
source and fused. **Not measured.** Scoring needs a golden element list per
window, and the corpus is deliberately not committed — a UIA dump carries
whatever is on the operator's screen, the file open in Notepad, the names in a
folder, the subject lines in a mail client. So the golden lists would have to
be hand-labelled on each machine, and nobody has done it. There is no accuracy
number in this build and this file will not invent one.

**The suite.** 428 tests, no clippy warnings, two consecutive full runs with
zero failures. Six live tests check whether the session is locked and skip with
a message rather than failing — a red suite that means "your screen was
locked" is a suite people learn to ignore. They also take the broker's own
machine-wide input guard, so they run one at a time and do not fight a broker
the operator has running.

---

## 8. Known and unfixed

**Nine `kratos-host.exe` per browser start**, one serving and eight orphaned.
Chromium does not close a dropped port's stdin *or* make it unwritable, so a
host whose service worker was evicted cannot tell. Two fixes were tried and
both were worse:

| attempt | result |
|---|---|
| liveness probe every 10 s | nine were still nine after 60 s — the write never fails |
| superseded host exits | count 1, flat for 75 s — **but 280 broker reconnections in 40 s** and `dom-ext` stopped answering |

Both reverted. Each replacement host speaks before being superseded, which
resets the extension's backoff to one second, which is the runaway the existing
comment in `native.rs` warns about. The shipped behaviour is the best of the
three measured. The eight idle processes all carry the same instance token, so
the broker registers one and the rest hold nothing: waste, not a fault.

**The first observe after a page loads** can miss `dom-ext` and fall back to
the tree (~5 s); a retry a moment later answers in 35 ms. Seen twice. Not
diagnosed.

**A tick in the settings window saves nothing without Apply.** Every tab edits
a local draft and only Apply sends it, with no "unsaved" marker — so a ticked
box looks exactly like a saved setting. Found by watching a real attempt to
approve a harness.

**The four real tasks in the spec's addendum have not been run.** Scripts are
written out step by step in [`real-tasks.md`](real-tasks.md). They need an
unlocked screen and an approved harness, and an agent may not approve itself.

**No GPU numbers for the detector**, as section 2.3 says. Unmeasured, not
estimated.

---

## 9. Summary, for quoting

| claim | number | confidence |
|---|---|---|
| protocol overhead per step | < 1 ms | measured, reproducible offline |
| broker cold start | 26 ms | measured, reproducible offline |
| read a browser page | 30 ms, 48 elements | measured live, repeatedly |
| read a native window, ~90 elements | 22 ms | measured live |
| read a native window, ~205 elements | 78–118 ms | measured live, varies with load |
| capture + OCR a window | 25 ms + 30–112 ms | measured live |
| icon detector, CPU | 380–630 ms | measured live |
| icon detector, GPU | — | **not measured** |
| one full action, browser, end to end | 1.8 s | measured live, once |
| decision model | 413 ms | measured live, once |
| faster than the Python backend it replaced | ~27× per element | measured on both |
| accuracy | — | **not measured** |
