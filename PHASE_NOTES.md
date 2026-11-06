# PHASE_NOTES

Working notes per phase of [docs/design-multi-harness.md](docs/design-multi-harness.md).
Newest phase first.

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
