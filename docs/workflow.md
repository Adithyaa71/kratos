# How it all fits together

## The four pieces, and which are optional

| piece | required? | what happens without it |
|---|---|---|
| **broker** (`kratos-broker.exe`) | yes | nothing works. Starts itself — see below. |
| **a perception backend** | **yes** | the broker can decide nothing: it cannot see the screen. Every `act` fails `no_backend`. |
| **a decision-model key** | **yes** | every `act` is refused `decider_unconfigured`. |
| **browser extension** | no, but | browser targeting falls back to UI Automation, which reports a flat list with no containers and no selectors — the thing that makes two fields labelled "To" indistinguishable. |

The status screen tells you which are missing. If it says *"None connected"*
under Perception backends and *"missing — actions will be refused"* next to API
key, you have the first two to go.

---

## 1. The decision-model key

Settings window → **Decision model** → paste into API key → **Save**.

Sealed with Windows DPAPI for your account. `config.json` is not a plaintext
credential, a copy of it is useless on another machine, and the key is never
returned over the wire or written to the log. The field comes up empty on
purpose even when a key is stored — there is nothing to read it back from.

To swap to Cloudflare Clef instead, change Provider and set Endpoint to
`https://api.cloudflare.com/client/v4/accounts/<account-id>/ai/run/@cf/cloudflare/clef`.
Same request shape, same Bearer auth; only the URL differs.

---

## 2. A perception backend

```
python D:\Kratos\backends\kratos_backend.py
```

Or `start-backend.cmd`. It waits for the broker and reconnects on its own, so
the order does not matter and it survives a broker restart. Leave it running in
a terminal — it is the thing that reads the screen.

Measured on your machine: **92 elements in 431ms** from a live Brave window.

---

## 3. The browser extension

Three steps, and the second is the one everyone misses.

### a. Load it

1. `brave://extensions` → Developer mode on → **Load unpacked**
2. pick `D:\Kratos\extension`
3. copy the **Extension ID**

### b. Register the native host

The extension cannot reach the broker on its own. A browser will only launch a
local program named in a registry-registered manifest, which is why the popup
currently says **Broker: not running** even when one is.

```powershell
cd D:\Kratos
.\install-extension.ps1 -ExtensionId <the id you copied>
```

This writes `%LOCALAPPDATA%\kratos\native-host.json` with the absolute
path to `kratos-host.exe` and your extension id, then adds the registry key
for Brave, Chrome and Edge.

### c. Restart the browser, reload your tabs

- **Fully quit and reopen the browser.** The native host is looked up when the
  extension starts; a reload of the extension is not enough.
- **Reload any tab you want readable.** Content scripts are injected at page
  load. A tab opened before you installed the extension has no content script
  in it, which is exactly what *"This tab: not readable here"* means.

Tabs that can never be read, by browser policy: `brave://`, `chrome://`,
`edge://`, the Chrome Web Store, and other extensions' pages. Nothing can
script those.

When it is working the popup reads **Broker: connected** and **This tab: N
elements**.

---

## 4. Connecting a harness

### Anything that speaks MCP over stdio

Point it at `kratos-mcp.exe`. That is the whole requirement — no port, no token, no
config file, no daemon to start first.

```bash
claude mcp add kratos -- D:/Kratos/target/release/kratos-mcp.exe
```

The same binary works for Claude Desktop, Cline, Continue, Zed, the OpenAI
Agents SDK, or anything else with an MCP stdio client. In a JSON config:

```json
{
  "mcpServers": {
    "kratos": {
      "command": "D:/Kratos/target/release/kratos-mcp.exe",
      "env": { "KRATOS_AGENT_NAME": "whatever-you-want-in-the-log" }
    }
  }
}
```

| env var | default | why you would set it |
|---|---|---|
| `KRATOS_AGENT_NAME` | `mcp-client` | names this harness in the journal |
| `KRATOS_PIPE` | `kratos.v1` | a second, isolated broker |
| `KRATOS_BROKER_EXE` | beside `kratos-mcp.exe` | the binaries live apart |
| `KRATOS_CONFIG` / `KRATOS_JOURNAL` | `%LOCALAPPDATA%\kratos\…` | alternate state |

### Skipping MCP entirely

The MCP shim is a translator with no policy in it. Any language that can open a
Windows named pipe can be a first-class peer: send NDJSON, start with `hello`,
and everything in `docs/protocol.md` is available. `backends/kratos_backend.py` is a
working 480-line example of exactly that in Python.

### Several harnesses at once

They share one broker. Each gets its own `peer_id`, appears separately in the
journal, and they take turns on the mouse via the input lease — one holder at a
time, FIFO queue, and a crashed agent's lease is released when its connection
drops. A second agent acting while the first holds the lease gets `lease_held`
with the holder's name and how long is left, not a collision on the mouse.

---

## 5. Does a connected agent start the broker by itself?

**Yes, and that is the normal path.** Any agent through `kratos-mcp.exe` gets this
for free:

1. `tools/list` → **starts nothing**. Enumerating capabilities must not spawn a
   background process. Measured: 14ms, no pipe created.
2. the first `desktop_*` call → shim connects; nothing listening; it spawns
   `kratos-broker.exe` detached, waits for the pipe with backoff, and carries on.
   **Measured: 49ms median from cold.**
3. five minutes with no agent calls → the broker exits. A settings window being
   open suppresses that; a backend does not, because backends reconnect.
4. the next call brings it back, as a fresh process.

Two harnesses racing to spawn one broker is safe: a named mutex and
`first_pipe_instance` both guard the name, and the loser exits quietly.

The **Start broker** button in the settings window is only for looking at a
broker when no agent is driving. Nothing is registered at logon; a machine
nobody is driving runs no part of this.

---

## 6. The Gate screen, box by box

The gate runs on every action, before anything moves, in this order: kill
switch → rate limit → never-allowed → needs-approval → allow. One rule per
line in each box.

### Never allowed
`format c:`, `rm -rf /`, `diskpart clean`, …

Matched against the **whole instruction including quoted text**, because typing
`rm -rf /` into a terminal is as destructive as clicking it. There is **no
confirmation path** — the action returns `blocked` and no token. Keep this list
short and genuinely catastrophic.

### Needs approval — keywords
`send`, `delete`, `pay`, `purchase`, `order`, `transfer`, …

Matched as **whole words** (so `sender` does not trip `send`) and **not against
text being typed**. That exception is load-bearing: without it, the subject line
`type "Order-taking AI agents" into Subject` trips the `order` rule, and no
ordinary email can be written. Found live, drafting exactly that email, where it
cost two minutes on a modal dialog.

Result: `needs_confirmation` with a single-use token. Nothing ran.

### Needs approval — applications
`1password`, `regedit`, `powershell`, `task manager`, …

Matched against the **window title**, so it catches the target rather than the
wording.

### Needs approval — sites
`chase.com`, `paypal.com`, `coinbase.com`, …

Matched on the **domain, subdomains included** — `secure.chase.com` matches a
`chase.com` rule, `notchase.com` does not. These additionally require a **click
in a native Windows dialog** that no agent can reach: an agent reporting that
you approved is not sufficient here.

### Blocked sites
Same domain matching, but a hard refusal with no confirmation path.

### Allowed recipients
Empty by default, which means no recipient filtering at all.

Put `someone@example.com` or a bare `example.com` here and **any email address
outside the list escalates to a native dialog**, even with no keyword match.
This is the rule that would have caught a subject line landing in a To field:
it is about *who*, which a keyword list cannot express.

### Actions per minute
Default 30. A runaway loop hits it and gets `blocked` with
`rate_limit_exceeded` rather than clicking 400 times. Refusals do not spend
budget, so a halted system answers the same way however often it is asked.

### What the gate is not
It is not the confidence threshold. That lives on the **Confidence** tab and
answers "is the model sure what you meant". The gate answers "is this allowed
to happen at all", and applies to every agent on the broker, not just one.

---

## 7. Efficiency

Measured on this machine, release build.

### Framework overhead — what the broker itself costs

| | |
|---|---|
| `desktop_status` round trip | **<1ms** |
| `desktop_observe`, 40 elements | **1ms** |
| `desktop_act` dry run (observe + decide + gate) | **1ms** |
| `desktop_verify` | **<1ms** |
| MCP shim start + initialize + tools/list | **14ms**, no broker spawned |
| cold start (spawn broker → pipe → handshake → status) | **49ms** |

The pipe, framing, routing, gate, lease and journal together are effectively
free. Nothing in the plumbing is worth optimising.

### A full action, with everything instant around it

**484ms**, decomposed:

| stage | cost | notes |
|---|---|---|
| observe before | backend-dependent | 431ms real UIA, ~1ms fake |
| decide + gate | model-dependent | Jev 70–500ms, mock ~0ms |
| **settle** | **350ms** | fixed wait for the UI to repaint |
| baseline + fingerprints + re-observe + prove | **~134ms** | three screen grabs |

### What a real action costs you

With the Python UIA backend and Jev:

```
  431ms   observe
+ ~200ms  Jev decides (70-500ms documented)
+ 350ms   settle
+ 134ms   proof: baseline and fingerprints
+ 431ms   re-observe
--------
~1.5s per action
```

For comparison, the previous build measured **2,906ms** per browser action. The
extension should cut the two observes substantially, since a DOM scan does not
walk an accessibility tree.

### Cost in money

Jev is $0.042 per million input tokens, output free. A typical action sends
roughly 1.8k tokens — on the order of **$0.00008 per action**, or about **12,000
actions per dollar**.

### The grab optimisation, applied

The proof step used to take eight BitBlts and two 35ms sleeps: two grabs for
the window baseline, two for the element-region baseline, two for the before
pair and two for the after pair. It now takes **three grabs and one sleep**.

Two changes. Each moment is captured once as the whole virtual desktop and both
rectangles are cropped out of it. And the second baseline grab *is* the
"before" state -- a baseline measures t0 against t1, and before is t1:

```
grab A        t0
sleep 35ms
grab B        t1   -> baseline = diff(A,B)  AND  before = B
...act, settle...
grab C             -> after = C
```

Measured: **266ms -> 134ms**, a full action **617ms -> 484ms**. Three
virtual-screen captures cost 100ms against 407ms for the eight they replaced.

It is also more correct than what it replaced: the window and region numbers
now describe the *same instant*, which separate grabs never quite did.

### Where the time is *not*

Not in Rust, not in the pipe, not in the gate. The two real costs are reading
the screen and waiting for it to settle, which is exactly where they were in the
previous build. What changed is that an action now has to *prove* it did
something before being called a success, and that proof costs 134ms.

The 350ms settle is now the single largest fixed cost. It is a conservative
guess at "long enough for a browser to repaint and a menu to open" and could
reasonably be made adaptive -- poll the window fingerprint until it stops
moving, with a ceiling.
