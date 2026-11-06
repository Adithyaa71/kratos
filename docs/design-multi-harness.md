=== SPEC ===

1. GOAL
- Connecting a harness = paste one snippet. No required config.
- Many harnesses and agents run at once, in parallel, without stepping on each other.
- Harness -> browser. Agent -> user profile. Unconfigured -> default browser, default profile.
- If the needed browser/profile isn't open, Kratos opens it, with Jev deciding each step.
- Native paths (UIA, OCR, pixel) are fast and accurate in Rust, independent of the browser extension.

2. ROLES AND RULES
- User: the only party who allocates browser per harness and profile per agent, approves new harnesses, edits gate rules, clears the kill switch, sets pause behaviour. Does all of it in the Kratos UI forms.
- Agent: MCP tools only. Can observe, act, verify, confirm, whoami, kill.engage. Cannot choose or change browser/profile, clear kill, approve itself, or edit config. Its only setting is an optional name (--agent <name> or KRATOS_AGENT) in its MCP snippet.
- Harness: the MCP client, identified automatically from MCP clientInfo.name. No setup.
- Jev (DecisionProvider): decides action kind and target element only. Never generates literals. Stands between every instruction and every execution, including the flows that open a browser or profile. Low confidence = refuse.
- Broker: ONE per OS user. Owns gate, kill scopes, journal, lease, routing, session/lane resolution, backend supervision.
- Backends: dom-ext (one per browser profile instance) and kratos-native (Rust).

3. IDENTITY AND RESOLUTION
- Session = one shim process. Every new session re-resolves; nothing persists per session.
- browser = harnesses[harness].browser if set, else the OS default browser.
- profile = agents[agent].profile_dir if set and present in that browser, else that browser's last-used profile (read Local State profile.last_used). If the assigned profile is missing: use default and flag a warning in the UI.
- lane = (browser, profile). Returned in the hello reply and in whoami: {browser, profile_dir, profile_name, assigned:{harness,agent}, state: ready|launching|degraded|unavailable}.
- Unknown harness: appears in the UI as unassigned. Before any op except status/whoami, a native OS dialog asks allow once / always / deny. The agent cannot answer it.

4. ENSURE LANE
When an op needs a browser lane and no dom-ext instance is attached for (browser, profile):
1. Instance attached -> done.
2. open_method=direct (default): OS launch of the browser exe with --profile-directory=<dir>. This works whether or not the browser is already running (it opens a window for that profile in the running process). Wait up to 8s for an instance to attach. If exactly one unbound instance appears, bind it to (brand, profile_dir).
3. open_method=ui: drive it through the native backend with Jev: launch the app (search/taskbar), then pick the profile by name in the picker. Every step is a normal gated, logged, verified act. Max 10 steps, then lane_unavailable.
4. If a direct launch is blocked by an unexpected UI (profile picker, restore-pages bubble, sign-in or default-browser prompt), run the same Jev-driven recovery, bounded.
5. If no extension ever attaches: state=degraded. Drive that window with UIA/OCR and say so in the result.
Launching is an act: it respects the gate, kill switch, pause and journal.

5. TARGETING AND CONCURRENCY
- observe/act with window:null resolve to the session's lane active tab, never the OS foreground. Windows of other lanes are invisible (no_target).
- Tab ownership: tabs a session opened are owned by it. Acting on another session's tab is refused unless the user marks the lane shared.
- DOM actions need no input lease and run in parallel across lanes and sessions.
- Native/physical actions share one machine-wide input lease (one broker = one lease), FIFO.
- Remove the broker-per-agent model and the env-var pairing from README/docs. KRATOS_PIPE remains a test override only.

6. USER-ACTIVITY PAUSE
- Low-level mouse/keyboard hooks that IGNORE injected events (LLMHF_INJECTED / LLKHF_INJECTED), so the agent never pauses itself.
- If the user gave real input within user_idle_ms (default 1500), any action needing OS input waits up to pause_max_ms (default 30000), then returns status paused with a resume hint.
- Background-tab DOM actions are unaffected. Configurable in the UI. Shown in status.

7. PROTOCOL (additive only; wire stays v:1; invent no version numbers)
- Update docs/protocol.md in place. Change its "LOCKED" status line to: evolving, additive-only until first working release.
- agent hello: optional harness, agent, session.
- hello reply: lane object and approved flag.
- dom-ext backend hello: optional instance {token, brand, pid, parent_hwnd}. Registry key becomes (source, instance.token). Supersede only the same key. No instance = current behaviour.
- New ops: whoami (agent); lane.list, harness.list, agent.list, instance.bind (ui).
- New status: paused. New error codes: lane_unavailable, not_approved.
- Extension: generate a random instance token per profile on first run and send it in hello.

8. CONFIG (config.json, hot reload)
- harnesses[]: {id, label, browser, approved, gate_overrides}
- agents[]: {id, label, profile_dir, gate_overrides}
- instances[]: {token, brand, profile_dir}
- defaults: {browser:"os-default", profile:"last-used", open_method:"direct", pause_on_user_input:true, user_idle_ms:1500, pause_max_ms:30000}

9. UI (kratos-ui) - all allocation lives here and only here
- Harnesses: auto-listed when first seen; browser dropdown; approve/revoke; gate overrides.
- Agents: name; profile dropdown filled from each browser's Local State (info_cache dir -> name), plus "default".
- Profiles: detected instances, pair/unpair, extension status.
- Connect: generates a copy-paste MCP snippet per harness with an ABSOLUTE exe path and optional --agent.
- Behaviour: open method, pause settings.
- Status: lanes, sessions, pending approvals, kill switch.
Add a "kratos doctor" command: checks the pipe, backends, native-host registry per brand, extension pairing, absolute paths, decider key.

10. NATIVE BACKEND (Rust): new binary kratos-native
- A normal backend peer on the pipe (sources uia, ocr, pixel). The broker spawns and supervises it on demand: restart with backoff, exit with the broker. It replaces backends/*.py only after parity.
- UIA: windows crate. One cached subtree fetch per window (CacheRequest + FindAllBuildCache with only the needed properties; prune offscreen and non-interactive in the condition). Never read properties one by one across processes. COM on a dedicated MTA thread. Baseline to beat: 24 elements in 223ms, 168 in 2.1s.
- Capture: Windows Graphics Capture (per window) or DXGI duplication, cropped to the target window, frame-hash cache.
- OCR: Windows.Media.Ocr by default (offline, no install). Optional accuracy mode with PaddleOCR via ONNX. Remove Tesseract after parity.
- Pixel: OmniParser v2 icon_detect weights exported to ONNX and run with ort (GPU provider if present; report CPU numbers too). Detector only on the hot path. Caption unlabeled icons (Florence-2) only on demand, cached by crop hash. Weights are AGPL: keep the model behind a config switch, do not commit weights, document it.
- Fusion: UIA first. Add OCR text and detector boxes where UIA has none (IoU > 0.5 = same element; UIA wins label and role). Every element carries source(s) and confidence. Ids stable within a gen. If the tree is sparse (<=2 elements), run the pixel path automatically. Jev still gets the compressed candidate list.
- Actuation: semantic first (UIA patterns), physical second (SendInput; clipboard paste for unicode). Report method as today. Respect lease, pause and kill.
- Targets (prove with kratos-bench, do not claim): UIA observe ~150 elements p50 <= 80ms; OCR of a window region p50 <= 100ms; detector <= 60ms on GPU. Put real numbers, including CPU-only, in docs/benchmarks.md.
- Accuracy: build a fixture corpus (screenshots + UIA dumps + golden element lists: Notepad, Explorer, Settings, Excel, Gmail in Brave, one canvas app). Report recall, label exactness and bbox IoU per source and fused. A regression fails CI.

11. PHASES
- P0 Audit and reproduce, no fixes. Write failing tests for: (a) two dom-ext instances, the older gets kicked; (b) observe window:null reads another agent's window; (c) env/pipe mismatch gives no_backend; (d) two brokers hold two leases. Output an audit table.
- P1 Instance-keyed backend registration, extension token, host hello. Accept: two fake browser instances stay registered; the same instance reconnecting supersedes.
- P2 Identity, sessions, lanes, resolution, defaults, harness approval, config, whoami. Accept: tests for harness->browser, agent->profile, unassigned->default, missing profile warning, new session re-resolves, unapproved harness blocked.
- P3 Scoped targeting, tab ownership, single broker, machine-wide lease, user-activity pause. Accept: two fake agents on two lanes act in parallel; cross-lane observe gives no_target; one lease; paused on real input, not on injected input.
- P4 kratos-native. 4a UIA, 4b capture+OCR, 4c actuation, 4d pixel+fusion. Each ends with benchmark and fixtures before the next starts.
- P5 Ensure-lane ladder, Local State profile discovery, Jev-driven open/recovery. Accept: cold launch, profile opened inside an already-running browser, picker recovery, bounded abort.
- P6 UI forms, Connect page, kratos doctor.
- P7 Retire the Python backend after parity, update README/skills/docs, full matrix test (two fake agents x two fake browser instances x kill scopes x pause), final benchmarks.

12. WORKING RULES
- One phase per session. Start by re-reading the spec and listing the files you will touch. Touch nothing else. No drive-by refactors.
- Write tests first for each acceptance item. cargo test and clippy must be clean before you say done. Paste the output.
- Never say "done" or "works" without test or live evidence. State what you did not verify.
- If the spec and the code conflict, stop and ask. Do not improvise.
- End every phase with PHASE_NOTES.md (done / decisions / known gaps / next) and one commit.
- Safety is unchanged: no TCP; the gate, kill switch and evidence rules stay; agents never clear kill or approve themselves.

=== END SPEC ===
