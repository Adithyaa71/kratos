# kratos wire protocol — v1

**Status: evolving, additive-only until first working release.** The v1 message
shapes below do not change *meaning*: new fields are optional with a documented
default, new ops and new members of a closed set are additive, and anything
that would change the meaning of an existing field ships as v2 alongside v1.
The wire version stays `1` throughout.

Locked was the right word while nothing had been built on it. Multi-harness
work ([design-multi-harness.md](design-multi-harness.md)) adds fields to the
handshake and ops to the agent and UI roles, and calling that v2 would mean two
protocols to support for a build that has not shipped once.

Everything in this repo speaks this protocol: the MCP shim, the browser native
host, the Python perception/actuation backends, and the configuration UI. There
is exactly one transport and one envelope, so a peer written in any language is
a first-class peer.

---

## 1. Transport

| | |
|---|---|
| Channel | Windows named pipe, byte stream |
| Name | `\\.\pipe\kratos.v1.<user-sid>` |
| TCP | **never.** No socket is opened on any interface, loopback included. |
| ACL | `D:P(A;;FA;;;<current-user-sid>)` — protected DACL, one ACE, owner only. No SYSTEM, no Administrators, no inheritance. |
| Framing | NDJSON: one UTF-8 JSON object per line, newline terminated, no embedded raw newlines |
| Max line | 8 MiB. A longer line is a protocol violation: the broker replies `too_large` and closes that connection. |
| Concurrency | Many simultaneous connections. Requests on one connection may be answered out of order; correlate on `id`. |

The SID suffix means two Windows users on one machine get two independent
brokers and cannot see each other's pipe — a property of the name, not only of
the ACL.

### 1.1 Browser native messaging

Chromium mandates its own framing on the host's stdio: 4-byte little-endian
unsigned length, then that many bytes of UTF-8 JSON, max 1 MiB inbound.
`kratos-host` is the only component that speaks it. It translates 1:1 to
NDJSON on the pipe and adds nothing: a message observed on the pipe is
byte-identical to the JSON the extension sent, modulo framing.

Replies larger than 1 MiB are chunked by the extension (section 6.3), because a
full DOM snapshot of a heavy page exceeds that limit.

---

## 2. Envelope

Every line, in every direction, is exactly one of these three shapes.

### Request
```json
{"v":1,"t":"req","id":"7f3a9c","op":"act","payload":{}}
```

### Response
```json
{"v":1,"t":"res","id":"7f3a9c","ok":true,"payload":{}}
{"v":1,"t":"res","id":"7f3a9c","ok":false,"error":{"code":"gate_blocked","message":"...","detail":{}}}
```

### Event (unsolicited, no reply)
```json
{"v":1,"t":"evt","op":"kill","payload":{}}
```

| field | type | rule |
|---|---|---|
| `v` | u16 | Protocol major. Always `1` here. A peer receiving an unknown `v` replies `bad_version` and closes. |
| `t` | `req` / `res` / `evt` | |
| `id` | string, 1-64 chars | Unique per connection per in-flight request. The responder echoes it verbatim. Absent on events. |
| `op` | string | Operation name. Required on `req` and `evt`, absent on `res`. |
| `ok` | bool | `res` only. |
| `payload` | object | Always an object, never a bare value. `{}` when empty. |
| `error` | object | `res` with `ok:false` only. Never present when `ok:true`. |

`error.code` is a closed set (section 7) and is the only field a program may
branch on. `error.message` is for humans and may change between builds.
`error.detail` is an open object.

Unknown fields are ignored, never echoed. An unknown `op` on a request is
`bad_request`.

---

## 3. Handshake

First message on every connection, before anything else. A peer that sends any
other op first gets `unauthorized` and is closed.

**Request `hello`**
```json
{"v":1,"t":"req","id":"1","op":"hello","payload":{
  "role":"agent",
  "name":"claude-code",
  "build":"kratos-mcp 0.1.0",
  "v_min":1,
  "v_max":1,
  "source":null,
  "handles":null,
  "harness":"claude-code",
  "agent":"agent-a",
  "session":"s-21884-1762502400123"
}}
```

**Response**
```json
{"v":1,"t":"res","id":"1","ok":true,"payload":{
  "v":1,
  "peer_id":"a-3",
  "broker":"kratos-broker 0.1.0",
  "decider":"jev/jev-latest",
  "kill_engaged":[],
  "backends":[{"source":"uia","handles":["native","browser"]},
              {"source":"dom-ext","handles":["browser"]}],
  "approved":true,
  "lane":{"browser":"brave","profile_dir":"Default","profile_name":"Work",
          "assigned":{"harness":"claude-code","agent":"agent-a"},
          "state":"ready"}
}}
```

| `role` | who | may call |
|---|---|---|
| `agent` | MCP shim | `observe` `act` `verify` `confirm` `lease.*` `kill.engage` `kill.status` `status` `whoami` `log.tail` |
| `backend` | Python backend, native host | `status`; **receives** `backend.*` requests from the broker |
| `ui` | config and log window | `status` `whoami` `log.tail` `config.get` `config.set` `kill.*` `shutdown` `lane.list` `harness.list` `agent.list` `instance.bind` |

### 3.0 Identity: `harness`, `agent`, `session`

Three optional strings on an `agent` hello, and all three are **claims**. They
decide which lane the session resolves to and nothing else — an agent cannot
approve itself, choose a browser or pick a profile by claiming a name, because
every allocation lives in a config file that only the `ui` role can write.

| field | where it comes from | what it decides |
|---|---|---|
| `harness` | the MCP client's own `clientInfo.name`, which every MCP client already sends | which browser, via `harnesses[]` |
| `agent` | `--agent <name>` or `KRATOS_AGENT` in that client's snippet. The only setting an agent has | which profile, via `agents[]` |
| `session` | one shim process | nothing. It lets the broker tell two sessions of one harness apart in the log |

A harness the broker has not seen before is **written into the config
unassigned and unapproved**, which is the only thing an unapproved client's
connection achieves, and is what gives the settings window something to list.
Until a human approves it, every op but `status` and `whoami` is refused
`not_approved`.

**The approval dialog does not block the request.** Spec §3 of the
multi-harness design requires a native dialog the agent cannot answer; it does
not require the request to wait on it, and waiting is the mistake this gate was
built to avoid — a blocking approval once cost 241 of 272 seconds of a session.
So the dialog is raised, the op is refused `not_approved` with
`retry_safe:true`, and the retry succeeds the moment somebody clicks. One
dialog per harness at a time: an agent in a retry loop raises one window, not
one per request.

### 3.0.1 `lane` — one browser, one profile

```json
{"browser":"brave","profile_dir":"Default","profile_name":"Work",
 "assigned":{"harness":"claude-code","agent":"agent-a"},
 "state":"ready","warning":null}
```

* `browser` — `harnesses[harness].browser`, else `defaults.browser`, else the
  OS default browser (`UserChoice\ProgId`), else the first Chromium brand
  installed.
* `profile_dir` — `agents[agent].profile_dir` when that profile is listed in
  that browser's `Local State`, else `defaults.profile`, else the brand's
  `profile.last_used`. A directory name, never a path.
* `profile_name` — what the browser calls it, from `Local State`'s
  `info_cache`. `null` when it is not listed there.
* `state` — a closed set:

| `state` | meaning |
|---|---|
| `ready` | a backend instance is attached **and paired** with this browser and profile |
| `launching` | something is opening it |
| `degraded` | drivable, but not through the extension: UIA and OCR only, with no `container` and no `selector` |
| `unavailable` | no such browser on this machine, or it cannot be opened |

* `warning` — why the lane is not what was asked for: a profile that is not
  there, a brand nobody has heard of, an attached extension that has not been
  paired with a profile yet. Shown in the settings window; an agent reads it to
  learn it is not where it expected to be.

**A lane is resolved per connection and never stored against a session.** There
is nothing to invalidate: an allocation changed in the settings window takes
effect on the next connection, and `whoami` re-resolves so a change mid-session
is visible without reconnecting.

`peer_id` is assigned by the broker, appears in every journal line, and is the
agent identity in the action log. A peer does not choose its own id: `name` is
a self-reported label and is logged as such.

Version negotiation: the broker picks the lower of the peer's `v_max` and its
own; if that is below the peer's `v_min` it replies `bad_version` with
`detail:{"broker_v_max":1}` and closes. The chosen `v` applies to every
subsequent message on that connection.

A `backend` must set `source` and `handles`:

| `source` | provided by | `handles` |
|---|---|---|
| `dom-ext` | MV3 extension via `kratos-host` | `["browser"]` |
| `uia` | Python, Windows UI Automation | `["native","browser"]` |
| `ocr` | Python, Tesseract | `["native","browser"]` |
| `pixel` | Python, detector plus OCR | `["native","browser"]` |

### 3.1 `instance` — which one of a source

A `backend` may also send `instance`, and `dom-ext` does:

```json
{"v":1,"t":"req","id":"1","op":"hello","payload":{
  "role":"backend","name":"browser-extension","build":"kratos-host 0.1.0",
  "source":"dom-ext","handles":["browser"],
  "instance":{"token":"9f2c…","brand":"brave","pid":21884}
}}
```

| field | who fills it | meaning |
|---|---|---|
| `token` | the extension, once per profile, stored in that profile | the only stable identity. Survives a reload, a restart and an upgrade. Required; a blank one is treated as no `instance` at all. |
| `brand` | the extension | `brave`, `chrome`, `edge`, `vivaldi`, `opera`. A hint for narrowing, not an identity. |
| `pid` | `kratos-host`, as its own parent process | the browser process, which is what links an instance to windows on screen. Absent if the lookup failed; a wrong pid would be worse than none. |
| `parent_hwnd` | nobody, today | reserved. `kratos-host` has no window of its own, so the broker resolves windows from `pid` rather than trusting a guess. |

`profile_dir` is deliberately **not** sent: no extension API reports the
profile directory name. A token is paired with a profile by the user in the
settings window, or by the launch that opened it.

**The registry key is `(source, instance.token)`.** A backend re-registering
the same key supersedes the older peer, which is sent `evt superseded` and
**dropped from the registry** — a crashed-and-restarted backend is
self-healing rather than a duplicate. A
backend with a *different* instance of the same source is a **second
registration**, not a replacement: one `dom-ext` source has as many instances
as there are browser profiles open, and a tab in one cannot be reached through
another.

No `instance` means "the only instance of this source", which is what `uia`,
`ocr` and `pixel` are, and is exactly the behaviour every backend had before
this field existed.

> Keying on `source` alone meant a second browser profile attaching **evicted
> the first**, so one harness opening its browser silently unplugged another's
> — and the displaced peer was told `evt superseded`, which reads like a
> restart and was not one.

`status` and the hello reply report `instance` per backend entry, so two
entries with the same `source` are two live instances rather than a duplicate
to be cleaned up.

**A superseded peer is expected to exit; the broker does not close it.** It
would be tidier to close the connection, and this document used to say the
broker did. It must not: every backend here reconnects on disconnect, so a
forced close is answered by a reconnect that supersedes the peer that just
replaced it, and two backends trade the registration forever. Dropping it from
the registry is the whole of the broker's part. `kratos-host` exits on
`evt superseded`, which is also how a host left behind by a service-worker
restart is reaped — Chromium does not close such a host's stdin, so being
superseded is the only notice it gets.

---

## 4. Agent operations

### 4.1 `observe`

```json
{"op":"observe","payload":{"window":null,"prefer":null,"timeout_ms":8000}}
```

`window` — substring match on title. **`null` means this session's lane**, not
the foreground window: the active tab of the browser and profile the session
resolved to (§3.0.1). A session with no lane — one whose allocation is
`browser: "none"` — keeps the old meaning, the window in front.

`prefer` — force a backend `source`; `null` means routing policy (5.1).

**Windows of other lanes are invisible.** A `window` string that matches only
another lane's browser is `no_target`, with the lane named in the detail, and
never a silent hit on somebody else's screen. Native windows belong to no lane
and stay visible to every session: there is one Notepad, and the input lease
is what serialises it.

> Before lanes, `null` was `GetForegroundWindow()` — the same answer for every
> agent on the broker. Two agents working at once drove whatever the *user* was
> looking at, and each could read the other's screen.

```json
{"ok":true,"payload":{
  "gen":17,
  "source":"dom-ext",
  "window":"Inbox - Gmail - Brave",
  "url":"https://mail.google.com/mail/u/0/#inbox",
  "target_class":"browser",
  "elements":[
    {"id":"brave:w3:t17:e42","role":"textbox","label":"To recipients",
     "value":"","enabled":true,"bbox":[412,268,733,22],
     "container":"dialog:New Message","selector":"div[role=dialog] input[aria-label=\"To recipients\"]"}
  ],
  "truncated":false,
  "took_ms":71
}}
```

* `gen` — monotonic per broker. **Element ids are only valid within their
  generation.** Acting on an id from an older `gen` returns `stale_id` rather
  than clicking whatever now sits at that position. This is the whole reason
  `gen` exists: an id that silently re-points is how a subject line ends up in
  a To field.

  One bounded exception, and it is bounded because the exception is in what
  the id *means*. An `ocr:` or `pixel:` id names a position, because a
  rectangle in an image is all those sources have; OCR and the detector
  re-measure every pass and disagree with themselves by a pixel or two. So
  when such an id is not found verbatim, the broker will accept the element of
  the same source whose centre is within six pixels of the pinned one **and**
  whose width and height are within six pixels of it — and only if there is
  exactly one such element, because two is the ambiguity `stale_id` exists to
  report. A `uia:` or `dom-ext` id names a node, so a missing node stays
  `stale_id`: something else moved into that rectangle is the normal case
  there, not the exception.
* `id` — opaque to the agent. Namespaced `browser:window:tab:element` for
  `dom-ext`, `uia:<hwnd>:<runtime-id>` for `uia`, and for anything the pixels
  found rather than the tree, `ocr:<cell>` or `pixel:<cell>` where the cell is
  the element's centre in four-pixel units, with a `#n` suffix if two land in
  one cell. Never parsed by the agent.

  The cell, rather than the element's position in the list, because the list
  changes: the icon detector found 33 controls on one pass and 34 on the next,
  and index-derived ids renumbered every element after the newcomer. An id
  derived from where the thing is survives a list that grew.
* `container` — the nearest enclosing named region (ARIA dialog, form or
  landmark; or the UIA parent pane). Supplied so the decision model can tell
  two identically labelled fields apart. **Required** from `dom-ext`,
  best-effort elsewhere.
* `bbox` — `[x, y, w, h]` in physical screen pixels, DPI-resolved. `dom-ext`
  converts from CSS pixels by `devicePixelRatio` and adds the tab's screen
  offset, so a bbox is directly clickable by OS input.
* `selector` — a document-unique CSS selector, `dom-ext` only, `null`
  elsewhere. This, not `bbox`, is how `dom-ext` acts (6.2).
* `truncated` — the element list hit a cap. The agent should narrow `window`
  rather than trust completeness.

### 4.2 `act`

The one call that does work. The broker observes, decides, gates, leases,
executes and verifies — in that order — and returns all of it.

```json
{"op":"act","payload":{
  "instruction":"click the \"Send\" button",
  "window":null,
  "dry_run":false,
  "lease":null,
  "target_id":null,
  "timeout_ms":20000
}}
```

`target_id` pins the element instead of letting the model choose it: an id
from a recent `observe`. The broker still re-observes, still decides the
*kind*, and still gates -- but the element is the agent's choice, and an id
that is no longer on screen is `stale_id` rather than a nearby guess. It is
the deterministic escape hatch for a screen where two controls genuinely
read alike.

`instruction` is plain language, written as if to a person looking at the same
screen, with literal text to be typed in quotes. The broker does not generate
text: the characters typed, the key pressed and the URL navigated to are
extracted from this string. Only *which element* and *what kind of action* are
decided by the model.

`dry_run:true` runs observe, decide and gate, then returns the plan with
`status:"planned"` without touching the input devices.

```json
{"ok":true,"payload":{
  "status":"executed",
  "gen":17,
  "decision":{
    "kind":"click",
    "target_id":"brave:w3:t17:e42",
    "target_label":"Send",
    "confidence":0.98,
    "kind_confidence":0.99,
    "target_confidence":0.98,
    "threshold":0.70,
    "decider":"jev/jev-latest",
    "decide_ms":138
  },
  "gate":{"category":"review","rule":"review_keyword:\"send\"","scope_checked":"browser:brave"},
  "effect":{
    "structural":{"added_count":0,"removed_count":31,"changed_count":2,"focus_changed":true},
    "screen_changed":0.1973,
    "region_changed":null,
    "readback":null,
    "evidence":"structural+pixel"
  },
  "backend":"dom-ext",
  "took_ms":1402
}}
```

`status` is a closed set:

| `status` | meaning |
|---|---|
| `executed` | Acted, and **something verifiably changed** (5.4). |
| `no_effect` | Acted, and nothing changed. The click missed or the control was inert. Not a success. |
| `planned` | `dry_run` only. |
| `needs_confirmation` | Gate said review. Nothing ran. `confirm_token` returned; the agent should do something else and come back. |
| `refused` | Decision confidence below `threshold`. Nothing ran. `decision` is still returned so the agent can see the near-miss. |
| `blocked` | Gate denylist or rate limit. Nothing ran, no confirmation possible. |
| `killed` | Kill switch engaged for a scope covering this target. Nothing ran. |
| `paused` | The person at the machine was using the mouse or keyboard, so an action that needed OS input did not take it. Nothing ran; the step is unchanged and safe to send again. |

`needs_confirmation` carries:
```json
{"status":"needs_confirmation","confirm_token":"c-9f21","expires_in_ms":180000,
 "gate":{"category":"review","rule":"review_keyword:\"send\""}}
```

The gate never blocks the connection waiting for a human. This is deliberate:
the blocking variant cost 241 of 272 seconds — 89% of wall clock — in measured
use.

### 4.3 `verify`

```json
{"op":"verify","payload":{"claim":"the message was sent and the compose window is closed","window":null}}
```
```json
{"ok":true,"payload":{"holds":true,"confidence":0.94,"decider":"jev/jev-latest","took_ms":121}}
```

A semantic question about the current screen, asked of the decision model.
Complementary to `effect` in 4.2, which is mechanical and cannot say *what*
changed. `holds` is `confidence >= 0.5`; the agent should read `confidence`.

### 4.4 `confirm`

```json
{"op":"confirm","payload":{"token":"c-9f21"}}
```

Resumes a parked `needs_confirmation` and returns exactly the 4.2 shape.

Tokens are **single-use**, expire at `expires_in_ms`, are bound to the
`peer_id` that created them, and are **refused if the foreground window changed
since parking** (`stale_confirm`). Re-observe and re-decide happen on resume:
the token authorises the *instruction*, never a cached coordinate.

A confirmation arriving over this protocol is the agent reporting that a human
approved. For gate rules configured `require_os_dialog`, that is not enough —
the broker pops a native OS dialog that no agent can click, and `confirm`
completes only if a human clicks it (5.3).

### 4.2a Who waits for whom

Two rules decide whether an `act` touches the input devices at all.

**A DOM action takes no lease.** It is delivered by the page's own event
system and touches no pointer, so two sessions in two browser profiles act at
the same time — which is the point of lanes. The lease is taken only when the
dispatch has to reach the OS: a native window, or a fallback after the page
ignored a synthetic event. Taking it for every action, which is what this did,
serialised two agents that could not have collided if they tried.

**An action that needs OS input waits for the person at the machine.** If real
mouse or keyboard input arrived within `defaults.user_idle_ms` (1500ms), the
action waits for quiet up to `defaults.pause_max_ms` (30s) and then returns
`paused` with nothing run. Injected input — every action the agent itself
takes — is ignored: `LLKHF_INJECTED` and `LLMHF_INJECTED` on a low-level hook
say which is which, and an agent that counted its own clicks would pause
itself after the first one. `status.pause` reports the settings, how long it
has been since real input, and whether the hooks are installed at all.

### 4.2b Tab ownership

Two sessions can share one lane — two agents of one harness — and then they
share a browser profile, its logins and its tabs. A tab belongs to the session
that first acted on it; another session's `act` on it returns `blocked` with
`gate.category: "tab_owned"` and the owner's `peer_id` in the rule. The user
can set `lanes[].shared` for a lane to turn that off. A session that
disconnects stops owning anything, so a crashed agent does not take its tabs
with it.

Ownership is recorded on first *use*, not on opening: nothing here can see a
tab being opened — the extension reports tabs, not their history — and the
session that opens a tab is the first to act on it in every case that matters.

### 4.5 `lease.acquire` and `lease.release`

```json
{"op":"lease.acquire","payload":{"scope":"input","ttl_ms":15000,"wait_ms":5000}}
```
```json
{"ok":true,"payload":{"token":"l-44","scope":"input","expires_in_ms":15000}}
```

There is exactly one mouse and one keyboard. The `input` lease serialises them
across agents — and across **brokers**: the holder also holds a named
semaphore `Local\kratos.input.<user-sid>`, so a second broker (an old build
still running, a test on another pipe) cannot hand out a second lease on the
same devices. The broker's own queue orders the agents attached to it; the
semaphore orders the brokers.

One broker per OS user is the other half of that: the single-instance guard is
keyed on the user, not on the pipe name, so `KRATOS_PIPE` no longer buys a
second broker with its own gate, journal and lease. It remains a test seam,
and setting it is what puts the pipe name back into both guards. `wait_ms` queues FIFO; `0` fails fast with `lease_held` and
`detail:{"holder":"a-2","expires_in_ms":4120}`.

An `act` without `lease` auto-acquires for the duration of that one action, so
a single-agent setup never manages leases. An `act` while another peer holds
the lease returns `lease_held` without touching input. A held lease that
expires is reclaimed; the former holder's next `act` gets `lease_invalid`
rather than silently sharing the mouse.

`lease.release` takes `{"token":"l-44"}` and is idempotent.

### 4.6 `kill.engage`, `kill.reset`, `kill.status`

```json
{"op":"kill.engage","payload":{"scope":"browser:brave","reason":"manual"}}
{"op":"kill.reset","payload":{"scope":"browser:brave"}}
{"op":"kill.status","payload":{}}
```
```json
{"ok":true,"payload":{"engaged":["browser:brave"],"hotkey":"ctrl+alt+k","hotkey_scope":"global"}}
```

| scope | matches |
|---|---|
| `global` | every target. Set by the hotkey. |
| `browser:<brand>` | `brave`, `chrome`, `edge`, `vivaldi`, `opera` — resolved from the target window's process image name |
| `app:<exe>` | one executable, for example `app:excel.exe` |

Scopes are a set, not a mode: `browser:brave` and `app:excel.exe` can be
engaged together while everything else keeps working. `act` is refused
`killed` if `global` is engaged or any engaged scope matches the resolved
target.

**An agent may engage, but `kill.reset` from an `agent` role is refused
`unauthorized`** — only the `ui` role or the hotkey can clear one. An agent
that can clear its own kill switch does not have a kill switch.

### 4.7 `status`

```json
{"ok":true,"payload":{
  "broker":"kratos-broker 0.1.0","v":1,"pid":21884,"uptime_ms":412903,
  "idle_ms":1203,"idle_exit_ms":300000,
  "peers":[{"peer_id":"a-3","role":"agent","name":"claude-code"}],
  "backends":[{"source":"dom-ext","handles":["browser"],"last_seen_ms":90,
               "instance":{"token":"9f2c…","brand":"brave","pid":21884}},
              {"source":"dom-ext","handles":["browser"],"last_seen_ms":12,
               "instance":{"token":"41ab…","brand":"vivaldi","pid":9120}}],
  "lease":{"scope":"input","holder":"a-3","expires_in_ms":11300},
  "kill_engaged":[],
  "decider":{"provider":"jev","model":"jev-latest","key_present":true},
  "counters":{"act":42,"executed":31,"no_effect":2,"refused":5,
              "needs_confirmation":3,"blocked":1,"killed":0},
  "journal":"C:\Users\you\AppData\Local\kratos\actions.jsonl",
  "config":"C:\Users\you\AppData\Local\kratos\config.json"
}}
```

`config` and `journal` are the files **this** broker read and is writing. They
are reported because a broker started by hand, from a shell with
`KRATOS_CONFIG` set, is running a different policy than the settings window is
editing -- and the first symptom of that is a key that appears to have
vanished.

### 4.7a What an act result now carries

Three fields added after the first live sessions, all optional and all
additive:

| field | on | meaning |
|---|---|---|
| `refusal` | `refused` | `ambiguous`, `unsure`, or `no_such_action` |
| `method` | anything dispatched | `semantic` or `physical` |
| `effect.settle_ms` / `effect.settled` | anything dispatched | how long the broker waited for the screen to move, and whether it ended early |

**`refusal` splits two failures that used to look alike.** A low confidence can
mean *nothing here looks right* or *two of these look identical*, and the fixes
are opposite: re-describe the step, or name the container. When the top two
options score within 0.10 of each other the decision carries
`ambiguous_with`, listing what it was torn between, and the refusal says
`ambiguous`.

**`method` makes a downgrade visible.** `semantic` means the control's own
interface carried the action -- a UI Automation pattern, a DOM event.
`physical` means the mouse or keyboard did it at a position, because no
pattern was available. That is allowed, since most of a Windows desktop
exposes nothing to invoke, but a position is a guess about where a control was
a moment ago, and an action that silently became one is not the action that
was asked for. Both are recorded in the journal.

**`settle_ms` is earned, not fixed.** The broker used to sleep 350ms after
every action. It now polls the target's pixels against the drift measured
moments before and returns as soon as they move, waiting out
`settle_max_ms` (default 1200) only when nothing does -- which is the case
that has to be waited out before `no_effect` can honestly be claimed.
`settled: false` beside `executed` means the pixels never moved and the
structure or a read-back carried the proof.

### 4.7b `whoami`

```json
{"op":"whoami","payload":{}}
```
```json
{"ok":true,"payload":{
  "peer_id":"a-3","role":"agent","name":"claude-code",
  "harness":"claude-code","agent":"agent-a","session":"s-21884-1762502400123",
  "approved":true,
  "lane":{"browser":"brave","profile_dir":"Default","profile_name":"Work",
          "assigned":{"harness":"claude-code","agent":"agent-a"},
          "state":"ready"}
}}
```

Where this session is, who it is accounted as, and whether the user has allowed
it. Allowed before approval, along with `status`, because a client that cannot
ask why it is being refused leaves its operator with nothing to go on.

The lane is **re-resolved** on every call rather than repeated from the
handshake: the user may have allocated a different browser since, and the answer
to "where am I" should be the current one. The session's stored lane is updated
to match, so the next action uses what was just reported.

### 4.8 `log.tail`

```json
{"op":"log.tail","payload":{"n":100,"since_ms":null}}
```
Returns `{"lines":[...]}` — journal records (section 8), newest last.

### 4.9 `config.get` and `config.set` — `ui` role only

`config.get` returns `{"config":{...}}` (section 9) with the API key replaced
by `"key_present":true|false`. The key is never read back over the wire, by any
role.

`config.set` takes `{"patch":{...}}`, a shallow merge, and returns the merged
config plus `{"restart_required":false}`. Changes to `idle_exit_ms`, gate
rules, kill scopes, decider model and API key take effect immediately. Changing
`pipe_name` requires a restart and is reported as such.

An edit made to the file by something other than the settings window is picked
up within one idle tick (five seconds). A file that will not parse is reported
once and then left alone until it changes again — it is not retried every tick,
and it never replaces the running policy with defaults.

### 4.10 `lane.list`, `harness.list`, `agent.list`, `instance.bind` — `ui` only

All four exist so the settings window can allocate browsers and profiles
without reading the machine itself. A second reader of `Local State` in the UI
process would be a second thing to keep in step, and it would be reading
whatever machine the window happens to run on rather than the broker's.

`lane.list` returns `{"sessions":[...], "browsers":[...], "os_default":"brave",
"instances":[...]}`:

* `sessions` — every connected agent, with the lane it resolves to now.
* `browsers` — per brand: `installed`, `exe`, `user_data`, `last_used`, and
  `profiles[{dir,name}]` read from that brand's `Local State`. This is what
  fills the profile dropdown.
* `instances` — attached backend instances and what each is paired with.

`harness.list` returns the configured harnesses with `approved`, the browser
each is allocated, whether it is connected, and `session_approval` — what was
decided since this broker started, which is how "allow just for now" is
visible at all.

`agent.list` returns the configured agents and, additionally, any agent name
that has connected but is not in the file yet: the user cannot allocate a
profile to a name they cannot see.

`instance.bind` takes `{"token":"…","brand":"brave","profile_dir":"Default"}`
and pairs an attached extension instance with the profile it is running in.
The extension cannot report this itself — no extension API gives a profile
directory name — so the pairing is a human's or a launch's. Sending neither
`brand` nor `profile_dir` **unpairs**, which is how a wrong pairing is undone
without hand-editing the file.

---

## 5. Broker semantics

### 5.1 Backend routing

Per `observe` and `act`, the broker classifies the target window into
`target_class`:

* `browser` — process image is a known Chromium brand **and** the window is not
  the automation profile
* `browser-automation` — Chromium started by this toolchain with a dedicated
  user-data-dir
* `native` — anything else

Preference order, first connected backend that `handles` the class:

| class | order |
|---|---|
| `browser` | `dom-ext` then `uia` then `pixel` |
| `browser-automation` | `dom-ext` then `uia` |
| `native` | `uia` then `pixel` then `ocr` |

`dom-ext` is first for browsers because it is the only backend that reports
`container` and `selector`, which is what distinguishes two identically
labelled fields. `prefer` in the request overrides this; an unavailable
`prefer` is `no_backend`, never a silent downgrade.

**Fallback, and why it is asymmetric.** For `observe` — read-only — the next
backend is tried on *any* failure, timeout included: looking twice costs
nothing.

For `act` it is narrower. The next backend is tried when the current one is
absent, replies `backend_error`, replies `no_target`, or replies `ok:true` with
`applied:false` — the last meaning the page received a synthetic event and
ignored it, so nothing happened and trying OS-level input is safe.

A **timeout on a mutating action is not retried.** The action may already have
landed, and clicking Send twice is worse than reporting that we do not know. A
`backend_timeout` is returned to the agent, whose `next` says to re-observe.
The same asymmetry applies in the MCP shim: a dropped connection is
transparently reconnected for a read, never for an `act` or a `confirm`.

A fallback that did happen is recorded in `backend` and in the journal as
`"dom-ext>uia"`, so a site that silently refuses synthetic events is visible in
the log rather than inferred.

### 5.2 Decision

The broker owns the model. No other component holds the API key and no backend
talks to it. Per `act`, exactly one call carrying two typed questions:

* `kind` — one of `click` `type` `key` `scroll` `select` `navigate` `wait`
  `open` `focus` `none`
* `target` — one of the observed elements, presented as
  `<role> "<label>" in <container>`

Both come back as a label plus a distribution plus a calibrated confidence. The
threshold is applied to the lower of `kind_confidence` and `target_confidence`
against a per-kind bar: the weakest link decides, because a certain action
aimed at the wrong element is the failure that matters.

Element lists longer than the provider's cardinality cap are compressed with
one slot reserved for an explicit "none of these" escape, so a cap overflow
surfaces as low confidence rather than as a forced wrong pick.

`DecisionProvider` is a trait. `jev` ships. Any provider that returns a label
**with a distribution and a calibrated confidence** over a fixed criteria set
can replace it; one that returns only a label cannot, because the thresholds
would become decoration.

### 5.3 Gate

Order, short-circuiting: kill, then rate limit, then block, then review, then
allow.

Matching is against the instruction, the window title and the URL. For a typing
instruction the quoted payload is **excluded** from review-keyword matching —
otherwise the subject line `type "Order-taking agents" ...` trips the `order`
rule and no ordinary email can be written. Block keywords still see the raw
string, because typing `rm -rf /` is as destructive as clicking it.

A review rule may be listed in `require_os_dialog_for`. Then `confirm` is not
sufficient: the broker shows a native dialog in its own process, which no agent
can click, and the action proceeds only on a real click.

### 5.4 Effect verification — success is earned, not assumed

A mutating `kind` (`click` `type` `select` `key` `scroll` `navigate` `open`
`focus`) returns `executed` only if at least one of these says so:

| evidence | what it means |
|---|---|
| structural | any element added, removed or enabled-state changed, or focus moved |
| `readback` | the target's value now contains the typed text — authoritative when present |
| `window_changed` | coarse delta over the **target window's** rectangle, 64x36 greyscale, per-cell tolerance 8 |
| `region_changed` | fine delta over the target **element's** rectangle, cropped to 96x96, same tolerance |

Otherwise `no_effect`. `null` for a pixel field means "could not look" and is
**not** evidence that nothing happened — a failed screen grab never converts
into a verdict.

**Pixel evidence has three outcomes, not two.** Each pixel measure is paired
with a baseline: the same rectangle grabbed twice immediately before acting, so
ambient motion is measured rather than assumed. The verdict is then

* `changed` — delta >= max(floor, 2 x baseline + 0.001)
* `unchanged` — below that, with the baseline quiet enough to be sure
* `inconclusive` — no capture, or baseline > 0.05

Floors: 0.004 for a window rectangle, 0.0015 for an element region.

This is not defensive decoration. Measured on this machine with an ordinary
desktop — one terminal streaming build output — two consecutive **full-screen**
grabs differed by 0.067, and a pair taken moments apart by 0.195. A real Gmail
navigation measured 0.197. A fixed full-screen threshold would therefore have
reported a click that missed entirely as `executed`, which is the exact
false-success this layer exists to prevent. Hence two changes: the coarse
fingerprint is taken over the **target window**, so unrelated motion is out of
frame, and a frame that is itself busy returns `inconclusive`.

`inconclusive` never counts as success. When every source is `inconclusive` or
`unchanged` the status is `no_effect` with `evidence:"inconclusive"`, and `next`
directs the agent to `verify` semantically rather than blindly retry. A false
`no_effect` costs a retry; a false `executed` costs the agent proceeding on a
lie, so the asymmetry is resolved deliberately in that direction.

The region fingerprint exists for the single-character-into-canvas case: one
glyph is roughly 20 cells of a 96x96 crop (0.002) but under 2 cells of a 64x36
full screen (0.0005), so a coarse test alone reports a false `no_effect` on
every keystroke into a canvas editor. Both are computed; either suffices.

### 5.5 Lifecycle — lazy start, idle stop

The broker is **not** autostarted; nothing is registered at logon.

`kratos-mcp`, on its first `desktop_*` tool call:

1. connect to the pipe;
2. on `ERROR_FILE_NOT_FOUND`, spawn `kratos-broker.exe` with
   `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, null stdio and no
   inherited handles, so the broker outlives the agent and no console
   window appears (a detached console application inherits no console and
   allocates none; `CREATE_NO_WINDOW` is deliberately not combined with
   `DETACHED_PROCESS`, which Windows documents as conflicting);
3. poll the pipe with backoff 25ms to 400ms, a 3s budget;
4. retry once end to end if the pipe appears and then vanishes.

A single named mutex `Local\kratos.v1.<sid>` makes the spawn race
harmless: exactly one broker binds, and a loser exits immediately. The
namespace is `Local\`, not `Global\`: creating a global object needs
SeCreateGlobalPrivilege, which a standard user does not have, and the broker
and the shims that spawn it always share one session. `first_pipe_instance`
on the listener is a second, independent guard behind it.

The broker exits when `now - last_agent_request > idle_exit_ms` (default
300,000) **and** no `ui` peer is connected. Backends do not keep it alive —
they are expected to reconnect, which is what 5.5 requires of them. Idle is
measured from agent requests only, so a UI polling `status` cannot pin it up.

`kratos-host` is started by the browser, not by us, and must idle at
near-zero cost: it blocks on stdin and on a reconnect timer, holds no spin
loop, and backs off 1s to 30s with jitter while the pipe is absent.

---

## 6. Backend operations — broker to backend

Same envelope, reversed direction. The broker is the requester.

### 6.1 `backend.observe`
```json
{"v":1,"t":"req","id":"b-91","op":"backend.observe",
 "payload":{"window":null,"class":"browser","timeout_ms":8000}}
```

The reply carries `elements`, `window`, `url` and `truncated` as in 4.1 but
**without** `gen` — the broker stamps that. A backend that cannot see the
requested window replies `ok:false` with `no_target`, which is routing
information, not an error to retry.

### 6.2 `backend.act`
```json
{"v":1,"t":"req","id":"b-92","op":"backend.act","payload":{
  "kind":"type",
  "target":{"id":"brave:w3:t17:e42","selector":"...","bbox":[412,268,733,22],"label":"To recipients"},
  "text":"someone@example.com",
  "key":null,"url":null,"amount":null
}}
```
```json
{"ok":true,"payload":{"applied":true,"readback":"someone@example.com","took_ms":34}}
```

* **`target` may be `null`.** `key` and `scroll` carry no element: a keypress
  goes to whatever has focus, and an unqualified scroll moves the page. A
  backend that refuses a null target breaks `press Enter` entirely, which is
  exactly what both of them did until a live run caught it.
* The broker sends `id`, `selector` **and** `bbox`; the backend uses whichever
  it is authoritative for. `dom-ext` **must** use `selector` or `id` and must
  never act on `bbox`: filling by position is the defect this protocol exists
  to remove. `uia` and `pixel` act on `bbox`.
* `applied:false` with `ok:true` means dispatched and not taken — a site
  ignoring synthetic events. The broker falls back (5.1). Reporting it as an
  error would hide it behind a retry.
* `readback` is the control's value after the action, or `null` if unreadable.
  `null` is not a failure.

### 6.3 Chunking

A `dom-ext` reply over 768 KiB is split:
`{"chunk":{"i":0,"n":3,"id":"b-91"},"part":"<fragment>"}`. The native host
reassembles in order and emits one pipe line. A missing chunk within 4s is
`backend_error`.

---

## 7. Error codes — closed set

Every error carries **`retry_safe`**. The question an agent has after a failure
is not what went wrong but *may I send that again*, and for anything that moves
a mouse the answer is sometimes no: a backend that stopped answering
mid-dispatch may already have delivered the input. Anything the broker turned
away never reached a screen and is safe to repeat. The field defaults to `true`
when absent, so an older peer is not read as forbidding retries, and the broker
can lower it per-case with more knowledge than the code alone carries.

The `retry?` column below is advice about whether repeating helps; `retry_safe`
is the narrower question of whether repeating is *safe*.

| code | meaning | retry? |
|---|---|---|
| `bad_version` | no overlapping protocol major | no |
| `bad_request` | malformed, unknown op, or wrong role | no |
| `unauthorized` | op not permitted for this role, including agent `kill.reset` | no |
| `too_large` | line over 8 MiB; connection closed | no |
| `no_backend` | nothing registered that handles this class, or `prefer` absent | after a backend connects |
| `no_target` | backend cannot see the requested window | no |
| `ambiguous_window` | `window` matched windows of more than one application | yes, with a more specific string |
| `backend_error` | backend failed | broker falls back |
| `backend_timeout` | backend silent past `timeout_ms` | broker falls back |
| `stale_id` | element id from a superseded `gen` | re-observe |
| `stale_confirm` | foreground window changed since parking | re-issue `act` |
| `gate_blocked` | denylist | no |
| `kill_switch` | an engaged scope covers this target | after reset |
| `rate_limited` | over `rate_limit_per_minute` | after backoff |
| `lease_held` | another peer holds input | after `expires_in_ms` |
| `lease_invalid` | token unknown or expired | re-acquire |
| `decider_error` | provider unreachable or malformed reply | yes |
| `decider_unconfigured` | no API key | after config |
| `lane_unavailable` | this session's browser and profile could not be reached or opened | after the lane is open |
| `not_approved` | the user has not allowed this harness to drive the machine | after a human approves it |
| `internal` | bug | no |

`refused`, `no_effect` and `needs_confirmation` are **not** errors. They are
`ok:true` with a `status`: the action was handled correctly and the outcome is
informative. Only transport and policy failures are `ok:false`.

---

## 8. Journal

One JSONL line per gate decision and per action, appended to
`%LOCALAPPDATA%\kratos\actions.jsonl`, rotated at 16 MiB keeping 5.
Never truncated in place, so a reader may tail it safely.

```json
{"ts":"2026-11-04T09:21:44.182Z","peer_id":"a-3","agent":"claude-code",
 "op":"act","instruction":"click the \"Send\" button",
 "window":"Inbox - Gmail - Brave","url":"https://mail.google.com/...",
 "target_class":"browser","backend":"dom-ext","gen":17,
 "kind":"click","target_id":"brave:w3:t17:e42","target_label":"Send",
 "confidence":0.98,"kind_confidence":0.99,"target_confidence":0.98,"threshold":0.70,
 "decider":"jev/jev-latest","tokens_in":1841,"decide_ms":138,
 "gate":"review_keyword:\"send\"","gate_category":"review","confirmed_by":"ui-dialog",
 "lease":"l-44","status":"executed",
 "structural":{"added":0,"removed":31,"changed":2,"focus_changed":true},
 "screen_changed":0.1973,"region_changed":null,"evidence":"structural+pixel",
 "took_ms":1402,"schema":1}
```

Typed text is recorded. When the target control is a password field, or a gate
rule marks the action sensitive, the value is written `"[REDACTED]"` — the
field is always present, so its absence is never ambiguous.

---

## 9. Configuration

`%LOCALAPPDATA%\kratos\config.json`, hot-reloaded on change. The
settings UI is the intended editor; the file is the source of truth.

```json
{
  "schema": 1,
  "pipe_name": "kratos.v1",
  "idle_exit_ms": 300000,
  "decider": {
    "provider": "jev",
    "model": "jev-latest",
    "endpoint": "https://api.typesafe.ai/v1/systemone",
    "api_key_enc": "<DPAPI blob, base64>",
    "timeout_ms": 8000
  },
  "thresholds": {"click":0.70,"type":0.75,"key":0.65,"scroll":0.55,"select":0.70,
                 "navigate":0.75,"open":0.70,"focus":0.60,"default":0.70},
  "kill": {"hotkey":"ctrl+alt+k","hotkey_scope":"global",
           "per_browser":{"brave":true,"chrome":true,"edge":true}},
  "gate": {"block_keywords":[],"review_keywords":[],"review_apps":[],
           "review_domains":[],"deny_domains":[],"deny_apps":[],
           "allow_recipients":[],"rate_limit_per_minute":30,
           "require_os_dialog_for":["review_domain"]},
  "backends": {"enabled":["dom-ext","uia","pixel","ocr"]},
  "log": {"path":null,"max_bytes":16777216,"keep":5},

  "harnesses": [{"id":"claude-code","label":"","browser":"brave","approved":true}],
  "agents": [{"id":"agent-a","label":"","profile_dir":"Profile 2"}],
  "instances": [{"token":"9f2c…","brand":"brave","profile_dir":"Default"}],
  "lanes": [{"browser":"brave","profile_dir":"Default","shared":false}],
  "defaults": {"browser":"os-default","profile":"last-used","open_method":"direct",
               "pause_on_user_input":true,"user_idle_ms":1500,"pause_max_ms":30000}
}
```

**`harnesses`, `agents` and `instances` are the allocation, and the settings
window is the only thing that writes them.** An `agent` peer cannot:
`config.set` is a `ui` op, and so are `instance.bind` and every listing.

* `harnesses[].id` is an MCP client's `clientInfo.name`. `browser` empty means
  `defaults.browser`. `approved` defaults to **false**, because the only way a
  harness gets into this file is by connecting, and a client that has just
  introduced itself has not been approved by anybody.
* `agents[].id` is the name in that client's snippet (`--agent`). `profile_dir`
  empty means `defaults.profile`.
* `instances[]` pairs an extension instance token with the profile it runs in.
  Written by `instance.bind`.
* `lanes[]` holds what the user has said about one `(browser, profile_dir)`
  pair. Today that is `shared` (4.2b). The design document names no field for
  it and §5 requires one, so it is keyed on what a lane *is*.
* **`defaults.browser: "none"`** turns lanes off for every harness that has no
  browser of its own, and `harnesses[].browser: "none"` does it for one. Those
  sessions get no lane, and `window: null` means the foreground window as it
  did before lanes existed. It is the deliberate choice for a single-agent
  desktop — one machine, driven as one machine — and the only way back to that
  behaviour. The default is `os-default`, which gives a lane.
* `defaults.open_method`, `pause_on_user_input`, `user_idle_ms` and
  `pause_max_ms` are read by the ensure-lane ladder and the user-activity
  pause.

The API key is encrypted at rest with Windows DPAPI scoped to the current user,
so the config file is not a plaintext credential and a copy of it is useless on
another machine or under another account. `api_key_enc` is never returned by
`config.get` and never written to the journal.

`kill.per_browser` sets which brands the kill switch may be scoped to in the
UI; it does not itself engage anything.

---

## 10. Compatibility rules

1. `v` increments only on a breaking change. v1 and v2 brokers may run side by
   side on differently-named pipes.
2. New ops are additive. A peer must treat an unknown `op` on a request as
   `bad_request` and an unknown `op` on an **event** as ignorable.
3. New fields are optional with a documented default. A peer must not fail on
   an unknown field.
4. `error.code`, `status` and `role` values are closed sets; adding a member is
   a minor version change, and every consumer must have a default branch.
5. Field *meanings* never change in place. `screen_changed` will always be a
   fraction in which `null` means "could not look".
