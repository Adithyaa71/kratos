<div align="center">

# ΚΡΑΤΟΣ

### _krátos_ — ancient Greek for **power held, and exercised**

In Hesiod, Kratos is the one who stands beside the throne and does what is
decided. Not the deciding. **The doing.**

That is the whole design. Your agent decides. This holds the machine.

<br>

[![tests](https://img.shields.io/badge/tests-274%20passing-3fb950?style=flat-square)](#)
[![clippy](https://img.shields.io/badge/clippy-0%20warnings-3fb950?style=flat-square)](#)
[![rust](https://img.shields.io/badge/rust-9%20crates-f0883e?style=flat-square)](#)
[![transport](https://img.shields.io/badge/transport-named%20pipe%20·%20no%20TCP-2f81f7?style=flat-square)](#)
[![cold start](https://img.shields.io/badge/cold%20start-49ms-a371f7?style=flat-square)](#)

</div>

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

**4 · The browser extension** _(optional, and the single biggest upgrade)_

```bash
# load extension/ unpacked at brave://extensions, copy the id, then:
powershell -File install-extension.ps1 -ExtensionId <your-id>
```

Then **fully restart the browser** and reload any tab you want readable.

Why it matters, measured on the same page:

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
tools/list        →  14ms,  starts nothing at all
first real call   →  49ms,  spawns the broker detached and connects
5 min idle        →  gone
next call         →  back, as a fresh process
```

Two agents racing to spawn one is safe: a named mutex and `first_pipe_instance`
both guard it, and the loser exits quietly.

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

Release build, one laptop, real numbers:

| | |
|---|---|
| `tools/list` | **14ms**, spawns no process |
| cold start — spawn, pipe, handshake, status | **49ms** |
| framework overhead per read | **under 1ms** |
| browser perception, 32 elements | **20ms** |
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

crates/kratos-protocol    envelope, ops, errors, config       (no I/O at all)
crates/kratos-ipc         named pipe, owner-only ACL, NDJSON, DPAPI
crates/kratos-gate        gate, kill scopes, the native dialog
crates/kratos-decide      DecisionProvider, Jev/Clef, offline mock
crates/kratos-shot        screen and region fingerprints
crates/kratos-broker      the daemon
crates/kratos-mcp         MCP stdio shim
crates/kratos-host        browser native-messaging relay
crates/kratos-ui          settings: key, gate rules, kill switch, live log

extension/                MV3 · Brave, Chrome, Edge
backends/                 the perception backend, self-contained:
  kratos_backend.py         speaks the wire protocol
  perception/               the UIA walk, the app index, OCR
  actuation/                mouse and keyboard
skills/kratos/            how an agent should drive this
```

## Bring your own model

The wire format is System One: `{state, questions:{name:{type, instructions,
criteria}}}` with Bearer auth. TypeSafe's **Jev** speaks it. So does
Cloudflare's open-weight **Clef** family — only the endpoint URL differs, so
switching is a settings change, not a port.

One hard requirement, enforced at parse time: a provider must return **a label
with a probability distribution and a calibrated confidence**. One that returns
only a label is rejected, because every threshold in this repo would otherwise
become decoration.

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
