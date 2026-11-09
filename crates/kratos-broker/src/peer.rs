//! One connection, from handshake to hangup.
//!
//! Each connection is its own tokio task with its own outbound queue. That is
//! what keeps peers independent: a panic in one agent's request handler aborts
//! that task and nothing else, and a peer that stops reading fills its own
//! channel rather than stalling the broker that other agents -- and the kill
//! switch -- depend on.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kratos_protocol::{codes, ops, Frame, Role, Source, WireError};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Mutex};

use crate::state::{Broker, PeerHandle, BROKER_NAME};

/// Outbound queue depth per peer. Deep enough for bursts of events, shallow
/// enough that a peer which has stopped reading is noticed rather than
/// buffered indefinitely.
const OUT_QUEUE: usize = 64;

pub async fn serve<S>(broker: Arc<Broker>, conn: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = kratos_ipc::split(conn);
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(OUT_QUEUE);

    let pump = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if writer.send(&frame).await.is_err() {
                break;
            }
        }
    });

    // -- handshake ----------------------------------------------------------
    let peer = match handshake(&broker, &mut reader, &out_tx).await {
        Some(p) => p,
        None => {
            // The refusal is already queued. Dropping the sender lets the pump
            // drain it and exit on its own; aborting here instead closes the
            // socket first, and the client sees a disconnect with no reason.
            drop(out_tx);
            let _ = tokio::time::timeout(Duration::from_millis(500), pump).await;
            return;
        }
    };
    let peer_id = peer.peer_id.clone();

    // -- request loop -------------------------------------------------------
    loop {
        match reader.next().await {
            Ok(Some(frame)) => {
                peer.touch(broker.now_ms());
                match &frame.body {
                    kratos_protocol::Body::Res { id, .. } => {
                        // A reply to something the broker asked this backend.
                        if let Some(tx) = peer.pending.lock().await.remove(id) {
                            let _ = tx.send(frame);
                        }
                    }
                    kratos_protocol::Body::Evt { .. } => { /* compatibility rule 2: ignorable */ }
                    kratos_protocol::Body::Req { id, op, payload } => {
                        let (id, op, payload) = (id.clone(), op.clone(), payload.clone());
                        let broker = broker.clone();
                        let peer = peer.clone();
                        let out = out_tx.clone();
                        // Handled on its own task so a slow action does not
                        // stop this peer from being able to engage the kill
                        // switch on the same connection.
                        tokio::spawn(async move {
                            let reply = dispatch(&broker, &peer, &id, &op, payload).await;
                            let _ = out.send(reply).await;
                        });
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                if e.is_fatal() {
                    break;
                }
                // Unparseable line: say so and carry on.
                let _ = out_tx
                    .send(Frame::err(
                        "0",
                        WireError::new(codes::BAD_REQUEST, e.to_string()),
                    ))
                    .await;
            }
        }
    }

    broker.remove_peer(&peer_id).await;
    pump.abort();
}

async fn handshake<R>(
    broker: &Arc<Broker>,
    reader: &mut kratos_ipc::FrameReader<R>,
    out: &mpsc::Sender<Frame>,
) -> Option<Arc<PeerHandle>>
where
    R: AsyncRead + Unpin,
{
    let frame = match reader.next().await {
        Ok(Some(f)) => f,
        _ => return None,
    };
    let id = frame.id().unwrap_or("0").to_string();

    if frame.op() != Some(ops::HELLO) {
        let _ = out
            .send(Frame::err(
                &id,
                WireError::new(codes::UNAUTHORIZED, "the first message must be hello"),
            ))
            .await;
        return None;
    }
    if frame.v != kratos_protocol::V {
        let _ = out
            .send(Frame::err(
                &id,
                WireError::with_detail(
                    codes::BAD_VERSION,
                    format!("this broker speaks protocol v{}", kratos_protocol::V),
                    json!({"broker_v_max": kratos_protocol::V}),
                ),
            ))
            .await;
        return None;
    }

    let hello: kratos_protocol::Hello = match frame.parse() {
        Ok(h) => h,
        Err(e) => {
            let _ = out
                .send(Frame::err(
                    &id,
                    WireError::new(codes::BAD_REQUEST, format!("unusable hello: {e}")),
                ))
                .await;
            return None;
        }
    };

    if hello.v_min > kratos_protocol::V || hello.v_max < kratos_protocol::V {
        let _ = out
            .send(Frame::err(
                &id,
                WireError::with_detail(
                    codes::BAD_VERSION,
                    format!(
                        "no overlapping protocol major: peer wants {}..={}, broker speaks {}",
                        hello.v_min,
                        hello.v_max,
                        kratos_protocol::V
                    ),
                    json!({"broker_v_max": kratos_protocol::V}),
                ),
            ))
            .await;
        return None;
    }

    let source = match hello.role {
        Role::Backend => match hello.source.as_deref().and_then(Source::parse) {
            Some(s) => Some(s),
            None => {
                let _ = out
                    .send(Frame::err(
                        &id,
                        WireError::new(
                            codes::BAD_REQUEST,
                            "a backend must declare a known source (dom-ext, uia, ocr, pixel)",
                        ),
                    ))
                    .await;
                return None;
            }
        },
        _ => None,
    };

    // An instance is meaningful only for a backend, and only when it carries a
    // token: keying the registry on an empty string would merge two instances
    // into one slot, which is the defect this field exists to remove.
    let instance = match hello.role {
        Role::Backend => hello.instance.clone().filter(|i| i.usable()),
        _ => None,
    };

    // Identity is for agents. A backend has no lane and the settings window is
    // the user, so neither resolves one -- and neither can claim a harness
    // name to borrow an allocation with.
    let identity = match hello.role {
        Role::Agent => crate::lane::Identity::from_hello(&hello),
        _ => crate::lane::Identity::default(),
    };
    let harness_id = identity.harness_id(&hello.name);

    // A harness nobody has seen before is written down before it is answered,
    // so the settings window can list it and the user has something to
    // allocate a browser to. This is the only thing an unapproved client's
    // connection achieves.
    let (lane, approved) = if hello.role == Role::Agent {
        broker.remember_harness(&harness_id).await;
        let attached = crate::lane::attached_instances(broker).await;
        let config = broker.config.read().await;
        let lane = crate::lane::resolve_opt(&config, &identity, &attached);
        let approved = config.harness(&harness_id).is_some_and(|h| h.approved)
            || matches!(
                broker.approvals.lock().await.get(&harness_id),
                Some(crate::state::Approval::Allowed)
            );
        (lane, approved)
    } else {
        (None, true)
    };

    let peer = Arc::new(PeerHandle {
        peer_id: broker.next_peer_id(hello.role),
        role: hello.role,
        name: hello.name.clone(),
        source,
        instance,
        identity,
        harness_id,
        lane: Mutex::new(lane.clone()),
        handles: hello.handles.clone().unwrap_or_else(|| vec!["native".into(), "browser".into()]),
        out: out.clone(),
        pending: Mutex::new(HashMap::new()),
        connected_at: Instant::now(),
        last_seen: AtomicU64::new(broker.now_ms()),
    });

    if let Some(old) = broker.add_peer(peer.clone()).await {
        // Said precisely, because "the same source" used to be a lie: a second
        // browser profile was told this when nothing about it had restarted.
        let reason = match &peer.instance {
            Some(i) => format!("this instance ({}) reconnected", i.token),
            None => "another backend registered the same source".to_string(),
        };
        let _ = old.out.try_send(Frame::evt(
            ops::EVT_SUPERSEDED,
            json!({"reason": reason, "source": peer.source.map(|s| s.as_str())}),
        ));
    }

    let ack = kratos_protocol::HelloAck {
        v: kratos_protocol::V,
        peer_id: peer.peer_id.clone(),
        broker: BROKER_NAME.to_string(),
        decider: broker.decider.read().await.name(),
        kill_engaged: broker.gate.lock().await.kill.engaged(),
        backends: broker.backend_infos().await,
        lane,
        approved,
    };
    let _ = out.send(Frame::ok(&id, serde_json::to_value(ack).unwrap_or_default())).await;
    Some(peer)
}

async fn dispatch(
    broker: &Arc<Broker>,
    peer: &Arc<PeerHandle>,
    id: &str,
    op: &str,
    payload: Value,
) -> Frame {
    // Order matters: an op nobody has heard of is `bad_request`, not
    // `unauthorized`. Checking the role first tells a caller it lacks
    // permission for something that does not exist, and sends it looking for a
    // credential rather than a typo.
    if !known_op(op) {
        return Frame::err(id, WireError::new(codes::BAD_REQUEST, format!("unknown op {op:?}")));
    }
    if !peer.role.may(op) {
        return Frame::err(
            id,
            WireError::new(
                codes::UNAUTHORIZED,
                format!("a {} may not call {op}", peer.role.as_str()),
            ),
        );
    }

    // A client the user has not allowed gets `status` and `whoami` and nothing
    // else. Those two are what tells it, and the person reading its output,
    // *why* -- a refusal with no way to ask what is wrong is how an hour goes
    // into a working setup that was simply never approved.
    if let Err(e) = require_approval(broker, peer, op).await {
        return Frame::err(id, e);
    }

    let result: Result<Value, WireError> = match op {
        ops::HELLO => Err(WireError::new(codes::BAD_REQUEST, "already introduced")),

        ops::STATUS => Ok(broker.status().await),

        ops::WHOAMI => Ok(whoami(broker, peer).await),

        ops::LANE_LIST => Ok(lane_list(broker).await),

        ops::HARNESS_LIST => Ok(harness_list(broker).await),

        ops::AGENT_LIST => Ok(agent_list(broker).await),

        ops::INSTANCE_BIND => instance_bind(broker, payload).await,

        ops::OBSERVE => match serde_json::from_value(payload) {
            Ok(req) => {
                broker.mark_agent_activity();
                crate::act::observe(broker, peer, req).await.and_then(to_value)
            }
            Err(e) => Err(bad(e)),
        },

        ops::ACT => match serde_json::from_value(payload) {
            Ok(req) => crate::act::act(broker, peer, req).await.and_then(to_value),
            Err(e) => Err(bad(e)),
        },

        ops::VERIFY => match serde_json::from_value(payload) {
            Ok(req) => crate::act::verify(broker, peer, req).await.and_then(to_value),
            Err(e) => Err(bad(e)),
        },

        ops::CONFIRM => match serde_json::from_value::<kratos_protocol::ConfirmReq>(payload) {
            Ok(req) => crate::act::confirm(broker, peer, &req.token).await.and_then(to_value),
            Err(e) => Err(bad(e)),
        },

        ops::LEASE_ACQUIRE => match serde_json::from_value::<kratos_protocol::LeaseAcquireReq>(payload)
        {
            Ok(req) => broker
                .acquire_lease(
                    &peer.peer_id,
                    &req.scope,
                    Duration::from_millis(req.ttl_ms.clamp(100, 120_000)),
                    Duration::from_millis(req.wait_ms.min(60_000)),
                )
                .await
                .and_then(to_value),
            Err(e) => Err(bad(e)),
        },

        ops::LEASE_RELEASE => match serde_json::from_value::<kratos_protocol::LeaseReleaseReq>(payload)
        {
            Ok(req) => {
                broker.release_lease(&req.token).await;
                Ok(json!({"released": true}))
            }
            Err(e) => Err(bad(e)),
        },

        ops::KILL_ENGAGE => match serde_json::from_value::<kratos_protocol::KillReq>(payload) {
            Ok(req) => {
                broker.gate.lock().await.kill.engage(&req.scope);
                let engaged = broker.gate.lock().await.kill.engaged();
                broker
                    .journal
                    .read()
                    .await
                    .append(json!({"op": "kill.engage", "peer_id": peer.peer_id,
                                   "agent": peer.name, "scope": req.scope,
                                   "reason": req.reason, "engaged": engaged}));
                broker
                    .broadcast(Frame::evt(
                        ops::EVT_KILL,
                        json!({"engaged": engaged, "by": peer.peer_id}),
                    ))
                    .await;
                Ok(json!({"engaged": engaged}))
            }
            Err(e) => Err(bad(e)),
        },

        ops::KILL_RESET => match serde_json::from_value::<kratos_protocol::KillReq>(payload) {
            Ok(req) => {
                broker.gate.lock().await.kill.reset(&req.scope);
                let engaged = broker.gate.lock().await.kill.engaged();
                broker.journal.read().await.append(
                    json!({"op": "kill.reset", "peer_id": peer.peer_id, "scope": req.scope,
                           "engaged": engaged}),
                );
                broker
                    .broadcast(Frame::evt(
                        ops::EVT_KILL,
                        json!({"engaged": engaged, "by": peer.peer_id}),
                    ))
                    .await;
                Ok(json!({"engaged": engaged}))
            }
            Err(e) => Err(bad(e)),
        },

        ops::KILL_STATUS => {
            let cfg = broker.config.read().await;
            Ok(json!({
                "engaged": broker.gate.lock().await.kill.engaged(),
                "hotkey": cfg.kill.hotkey,
                "hotkey_scope": cfg.kill.hotkey_scope,
                "per_browser": cfg.kill.per_browser,
            }))
        }

        ops::LOG_TAIL => match serde_json::from_value::<kratos_protocol::LogTailReq>(payload) {
            Ok(req) => {
                Ok(json!({"lines": broker.journal.read().await.tail(req.n, req.since_ms)}))
            }
            Err(e) => Err(bad(e)),
        },

        ops::CONFIG_GET => Ok(json!({"config": broker.config.read().await.redacted()})),

        ops::CONFIG_SET => config_set(broker, payload).await,

        ops::SHUTDOWN => {
            broker.shutdown.notify_waiters();
            Ok(json!({"stopping": true}))
        }

        other => Err(WireError::new(codes::BAD_REQUEST, format!("unknown op {other:?}"))),
    };

    match result {
        Ok(payload) => Frame::ok(id, payload),
        Err(e) => Frame::err(id, e),
    }
}

/// Apply a settings patch, sealing a new API key if one was supplied.
///
/// The UI sends the key as plaintext `decider.api_key` exactly once, on the
/// way in. It is sealed here and the plaintext is dropped before the patch
/// touches the config, so it never reaches disk or the log in the clear -- and
/// `config.get` has no field to return it through.
async fn config_set(broker: &Arc<Broker>, payload: Value) -> Result<Value, WireError> {
    let mut patch = payload.get("patch").cloned().unwrap_or(payload);
    let mut new_key: Option<String> = None;

    if let Some(decider) = patch.get_mut("decider").and_then(|d| d.as_object_mut()) {
        // `key_present` is part of the redacted form a UI reads back; it is
        // not a setting, so a round-trip must not try to store it.
        decider.remove("key_present");
        if let Some(raw) = decider.remove("api_key") {
            let plain = raw.as_str().unwrap_or("").to_string();
            let sealed = crate::seal::seal_key(&plain)
                .map_err(|e| WireError::new(codes::INTERNAL, e))?;
            decider.insert("api_key_enc".into(), json!(sealed));
            new_key = Some(plain.trim().to_string());
        }
    }

    let merged = broker.apply_config(&patch, new_key).await?;
    let restart_required = patch.get("pipe_name").is_some();
    Ok(json!({"config": merged, "restart_required": restart_required}))
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value, WireError> {
    serde_json::to_value(v)
        .map_err(|e| WireError::new(codes::INTERNAL, format!("could not encode reply: {e}")))
}

// ---------------------------------------------------------------------------
// Identity, lanes, approval
// ---------------------------------------------------------------------------

/// Which ops a client may call before the user has allowed it.
fn allowed_unapproved(op: &str) -> bool {
    matches!(op, ops::HELLO | ops::STATUS | ops::WHOAMI)
}

/// Refuse anything but `status` and `whoami` until the user has allowed this
/// client, and put the question on screen.
///
/// The dialog is raised and **not waited for**. Spec §3 requires a native
/// dialog that the agent cannot answer; it does not require the request to sit
/// on it, and sitting on it would break the rule this whole gate was built
/// around -- a blocking approval once cost 241 of 272 seconds of a session.
/// So the op is refused `not_approved`, which is safe to retry, and the
/// retry succeeds the moment a human clicks. An agent whose operator is
/// asleep gets an immediate, explicable refusal rather than a two-minute hang.
async fn require_approval(
    broker: &Arc<Broker>,
    peer: &Arc<PeerHandle>,
    op: &str,
) -> Result<(), WireError> {
    if peer.role != Role::Agent || allowed_unapproved(op) {
        return Ok(());
    }
    let harness = peer.harness_id.clone();
    match broker.approval(&harness).await {
        Some(crate::state::Approval::Allowed) => return Ok(()),
        Some(crate::state::Approval::Denied) => return Err(not_approved(&harness, "declined")),
        Some(crate::state::Approval::Asking) => return Err(not_approved(&harness, "asking")),
        None => {}
    }

    // Claim the question before raising it, so an agent in a retry loop raises
    // one dialog rather than one per request.
    if !broker.begin_asking(&harness).await {
        return Err(not_approved(&harness, "asking"));
    }

    let timeout =
        Duration::from_secs(broker.config.read().await.gate.review_dialog_timeout_s.clamp(5, 600));
    let detail = match peer.lane.lock().await.clone() {
        Some(l) => format!(
            "It would drive {} (profile {}). Every action it takes still goes through the gate.",
            l.browser, l.profile_dir
        ),
        None => "Every action it takes still goes through the gate.".to_string(),
    };

    let broker = broker.clone();
    let peer_id = peer.peer_id.clone();
    let name = peer.name.clone();
    let asking = harness.clone();
    tokio::spawn(async move {
        let harness = asking;
        // One dialog at a time, machine-wide: two clients connecting together
        // would otherwise stack two system-modal windows, and the user would
        // answer whichever was on top for whichever client they thought they
        // were answering.
        let _one_at_a_time = broker.approval_gate.lock().await;
        let label = harness.clone();
        let answer = tokio::task::spawn_blocking(move || {
            kratos_gate::dialog::ask_approval(&label, &detail, timeout)
        })
        .await
        .unwrap_or(kratos_gate::dialog::Approval::TimedOut);

        broker.record_approval(&harness, answer).await;
        broker.journal.read().await.append(json!({
            "op": "harness.approval", "peer_id": peer_id, "agent": name,
            "harness": harness, "answer": answer.as_str(),
        }));
    });

    Err(not_approved(&harness, "asking"))
}

fn not_approved(harness: &str, answer: &str) -> WireError {
    let what = match answer {
        "asking" => format!(
            "A dialog is on screen now asking whether {harness:?} may drive this machine. Once \
             somebody answers it, try again."
        ),
        _ => format!(
            "{harness:?} has not been allowed to drive this machine. Only the person at the \
             keyboard can allow it: open the kratos settings window, find {harness:?} under \
             Harnesses, and approve it."
        ),
    };
    WireError::with_detail(
        codes::NOT_APPROVED,
        what,
        json!({"harness": harness, "answer": answer}),
    )
}

/// Re-resolve and report this session's lane.
///
/// Re-resolved rather than repeated from the handshake, because the user may
/// have allocated a different browser since this session connected and the
/// answer to "where am I" should be the current one. The stored copy is
/// updated to match, so the next action uses what was just reported.
async fn whoami(broker: &Arc<Broker>, peer: &Arc<PeerHandle>) -> Value {
    let lane = if peer.role == Role::Agent {
        let attached = crate::lane::attached_instances(broker).await;
        let config = broker.config.read().await;
        let fresh = crate::lane::resolve_opt(&config, &peer.identity, &attached);
        *peer.lane.lock().await = fresh.clone();
        fresh
    } else {
        None
    };
    let approved = matches!(
        broker.approval(&peer.harness_id).await,
        Some(crate::state::Approval::Allowed)
    );
    json!({
        "peer_id": peer.peer_id,
        "role": peer.role.as_str(),
        "name": peer.name,
        "harness": peer.harness_id,
        "agent": peer.identity.agent,
        "session": peer.identity.session,
        "approved": approved,
        "lane": lane,
    })
}

/// Every session's lane, plus what this machine has to offer -- the settings
/// window's dropdowns are filled from this rather than from a second reader of
/// `Local State` in the UI process.
async fn lane_list(broker: &Arc<Broker>) -> Value {
    let attached = crate::lane::attached_instances(broker).await;
    let sessions: Vec<Value> = {
        let config = broker.config.read().await;
        let peers: Vec<Arc<PeerHandle>> =
            broker.peers.read().await.values().cloned().collect();
        peers
            .iter()
            .filter(|p| p.role == Role::Agent)
            .map(|p| {
                let lane = crate::lane::resolve_opt(&config, &p.identity, &attached);
                json!({
                    "peer_id": p.peer_id,
                    "harness": p.harness_id,
                    "agent": p.identity.agent,
                    "session": p.identity.session,
                    "lane": lane,
                })
            })
            .collect()
    };

    let browsers: Vec<Value> = crate::browsers::BRANDS
        .iter()
        .filter_map(|b| crate::browsers::locate(b))
        .map(|b| {
            json!({
                "brand": b.brand,
                "installed": b.installed(),
                "exe": b.exe.as_ref().map(|p| p.display().to_string()),
                "user_data": b.user_data.as_ref().map(|p| p.display().to_string()),
                "last_used": crate::browsers::last_used(b.brand),
                "profiles": crate::browsers::profiles(b.brand)
                    .into_iter()
                    .map(|p| json!({"dir": p.dir, "name": p.name}))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    json!({
        "sessions": sessions,
        "browsers": browsers,
        "os_default": crate::browsers::os_default_brand(),
        "instances": attached
            .iter()
            .map(|i| json!({
                "token": i.token,
                "brand": i.brand,
                "bound_to": i.bound_to.as_ref().map(|(b, p)| json!({"brand": b, "profile_dir": p})),
            }))
            .collect::<Vec<_>>(),
    })
}

async fn harness_list(broker: &Arc<Broker>) -> Value {
    let config = broker.config.read().await;
    let connected: Vec<String> = broker
        .peers
        .read()
        .await
        .values()
        .filter(|p| p.role == Role::Agent)
        .map(|p| p.harness_id.clone())
        .collect();
    let approvals = broker.approvals.lock().await.clone();
    json!({
        "harnesses": config
            .harnesses
            .iter()
            .map(|h| json!({
                "id": h.id,
                "label": h.label,
                "browser": h.browser,
                "approved": h.approved,
                // What was decided since this broker started, which is how
                // "allow just for now" is visible at all.
                "session_approval": approvals.get(&h.id).map(|a| match a {
                    crate::state::Approval::Allowed => "allowed",
                    crate::state::Approval::Denied => "denied",
                    crate::state::Approval::Asking => "asking",
                }),
                "connected": connected.iter().any(|c| c.eq_ignore_ascii_case(&h.id)),
            }))
            .collect::<Vec<_>>(),
    })
}

async fn agent_list(broker: &Arc<Broker>) -> Value {
    let config = broker.config.read().await;
    let live: Vec<String> = broker
        .peers
        .read()
        .await
        .values()
        .filter_map(|p| p.identity.agent.clone())
        .collect();
    let configured: Vec<Value> = config
        .agents
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "label": a.label,
                "profile_dir": a.profile_dir,
                "connected": live.iter().any(|l| l.eq_ignore_ascii_case(&a.id)),
            })
        })
        .collect();
    // An agent name that has connected but is not in the file yet is listed
    // too, unassigned: the user cannot allocate a profile to a name they
    // cannot see.
    let unknown: Vec<Value> = live
        .iter()
        .filter(|l| config.agent(l).is_none())
        .map(|l| json!({"id": l, "label": "", "profile_dir": "", "connected": true}))
        .collect();
    let all: Vec<Value> = configured.into_iter().chain(unknown).collect();
    json!({"agents": all})
}

/// Pair an instance token with the browser profile it runs in.
async fn instance_bind(broker: &Arc<Broker>, payload: Value) -> Result<Value, WireError> {
    let token = payload.get("token").and_then(|t| t.as_str()).unwrap_or_default().trim();
    if token.is_empty() {
        return Err(WireError::new(codes::BAD_REQUEST, "instance.bind needs a token"));
    }
    let brand_in = payload.get("brand").and_then(|b| b.as_str()).unwrap_or_default();
    let profile_dir =
        payload.get("profile_dir").and_then(|p| p.as_str()).unwrap_or_default().trim().to_string();

    // An empty brand or profile unpairs, which is how a mistaken pairing is
    // undone without hand-editing the file.
    let unpair = brand_in.trim().is_empty() && profile_dir.is_empty();
    let brand = match (unpair, crate::browsers::normalise(brand_in)) {
        (true, _) => String::new(),
        (false, Some(b)) => b.to_string(),
        (false, None) => {
            return Err(WireError::new(
                codes::BAD_REQUEST,
                format!("{brand_in:?} is not a browser this build knows"),
            ))
        }
    };

    let patch = {
        let config = broker.config.read().await;
        let mut list = config.instances.clone();
        list.retain(|i| i.token != token);
        if !unpair {
            list.push(kratos_protocol::config::InstanceConfig {
                token: token.to_string(),
                brand: brand.clone(),
                profile_dir: profile_dir.clone(),
            });
        }
        json!({"instances": list})
    };
    broker.apply_config(&patch, None).await?;
    Ok(json!({"token": token, "brand": brand, "profile_dir": profile_dir, "paired": !unpair}))
}

/// Every op this build implements, whatever the role.
fn known_op(op: &str) -> bool {
    matches!(
        op,
        ops::HELLO
            | ops::STATUS
            | ops::WHOAMI
            | ops::OBSERVE
            | ops::ACT
            | ops::VERIFY
            | ops::CONFIRM
            | ops::LEASE_ACQUIRE
            | ops::LEASE_RELEASE
            | ops::KILL_ENGAGE
            | ops::KILL_RESET
            | ops::KILL_STATUS
            | ops::LOG_TAIL
            | ops::CONFIG_GET
            | ops::CONFIG_SET
            | ops::SHUTDOWN
            | ops::LANE_LIST
            | ops::HARNESS_LIST
            | ops::AGENT_LIST
            | ops::INSTANCE_BIND
    )
}

fn bad(e: serde_json::Error) -> WireError {
    WireError::new(codes::BAD_REQUEST, format!("unusable payload: {e}"))
}
