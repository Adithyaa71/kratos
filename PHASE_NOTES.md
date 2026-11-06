# PHASE_NOTES

Working notes per phase of [docs/design-multi-harness.md](docs/design-multi-harness.md).
Newest phase first.

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
