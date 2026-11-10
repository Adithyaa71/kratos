# PHASE_NOTES

Working notes per phase of [docs/design-multi-harness.md](docs/design-multi-harness.md).
Newest phase first.

---

## P7 — the Python backend is gone, and the screen was locked

### Done

* **The Python backend is retired.** `backends/` and `start-backend.cmd` are
  deleted; `backends.spawn_native` now defaults to **on**, so the broker starts
  `kratos-native.exe` itself. Nothing to install, nothing to remember to
  start, no `pip`, no Tesseract.
* **Parity first, retirement second.** Three kinds only the Python backend
  could do are now in the Rust one: `open`, `focus` and `navigate`. The app
  catalogue is the Start Menu plus the system tools that have no shortcut —
  enumerate, do not guess — and a name this machine does not have comes back
  as a refusal *with candidates*, which is the shape every other ambiguity
  here takes.
* **Two long-standing gaps in CLAUDE.md are closed.**
  * *An act that fails before the decide stage is never journalled.* It is now:
    no window, no backend, a pin that is gone. Those are the lines you most
    want afterwards, because they say the machine could not get as far as
    judging the instruction.
  * *`press Enter` has never been tested end to end.* It is, twice — through
    the broker to a recording backend, and through the real keyboard into a
    real window.
* **A keypress ignores the element attached to it.** Found by that first test:
  the decision model answers both of its questions for a keypress too, so the
  broker passes along whatever scored highest as "where the key is expected to
  land". The backend was looking it up, which is how `press Enter` fails
  because an unrelated link scrolled out of the tree. Documented in
  `docs/protocol.md` §3.0.
* **A spawned perception backend dies with its broker.** It did not: the
  benchmark starts three brokers to measure a cold start, and left three
  `kratos-native.exe` processes behind, retrying a pipe nobody was listening
  on, for ever. `native.rs` claimed this was impossible on the strength of
  tokio's `kill_on_drop`, which does not run when the parent exits without
  dropping the handle and cannot run at all when the parent is force-killed.
  The broker now says "you were started by a broker" in the environment, and a
  backend that cannot find one for fifteen seconds exits.
* **A locked screen is reported, not dispatched into.** New error code
  `screen_locked`, a `doctor` row, and the physical path refuses instead of
  sending input that Windows discards.
* **The full matrix**, the spec's P7 acceptance item: two agents, two browser
  instances, four kill scopes, pause on and off — 16 cells, one test.

### Evidence

```text
P7 matrix: brave and chrome, 2 pause settings x 4 kill scopes x 2 lanes, 16 cells
424 tests, no clippy warnings
```

`kratos-broker.exe doctor` on this machine, which is also the reason half of
this section says "not run":

```text
[ok  ] kratos-native.exe  D:\Kratos\target\release\kratos-native.exe
[FAIL] session            locked. Nothing that needs the mouse, the keyboard
                          or a visible window can run until the person at this
                          machine unlocks it; a `dom-ext` action through a
                          browser still can.
[ok  ] decision model     jev/jev-latest with a sealed key
[ok  ] bridge registry    registered for brave, chrome, edge, vivaldi, chromium
```

What a locked session actually does, measured here rather than assumed — the
first two versions of the check were both wrong:

| | |
|---|---|
| UI Automation | works |
| A control's own pattern | works: typed into a real box, read the value back |
| `open "notepad"` | works: the process was running afterwards |
| `SetForegroundWindow` | **refused** |
| `SendInput` | **unreliable**: returned 0 events sent at least once |
| Window capture | **unreliable**: the same window, 16ms with ten lines of text one minute and 428ms with nothing in it the next |
| Full-screen capture | 1.7s, against the 30ms it costs on a desktop in use |

Two probes for "is this locked" were tried and discarded before the right one:
`OpenInputDesktop` succeeding (it succeeds while locked) and the input
desktop's *name* being `Default` (it is `Default` while locked — Windows 10
and 11 draw the lock screen with an ordinary app on the ordinary desktop; only
UAC moves input to `Winlogon`). The answer is
`WTSQuerySessionInformationW(WTSSessionInfoEx)`, which is the question rather
than a proxy for it.

Benchmarks, regenerated on this build:

```text
broker cold start   p50 36.3ms  best 25.2ms   (P4-era figure: 26ms p50)
mcp cold            p50 16.3ms  best 15.4ms
status round trip   p50 0.064ms
gate verdict        p50 0.006ms
screen capture      not measured: this session was locked
```

The p50s are a little above the P4-era run and the best-case figures match it,
which is load on the machine rather than a regression. One real number did
change: with `spawn_native` on, the cold start was p50 40.6ms / best 38.4ms
against best 25.2ms with it off — so launching the perception backend costs
roughly 13ms on the cold path. The benchmark rig turns it off, because this
file measures the broker's own overhead and a second process starting is not
that.

### Decisions

* **The locked-screen refusal lives in the backend, not the broker.** The
  first attempt put it before the input lease, which would have failed every
  integration test on a locked machine: the broker cannot know whether the
  backend it is about to dispatch to needs local input, and `dom-ext`, a
  remote backend and the tests' fake ones all do not. `kratos-native` is the
  thing that needs the mouse, so it is the thing that says so.
* **And only in the physical path.** The first version refused every act and
  every pixel observation. Then the native benchmark read ten lines out of a
  real window on this locked machine and the actuation test typed through a
  ValuePattern. A blanket refusal turns a working path off and gives a reason
  that is not true of it, which is worse than no check.
* **A test skips on a locked screen; it does not fail.** A red suite that
  means "your screen was locked" is a suite people learn to ignore. Six live
  tests check first and say which one skipped.
* **The live tests take the broker's own machine-wide input guard**, so they
  run one at a time and do not fight a broker the operator has running. Found
  the hard way: two Notepads up at once, a title search landing on the wrong
  one, and a capture read while another test was typing into it — one failure
  in about a dozen runs, which is the worst frequency a test can have.
* **A benchmark that cannot measure something leaves the cell empty.** It
  already prints `--` for a capture that failed; a locked session now gets the
  same treatment plus a note saying why. `kratos-native --bench` refuses
  outright and writes nothing, because half its rows would be about the lock
  screen and that file exists to be quoted.

### Known gaps

* **The four real tasks have not been run.** The screen was locked for the
  whole session and three of the four are browser work that needs a window
  brought to the front. The scripts are written out step by step, with what
  each step should return, in [`docs/real-tasks.md`](docs/real-tasks.md); the
  loop itself was exercised live in P6 against a canvas app, so what is
  missing is the run, not the design. **This is the one P7 acceptance item
  that is not met.**
* **`kratos-bench --live-tasks` is not built.** It would drive four tasks
  whose last step is irreversible, and it cannot be tested without a screen.
  An untested driver for that is the wrong thing to hand over, so the scripts
  are data and prose instead. With a desktop it is one sitting's work.
* **No scored accuracy for the fixture corpus.** Spec §10 asks for recall,
  label exactness and bbox IoU per source and fused. What exists checks
  *invariants* — ids unique within an observation, rectangles that are
  somewhere, roles from one vocabulary, containers that name something —
  which is what a fusion pass can silently break. Scoring needs a golden
  element list per window, and the corpus is deliberately not committed
  because a UIA dump carries what is on the operator's screen. So the golden
  lists would have to be hand-labelled on each machine.
* **No GPU numbers for the icon detector.** Still CPU-only here: 352-700ms
  against the spec's 60ms, which is a GPU figure. Not met, and not claimed.
* **The sparse-tree threshold still does not fire on a canvas app** (P6's
  gap, unchanged): six title-bar elements is above "<=2", so the pixel path
  has to be asked for by name.
* **Pause's paused branch is not in the matrix.** It fires on real keyboard
  and mouse input, and input this process injects is ignored on purpose —
  that is the whole point of the `LLMHF_INJECTED` check — so no test outside
  the broker's own process can produce the condition. The branch is covered by
  `activity::tests`; the matrix checks that the setting is honoured, reported,
  and does not stop a DOM action that needs no pointer.

### Next

Unlock the screen and run `docs/real-tasks.md`, recording the per-segment
numbers in `docs/benchmarks.md`. Then regenerate both benchmark files from an
unlocked session, which fills in the screen rows and gives the native
measurements a desktop that is actually drawing.

---

## P6 — the allocation lives in the settings window

### Done

* **Five new tabs**, spec §8. *Harnesses*: one row per MCP client that has
  connected, its browser, and the tick that approves it. *Agents*: one row per
  agent name and the browser profile it gets. *Profiles*: which profile each
  attached extension instance is running in, every browser and profile on this
  machine, and every session's lane. *Connect*: the two snippets to paste into
  a harness, with and without `--agent`. *Behaviour*: what to do when a lane is
  not open, what to do while the person is using the machine, and the defaults
  for anything unallocated.
* **The window reads the machine through the broker.** `lane.list` on the same
  poll as `status`, rather than this process reading `Local State` itself: the
  broker is the one on the machine being driven, and two readers of that file
  would be two things to keep in step.
* **`kratos-broker.exe doctor`**, spec §12 — one command that says what is
  wrong. Binaries, config, decider, harnesses, agents, defaults, browsers and
  their profiles, the OS default browser, the bridge manifest and each brand's
  registry hive, the icon weights, and a running broker's backends, lanes, kill
  state and pause state. Every check is `ok` / `warn` / `fail` / `off` with the
  evidence beside it. It is read-only, and a test enforces that by grepping its
  own source for `fs::write`, `connect_or_start`, `RegSetValueW` and
  `Command::new`: a doctor that repairs things is a doctor you cannot run when
  you do not yet know what is broken.
* **Fused element ids are derived from where the element is**, not from its
  position in the list the detector returned. See below; this was a real
  failure, found live.
* **A control that only the pixels found can now be clicked.** The `uia` source
  accepts an `ocr:`/`pixel:` id with a rectangle and goes straight to the
  physical path. That closes the gap P4c left open: "no live test of the
  physical path".

### Evidence

The settings window has no accessibility tree — egui draws into a GL canvas, so
UIA reports six elements and all six are title bar. It was therefore read and
driven entirely through the pixel path, which is the first end-to-end use of
that path on a real window:

```text
observe (default route)  ->  6 el, all title bar
observe (prefer: pixel)  -> 55 el | 461ms
  ocr:77x30   [ocr]       text   'Harnesses Agents Profiles'   222,116,174,13
  pixel:109x30 [pixel+ocr] button 'Connect'                    405,108,62,24
  pixel:126x30 [pixel+ocr] button 'Behaviour'                  468,108,73,24

act 'click the Behaviour tab'  target_id=pixel:126x30
  -> executed | backend uia | method physical | 1103ms

and the tab that opened, read back off the screen:
  'When a lane is not open'                     -> direct / semantic / none
  'When you are using the machine'              -> stop if input within 1500
                                                   ms, give up after 30000
  'Defaults for anything unallocated'           -> browser none,
                                                   profile last-used
```

All five tabs were clicked this way and their contents read back:

| tab | what the screen said afterwards |
|---|---|
| Harnesses | `claude-code`, `bluee-ide`, a browser dropdown, Forget, Apply |
| Agents | `agent-a` -> `Profile 1`, Remove, Add an agent, Apply |
| Profiles | brave/chrome/edge/vivaldi with their real profile names, `the default browser for this account is brave` |
| Connect | both snippets, including `--agent agent-a` |
| Behaviour | the three groups above |

`doctor` on this machine found the sealed Jev key present, four Chrome profiles
with their display names, the bridge registered for all five brands, and Brave
as the OS default.

413 tests, no clippy warnings.

### Decisions

* **An id for something found in an image names a position.** `ocr:77x30` is
  the element's centre in four-pixel cells. Four because OCR and the detector
  re-measure every pass and a box jitters by a pixel or two; four is above that
  and far below the gap between two controls.
* **The broker re-pins a pixel element by position when its id is gone** —
  within six pixels, same source, same size to within six pixels, and only if
  exactly one element qualifies. A tree id is never re-pinned that way: a
  missing node means the node is gone, and whatever now fills its rectangle is
  something else. `docs/protocol.md` states the exception where it states the
  rule, because it *is* a weakening of "ids are only valid within their gen"
  and burying it would be dishonest.
* **`act` observes through the pixel source for exactly as long as the pin is a
  pixel id.** It used the default route before, which prefers the tree wherever
  the tree answers — and a canvas app's tree answers with its title bar.
* **The window edits config, and config is the only allocation.** Every tab
  writes through `config.set`; nothing in the UI binds a lane or approves a
  harness by any other path. An agent cannot approve itself because the thing
  that approves it is a file this window owns.

### Known gaps

* **The sparse-tree threshold does not catch a canvas app.** Spec §10 says run
  the pixel path automatically when the tree has two elements or fewer. The
  kratos settings window has six, all title bar, so the automatic path does not
  fire and an agent has to ask for `prefer: "pixel"`. The threshold is the
  spec's number and is left alone here; what a title-bar-only tree should count
  as is a judgement the spec does not make.
* **OCR merges a tab strip into one line.** `'Harnesses Agents Profiles'` comes
  back as a single element 174 pixels wide, so pinning a tab by its label picks
  the middle of three. The detector does find each tab as its own box, which is
  how they were clicked here — but those boxes have no caption, so they were
  pinned by id. An agent naming a tab by name on a screen like this would have
  to be told which box, which is not yet solved.
* **Nothing in the UI is covered by a test that drives it.** egui has no tree
  to assert against; the live run above is the evidence, and it is a run, not a
  regression test.
* The UI's own unit tests cover the draft/epoch logic and the config fallbacks,
  not the new tabs' widgets.

### Next

P7: retire the Python backend after parity, update the README and the skills
(the broker-per-agent model and the env-var pairing are both gone), the full
matrix test, the final benchmarks, and the four real tasks the operator added
to the spec as an addendum.

---

## P5 — the ensure-lane ladder

### Done

* **The ladder**, spec §4, in order, stopping at the first rung that works:
  an instance already paired; an instance of that brand attached and
  unpaired; a direct launch with `--profile-directory`; bounded, Jev-driven
  recovery; degraded.
* **Opening a browser is an action.** The kill switch is checked first, the
  launch is journalled with the executable and the profile, the outcome is
  journalled, and each recovery step is a *real* `act` — observed,
  decided, gated, leased, executed, proved. A broker that clicked things on
  its own initiative outside the gate would be a second, unaudited way to
  drive the machine.
* **Auto-binding.** Exactly one new unbound instance appearing after a launch
  is that profile's, and it is paired without a human. Exactly one, because
  "whichever appeared" is a coin toss when two browsers start together, and
  binding the wrong token points a lane at another profile's tabs.
* **Rung 1b, which the spec implies and does not state**: an instance of that
  brand already attached and unpaired is adopted *before* launching. The rule
  as written is about an instance that appears after a launch, and on a
  machine where the browser is already open with the extension running there
  is nothing to appear — so launching would open a window to produce an
  event that has already happened.
* **A cooldown.** The ladder runs when a lane is not ready, and a lane that
  stays degraded is not ready on the next request either: without this, every
  observation paid for a launch, an eight-second wait and three recovery
  steps. Measured at fifteen seconds per observe, for ever. Thirty seconds.
* **The ladder only runs for `window: null`.** A request that names a window
  is asking about something already there — which is also what stops the
  recovery's own acts from recursing into the ladder that started them.
* `open_method: "none"` turns opening off: drive a browser that is open,
  never open one.

### Evidence

Live, on this machine, with the real Brave:

```text
cold, nothing running:
  lane.open      exe=...\BraveSoftware\...\brave.exe  profile=Default
  instance.bind  token=26f538bc...  brand=brave  profile=Default  by=lane.open
  whoami         lane: brave/Default  state=ready
  brave processes 0 -> 14

into a browser already running:
  lane.open      process count unchanged -- a window opened inside the
                 running browser, which is what --profile-directory does
  lane.recover   three steps, then stopped: the bounded abort
  observe        dom-ext>uia | New Tab - Brave | 71 elements

the ladder runs once per lane, not once per request:
  first observe 6.9s, second 5.2s, third 5.4s
  journal: lane.open x1, lane.ensure outcome=launched x1
```

The restore-pages bubble appeared on its own during a cold start, which is
exactly the rung-4 case the spec describes: the window the broker read was
titled "Restore pages?".

```text
cargo test --workspace     401 passed; 0 failed; 0 ignored
cargo clippy --workspace --all-targets     0 warnings
```

Four new tests pin the rungs a test can prove without opening a browser on
somebody's desktop: a brand that is not installed is `lane_unavailable` and
names the profile, `open_method: "none"` opens nothing, an engaged kill switch
opens nothing, and a lane whose instance is already paired opens nothing.

### Two bugs the live run found

1. **A window that vanishes is `no_target`, not `backend_error`.** A cold
   browser start creates and destroys windows, and `GetWindowRect` on one of
   them failed the whole observation instead of letting the broker try the
   next backend. The native backend now distinguishes "that window is not
   there" from "this backend is broken", which is the difference between
   routing information and a stop.
2. **The extension hung on a tab it cannot script.**
   `chrome.tabs.sendMessage` on a privileged page — a new-tab page, the
   Web Store — can leave its callback unfired, so an observation never
   returned, the broker waited out eight seconds, and the native host
   concluded the extension was dead and exited. Every frame now has a
   1.5-second bound and an unscriptable tab returns no elements, which is an
   answer. And the host no longer exits when an extension that *has* answered
   before is merely late: measured seven host restarts for three observations
   before that distinction existed.

### Known gaps

* **The extension fix is unverified live.** This machine runs an installed
  copy of the extension, which takes precedence over `--load-extension`, so
  the browser keeps running the code from before the fix. Verifying it means
  removing the installed copy — a change to the operator's browser that is
  P6/P7's business, not a thing to do unasked.
* **An unresponsive-but-registered instance costs 5s per observation**, which
  is the host's "is the extension dead" threshold, and then falls back to UI
  Automation. The broker does not yet demote an instance that keeps failing;
  that is the fix rather than tuning the timeout.
* **The cooldown is not exercised by a test**, because on this machine the
  ladder succeeds. Its logic is three lines and its measurement is above.
* **`open_method: "ui"` is accepted and launches nothing.** Rung 3 is "launch
  the app through its own interface, then pick the profile in the picker", and
  the native backend refuses the `open` action kind — so `ui` covers the
  picker half only. Implementing `open` is small; it is also the one action in
  this design that starts an arbitrary program, so it wants its gate rule
  written first.
* **Picker recovery is not proved against a real picker.** The steps ran,
  bounded and journalled, but the window that appeared was a restore-pages
  bubble; a profile picker needs a second profile on this machine to produce.

## P4d — the pixel path and fusion

### Done

* **The icon detector runs.** OmniParser v2's icon detector, the community
  ONNX export, loaded through `ort` with `load-dynamic` and nothing else:
  its default features download an onnxruntime build during `cargo build`,
  which puts a network fetch in the build and a binary nobody audited in the
  tree. This loads a library the machine already has.
* **Preprocessing, decode, suppression**: letterbox (bilinear, because the
  things being found are twelve pixels across and a nearest-neighbour
  downsample of a 1920-wide window drops every other row of a toolbar icon),
  YOLOv8 single-class decode, non-maximum suppression at IoU 0.45, boxes
  mapped back to screen coordinates offset by the window origin.
* **The detector caches its last frame**, against the same pixel hash the OCR
  engine uses. Four hundred milliseconds of CPU is worth not spending on a
  screen nobody touched, and an agent looks at the same screen repeatedly.
* **Fusion** (`fuse.rs`): UI Automation leads; OCR text and detector boxes are
  added only where the tree has nothing. Overlap above 0.5 means one element
  and the tree's answer wins, because it has a role, a name, a container and a
  value. Every element carries `source` — `uia`, `ocr`, `pixel`,
  `pixel+ocr` — and a `confidence` only where something measured one.
* **A sparse tree runs the pixel path unasked.** Two elements or fewer is not
  a tree: a canvas app reports its own window and perhaps a scroll bar, and
  taking that at face value is how an agent concludes a screen is empty when
  it is full.
* `Element.source` and `Element.confidence` added to the protocol, both
  optional.
* The backend now attaches as three sources: `uia`, `ocr`, `pixel`.
* `docs/pixel-path.md`: what it is, why the weights and the runtime are not
  included, how to add them, both off switches, and what it costs.

### Evidence

Live, this machine, four real windows (`docs/native-benchmarks.md`):

```text
| window         | icons | lines | fused | from pixels | resize | network | again |
| File Explorer  |    84 |    53 |   105 |          84 |    5ms |   376ms |   0ms |
| Settings       |    57 |    59 |    62 |          57 |    4ms |   364ms |   0ms |
| Example Domain |    62 |    33 |    80 |          62 |    6ms |   693ms |   0ms |
```

**The split is the finding.** The letterbox this code does costs 4–6ms; the
network costs 360–700ms. There is nothing in this code to optimise —
it is the model on a CPU.

```text
cargo test --workspace     395 passed; 0 failed; 0 ignored
cargo clippy --workspace --all-targets     0 warnings
```

Seven fusion unit tests pin the rules that are easy to get wrong: the tree
winning where it has an answer, a detector-only control being kept with no
invented label, an icon taking the caption that is *on* it, a caption merely
*near* an icon not becoming its name, text with nothing under it being
reported, what counts as sparse, and every element naming its source.

### Decisions

* **Containment, not overlap, for text.** A "Send" label inside a Send button
  covers 0.29 of it, so an overlap test alone reported the word as an element
  of its own and the decision model got the button twice. Caught by the first
  fusion test written. Text is small and controls are not: what matters is
  whether the words are *on* something.
* **An unlabelled icon gets role `image` and an empty label**, not `button`
  and a guess. Nobody knows what it is yet, and the caption model that would
  is not built.
* **The 60ms detector target is not met and not claimed.** It is a GPU figure
  in the design document, and this machine's onnxruntime offers
  `CPUExecutionProvider` and `AzureExecutionProvider` and no GPU provider. The
  CPU number is reported instead, which is what the spec also asks for.
* **Threading the session changed nothing** — 420ms before and after
  `with_intra_threads(6)` — so the measurement was split into resize and
  network to find out why, and the answer is that the network is simply this
  expensive at fp32 on a CPU.
* **The switch is two switches**, and both are the operator's: the weights not
  being on disk, and `"pixel"` not being in `backends.enabled`. The spec asks
  for a config switch; routing is where this build already has one, and the
  AGPL half is answered by the file simply not being there.

### Known gaps

* **No accuracy numbers.** Recall, label exactness and bbox IoU against a
  golden element list need a corpus that can be committed, and a UIA dump or
  a screenshot of this machine is a picture of somebody's screen. What exists
  is the structural check against a locally generated corpus, the per-source
  element counts above, and fusion rules pinned by unit tests. The spec's "a
  regression fails CI" needs a corpus built from applications a CI machine can
  open for itself — which is P7's problem and is written down there.
* **No captions.** Florence-2 on demand, cached by crop hash, is in the design
  and not built.
* **The detector is not wired into actuation.** A box found in an image is
  somewhere to click, and the broker's own physical fallback is what would
  turn that into a click; the `pixel` source refuses `backend.act` by name so
  the fall-through happens.
* **84 icons in File Explorer against 206 UIA elements** is not obviously
  right or wrong without a golden list. The fusion keeps all 84 because
  nothing in the tree covered them, which may mean the detector is finding
  real controls UIA hides, or may mean it is finding file thumbnails. This is
  exactly what the accuracy corpus is for.

## P4c — actuation

### Done

* **The control's own interface first.** `click` tries `InvokePattern`, then
  `TogglePattern` (a checkbox has no Invoke; toggling it *is* clicking it),
  then `SelectionItemPattern` (a list item, a tab, a radio button), then
  `ExpandCollapsePattern` (a combo box opens). `type` uses `ValuePattern`,
  `select` uses `SelectionItemPattern`, `focus` uses `SetFocus`, `scroll` uses
  `ScrollPattern`.
* **The mouse and keyboard second**, and the result says which: `semantic` or
  `physical`. A click at a coordinate is a guess that the button is still
  where it was, and a guess that can land on whatever moved into that
  rectangle. It is allowed — most of a Windows desktop exposes no pattern for
  most things — and it is never silent.
* **Text the keyboard cannot spell goes through the clipboard.** A unicode
  character has no scan code, a dead-key layout turns `'` into nothing until
  the next keystroke, and an emoji has no keys at all. One paste handles any
  text in any layout.
* **A read-only control is reported, not retyped.** `ValuePattern` says so,
  and falling back to the keyboard there would type into whatever has focus
  instead.
* **Acting runs on the same apartment thread as observing**, so an action
  cannot overlap the observation the broker took a moment earlier to decide
  it — which is the ordering the whole evidence story rests on.
* An element id is a path of child indices through the *same* filter the
  observation used, so the walk back down counts the same children. A tree
  that changed underneath runs out of children and the action is refused
  rather than landing on whatever is at that index now.
* An id from another backend is refused rather than aimed at by rectangle.

### Evidence

Live, through the whole stack — MCP shim, broker, gate, lease, the Rust
backend — against a window the test opens for itself:

```text
observe: source=uia  11 elements  36ms
type   : executed / semantic / readback='someone@example.com'
click  : executed / semantic / evidence=structural+window+region
labels now: Recipient | Recipient | Urgent | Send | sent to someone@example.com | ...
```

That last line is the application saying what it did, which is the only
evidence that cannot be produced by the thing under test.

`crates/kratos-native/tests/actuation.rs` does the same thing as a test: it
opens its own WinForms window, speaks the protocol to `kratos-native` over a
real pipe, and asserts the type is semantic with a read-back, the click is
semantic, the window's own label changed, the checkbox toggles, a keypress
works with no element at all, and another backend's id is refused.

```text
cargo test --workspace     382 passed; 0 failed; 0 ignored
cargo clippy --workspace --all-targets     0 warnings
```

### Decisions

* **A window of its own for the live test.** The alternative is typing into
  whatever the operator has open, and a test that edits somebody's document
  is a test that gets deleted. The scratch window is WinForms through
  PowerShell: no build step, no dependency, and every control carries the
  patterns this phase is about.
* **`SendInput` for everything physical**, which marks its events injected —
  and that is load-bearing: the broker's user-activity pause reads the same
  flag to tell the operator's hand from the agent's, so an agent that drove
  the machine any other way would pause itself after its own first click.
* **A keypress has no element**, by design. A backend that refused a null
  target would break `press Enter` entirely, which is exactly what both
  earlier backends did until a live run caught it.
* **Scroll inverts the wheel.** Positive means down the page and a wheel
  delta is positive *away* from the user. Getting it backwards is a bug
  nobody notices until an agent scrolls away from what it was looking for.
* **The value is read back even when the keyboard did the typing.** The
  broker's strongest evidence is the field containing what was asked for, and
  it does not care how it got there.

### Known gaps

* **`open` and `navigate` are not implemented here.** Launching an
  application and going to a URL are P5's ensure-lane and the browser's
  respectively; this backend refuses them by name so the broker can fall
  through.
* **No live test of the physical path.** Everything in the scratch window has
  a pattern, which is the point of it — so `physical` is exercised only by
  the keypress. A window with no patterns at all (a canvas app) is the case
  to add with the pixel path in P4d.
* **The clipboard is not restored after a paste.** A paste replaces whatever
  the operator had copied. Saving and restoring it is possible and racy
  (another process can own the clipboard between the two), and the honest
  options are to do it imperfectly or to say so. Saying so, for now.
* `select` on a combo box only works when the item is already an element;
  opening the list first is a two-step the agent has to ask for.

### Next

**P4d — the pixel path and fusion.** OmniParser v2's icon detector exported
to ONNX and run with `ort`, UIA first with OCR text and detector boxes added
where UIA has none, IoU > 0.5 as the same element, and the accuracy numbers
the spec asks for.

---

## P4b — capture and OCR

### Done

* **Per-window capture** (`capture.rs`): `PrintWindow` with
  `PW_RENDERFULLCONTENT`, so the window renders *itself* into the bitmap and
  the answer is right even when something is sitting on top of it — which a
  screen grab cannot say. A blank result falls back to copying the window's
  rectangle off the screen, because a hardware-composited window renders
  nothing into a bitmap and an all-black frame handed to OCR is not a failure
  anybody notices: it reads as a window with no text in it.
* **A frame-hash cache**: FNV-1a over every sixteenth pixel plus the
  dimensions. The same pixels are never read twice, and most of what an agent
  looks at between two steps is identical.
* **`Windows.Media.Ocr`**: ships with Windows, no install, no model download,
  no network, and the languages it reads are the ones the user already has.
  A machine with no language pack gets a backend that says so rather than one
  that refuses to start.
* **One connection per source.** The process now attaches twice, as `uia` and
  as `ocr`. The protocol gives a peer one source, and these are different
  answers to "what can you see" — a tree, against text with boxes and no tree
  at all. It is also what lets the broker fall back from one to the other and
  the operator disable one in `backends.enabled`.
* OCR elements are `text` with a box and **no container**, because that is
  honestly all there is. Inventing structure would give the decision model
  something that is not on the screen.

### Evidence

Measured here, four real windows:

```text
| window         | size      | capture | how        |  OCR | lines | again |
| Notepad        |  970x1032 |    21ms | printwindow|  43ms |   37 |   0ms |
| File Explorer  |  1210x735 |    19ms | printwindow|  47ms |   52 |   0ms |
| Settings       | 1936x1048 |    35ms | printwindow|  66ms |   59 |   0ms |
| Example Domain | 1936x1048 |    22ms | printwindow|  92ms |   33 |   0ms |
```

The spec's target is a **region** at p50 ≤ 100ms; these are whole windows at
43–92ms, which is the harder measurement. `PrintWindow` worked for every
window tried, including a Chromium one, so the screen fallback has not fired
in practice yet — it is there for the case that it does.

The cache column is the second read of an unchanged window: 0ms, every time.

```text
cargo test --workspace     372 passed; 0 failed; 0 ignored
cargo clippy --workspace --all-targets     0 warnings
```

A live test opens Notepad, drives the real executable, and fails if the frame
has no area, if nothing was read from a window that has text in it, or if the
second read is not served from the cache.

### Decisions

* **`PrintWindow` rather than Windows Graphics Capture.** WGC is what the spec
  names and it needs a D3D device, a frame pool and a WinRT dispatcher to read
  one still image. `PrintWindow` is one call, handles occlusion, and measured
  19–35ms for a full window. WGC earns its complexity when frames are wanted
  continuously, which is a thing to reach for in P4d if the detector needs it.
* **The OCR engine is built once and shared.** It is agile, so the two source
  connections share one; building it per call costs more than a recognition.
* **A ten-second ceiling on the recognition.** OCR runs in a system service,
  and a wedged service would otherwise hang the backend thread for good — and
  with it every observation of every window.
* **`BitmapAlphaMode::Ignore`.** The capture path leaves alpha unset for some
  windows, and a bitmap declared premultiplied with a zero alpha channel reads
  as entirely transparent: a blank page to OCR.
* **The async is awaited by hand.** `windows-future` has a `join` for exactly
  this and keeps it private in 0.3.2, so the completion handler is wired
  directly. This thread pumps no message loop, so there is nothing to deadlock
  on.

### Known gaps

* **No region OCR yet.** Everything reads the whole window. A region is
  strictly cheaper and the fusion pass (P4d) is what will want one, around a
  detector box.
* **No accuracy measurement.** These are timings. Recall, label exactness and
  bbox IoU against a golden list are P4d's, and need a corpus that can be
  committed — which the local one cannot be.
* **The screen fallback is untested in practice**, because `PrintWindow`
  worked everywhere tried. It is exercised only by the unit test of
  `is_blank`.
* OCR confidence is not reported: `Windows.Media.Ocr` does not expose a
  per-line confidence, so a fused element from this source will carry the
  source and no number. Something for the fusion to be honest about.

### Next

**P4c — actuation.** Semantic first through UIA patterns, physical second
with `SendInput` and a clipboard paste for unicode, reporting `method` as the
protocol already requires, and respecting the lease, the pause and the kill
switch.

---

## P4a — UI Automation in Rust

### Done

* **`crates/kratos-native`**, a backend peer like any other: same protocol,
  same handshake, and the broker does not know or care which backend answers.
* **One cached fetch per window.** `BuildUpdatedCache` with a tree scope and a
  filter brings back the whole subtree *and* its properties in a single
  cross-process call; everything after that — names, roles, rectangles,
  parents — is read out of the local cache. Pruning (`IsControlElement` and
  not offscreen) is in the **condition**, so a filtered element is never
  marshalled at all.
* `AutomationElementMode_None`: a cached-only element cannot be used for
  patterns and nothing here needs to be, so the window's live references are
  never held.
* Containers come from the cached **parent**, which is why the tree is cached
  rather than a flat `FindAll`: `dialog:New Message` is what tells two
  identically labelled controls apart.
* Element ids are `uia:<hwnd>:<child path>` rather than a runtime id, because
  a runtime id is one cross-process call per element — the exact cost this
  crate exists to remove — and a path is what actuation will re-find by.
* **The broker spawns and supervises it** (`native.rs`): restart with backoff,
  dies with the broker, behind `backends.spawn_native` (off until parity).
* `--bench` and `--dump` modes that need no broker.

### Evidence

Measured on this machine, four real windows, 21 runs each:

```text
| window         | elements | cold ms | p50 ms | p90 ms | of which fetch |
| Notepad        |       57 |    51.1 |   31.6 |   32.1 |           30ms |
| File Explorer  |      206 |    79.9 |   78.0 |   80.3 |           75ms |
| Settings       |      128 |    45.2 |   42.4 |   46.1 |           41ms |
| Example Domain |      129 |    20.8 |   21.7 |   22.2 |           19ms |
```

The Python backend this replaces: **24 elements in 223ms, 168 in 2.1s**. So
206 elements in 78ms is about 27x faster at a larger size, and the spec's
target — ~150 elements at p50 ≤ 80ms — is met with 206.

The split is the point: of 78ms, **75ms is the single fetch** and the local
walk is 2ms. A test asserts that ratio against the corpus, because the way to
lose it is for something to start calling across the boundary per element
again, which is what 2.1 seconds looked like.

```text
cargo test --workspace
   369 passed; 0 failed; 0 ignored
cargo clippy --workspace --all-targets
   0 warnings
```

### Decisions

* **The accuracy corpus is machine-local and gitignored.** A UIA dump carries
  what is on the operator's screen: the file open in Notepad, the names in a
  folder, the subject lines in a mail client. The harness skips what is not
  there and says so. This means the regression check cannot run in CI as the
  spec asks; what can is in the repo (the structural invariants), and the
  honest version of "a regression fails CI" needs a corpus built from
  applications a CI machine can open for itself. Recorded as a gap for P7.
* **The benchmark table is written by `kratos-native --bench --out` and
  *inlined* into `docs/benchmarks.md`** by `kratos-bench`, rather than being
  generated there. These numbers are live — they depend on which windows are
  open — and `kratos-bench` is the offline, reproducible half; generating both
  into one file would make the reproducible one a lie. Regenerating
  `benchmarks.md` now preserves the live section instead of dropping it.
* **Rows are labelled by the search string the operator typed**, not by the
  live window title. A title is the file open in Notepad or the folder in
  Explorer, and this table is committed.
* **`backends.spawn_native` is off by default.** Both this and the Python
  backend register as `uia`, and one registration per source means the newer
  one displaces the other. Turning it on is the operator's decision until
  parity.
* **Two VARIANTs are built by hand.** The `windows` crate exposes the raw
  union with no conversions, and the alternative — caching patterns instead of
  property values — needs `AutomationElementMode_Full`, which holds a live
  reference to every element in the window.

### Known gaps

* **No actuation.** `backend.act` returns `backend_error` naming the phase, so
  the broker falls back to another backend. That is P4c.
* **No OCR, no capture, no pixel path**, so `kratos-native` reports nothing for
  a window that exposes no accessibility tree. P4b and P4d.
* **No URL** from this backend: a browser's address is the DOM backend's to
  report, and a UIA-driven browser lane therefore gates on title alone.
* **Cold start has regressed**: 13.3ms recorded earlier, 25–30ms today. The
  pause hooks were the obvious suspect and are **ruled out** — measured
  spawn-to-pipe with them on and off, 48.6/46.3/52.0 against 49.3/43.2/48.7ms,
  no difference. The cause is not isolated; it may be nothing more than a
  machine with five applications open. Worth a bisect before P7 quotes the
  number again.
* The corpus has four windows. The spec asks for Excel and Gmail too; Excel is
  not installed here, and a Gmail dump is somebody's mail.

### Next

**P4b — capture and OCR.** Windows Graphics Capture or DXGI cropped to the
target window with a frame-hash cache, `Windows.Media.Ocr` by default, and the
numbers in the same table.

---

## P3 — scoped targeting, tab ownership, one lease, the pause

### Done

* **`window: null` is the session's lane, never the OS foreground**
  (`crates/kratos-broker/src/target.rs`). One place resolves what a request is
  aimed at, so `observe`, `act` and `verify` cannot drift apart about it.
* **Windows of other lanes are invisible.** A `window` string that matches only
  another lane's browser is `no_target`, naming the lane. Native windows belong
  to no lane and stay visible to everyone — there is one Notepad, and the input
  lease is what serialises it.
* **A lane never borrows another lane's extension.** `Dom::Lane(None)` means
  "this lane's instance or nothing": the router can no longer fall through to
  whichever `dom-ext` backend happens to be attached.
* **Tab ownership.** A tab belongs to the session that first acted on it;
  another session's act is `blocked` with `gate.category: "tab_owned"` and the
  owner's id. `lanes[].shared` turns it off per lane. A disconnected session
  stops owning anything.
* **One broker per OS user.** The single-instance guard is keyed on the user,
  not the pipe name; `KRATOS_PIPE` is a test seam and setting it is what puts
  the pipe back in the key.
* **One machine-wide input lease.** The holder also holds a named semaphore
  `Local\kratos.input.<sid>`, so a second broker cannot hand out a second lease
  on the same mouse. A semaphore, not a mutex: a Win32 mutex is owned by the
  thread that took it, which a tokio runtime cannot promise.
* **DOM actions take no lease** and no longer wait for the user, so two
  sessions in two profiles act at the same time.
* **User-activity pause** (`activity.rs`): low-level keyboard and mouse hooks
  on their own thread with their own message pump, ignoring `LLKHF_INJECTED`
  and `LLMHF_INJECTED`. An action needing OS input waits out `user_idle_ms`
  up to `pause_max_ms`, then returns the new status `paused`.
* **`status` reports `lanes` and `pause`**, so "which browser is this session
  pointed at" and "are the hooks even installed" are answerable from any role.
* `Local State` reads are cached by modification time: lane resolution happens
  on every connection and every `status`, and it was re-parsing a megabyte of
  JSON per browser per call.
* `defaults.browser: "none"` (and the same on a harness) turns lanes off: no
  lane, and `window: null` means the foreground window as it always did.

### The three defects this phase found by running it

1. **A superseded host must not exit.** P1's reaping fix was wrong, and only a
   live two-browser run showed it: a superseded host that exits closes its end
   of the extension's native port, Chromium wakes the service worker to tell
   it, the worker reconnects, the new host supersedes the one before it —
   **eight processes in fourteen seconds, each starting the next**. It now
   stops relaying and stays put. The lifecycle test was inverted to pin the
   behaviour that stops the loop, because nobody would guess it from the code.
2. **A stale host black-holes its lane.** The flip side: a host outlives the
   worker that opened its port, keeps the registration, and silently swallows
   everything. Live, every `observe` on that lane timed out at eight seconds,
   indefinitely. The host now gives its extension five seconds to answer a
   request, replies `backend_error` — which the broker can fall back from,
   unlike a timeout — and exits. That exit is safe because the port is already
   dead, and the wake-and-reconnect that follows is the repair.
3. **`KRATOS_HOST_LOG`.** Every question about this process has had to be
   inferred from the broker's side, because a browser-started host's stderr
   goes nowhere anybody looks. Opt-in, ops and ids only, never payloads.

### Evidence

```text
cargo test --workspace
   359 passed; 0 failed; 0 ignored     (no test in the tree is ignored any more)
cargo clippy --workspace --all-targets
   0 warnings
```

All three P0 reproductions are now acceptance tests that pass:

```text
a_session_observes_its_own_lane_not_another_agents_window   (b)
a_stranded_session_is_told_which_lane_it_cannot_reach       (c, rewritten)
only_one_input_lease_exists_for_one_os_user                 (d)
two_sessions_on_two_lanes_act_while_the_input_lease_is_held
a_tab_belongs_to_the_session_that_acted_on_it_first
```

Live, one broker, Brave and Vivaldi both attached, two sessions with
`window: null` — repeated twice to show it is not a fluke:

```text
claude-code (brave lane)   -> dom-ext | Example Domain | https://example.com/ | 10ms
bluee-ide   (vivaldi lane) -> dom-ext | Example Domain | https://example.org/ |  6ms
claude-code (brave lane)   -> dom-ext | Example Domain | https://example.com/ |  5ms
bluee-ide   (vivaldi lane) -> dom-ext | Example Domain | https://example.org/ |  5ms

claude-code asking for a window by the title "Vivaldi":
  no_target: "Vivaldi" matches 1 window(s), all of them another lane's browser.
  This session drives brave (Default); windows of other lanes are not visible to it.

status.pause: watching=true  on_user_input=true  user_idle_ms=1500
              since_real_input_ms=299453
```

That last line is also the pause's own evidence: the broker had been driven
through five minutes of agent activity and the hooks recorded **no real
input**, which is the injected-flag behaviour working in the running broker
rather than in a test.

### Decisions

* **The approval of a lane's reachability is the broker's, not the
  foreground's.** A session whose browser is closed gets `lane_unavailable`
  rather than silently falling back to whatever is in front. That is a real
  change in reach: a native window must now be named with `window`. Refusing
  is what the spec asks for, and falling back to the foreground would undo the
  isolation this phase exists for. P5's ensure-lane opens the browser instead.
* **`defaults.browser: "none"`** is how a single-agent desktop keeps the old
  behaviour, and is the only way back to it. The alternative was a conditional
  fallback — "the lane, unless there isn't one" — which is the kind of rule
  that is impossible to reason about at three in the morning.
* **Tab ownership is recorded on first use, not on opening.** Nothing here can
  see a tab being opened; the session that opens one is the first to act on it
  in every case that matters.
* **The input guard is a semaphore with a documented caveat**: its count is not
  restored when a process holding it dies. With one broker per user the object
  is destroyed with the last handle, so a crash leaves nothing behind; two
  processes holding handles is a test-only configuration, and the wait budget
  is what stops it being a hang.
* **`lanes[]` in config** is an addition the design document does not name. §5
  requires "the user marks the lane shared" and gives no field for it, and a
  lane is a `(browser, profile_dir)` pair, so that is what it is keyed on.

### Known gaps

* **The pause is not verified end to end against a real hand on the mouse.**
  Injected input cannot stand in for real input — that is the whole point —
  and nothing can synthesise an event without the injected flag. What *is*
  verified: the hooks install and run on this machine, injected input does not
  register (tested against real `SendInput` on real hardware), and the wait
  arithmetic is unit-tested in all three outcomes. The one untested link is a
  human touching the mouse mid-action.
* **Lane ownership of OS windows is by brand, not by profile.** Chromium runs
  one process per user-data-dir with windows for every profile, so two lanes on
  the same browser and different profiles cannot be told apart from the OS
  side. They are told apart perfectly through the DOM path, where each profile
  has its own extension instance. A UIA-driven action on a shared-brand lane
  can therefore reach the other profile's window; P4's native backend and P5's
  launcher are where that is fixed.
* **Stale hosts still accumulate**, bounded by worker restarts: six after a
  two-browser session. They cost a pipe connection and about 2MB each, hold no
  registration once superseded, and are reaped when the browser exits. The
  registration itself is now self-healing.
* `LaneState::Launching` is still never set, and `lane_unavailable` is returned
  but nothing opens a browser to clear it. Both are P5.
* `gate_overrides` is still parsed and unread.

### Next

**P4 — `kratos-native`.** 4a UIA, 4b capture and OCR, 4c actuation, 4d pixel
and fusion, each ending with a benchmark and fixtures before the next starts.
The baseline to beat is the Python backend's 24 elements in 223ms and 168 in
2.1s; the target is ~150 elements at p50 ≤ 80ms.

---

## P2 — identity, sessions, lanes, resolution, approval

### Done

* **Identity in the handshake.** `Hello` gains optional `harness`, `agent` and
  `session`. The shim learns `harness` from the MCP client's own
  `clientInfo.name` in `initialize` — which every MCP client already sends, so
  connecting a harness really is pasting one snippet — takes `agent` from
  `--agent <name>` or `KRATOS_AGENT`, and mints a `session` per process.
* **Lane resolution** (`crates/kratos-broker/src/lane.rs`): browser from
  `harnesses[h].browser`, else `defaults.browser`, else the OS default, else
  the first installed Chromium; profile from `agents[a].profile_dir` when that
  profile is listed in the browser's own `Local State`, else `defaults.profile`,
  else `profile.last_used`. A missing profile falls back **and says so** in
  `lane.warning`.
* **Machine discovery** (`browsers.rs`): per-brand exe and `User Data` paths,
  profiles and names from `Local State`'s `info_cache`, `profile.last_used`,
  and the OS default browser from `UserChoice\ProgId` (prefix-matched, because
  a second channel installed adds a per-install suffix).
* **Lane state**: `ready` only when an attached instance is *paired* with this
  browser and profile; `degraded` when it is attached but unpaired, with the
  reason in the warning; `unavailable` when the browser is not installed.
* **Harness approval.** An unknown harness is written into the config
  unassigned and unapproved, and may call `status` and `whoami` and nothing
  else. A native dialog asks allow-always / allow-once / deny, and the agent
  cannot answer it.
* **New ops**: `whoami` (agent), and `lane.list`, `harness.list`, `agent.list`,
  `instance.bind` (ui only). New codes `lane_unavailable` and `not_approved`.
  New MCP tool `desktop_whoami`.
* **Config** gains `harnesses[]`, `agents[]`, `instances[]`, `defaults{}`, and
  the broker now picks up an external edit to `config.json` within one idle
  tick — §9 has always said the file is hot-reloaded and it was not.
* `docs/protocol.md`: §3.0 identity, §3.0.1 the lane object, §4.7b `whoami`,
  §4.10 the four UI ops, both new error codes, and the new config sections.

### Evidence

```text
cargo test --workspace
   337 passed; 0 failed; 3 ignored
cargo clippy --workspace --all-targets
   0 warnings

cargo test -p kratos-broker --test multi_harness_audit
   9 passed; 3 ignored
     two_browser_instances_both_stay_registered              (P1)
     the_same_instance_reconnecting_supersedes_itself        (P1)
     the_hello_reply_carries_the_lane_and_the_approved_flag
     whoami_answers_with_the_lane_and_follows_the_users_allocation
     an_unapproved_harness_may_only_ask_who_it_is
     a_new_session_resolves_again_from_the_config
     an_agents_profile_is_used_and_a_missing_one_warns
     pairing_an_instance_with_a_profile_makes_the_lane_ready
     an_agent_cannot_allocate_or_approve_itself
```

Live, through the real shim and a broker on a scratch config:

```text
initialize clientInfo.name = "claude-code", --agent agent-a
desktop_whoami ->
  harness=claude-code  agent=agent-a  session=s-24992-…  approved=true
  lane: browser=vivaldi  profile_dir=Default  profile_name="Work"  state=degraded

initialize clientInfo.name = "some-other-ide"   (not in the config)
desktop_observe -> not_approved in 95ms, dialog raised
desktop_whoami  -> approved=false, lane browser=brave (the OS default), "Work"
config.json     -> harnesses += {id: "some-other-ide", browser: "", approved: false}
journal         -> {"op":"harness.approval","answer":"timed_out",…}
```

`profile_name: "Work"` and `browser: brave` are read off this machine — Brave
is its default browser and `Work` is what its `Local State` calls the profile —
so the discovery path is exercised against real files, not fixtures.

### Decisions

* **The approval dialog does not block the request.** Spec §3 requires a native
  dialog the agent cannot answer; it does not require the op to wait on one,
  and waiting is the mistake the gate was built around (241 of 272 seconds,
  once). So the dialog is raised, the op is refused `not_approved` with
  `retry_safe: true`, and the retry succeeds the moment somebody clicks.
  Measured at 95ms rather than 120 seconds. One dialog per harness at a time,
  so an agent in a retry loop raises one window.
* **A timeout is not a decision.** The claim is released, so the question is
  asked again next time: a user who was away from the machine has refused
  nothing. A deny, by contrast, is remembered for the broker's lifetime.
* **`whoami` re-resolves.** The handshake's answer was true when it was given;
  `whoami` is asked later, and the user may have reallocated since. The stored
  lane is updated to match, so the next action uses what was just reported.
* **An unknown harness is written to the config file on sight.** A harness that
  existed only in memory would vanish when the broker idled out, leaving the
  user nothing to allocate a browser to.
* **`browsers.rs` lives in the broker, not in the UI and not in
  `kratos-protocol`.** The protocol crate has no I/O by design; a second reader
  of `Local State` in the settings window would be a second thing to keep in
  step, and it would be reading whatever machine the window runs on. The UI
  gets these facts from `lane.list`.
* **Tests approve their own harnesses in config.** The integration suites list
  the client names they connect under. An approval a test could grant itself
  would not be an approval, and the dialog cannot be clicked by a test.
* **`defaults.browser = "os-default"` resolves through `UserChoice`**, not
  through "whatever is installed first": it is the value Windows itself honours
  when a link is opened, so it matches what the user would see. A default that
  is not a Chromium browser (Firefox) falls through to the first installed
  Chromium rather than failing, because a machine whose default is Firefox
  usually still has one to drive.

### Known gaps

* **Nothing uses the lane for targeting yet.** `observe` and `act` still
  resolve `window:null` to the OS foreground, so the P0 reproduction (b) is
  down to **one** gap — "a session in the Vivaldi lane read the Brave lane's
  window" — and that is P3.
* `lane_unavailable` is defined and documented but never returned yet: the
  ensure-lane ladder that would return it is P5. Reproduction (c) still names
  it.
* `gate_overrides` on a harness and an agent is parsed and stored and **nothing
  reads it**. It is in the spec's config shape; per-harness gate rules are not
  in any phase's acceptance list.
* `LaneState::Launching` is never set; P5 sets it.
* A lane is `ready` only once a human pairs the instance (`instance.bind`).
  Auto-binding on launch is P5, and the UI that makes pairing pleasant is P6,
  so on this machine every lane reads `degraded` until then. The extension
  genuinely cannot report its profile directory — no API gives it — so the
  alternatives are a human, a launch that knows what it opened, or reading the
  browser process's open handles, which is P5's problem.
* The approval dialog's three answers ride on `MessageBoxW`'s Yes/No/Cancel
  with the meanings spelled out in the text, because a `TaskDialog` needs
  version-6 common controls and an application manifest, and a dialog that
  fails to appear is an approval that can never be given.

### Next

**P3 — scoped targeting, tab ownership, one broker, a machine-wide lease,
user-activity pause.** Acceptance: two fake agents on two lanes act in
parallel; a cross-lane observe gives `no_target`; one lease; paused on real
input and not on injected input. That un-ignores reproductions (b) and (d), and
(c)'s remaining three gaps.

---

## P1 — instance-keyed backend registration

### Done

* **The registry key is `(source, instance.token)`.** `Broker::add_peer`
  supersedes only a peer with the *same* key, so two browser profiles coexist;
  a source that sends no instance still behaves as one-per-source.
  `backends_for(source)` returns every live instance, newest first;
  `backend_for` is that list's head.
* **`Instance { token, brand, pid, parent_hwnd }`** added to `Hello` and to
  `BackendInfo`, both optional, both omitted when absent.
  `Peer::handshake` now takes a typed `Hello` instead of six positional
  arguments and serialises it, so a field added to the struct reaches the wire
  without a second place to remember.
* **The extension mints a token per profile**, `crypto.randomUUID()` stored in
  `chrome.storage.local` (new `storage` permission), and sends
  `evt instance {token, brand}` as the first message on every native port.
* **`kratos-host` waits up to 1.5s for that message**, then attaches with the
  instance — or without one, so an older extension still works. It fills in
  `brand` and `pid` from the process tree, which outrank what the extension
  claims.
* **`status` and the hello reply name the instance** behind each backend, sorted
  by `(source, token)` so two instances read the same way twice.
* `docs/protocol.md`: status line is now "evolving, additive-only until first
  working release"; new §3.1 documents the instance object, the registry key
  and why a superseded peer is not force-closed.

### Four defects found by running it, three of them real

Live, with Brave and Vivaldi both loading the extension against one broker:

1. **Five tokens for two profiles.** Making `connect()` async broke its own
   `if (port) return` guard: every wakeup that arrived before the port was
   assigned walked past it, opened its own native port, and minted its own
   token inside the window before the first one was stored.
   `reviveOnActivity` fires on every tab update, so that window was busy.
   Fixed with a `connecting` flag and by memoising the instance *promise*
   rather than the value.
2. **The pid was `cmd.exe`'s.** Chromium on Windows does not launch a native
   messaging host directly — it goes through `cmd.exe`, so the browser is the
   **grandparent**:
   `kratos-host.exe#27584 <- cmd.exe#2644 <- brave.exe#17472`.
   Taking the immediate parent reported a `cmd.exe` pid as the browser, which
   is exactly the wrong-pid case the lookup was written to avoid. Now walks up
   to four hops and returns the first ancestor whose image is a known browser.
3. **Vivaldi reported itself as `chrome`.** It ships Chrome's user agent and
   offers no `navigator.brave`-style hook, so the extension's detection was
   wrong — and lane resolution keyed on that would look for the wrong
   browser's windows. The host now takes `brand` from the ancestor's image
   name and keeps the extension's answer only as a fallback.
4. **Stale hosts.** One browser had three hosts alive, two of them registered
   to nothing, because a service-worker restart opens a new port and Chromium
   does **not** close the old host's stdin. A host now exits when it is sent
   `evt superseded`, and when a write to the extension fails — the two
   notices it can actually get.

### Evidence

```text
cargo test --workspace
   307 passed; 0 failed; 3 ignored   (was 295 + 4 ignored)
cargo clippy --workspace --all-targets
   0 warnings

cargo test -p kratos-broker --test multi_harness_audit
   two_browser_instances_both_stay_registered      ... ok   (P0 (a), un-ignored)
   the_same_instance_reconnecting_supersedes_itself ... ok
```

Live, one broker, two browsers, after the four fixes:

```text
backends:
  dom-ext token=26f538bc-9ed5-4ce3-9203-d67b52611094 brand=brave   pid=26968
  dom-ext token=eb445cf7-3f7f-492e-96ea-96877126247f brand=vivaldi pid=27836
  uia     (no instance)
```

Both tokens survived a full browser restart unchanged, which is the proof that
`chrome.storage.local` persisted them and that the earlier five-token run was
the re-entrancy bug and nothing else.

### Decisions

* **`backend_for` picks the newest instance** until lanes choose (P3). It is a
  tie-break, not a policy: before instances existed the newest had evicted
  every older one, so "newest" is the only answer that changes no observable
  behaviour for a single-browser setup.
* **The broker does not force-close a superseded peer**, although this document
  used to say it did. Every backend here reconnects on disconnect, so a forced
  close is answered by a reconnect that supersedes the peer that just replaced
  it, forever. Dropping it from the registry is the broker's whole part;
  exiting is the peer's.
* **`profile_dir` is not sent by the extension.** No extension API reports it.
  The token is paired to a profile by the user in the settings window (P6) or
  by the launch that opened it (P5), which is what spec §4.2 already describes.
* **A blank token is no instance at all**, enforced in `Instance::usable()`,
  `Hello::with_instance` and the broker's handshake. Keying on an empty string
  would let two profiles share one slot — the defect wearing a different hat.

### Known gaps

* `instance.parent_hwnd` is always absent. The broker will resolve windows from
  `pid` in P3/P5; a host has no window of its own and guessing one would be
  wrong.
* The stale-host reaping is **not yet proved over a long run.** It is correct in
  the two cases a host can observe, and the count fell from 8 to 5 in the first
  minute, but a host whose extension is gone *and* whose broker is absent has
  no signal at all until one of them returns.
* **Nothing yet *reaches* the second instance, and that is now visible.** Live,
  with both browsers attached:

  ```text
  observe window="Compare OCR" prefer=dom-ext
    -> source=dom-ext  73 elements in 20ms  url=https://chatgpt.com/...   (Vivaldi)
  observe window="New Tab"    prefer=dom-ext
    -> backend_error: no active tab in this browser                       (Brave)
  ```

  Routing takes the newest instance, so the Brave tab is unreachable while
  Vivaldi is attached. P1 made both instances *registered*; addressing one is
  P3's lane routing, and until then a two-browser setup can read only the
  newest.
* **`window` means two different things.** The broker resolves it against OS
  window titles (`EnumWindows`), and the extension matches it against *tab*
  titles and URLs, so `window:"Brave"` picks the right OS window and then
  matches no tab. P3 replaces the hint for browser lanes with "the lane's
  active tab" and must settle what `window` means for a tab.
* The Python `uia` backend sends no instance, by design for now.

### Next

**P2 — identity, sessions, lanes, resolution, defaults, harness approval,
config, `whoami`.** Acceptance is the second P0 reproduction:
harness→browser, agent→profile, unassigned→default, a missing profile warns,
a new session re-resolves, an unapproved harness is blocked.

---

## P0 — audit and reproduce (no fixes)

### Done

* Wrote the spec verbatim to `docs/design-multi-harness.md`. It is the source
  of truth; every later phase re-reads it first.
* Read `README.md`, `docs/protocol.md`, `docs/workflow.md` before touching
  anything.
* Added `crates/kratos-broker/tests/multi_harness_audit.rs`: four tests that
  assert what the spec requires and fail against this build.
  * (a) `two_browser_instances_both_stay_registered` — two `dom-ext` instances;
    the second evicts the first and sends it `evt superseded`.
  * (b) `a_session_observes_its_own_lane_not_another_agents_window` — no lane,
    no `approved`, no `whoami`, and `window:null` is the OS foreground.
  * (c) `a_harness_and_its_browser_cannot_be_split_across_two_brokers` — two
    brokers for one user; the stranded agent is told `no_backend`.
  * (d) `only_one_input_lease_exists_for_one_os_user` — two brokers each grant
    the input lease, both numbered `l-1`.
* Wrote the audit table: `docs/audit-p0.md`, 41 rows against spec §2–§10 plus
  the measured failure output, five properties worth not breaking, and three
  open questions.
* Changed **no** production code. `crates/`, `extension/`, `backends/` and
  `docs/protocol.md` are untouched.

### Evidence

```text
cargo test --workspace
   295 passed; 0 failed; 4 ignored   (across 17 test binaries)

cargo clippy --workspace --all-targets
   Finished `dev` profile — 0 warnings

cargo test -p kratos-broker --test multi_harness_audit -- --ignored --test-threads=1
   test result: FAILED. 0 passed; 4 failed
   (full output in docs/audit-p0.md)
```

### Decisions

* **The four reproductions are `#[ignore]`d.** P0 asks for failing tests; the
  working rules ask for a clean `cargo test`. Ignored satisfies both: the
  reproduction is in the tree, runs on demand with `-- --ignored`, and the
  suite stays green. The phase that fixes a defect removes its `#[ignore]`, so
  **a test still ignored is a defect still present**.
* **Each test collects every gap and asserts the list is empty**, rather than
  stopping at the first failed assertion. A test that aborts on gap one hides
  gaps two and three, and this file exists to enumerate them.
* **A new test file rather than additions to `tests/integration.rs`.** P0 is
  audit-only; keeping the reproduction separate means nothing in the existing
  suite was edited, and the file can be deleted wholesale once every
  `#[ignore]` is gone.
* **A raw `hello` client (`RawPeer`) in the test file.** `PipePeer::handshake`
  builds the hello payload itself and cannot carry `harness`, `agent`,
  `session` or `instance`, so the tests send the frame directly. P1/P2 should
  widen the shared handshake instead, and then these can use it.
* **Line numbers in the audit table were read off the files, not recalled.**

### Known gaps found while auditing (not in the four)

* One `last_observation` and one `generation` counter are shared by every
  session. The journal's password redaction looks an element id up in whichever
  observation landed last, so a concurrent session's observation can make a
  sensitive field read as not sensitive (`state.rs:181`, `act.rs:1230`). Row 21
  of the audit table. Not reproduced by a test yet.
* Every non-dry-run `act` auto-acquires the `input` lease before dispatch,
  whichever backend will carry it, so two browser sessions serialise behind one
  another although neither touches the mouse (`act.rs:859`). Row 19.
* `config.set` writes and applies in memory, but nothing watches the file, so
  the "hot reload" of spec §8 does not exist yet for an external edit. Row 33.

### Open questions for later phases

1. **How is the machine-wide lease named?** Test (d) asserts one lease per OS
   user. The integration suite deliberately runs several brokers at once,
   isolated by `KRATOS_PIPE`; a guard keyed to the SID alone would make the
   test suite contend with itself, and one keyed to the pipe name would not
   satisfy (d). P3 must choose and say which, in a comment at the guard.
2. **What becomes of `start-browser.cmd`, `start-agent-browser.cmd` and
   `start-daemon.cmd <pipe>`?** They exist to pair by pipe, which §5 removes.
   P7 owns the removal; P3 should not delete them silently.
3. **An act that fails before the decide stage is still not journalled**
   (`CLAUDE.md`, known gap 2). Lane resolution and harness approval both fail
   *before* decide, so P2 and P3 would add two more unjournalled refusal paths
   unless that gap is closed first.

### Next

**P1 — instance-keyed backend registration.** Extension instance token, host
`hello` carrying `instance {token, brand, pid, parent_hwnd}`, registry keyed on
`(source, instance.token)`, supersede only the same key. Acceptance: two fake
browser instances stay registered; the same instance reconnecting supersedes —
which is test (a) with its `#[ignore]` removed.

Files P1 is expected to touch: `crates/kratos-protocol/src/lib.rs` (the
`Hello` and `HelloAck` shapes), `crates/kratos-ipc/src/peer.rs` (the shared
handshake), `crates/kratos-broker/src/{state.rs,peer.rs}` (the registry key),
`crates/kratos-host/src/main.rs`, `extension/background.js`,
`docs/protocol.md` §3 and §7, and
`crates/kratos-broker/tests/multi_harness_audit.rs`.
