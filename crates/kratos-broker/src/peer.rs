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

    let peer = Arc::new(PeerHandle {
        peer_id: broker.next_peer_id(hello.role),
        role: hello.role,
        name: hello.name.clone(),
        source,
        handles: hello.handles.clone().unwrap_or_else(|| vec!["native".into(), "browser".into()]),
        out: out.clone(),
        pending: Mutex::new(HashMap::new()),
        connected_at: Instant::now(),
        last_seen: AtomicU64::new(broker.now_ms()),
    });

    if let Some(old) = broker.add_peer(peer.clone()).await {
        let _ = old
            .out
            .try_send(Frame::evt(
                ops::EVT_SUPERSEDED,
                json!({"reason": "another backend registered the same source"}),
            ));
    }

    let ack = kratos_protocol::HelloAck {
        v: kratos_protocol::V,
        peer_id: peer.peer_id.clone(),
        broker: BROKER_NAME.to_string(),
        decider: broker.decider.read().await.name(),
        kill_engaged: broker.gate.lock().await.kill.engaged(),
        backends: broker.backend_infos().await,
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

    let result: Result<Value, WireError> = match op {
        ops::HELLO => Err(WireError::new(codes::BAD_REQUEST, "already introduced")),

        ops::STATUS => Ok(broker.status().await),

        ops::OBSERVE => match serde_json::from_value(payload) {
            Ok(req) => {
                broker.mark_agent_activity();
                crate::act::observe(broker, req).await.and_then(to_value)
            }
            Err(e) => Err(bad(e)),
        },

        ops::ACT => match serde_json::from_value(payload) {
            Ok(req) => crate::act::act(broker, peer, req).await.and_then(to_value),
            Err(e) => Err(bad(e)),
        },

        ops::VERIFY => match serde_json::from_value(payload) {
            Ok(req) => crate::act::verify(broker, req).await.and_then(to_value),
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

/// Every op this build implements, whatever the role.
fn known_op(op: &str) -> bool {
    matches!(
        op,
        ops::HELLO
            | ops::STATUS
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
    )
}

fn bad(e: serde_json::Error) -> WireError {
    WireError::new(codes::BAD_REQUEST, format!("unusable payload: {e}"))
}
