# P0 — audit and reproduce

Phase P0 of [design-multi-harness.md](design-multi-harness.md): measure what
this build actually does against the spec, reproduce the four named defects,
**fix nothing**.

Nothing in `crates/`, `extension/`, `backends/` or `docs/protocol.md` was
changed in this phase. The only code added is one test file that fails on
purpose.

---

## The four reproductions

`crates/kratos-broker/tests/multi_harness_audit.rs`. Each test asserts the
behaviour the spec requires, collects **every** gap it finds rather than
stopping at the first, and fails. They are `#[ignore]`d so the suite stays
green; the phase that fixes a defect removes its `#[ignore]`.

```bash
cargo test -p kratos-broker --test multi_harness_audit -- --ignored --test-threads=1
```

| spec item | test | fixed in |
|---|---|---|
| (a) two dom-ext instances, the older gets kicked | `two_browser_instances_both_stay_registered` | P1 |
| (b) `observe window:null` reads another agent's window | `a_session_observes_its_own_lane_not_another_agents_window` | P2 + P3 |
| (c) env/pipe mismatch gives `no_backend` | `a_harness_and_its_browser_cannot_be_split_across_two_brokers` | P1 + P3 |
| (d) two brokers hold two leases | `only_one_input_lease_exists_for_one_os_user` | P3 |

Measured output, run on this machine:

```text
running 4 tests
test a_harness_and_its_browser_cannot_be_split_across_two_brokers ... FAILED
test a_session_observes_its_own_lane_not_another_agents_window ... FAILED
test only_one_input_lease_exists_for_one_os_user ... FAILED
test two_browser_instances_both_stay_registered ... FAILED

---- two_browser_instances_both_stay_registered stdout ----
two dom-ext instances: 2 gap(s)
  1. status lists 1 dom-ext backend(s); two attached instances must both be
     registered under (source, instance.token)
  2. the first instance was sent `evt superseded` by a different instance;
     only the same (source, token) may supersede

---- a_session_observes_its_own_lane_not_another_agents_window stdout ----
lane-scoped targeting: 7 gap(s)
  1. the hello reply to claude/agent-a carries no `lane` object
  2. the hello reply to claude/agent-a carries no `approved` flag
  3. the hello reply to bluee/agent-b carries no `lane` object
  4. the hello reply to bluee/agent-b carries no `approved` flag
  5. whoami is not implemented: bad_request unknown op "whoami"
  6. the Brave lane's own session could not observe its lane: no_backend (no
     backend is connected that handles a native window) -- window:null resolved
     to the OS foreground, classified "native", so the session's own lane was
     never consulted
  7. cross-lane observe failed with no_backend (...), expected no_target or
     lane_unavailable -- window:null resolved to the OS foreground, classified
     "native", so the session's own lane was never consulted

---- a_harness_and_its_browser_cannot_be_split_across_two_brokers stdout ----
one broker per OS user: 3 gap(s)
  1. a second broker for the same OS user serves agents with no sight of the
     instance attached to the first; one broker per OS user must make this
     impossible
  2. status reports no `lanes`, so a split like this is not diagnosable
  3. an unreachable browser lane reports no_backend (...); the spec's code for
     it is lane_unavailable, and no_backend names the wrong cause -- window:null
     resolved to the OS foreground, classified "native", so the session's own
     lane was never consulted

---- only_one_input_lease_exists_for_one_os_user stdout ----
one machine-wide input lease: 1 gap(s)
  1. two brokers for one OS user each granted the input lease (l-1 and l-1);
     there is one mouse, so there must be one lease

test result: FAILED. 0 passed; 4 failed
```

Two of these turned out to have a cause worth writing down separately from the
symptom:

* **(b) and (c) both failed on the foreground, not on the backend.**
  `window:null` is resolved by `GetForegroundWindow`, the window is classified,
  and routing is chosen from *that* class. In a test run the foreground is a
  terminal, so the target is `native`, and an attached browser instance is
  never consulted at all — the reply is `no_backend` for a class the agent
  never asked about. The error names the wrong thing twice over.
* **(d) handed out token `l-1` twice.** Two brokers, two independent lease
  counters, one physical mouse.

---

## Audit table

What the spec requires, what this build does, and where to look. "Repro" names
the P0 test that demonstrates it; blank means it is read off the code and not
yet exercised by a test.

### Identity and resolution (spec §2, §3)

| # | spec requires | today | evidence | repro | phase |
|---|---|---|---|---|---|
| 1 | harness identified from MCP `clientInfo.name` | `initialize` discards `params`; the harness name is `KRATOS_AGENT_NAME` or the literal `mcp-client` | `crates/kratos-mcp/src/main.rs:52`, `:110` | | P2 |
| 2 | agent named by `--agent <name>` or `KRATOS_AGENT` | no argument parsing at all; only `KRATOS_AGENT_NAME`, which names the *harness* in the journal | `crates/kratos-mcp/src/main.rs:52` | | P2 |
| 3 | agent `hello` carries optional `harness`, `agent`, `session` | `Hello` has `role`, `name`, `build`, `v_min`, `v_max`, `source`, `handles` and nothing else; extra fields are ignored per compatibility rule 3 | `crates/kratos-protocol/src/lib.rs:360` | (b) | P2 |
| 4 | hello reply carries a `lane` object and an `approved` flag | `HelloAck` has `v`, `peer_id`, `broker`, `decider`, `kill_engaged`, `backends` | `crates/kratos-protocol/src/lib.rs:381` | (b) | P2 |
| 5 | `whoami` for agents; `lane.list`, `harness.list`, `agent.list`, `instance.bind` for the UI | none of the five exist; an unknown op is `bad_request` by design | `crates/kratos-protocol/src/lib.rs:232`, `crates/kratos-broker/src/peer.rs:224` | (b) | P2, P6 |
| 6 | a session re-resolves its lane on every connection, nothing persists | there is no lane and no session; a connection is a `peer_id` and a self-reported `name` | `crates/kratos-broker/src/state.rs:26` | | P2 |
| 7 | unknown harness is unassigned, and a native dialog asks allow once / always / deny before any op but `status`/`whoami` | any peer that completes the handshake may call every op its role allows; there is no approval concept | `crates/kratos-broker/src/peer.rs:191`, `crates/kratos-protocol/src/lib.rs:262` | (b) | P2 |
| 8 | `browser` = `harnesses[h].browser` else the OS default | nothing reads the OS default browser; nothing maps a harness to a brand | — | | P2, P5 |
| 9 | `profile` = `agents[a].profile_dir` else the brand's `Local State` `profile.last_used` | `Local State` is never read; the word "profile" appears only in launcher scripts | — | | P2, P5 |

### Backend registration and instances (spec §7, P1)

| # | spec requires | today | evidence | repro | phase |
|---|---|---|---|---|---|
| 10 | registry key is `(source, instance.token)`; only the same key supersedes | the key is `source` alone: a second `dom-ext` evicts the first and is sent `evt superseded` | `crates/kratos-broker/src/state.rs:254` | (a) | P1 |
| 11 | lookup resolves to the instance for a lane | `backend_for(source)` returns the first peer with that source — and there can only be one | `crates/kratos-broker/src/state.rs:279` | (a) | P1 |
| 12 | dom-ext `hello` carries optional `instance {token, brand, pid, parent_hwnd}` | the host sends `source:"dom-ext"` and fixed handles; `attach_backend` has no parameter for anything else | `crates/kratos-host/src/main.rs:25`, `crates/kratos-ipc/src/peer.rs:167` | (a) | P1 |
| 13 | the extension generates a random instance token per profile on first run | no token anywhere in `extension/`; identity is the brand string, detected from the user agent | `extension/background.js:25`, `:39` | | P1 |
| 14 | `instances[]` in config pairs a token with `(brand, profile_dir)` | no such config section | `crates/kratos-protocol/src/config.rs:15` | | P1, P2 |

### Targeting and concurrency (spec §5, P3)

| # | spec requires | today | evidence | repro | phase |
|---|---|---|---|---|---|
| 15 | `window:null` means the session's lane active tab, never the OS foreground | `resolve_window(None)` is `GetForegroundWindow()`, identical for every agent, and its class picks the backend | `crates/kratos-broker/src/classify.rs:256`, `crates/kratos-broker/src/act.rs:1028` | (b), (c) | P3 |
| 16 | windows of other lanes are invisible (`no_target`) | every agent on the broker sees whatever is in front, and one attached `dom-ext` answers for all of them | `crates/kratos-broker/src/act.rs:102` | (b) | P3 |
| 17 | tabs a session opened are owned by it | no ownership anywhere; the extension routes by brand, window and tab id with no notion of who asked | `extension/background.js:8` | | P3 |
| 18 | one machine-wide input lease, one broker | the lease is a field of `Broker`, and the single-instance mutex is keyed to the **pipe name**, so a second pipe is a second broker with a second lease | `crates/kratos-broker/src/state.rs:178`, `crates/kratos-ipc/src/lib.rs:28`, `crates/kratos-broker/src/main.rs:85` | (d) | P3 |
| 19 | DOM actions need no lease and run in parallel across lanes | an `act` auto-acquires the input lease whatever the backend, so two browser sessions serialise behind one another even though neither touches the mouse | `crates/kratos-broker/src/act.rs:859` | | P3 |
| 20 | `KRATOS_PIPE` is a test override only; no env-var pairing | the pipe name is the pairing mechanism, read from `KRATOS_PIPE` by the broker, the shim, the host and the UI alike, and documented as the way to run a broker per agent | `crates/kratos-broker/src/main.rs:85`, `crates/kratos-mcp/src/main.rs:51`, `crates/kratos-host/src/main.rs:54`, `README.md` "Many agents, one machine", `docs/workflow.md` §4 | (c), (d) | P3, P7 |
| 21 | — (found while auditing) | one `last_observation` and one `generation` counter are shared by every session; the journal's password-redaction looks an element id up in whichever observation landed last, so a concurrent session's observation can make a sensitive field read as not sensitive | `crates/kratos-broker/src/state.rs:181`, `crates/kratos-broker/src/act.rs:298`, `:674`, `:1230` | | P3 |

### Ensure lane (spec §4, P5)

| # | spec requires | today | evidence | repro | phase |
|---|---|---|---|---|---|
| 22 | launch the browser with `--profile-directory=<dir>` and wait up to 8s for an instance | nothing launches a browser. `register_automation_pid` exists, is `#[allow(dead_code)]`, and says so in its own comment | `crates/kratos-broker/src/classify.rs:102` | | P5 |
| 23 | `open_method=ui`: Jev-driven launch and profile pick, max 10 steps | no ladder, no step budget, no `open_method` | — | | P5 |
| 24 | recovery from a profile picker / restore bubble / sign-in prompt | none | — | | P5 |
| 25 | `state=degraded` when no extension ever attaches, and say so in the result | a browser window with no `dom-ext` silently falls through to `uia`; the fallback is recorded as `"dom-ext>uia"` but there is no lane state to report | `crates/kratos-broker/src/act.rs:102` | | P5 |
| 26 | `lane_unavailable` and `not_approved` error codes; `paused` status | the code set and the status set are closed and contain neither | `crates/kratos-protocol/src/lib.rs:196`, `:221` | (c) | P2, P3 |

### User-activity pause (spec §6, P3)

| # | spec requires | today | evidence | repro | phase |
|---|---|---|---|---|---|
| 27 | low-level mouse and keyboard hooks that ignore injected events | no hook of any kind. The only Win32 input code is the kill-switch hotkey, which is `RegisterHotKey`, not a hook | `crates/kratos-broker/src/hotkey.rs` | | P3 |
| 28 | wait up to `pause_max_ms` then return `paused` with a resume hint | no pause path; an action runs whatever the user is doing | — | | P3 |
| 29 | `pause_on_user_input`, `user_idle_ms`, `pause_max_ms` in config and shown in `status` | absent from both | `crates/kratos-protocol/src/config.rs:15`, `crates/kratos-broker/src/state.rs:504` | | P3, P6 |

### Config and UI (spec §8, §9)

| # | spec requires | today | evidence | repro | phase |
|---|---|---|---|---|---|
| 30 | `harnesses[]`, `agents[]`, `instances[]`, `defaults` | `Config` is `schema`, `pipe_name`, `idle_exit_ms`, `settle_max_ms`, `decider`, `thresholds`, `kill`, `gate`, `backends`, `log` | `crates/kratos-protocol/src/config.rs:15` | | P2 |
| 31 | UI forms for harnesses, agents, profiles, Connect, behaviour, lanes | the settings window has Decision model, Confidence, Gate, Browser (extension id), Status and Log. No allocation UI | `crates/kratos-ui/src/main.rs` | | P6 |
| 32 | a `kratos doctor` command | nothing by that name; the Browser tab's Register button reports registry writes, which is the closest thing | `crates/kratos-ui/src/main.rs` | | P6 |
| 33 | config is hot-reloaded on change | `config.set` applies in memory and saves; there is no file watcher, so an external edit is not picked up | `crates/kratos-broker/src/state.rs:473` | | P2 |

### Native backend (spec §10, P4)

| # | spec requires | today | evidence | repro | phase |
|---|---|---|---|---|---|
| 34 | a `kratos-native` binary, a backend peer on the pipe, spawned and supervised by the broker | no such crate; the ten crates are protocol, ipc, gate, decide, shot, broker, mcp, host, ui, bench | `Cargo.toml` | | P4 |
| 35 | UIA in Rust, one cached subtree fetch per window | UIA is Python, in `backends/perception/`, and must be started by hand (`start-backend.cmd`) | `backends/`, `CLAUDE.md` state note 5 | | P4 |
| 36 | `Windows.Media.Ocr` by default; Tesseract removed after parity | OCR is Tesseract, in Python | `backends/perception/` | | P4 |
| 37 | OmniParser v2 detector via `ort`, behind a config switch, weights not committed | no detector, no ONNX, no `ort` dependency | `Cargo.lock` | | P4 |
| 38 | fusion with per-element `source`(s) and confidence, IoU > 0.5 merge | `Element` has neither field: one `source` string describes the whole observation, so there is nothing to fuse into. Backends are tried in order and the first answer wins | `crates/kratos-protocol/src/lib.rs:449`, `crates/kratos-broker/src/act.rs:102` | | P4 |
| 39 | capture via Windows Graphics Capture or DXGI, cropped, frame-hash cached | `kratos-shot` grabs the virtual desktop with BitBlt and crops; no per-window capture, no frame cache | `crates/kratos-shot/src/lib.rs` | | P4 |

### Documentation (spec §7, P7)

| # | spec requires | today | evidence | repro | phase |
|---|---|---|---|---|---|
| 40 | `docs/protocol.md` status line becomes "evolving, additive-only until first working release" | reads **Status: LOCKED** | `docs/protocol.md:3` | | P2 |
| 41 | the broker-per-agent model and env-var pairing removed from README and docs | both are documented as the supported way to run several agents | `README.md` "Many agents, one machine", `docs/workflow.md` §4, §5 | | P7 |

---

## What this build already does that the spec keeps

Worth recording so a later phase does not "fix" it:

* The handshake, the role table, the gate order, the kill scopes, the evidence
  rules and the journal are all as documented, and 295 tests cover them.
* `Source::parse` already knows `dom-ext`, `uia`, `ocr`, `pixel`, so adding a
  native backend needs no new source names.
* Backend routing is per window class with an explicit `prefer` that is
  honoured or refused, never downgraded — which is the shape lane routing needs.
* `choose()` already refuses a `window` string that matches two applications
  rather than guessing, which is the same rule lanes will need for two browsers.
* A fallback chain is recorded (`"dom-ext>uia"`), so a degraded lane has
  somewhere to be reported from.

## Open questions for later phases

1. **How is the machine-wide lease named?** (d) asserts one lease per OS user.
   The integration suite deliberately runs several brokers at once, isolated by
   `KRATOS_PIPE`, so a lease guard keyed to the SID alone would make the test
   suite contend with itself, and one keyed to the pipe name would not satisfy
   (d). P3 has to choose, and say which, in a comment at the guard.
2. **What happens to `start-browser.cmd`, `start-agent-browser.cmd` and
   `start-daemon.cmd kratos.agent-a`?** They exist to pair by pipe, which §5
   removes. P7 owns it; P3 should not delete them silently.
3. **An act that fails before the decide stage is still not journalled**
   (`CLAUDE.md`, known gap 2). Lane resolution and harness approval both fail
   *before* decide, so P2 and P3 add two more unjournalled refusal paths unless
   that gap is closed first.
