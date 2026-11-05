<div align="center">

# ΚΡΑΤΟΣ

### _krátos_ — ancient Greek for **power held, and exercised**

In Hesiod, Kratos is the one who stands beside the throne and does what is
decided. Not the deciding. **The doing.**

That is the whole design. Your agent decides. This holds the machine.

<br>

[![tests](https://img.shields.io/badge/tests-283%20passing-3fb950?style=flat-square)](#)
[![clippy](https://img.shields.io/badge/clippy-0%20warnings-3fb950?style=flat-square)](#)
[![rust](https://img.shields.io/badge/rust-10%20crates-f0883e?style=flat-square)](#)
[![transport](https://img.shields.io/badge/transport-named%20pipe%20·%20no%20TCP-2f81f7?style=flat-square)](#)
[![cold start](https://img.shields.io/badge/cold%20start-13ms-a371f7?style=flat-square)](docs/benchmarks.md)
[![reach](https://img.shields.io/badge/reach-any%20Windows%20app-3fb950?style=flat-square)](#it-is-not-a-browser-tool)

</div>

<br>

<img src="docs/images/kratos.gif" alt="One instruction executed and proved, one refused for want of a target" width="100%">

<br>

<img src="docs/images/architecture.svg" alt="Kratos architecture: agent to MCP shim to broker to perception backends" width="100%">

<br>

## The one-sentence version

An agent says **"click the Send button in the compose dialog."** A typed
decision model grounds that sentence against the real screen, picks the action
and the element, and returns a **calibrated probability**. A gate decides
whether it is allowed to happen at all. It runs. Then it has to **prove it did
something** before anyone is allowed to call it a success.

When the model is not sure, it says so and stops. That is the feature.

## What makes it different

**The model generates no text.** Ever. It answers two typed questions — *which
action kind* and *which element* — as labels with probability distributions.
Every literal comes out of your own sentence:

| decided by the model | taken from your sentence |
|---|---|
| click / type / navigate / open / … | the characters to type |
| which element on screen | the key to press |
| | the URL, the application name |

A layer that cannot emit a string **cannot invent a recipient**. That is not a
guardrail bolted on afterwards; it is the shape of the thing.

**The threshold is applied to the weaker of the two answers.** An action it is
certain about, aimed at an element it is guessing at, is the exact failure that
once typed a subject line into a To field. The weakest link decides.

<br>

<img src="docs/images/evidence.svg" alt="Four evidence sources decide between executed, no_effect and inconclusive" width="100%">

<br>

## Earned, not assumed

Most desktop automation reports success when the click returns without
throwing. Kratos does not accept that as evidence of anything.

A mutating action is `executed` only if **structure changed**, or the field
**reads back** what you asked for, or **pixels moved** beyond the ambient
drift measured moments earlier. Otherwise `no_effect` — and `no_effect` is not
a failure to report politely, it is the agent being told *do not assume this
worked*.

Pixel evidence has three outcomes, not two. On a live desktop, two consecutive
full-screen grabs differed by **0.067** while a real page navigation measured
**0.197**. A fixed threshold would have reported a click that missed entirely
as `executed`. So the coarse fingerprint is scoped to the target window, the
baseline is measured immediately before acting, and a frame too busy to read
returns **`inconclusive`** — which never counts as success.

## It is not a browser tool

The browser extension gets the loudest numbers, so it is worth being blunt
about what this reaches: **every window on the machine**. UI Automation,
Win32 and an OCR fallback are the default path; the extension is an
optimisation for one kind of window.

| | any window | a browser tab |
|---|---|---|
| how | UI Automation + Win32 + OCR | the MV3 extension |
| reaches | Explorer, Settings, Office, Notepad, installers, Electron apps, terminals, native dialogs, anything with a handle | the page, its iframes and its shadow roots |
| gives | role, label, value, enabled, bounds | …and container, CSS selector, URL |
| acts by | the control, via UI Automation patterns, falling back to a click at its centre | the element, via DOM events — never a coordinate |
| costs | 24 elements in 223ms; 168 in 2.1s on a dense window | 32 elements in 20ms |
| needs | nothing | the extension loaded and registered |

**The broker routes per window, not per session.** A tab goes to the extension
when one is attached; everything else goes to UI Automation; a tab goes there
too when it is not. One agent, one sentence, same gate, same evidence rules —
whichever path answered. Nothing in your prompt changes.

**If a window exposes nothing readable** — a canvas app, a remote desktop, a
game — OCR reads the pixels and the elements come back with bounds and text but
no tree. The decision model is told that is what it is looking at.

So: `open Excel`, `click Save As in the File menu`, `type "Q3" into the
filename box`, `focus the Settings window` are ordinary instructions here. The
browser is one target among many; it is simply the one where we could do
better than the accessibility tree, so we did.

<br>

## Quickstart

```bash
git clone https://github.com/Adithyaa71/kratos
cd kratos
cargo build --release
python -m pip install -r backends/requirements.txt
```

**1 · Point an agent at it.** That is the entire integration — no port, no
token, no daemon to start first.

```bash
claude mcp add kratos -- ./target/release/kratos-mcp.exe
```

Works with anything that speaks MCP over stdio. Or skip MCP entirely: the shim
holds no policy, so any language that can open a named pipe is a first-class
peer — [`docs/protocol.md`](docs/protocol.md) is the whole contract, and
`backends/kratos_backend.py` is a working example in 480 lines.

**2 · Give it eyes.**

```bash
start-backend.cmd          # or: python backends/kratos_backend.py
```

The perception and actuation packages ship inside `backends/`, so this runs
from a fresh clone with nothing else on disk. It waits for the broker and
reconnects on its own, so the order you start things in does not matter.

**3 · Give it a decision model.**

```bash
./target/release/kratos-ui.exe
```

Paste a key under **Decision model**. It is sealed with Windows DPAPI for your
account — the config file is not a plaintext credential, a copy of it is
useless on another machine, and nothing can read it back out over the wire.

**4 · The browser extension** _(optional — everything works without it)_

Load `extension/` unpacked at `brave://extensions`, copy the id it generates,
and paste it into the settings window: **Browser** → **Extension ID** →
**Register**. That writes the native-host manifest and the per-browser registry
keys for you, and shows you where it put them.

<img src="docs/images/ui-browser-tab.svg" alt="The Browser tab: paste the extension id and press Register" width="760">

Or from a shell, if you prefer:

```bash
powershell -File install-extension.ps1 -ExtensionId <your-id>
```

Either way, **fully restart the browser** afterwards and reload any tab you
want readable. The host is looked up once when the extension starts, so
reloading the extension is not enough.

What it buys you, measured on the same page:

| | dom-ext | uia |
|---|---|---|
| elements | 32 in **20ms** | 24 in 223ms |
| containers | **32 / 32** | 0 |
| CSS selectors | **32 / 32** | 0 |
| URL | ✅ | ❌ |

`container` is the field that tells `combobox "Search" in form` apart from
`button "Search" in search`. Without it a decision model has no basis to
choose — and it will still answer, confidently.

## The daemon you never start

Nothing is registered at logon. The broker **does not exist** until the first
`desktop_*` tool call, and it exits again after five idle minutes.

```
tools/list        →  0.19ms,  starts nothing at all
first real call   →  13ms,    spawns the broker detached and connects
5 min idle        →  gone
next call         →  back, as a fresh process
```

Two agents racing to spawn one is safe: a named mutex and `first_pipe_instance`
both guard it, and the loser exits quietly.

**When you do want to start it yourself** — watching a new backend, tuning a
gate rule, keeping one session alive — there are three ways, and none of them
changes behaviour:

```bash
./target/release/kratos-ui.exe     # Status → Start broker
start-daemon.cmd                   # foreground, log on your console
start-daemon.cmd kratos.agent-a    # a broker of its own, for one agent
```

An agent connecting afterwards finds the running one. The pipe name is the only
thing that has to match.

## The gate

Every action passes through, in order: **kill → rate limit → never-allowed →
needs-approval → allow.**

- **Quoted text is a payload, not a command.** `type "Order-taking agents"` does
  not trip the `order` rule. Without that exception no ordinary email can be
  written — found live, where it cost two minutes on a modal nobody was looking
  at.
- **Review never blocks the connection.** You get a single-use token and get on
  with something else. The blocking version once ate **241 of 272 seconds** of
  a session.
- **Some approvals cannot be given by the agent.** Banking domains and
  off-allowlist recipients pop a native Windows dialog that no wire protocol
  can reach.
- **An agent may engage the kill switch and may never clear it.** Only the
  settings window or the global hotkey can. An agent that could un-stop itself
  does not have a kill switch.

Scopes, not a mode: `global`, `browser:brave`, `app:excel.exe`. Halt one
browser and leave everything else running.

## Many agents, one machine

Each agent gets its own broker, browser profile and extension instance, paired
by one environment variable:

```bash
start-agent-browser.cmd agent-a "Profile 1"
start-agent-browser.cmd agent-b "Profile 2"
```

```json
{ "env": { "KRATOS_PIPE": "kratos.agent-a" } }
```

Browser actions go through DOM events rather than the mouse, so two agents in
two profiles genuinely run in parallel and cannot see each other's tabs. Native
actions still share one physical pointer — there is one, and an input lease
serialises it with a FIFO queue.

## Measured

Not claimed — run it yourself, on your machine, in one command:

```bash
cargo run --release -p kratos-bench -- --out docs/benchmarks.md
```

| | p50 |
|---|---|
| MCP `tools/list` | **0.19ms**, and starts no broker |
| cold start — spawn, pipe, ACL, handshake, status | **13ms** |
| `status` round trip, agent to broker and back | **0.031ms** |
| gate verdict — block, deny, review, brand, rate | **0.005ms** |
| candidate compression, 64 elements | **0.025ms** |
| screen grab, 1920×1080 virtual desktop | **33ms** |
| element fingerprint, 96×96 crop | **0.29ms** |

Everything there is this repository's own overhead, measured against the
offline mock model: no network, no key, nothing of anybody else's. The full
table and what it excludes: [**docs/benchmarks.md**](docs/benchmarks.md).

Observed on a live desktop, which is a different kind of number and is labelled
as one:

| | |
|---|---|
| browser perception, 32 elements | **20ms**, all with containers and selectors |
| UI Automation, 24 elements | **223ms**; 168 on a dense window, 2.1s |
| a decision, live | **~300ms**, typed and calibrated |
| full action around an instant backend | **484ms** — 350ms of it a deliberate settle |
| proof step | **134ms**, three screen grabs |
| cost | **~$0.00008** per action · ≈12,000 actions per dollar |

Confidence separation on real traffic: executed decisions at **0.93–0.99**,
refusals at **0.42–0.71**. Nothing in between.

## Layout

```
docs/protocol.md          the wire protocol, v1, locked before any code
docs/workflow.md          how the pieces fit, and how to run them
docs/brave.md             the browser profile, and what it may reach
docs/benchmarks.md        the numbers above, and the command that made them

crates/kratos-protocol    envelope, ops, errors, config       (no I/O at all)
crates/kratos-ipc         named pipe, owner-only ACL, NDJSON, DPAPI
crates/kratos-gate        gate, kill scopes, the native dialog
crates/kratos-decide      DecisionProvider, Jev/Clef, offline mock
crates/kratos-shot        screen and region fingerprints
crates/kratos-broker      the daemon
crates/kratos-mcp         MCP stdio shim
crates/kratos-host        browser native-messaging relay
crates/kratos-ui          settings: key, gate rules, kill switch, live log
crates/kratos-bench       the benchmark, offline and reproducible

extension/                MV3 · Brave, Chrome, Edge
backends/                 the perception backend, self-contained:
  kratos_backend.py         speaks the wire protocol
  perception/               the UIA walk, the app index, OCR
  actuation/                mouse and keyboard
skills/kratos/            how an agent should drive this
```

## Bring your own decision model

<img src="docs/images/deciders.svg" alt="The DecisionProvider port: Jev, Clef, or your own model" width="100%">

<br>

The model is a part, not a dependency. Three ship ready to use, and swapping
between them is a dropdown in the settings window:

| | what it is |
|---|---|
| **Jev** | TypeSafe System One, hosted. ~300ms, typed, calibrated. |
| **Clef / Clef-flash** | Cloudflare, open weights. The same wire shape — only the endpoint differs. |
| **mock** | offline, deterministic, no key. What the tests and the benchmark run against. |

**A fourth option is yours.** The port is one trait with three methods and no
runtime of its own:

```rust
pub trait DecisionProvider: Send + Sync {
    fn name(&self) -> String;
    fn configured(&self) -> bool;
    fn ask(&self, state: &Value, questions: &BTreeMap<String, Question>)
        -> Result<Answers, DecideError>;
}
```

Local weights behind llama.cpp or vLLM, a hosted endpoint of your own, a
fine-tune, a distilled classifier, or something that is not a language model at
all — a ranker over the candidate list would satisfy this interface completely.
**How it was trained is not this code's business**: RLHF, RLAIF, RLCD, plain
supervised, hand-written heuristics. It is asked two typed questions and has to
answer them.

One hard requirement, enforced at parse time: a provider must return **a label
with a probability distribution and a calibrated confidence**. One that returns
only a label is rejected, because every threshold in this repository would
otherwise become decoration — and a confidence bar you cannot trust is worse
than no bar at all.

Everything else is unaffected by which model answers. The gate, the kill
switch, the lease, the journal and the evidence rules do not know and do not
ask.

## Safety, stated plainly

- **No TCP.** One named pipe, DACL admitting a single SID. Loopback is
  reachable by every process on the machine; it is not used.
- **Screen content is data, never instructions.** A page telling an agent to
  run a command is content on a screen, not a request from you.
- **The key lives in exactly one process.** Never on the wire, never in the
  log, never returned by `config.get`.
- **Everything is written down.** One JSONL line per action: who asked, what
  was decided, at what confidence, what the gate said, what changed.

---

<div align="center">

**Kratos does not decide. Kratos does.**

</div>
