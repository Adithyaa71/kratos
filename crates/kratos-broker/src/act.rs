//! The action pipeline: observe, decide, gate, lease, execute, prove.
//!
//! In that order, every time, with no shortcuts. The ordering is the design:
//!
//! * **Observe before deciding**, so the model chooses from what is actually
//!   on screen rather than from an id an agent remembered.
//! * **Gate before leasing**, so a blocked action never touches the mouse.
//! * **Prove after executing**, so `executed` means something changed and not
//!   merely that a click was dispatched. Three actions once reported success
//!   at confidence 0.99, 1.00 and 1.00 while doing nothing at all; this step
//!   is the answer to that.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kratos_protocol::{
    codes, status as st, ActReq, ActRes, BackendActRes, BackendObservation, Decision, Effect,
    Element, Frame, Kind, Observation, Source, TargetClass, VerifyReq, VerifyRes, WireError,
};
use kratos_decide::{extract, Question};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::classify::{self, WindowRef};
use crate::state::{Broker, ObservationRecord, Parked, PeerHandle};

/// How long the before/after drift measurement is allowed to take.
const DRIFT_GAP: Duration = Duration::from_millis(35);

/// How close two options have to score before the model is called torn
/// rather than unsure.
///
/// Not a tuning knob so much as a definition: a gap this small between the
/// best and second-best option means the evidence did not separate them, and
/// an agent told "not sure enough" would re-phrase when it should be naming
/// the container instead.
const AMBIGUITY_MARGIN: f64 = 0.10;

/// How often to look, while waiting for the screen to move.
///
/// A grab of the virtual desktop costs about 33ms on its own, so this is a
/// floor rather than a real period: the loop is capture-bound, and asking for
/// 10ms here only means "again, immediately".
const SETTLE_POLL: Duration = Duration::from_millis(10);

/// After the pixels first move, wait this long and look once more.
///
/// A UI caught mid-animation gives a true "something changed" and a half-drawn
/// accessibility tree. This is the difference between returning as soon as
/// something happened and returning as soon as it has finished happening --
/// still a fraction of a fixed settle, and the observe that follows is of a
/// finished frame.
const SETTLE_CONFIRM: Duration = Duration::from_millis(60);

// ---------------------------------------------------------------------------
// Backend plumbing
// ---------------------------------------------------------------------------

async fn call_backend(
    backend: &Arc<PeerHandle>,
    op: &str,
    payload: Value,
    timeout: Duration,
) -> Result<Value, WireError> {
    let id = kratos_ipc::next_id("bk");
    let (tx, rx) = oneshot::channel();
    backend.pending.lock().await.insert(id.clone(), tx);

    if backend.out.send(Frame::req(&id, op, payload)).await.is_err() {
        backend.pending.lock().await.remove(&id);
        return Err(WireError::new(codes::BACKEND_ERROR, "backend disconnected"));
    }

    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(frame)) => {
            if frame.is_ok() {
                Ok(frame.payload().clone())
            } else {
                Err(frame
                    .wire_error()
                    .cloned()
                    .unwrap_or_else(|| WireError::new(codes::BACKEND_ERROR, "backend refused")))
            }
        }
        Ok(Err(_)) => Err(WireError::new(codes::BACKEND_ERROR, "backend closed mid-request")),
        Err(_) => {
            backend.pending.lock().await.remove(&id);
            Err(WireError::with_detail(
                codes::BACKEND_TIMEOUT,
                format!("backend did not answer {op} within {}ms", timeout.as_millis()),
                json!({"op": op}),
            ))
        }
    }
}

/// Observe through the first backend that answers.
///
/// Read-only, so falling back on *any* failure is safe -- including a timeout.
/// Acting is different (see [`execute`]).
async fn observe_with_fallback(
    broker: &Arc<Broker>,
    class: TargetClass,
    prefer: Option<Source>,
    window: Option<&str>,
    timeout: Duration,
) -> Result<(String, BackendObservation), WireError> {
    let order = broker.route(class, prefer).await;
    if order.is_empty() {
        return Err(WireError::with_detail(
            codes::NO_BACKEND,
            match prefer {
                Some(p) => format!("no {} backend is connected", p.as_str()),
                None => format!("no backend is connected that handles a {} window", class.as_str()),
            },
            json!({"class": class.as_str(), "prefer": prefer.map(|p| p.as_str())}),
        ));
    }

    let mut chain: Vec<&str> = Vec::new();
    let mut last: Option<WireError> = None;
    for source in order {
        chain.push(source.as_str());
        let Some(backend) = broker.backend_for(source).await else { continue };
        let payload = json!({
            "window": window, "class": class.as_str(), "timeout_ms": timeout.as_millis() as u64
        });
        match call_backend(&backend, kratos_protocol::ops::BACKEND_OBSERVE, payload, timeout).await {
            Ok(v) => match serde_json::from_value::<BackendObservation>(v) {
                Ok(obs) => return Ok((chain.join(">"), obs)),
                Err(e) => {
                    last = Some(WireError::new(
                        codes::BACKEND_ERROR,
                        format!("{} sent an unreadable observation: {e}", source.as_str()),
                    ))
                }
            },
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| WireError::new(codes::NO_BACKEND, "every backend declined")))
}

/// Dispatch an action.
///
/// Fallback here is deliberately narrower than for observation. `dom-ext`
/// reporting `applied:false` means the page received a synthetic event and
/// ignored it, so nothing happened and trying the next backend is safe. A
/// **timeout** on a mutating action is not safe to retry: the action may have
/// landed, and clicking Send twice is worse than reporting that we do not
/// know. So a timeout is returned, not retried.
async fn execute(
    broker: &Arc<Broker>,
    class: TargetClass,
    prefer: Option<Source>,
    payload: Value,
) -> Result<(String, BackendActRes), WireError> {
    let order = broker.route(class, prefer).await;
    if order.is_empty() {
        return Err(WireError::new(codes::NO_BACKEND, "no backend is connected to act through"));
    }
    let timeout = Duration::from_millis(12_000);
    let mut chain: Vec<&str> = Vec::new();
    let mut last: Option<WireError> = None;

    for source in order {
        chain.push(source.as_str());
        let Some(backend) = broker.backend_for(source).await else { continue };
        match call_backend(&backend, kratos_protocol::ops::BACKEND_ACT, payload.clone(), timeout).await
        {
            Ok(v) => {
                let res: BackendActRes = serde_json::from_value(v).unwrap_or_default();
                if res.applied {
                    return Ok((chain.join(">"), res));
                }
                // Dispatched and not taken: the next backend may use OS-level
                // input, which a page cannot ignore.
                last = Some(WireError::with_detail(
                    codes::BACKEND_ERROR,
                    format!("{} dispatched the action and the target ignored it", source.as_str()),
                    json!({"applied": false}),
                ));
            }
            Err(e) if e.code == codes::BACKEND_TIMEOUT => return Err(e),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| WireError::new(codes::BACKEND_ERROR, "every backend declined")))
}

// ---------------------------------------------------------------------------
// observe
// ---------------------------------------------------------------------------

/// What to tell an agent whose action was refused.
///
/// The generic advice -- be more specific -- is wrong when the real problem is
/// that the action went to the foreground window while the browser the agent
/// meant sat untouched. Rewording cannot fix that; passing `window` can. The
/// broker can tell the difference, so it should.
async fn targeting_advice(broker: &Arc<Broker>, answered_by: &str) -> String {
    const BASE: &str = "nothing has run. Name the target more specifically -- quote the exact \
                        label, or say which dialog or pane it is in -- and try again";

    let used_dom = answered_by.rsplit('>').next() == Some(Source::DomExt.as_str());
    if used_dom {
        return BASE.to_string();
    }
    if broker.backend_for(Source::DomExt).await.is_none() {
        return BASE.to_string();
    }
    format!(
        "{BASE}. Note: a browser backend is connected but did not answer, because this action \
         went to the window in front. If you meant something in the browser, pass `window` with \
         part of its title -- that also routes you to the DOM backend, which reports the \
         containing dialog or pane and can tell two identically-labelled controls apart."
    )
}

/// Who the gate should actually judge, given who answered the observation.
///
/// The foreground window is the right answer for a native backend, which can
/// only see what is in front. It is the *wrong* answer for `dom-ext`, which
/// reports the active tab of the last focused browser window whether or not
/// that browser has focus.
///
/// Found live: a `dom-ext` observation of a Brave tab was gated against the
/// Windows Notification Center, so a kill scope of `browser:brave` did not
/// cover it. The URL was already taken from the observation, so domain rules
/// were unaffected -- but a kill switch that does not stop the backend it
/// names is not a kill switch.
fn effective_target(
    win: &WindowRef,
    obs: &BackendObservation,
    answered_by: &str,
) -> (kratos_gate::TargetRef, TargetClass) {
    let last = answered_by.rsplit('>').next().unwrap_or(answered_by);
    if last != Source::DomExt.as_str() {
        return (win.target_ref(obs.url.clone()), win.class());
    }

    // dom-ext namespaces every id `brand:window:tab:frame:element`, so the
    // browser that answered names itself in its own data.
    let brand = obs
        .elements
        .first()
        .and_then(|e| e.id.split(':').next())
        .filter(|b| !b.is_empty() && b.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|b| b.to_ascii_lowercase());

    let title = if obs.window.trim().is_empty() { win.title.clone() } else { obs.window.clone() };

    (
        kratos_gate::TargetRef {
            window_title: title,
            url: obs.url.clone(),
            exe: brand.as_ref().map(|b| format!("{b}.exe")),
            brand,
        },
        TargetClass::Browser,
    )
}

pub async fn observe(
    broker: &Arc<Broker>,
    req: kratos_protocol::ObserveReq,
) -> Result<Observation, WireError> {
    let started = Instant::now();
    let win = target_window(req.window.as_deref())?;
    let class = win.class();
    let prefer = match req.prefer.as_deref() {
        None => None,
        Some(name) => match Source::parse(name) {
            Some(s) => Some(s),
            // An unknown name is refused rather than ignored: a caller that
            // asked for a backend and silently got another would draw the
            // wrong conclusion from the result.
            None => {
                return Err(WireError::new(
                    codes::BAD_REQUEST,
                    format!("{name:?} is not a known backend"),
                ))
            }
        },
    };

    let (source, obs) = observe_with_fallback(
        broker,
        class,
        prefer,
        req.window.as_deref(),
        Duration::from_millis(req.timeout_ms),
    )
    .await?;

    let generation = broker.next_generation();
    *broker.last_observation.write().await =
        Some(ObservationRecord { elements: obs.elements.clone() });

    // The class of whoever actually answered, not of whatever has focus.
    let (_, reported_class) = effective_target(&win, &obs, &source);

    Ok(Observation {
        gen: generation,
        source,
        window: if obs.window.is_empty() { win.title.clone() } else { obs.window },
        url: obs.url,
        target_class: reported_class.as_str().to_string(),
        elements: obs.elements,
        truncated: obs.truncated,
        took_ms: started.elapsed().as_millis() as u64,
    })
}

// ---------------------------------------------------------------------------
// act
// ---------------------------------------------------------------------------

struct Resolved {
    kind: Kind,
    target: Option<Element>,
    decision: Decision,
}

/// Ask the model which action and which element.
async fn decide(
    broker: &Arc<Broker>,
    instruction: &str,
    win: &WindowRef,
    url: Option<&str>,
    class: TargetClass,
    elements: &[Element],
    pinned: Option<&Element>,
) -> Result<Resolved, WireError> {
    let decider = broker.decider.read().await.clone();
    let thresholds = broker.config.read().await.thresholds.clone();

    // State is kept small on purpose: the options live in the questions, so
    // repeating the screen here would double the token cost of every action.
    let state = json!({
        "instruction": instruction,
        "window": win.title,
        "url": url,
        "target_class": class.as_str(),
        "element_count": elements.len(),
    });

    let mut questions: BTreeMap<String, Question> = BTreeMap::new();
    questions.insert(
        "kind".to_string(),
        Question::Choice {
            instructions: format!(
                "Which single action kind does this instruction call for, on the window titled {:?}",
                win.title
            ),
            criteria: Kind::ALL
                .iter()
                .map(|k| (k.as_str().to_string(), k.description().to_string()))
                .collect(),
        },
    );

    // A pinned target is the agent's own choice, already validated against the
    // current generation, so there is nothing to ask.
    //
    // The options are presented as short ordinals, never as element ids. An id
    // is routing plumbing -- `brave:w1237019769:t1237019932:f0:e2` -- and a
    // model sees an option's name as well as its description. Thirty-five
    // characters of near-identical noise per option drowns the one line that
    // actually distinguishes them: measured live, with a `combobox "Search"`
    // plainly on screen, every phrasing came back "none of these" at a diffuse
    // 0.42-0.64. The broker maps the ordinal back.
    let candidates = if pinned.is_none() && !elements.is_empty() {
        let (c, _truncated) =
            kratos_decide::compress(instruction, elements, kratos_decide::MAX_CARDINALITY);
        questions.insert(
            "target".to_string(),
            Question::Choice {
                instructions: "Which element on screen does the instruction refer to".to_string(),
                criteria: c.iter().enumerate().map(|(i, x)| (option_key(i, x), x.criterion.clone())).collect(),
            },
        );
        c
    } else {
        Vec::new()
    };

    let started = Instant::now();
    let answers = tokio::task::spawn_blocking(move || decider.ask(&state, &questions))
        .await
        .map_err(|e| WireError::new(codes::INTERNAL, format!("decider task failed: {e}")))?
        .map_err(|e| WireError::new(e.code(), e.to_string()))?;
    let decide_ms = started.elapsed().as_millis() as u64;

    let kind_answer = answers
        .choice("kind")
        .ok_or_else(|| WireError::new(codes::DECIDER_ERROR, "no answer for the action kind"))?;
    let kind = Kind::parse(&kind_answer.choice).ok_or_else(|| {
        WireError::new(
            codes::DECIDER_ERROR,
            format!("{:?} is not a known action kind", kind_answer.choice),
        )
    })?;

    // Only score the target question when it was actually asked and the kind
    // needs one. `navigate` and `open` address a target by name, so a
    // low-confidence element pick must not drag them down.
    let scored: Vec<&str> =
        if questions_included_target(&candidates) && kind.needs_target() {
            vec!["kind", "target"]
        } else {
            vec!["kind"]
        };
    let confidence = answers.weakest(&scored);
    let threshold = thresholds.for_kind(kind);

    let target = match pinned {
        Some(e) => Some(e.clone()),
        None => answers
            .choice("target")
            .and_then(|t| resolve_option(&t.choice, &candidates))
            .and_then(|id| elements.iter().find(|e| e.id == id).cloned()),
    };

    // Torn between two options, or just unsure about all of them? The two
    // have different fixes, so they are told apart here rather than averaged
    // into one number.
    let ambiguous_with = answers.choice("target").and_then(|t| {
        let mut ranked: Vec<(&String, &f64)> = t.probabilities.iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
        let (first, second) = (ranked.first()?, ranked.get(1)?);
        if first.1 - second.1 >= AMBIGUITY_MARGIN {
            return None;
        }
        let name = |opt: &str| {
            resolve_option(opt, &candidates)
                .and_then(|id| elements.iter().find(|e| e.id == id))
                .map(|e| e.criterion())
                .unwrap_or_else(|| "none of these".to_string())
        };
        let tied: Vec<String> = ranked
            .iter()
            .take_while(|(_, p)| *first.1 - **p < AMBIGUITY_MARGIN)
            .map(|(opt, _)| name(opt))
            .collect();
        (tied.len() > 1).then_some(tied)
    });

    let decision = Decision {
        kind: kind.as_str().to_string(),
        target_id: target.as_ref().map(|e| e.id.clone()),
        target_label: target.as_ref().map(|e| e.criterion()),
        confidence,
        kind_confidence: kind_answer.confidence,
        target_confidence: answers.choice("target").map(|t| t.confidence),
        threshold,
        decider: answers.model.clone(),
        decide_ms,
        ambiguous_with,
    };
    Ok(Resolved { kind, target, decision })
}

/// How one candidate is named to the model: a 1-based ordinal, or `none` for
/// the escape hatch. Short, distinct, and carrying no routing detail.
fn option_key(index: usize, candidate: &kratos_decide::Candidate) -> String {
    if candidate.id == kratos_decide::NONE_OF_THESE {
        "none".to_string()
    } else {
        (index + 1).to_string()
    }
}

/// Map the model's answer back to a real element id.
///
/// `None` for the escape hatch, and `None` for anything unrecognised -- a
/// reply that does not name one of the options offered is not a target, and
/// guessing the nearest one is how a confident answer gets aimed at the wrong
/// control.
fn resolve_option(choice: &str, candidates: &[kratos_decide::Candidate]) -> Option<String> {
    let choice = choice.trim();
    if choice.eq_ignore_ascii_case("none") || choice == kratos_decide::NONE_OF_THESE {
        return None;
    }
    let index: usize = choice.parse().ok()?;
    let candidate = candidates.get(index.checked_sub(1)?)?;
    if candidate.id == kratos_decide::NONE_OF_THESE {
        return None;
    }
    Some(candidate.id.clone())
}

fn questions_included_target(candidates: &[kratos_decide::Candidate]) -> bool {
    !candidates.is_empty()
}

/// Turn a decided kind plus the instruction into a `backend.act` payload.
///
/// Returns `Err` with a human-readable reason when the instruction does not
/// carry the literal the kind needs -- that becomes a `refused`, never a
/// guess.
fn build_action(kind: Kind, instruction: &str, target: Option<&Element>) -> Result<Value, String> {
    let target_json = target.map(|e| {
        json!({"id": e.id, "selector": e.selector, "bbox": e.bbox, "label": e.label})
    });

    let mut payload = json!({"kind": kind.as_str(), "target": target_json});
    let obj = payload.as_object_mut().expect("object");

    match kind {
        Kind::Type => {
            let text = extract::quoted(instruction).ok_or_else(|| {
                "the text to type must be in quotes -- nothing in this instruction is quoted, and \
                 inventing the text is not an option"
                    .to_string()
            })?;
            obj.insert("text".into(), json!(text));
        }
        Kind::Key => {
            let key = extract::key(instruction)
                .ok_or_else(|| "no keyboard key is named in this instruction".to_string())?;
            obj.insert("key".into(), json!(key));
        }
        Kind::Navigate => {
            let url = extract::url(instruction)
                .ok_or_else(|| "no URL is named in this instruction".to_string())?;
            obj.insert("url".into(), json!(url));
        }
        Kind::Open | Kind::Focus => {
            let name = extract::app_name(instruction)
                .ok_or_else(|| "no application is named in this instruction".to_string())?;
            obj.insert("text".into(), json!(name));
        }
        Kind::Scroll => {
            obj.insert("amount".into(), json!(extract::scroll_amount(instruction)));
        }
        Kind::Select => {
            // A selection may name the option in quotes; without it the
            // backend opens the control and the agent picks next.
            if let Some(text) = extract::quoted(instruction) {
                obj.insert("text".into(), json!(text));
            }
        }
        Kind::Click | Kind::Wait | Kind::None => {}
    }

    if kind.needs_target() && target.is_none() {
        return Err(format!(
            "a {} needs an element to aim at and none was identified on this screen",
            kind.as_str()
        ));
    }
    Ok(payload)
}

pub async fn act(
    broker: &Arc<Broker>,
    peer: &Arc<PeerHandle>,
    req: ActReq,
) -> Result<ActRes, WireError> {
    act_inner(broker, peer, req, None).await
}

pub async fn confirm(
    broker: &Arc<Broker>,
    peer: &Arc<PeerHandle>,
    token: &str,
) -> Result<ActRes, WireError> {
    let parked = broker.redeem(token, &peer.peer_id).await?;

    // An approval that must be clicked by a human is clicked by a human. The
    // agent reporting consent over the wire is not that, and this is the one
    // place the distinction is enforced.
    if parked.requires_os_dialog {
        let timeout = broker.gate.lock().await.review_dialog_timeout();
        let description = format!(
            "{}\n\nrule: {}",
            parked.instruction,
            parked.rule.clone().unwrap_or_else(|| "review".into())
        );
        let answer = tokio::task::spawn_blocking(move || kratos_gate::dialog::ask(&description, timeout))
            .await
            .map_err(|e| WireError::new(codes::INTERNAL, format!("dialog task failed: {e}")))?;
        if !answer.approved() {
            let mut res = ActRes::new(st::BLOCKED, 0);
            res.gate = Some(kratos_protocol::GateReport {
                category: "block".into(),
                rule: Some(format!(
                    "{}:{}",
                    parked.rule.unwrap_or_else(|| "review".into()),
                    answer.as_str()
                )),
                scope_checked: None,
            });
            res.detail = Some(format!(
                "a human did not approve this action at the native dialog ({})",
                answer.as_str()
            ));
            res.next = Some(
                "do not retry this action; tell the operator it was declined and ask what to do \
                 instead"
                    .into(),
            );
            broker.counters.record(st::BLOCKED);
            return Ok(res);
        }
    }

    let req = ActReq {
        instruction: parked.instruction,
        window: parked.window,
        dry_run: false,
        lease: None,
        target_id: None,
        timeout_ms: 20_000,
    };
    act_inner(broker, peer, req, Some(parked.rule.unwrap_or_else(|| "confirmed".into()))).await
}

async fn act_inner(
    broker: &Arc<Broker>,
    peer: &Arc<PeerHandle>,
    req: ActReq,
    approved_rule: Option<String>,
) -> Result<ActRes, WireError> {
    let started = Instant::now();
    broker.mark_agent_activity();
    broker.counters.act.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let elapsed = || started.elapsed().as_millis() as u64;

    // -- 1. what are we aiming at ------------------------------------------
    let win = target_window(req.window.as_deref())?;
    let class = win.class();

    // The kill switch is consulted before any work at all, and spends no rate
    // budget: a halted system must answer the same way however often it is
    // asked.
    {
        let gate = broker.gate.lock().await;
        if let Some(scope) = gate.kill.covers(&win.target_ref(None)) {
            let mut res = ActRes::new(st::KILLED, elapsed());
            res.gate = Some(kratos_protocol::GateReport {
                category: "kill_switch".into(),
                rule: Some(format!("kill_switch:{scope}")),
                scope_checked: Some(scope.clone()),
            });
            res.detail = Some(format!("the kill switch is engaged for {scope}"));
            res.next = Some(
                "stop acting on this target; only the operator can clear the kill switch".into(),
            );
            broker.counters.record(st::KILLED);
            journal_line(broker, peer, &req, &win, &res, None, None).await;
            return Ok(res);
        }
    }

    // -- 2. observe ---------------------------------------------------------
    let before_obs = observe_with_fallback(
        broker,
        class,
        None,
        req.window.as_deref(),
        Duration::from_millis(req.timeout_ms.min(10_000)),
    )
    .await;
    let (obs_source, before) = match before_obs {
        Ok(v) => v,
        Err(e) => return Err(e),
    };
    let url = before.url.clone();
    let generation = broker.next_generation();
    *broker.last_observation.write().await =
        Some(ObservationRecord { elements: before.elements.clone() });

    // A pinned target is checked against what is on screen *now*, not against
    // the generation the agent saw. Backends mint fresh ids each observation,
    // so demanding the old generation would reject every pin; demanding the
    // element still exists is the check that actually matters.
    let pinned: Option<Element> = match &req.target_id {
        Some(id) => match before.elements.iter().find(|e| &e.id == id) {
            Some(e) => Some(e.clone()),
            None => {
                return Err(WireError::with_detail(
                    codes::STALE_ID,
                    format!("element {id:?} is not on screen any more; observe again and re-pin"),
                    json!({"gen": generation, "elements_now": before.elements.len()}),
                ))
            }
        },
        None => None,
    };

    // -- 3. gate ------------------------------------------------------------
    // Judge the target that answered, which for a browser backend is the tab
    // it reported rather than whatever happens to hold focus.
    let (gate_target, effective_class) = effective_target(&win, &before, &obs_source);
    let verdict = {
        let mut gate = broker.gate.lock().await;
        gate.check(&req.instruction, &gate_target)
    };
    let gate_report = verdict.report();

    match verdict.category {
        kratos_gate::Category::Block | kratos_gate::Category::RateLimited => {
            let mut res = ActRes::new(st::BLOCKED, elapsed());
            res.gate = Some(gate_report);
            res.gen = Some(generation);
            res.detail = Some(format!(
                "the gate refused this action ({})",
                verdict.rule.clone().unwrap_or_else(|| "denylisted".into())
            ));
            res.next = Some(if verdict.category == kratos_gate::Category::RateLimited {
                "wait and retry; the per-minute action limit was reached".into()
            } else {
                "there is no confirmation path for a denylisted action; ask the operator".into()
            });
            broker.counters.record(st::BLOCKED);
            journal_line(broker, peer, &req, &win, &res, None, None).await;
            return Ok(res);
        }
        kratos_gate::Category::Killed => {
            let mut res = ActRes::new(st::KILLED, elapsed());
            res.gate = Some(gate_report);
            broker.counters.record(st::KILLED);
            journal_line(broker, peer, &req, &win, &res, None, None).await;
            return Ok(res);
        }
        kratos_gate::Category::Review if approved_rule.is_none() && !req.dry_run => {
            let (token, ttl) = broker
                .park(Parked {
                    instruction: req.instruction.clone(),
                    window: req.window.clone(),
                    rule: verdict.rule.clone(),
                    requires_os_dialog: verdict.requires_os_dialog,
                    peer_id: peer.peer_id.clone(),
                    foreground_hwnd: classify::foreground().map(|w| w.hwnd).unwrap_or(0),
                    created: Instant::now(),
                })
                .await;
            let mut res = ActRes::new(st::NEEDS_CONFIRMATION, elapsed());
            res.gate = Some(gate_report);
            res.gen = Some(generation);
            res.confirm_token = Some(token);
            res.expires_in_ms = Some(ttl);
            res.detail = Some(format!(
                "this action needs approval ({}){}",
                verdict.rule.clone().unwrap_or_else(|| "review".into()),
                if verdict.requires_os_dialog {
                    " and must be clicked by a human in a native dialog"
                } else {
                    ""
                }
            ));
            res.next = Some(
                "nothing has run. Ask the operator, then call confirm with this token -- and do \
                 something else meanwhile rather than waiting"
                    .into(),
            );
            broker.counters.record(st::NEEDS_CONFIRMATION);
            journal_line(broker, peer, &req, &win, &res, None, None).await;
            return Ok(res);
        }
        _ => {}
    }

    // -- 4. decide ----------------------------------------------------------
    let resolved = decide(
        broker,
        &req.instruction,
        &win,
        url.as_deref(),
        effective_class,
        &before.elements,
        pinned.as_ref(),
    )
    .await?;

    if resolved.decision.confidence < resolved.decision.threshold {
        let mut res = ActRes::new(st::REFUSED, elapsed());
        res.gen = Some(generation);
        res.gate = Some(gate_report);
        res.decision = Some(resolved.decision.clone());
        match &resolved.decision.ambiguous_with {
            // Torn: say between what, because that is the whole fix.
            Some(tied) => {
                res.refusal = Some("ambiguous".into());
                res.detail = Some(format!(
                    "two targets scored alike and the model could not separate them: {}",
                    tied.iter().map(|t| format!("{t:?}")).collect::<Vec<_>>().join(" and ")
                ));
                res.next = Some(
                    "name the one you mean by its container -- the dialog, pane, form or                      section it is in -- or pin it: observe, then pass that element's id as                      target_id"
                        .into(),
                );
            }
            None => {
                res.refusal = Some("unsure".into());
                res.detail = Some(format!(
                    "the decision model is not sure enough: {:.2} against a bar of {:.2} for a {}",
                    resolved.decision.confidence, resolved.decision.threshold, resolved.decision.kind
                ));
                res.next = Some(targeting_advice(broker, &obs_source).await);
            }
        }
        broker.counters.record(st::REFUSED);
        journal_line(broker, peer, &req, &win, &res, Some(&resolved.decision), None).await;
        return Ok(res);
    }

    if resolved.kind == Kind::None {
        let mut res = ActRes::new(st::REFUSED, elapsed());
        res.gen = Some(generation);
        res.gate = Some(gate_report);
        res.decision = Some(resolved.decision.clone());
        res.refusal = Some("no_such_action".into());
        res.detail =
            Some("this instruction does not describe an action available on this screen".into());
        res.next = Some("re-read the screen with observe and choose a different step".into());
        broker.counters.record(st::REFUSED);
        journal_line(broker, peer, &req, &win, &res, Some(&resolved.decision), None).await;
        return Ok(res);
    }

    // -- 5. build the concrete action --------------------------------------
    let payload = match build_action(resolved.kind, &req.instruction, resolved.target.as_ref()) {
        Ok(p) => p,
        Err(reason) => {
            let mut res = ActRes::new(st::REFUSED, elapsed());
            res.gen = Some(generation);
            res.gate = Some(gate_report);
            res.decision = Some(resolved.decision.clone());
            res.detail = Some(reason);
            res.next = Some(
                "nothing has run. Rewrite the instruction so it carries the literal it needs, \
                 putting any text to type in quotes"
                    .into(),
            );
            broker.counters.record(st::REFUSED);
            journal_line(broker, peer, &req, &win, &res, Some(&resolved.decision), None).await;
            return Ok(res);
        }
    };

    if req.dry_run {
        let mut res = ActRes::new(st::PLANNED, elapsed());
        res.gen = Some(generation);
        res.gate = Some(gate_report);
        res.decision = Some(resolved.decision.clone());
        res.backend = Some(obs_source);
        res.detail = Some("dry run: decided and gated, nothing dispatched".into());
        res.next = Some("call act again without dry_run to carry this out".into());
        broker.counters.record(st::PLANNED);
        return Ok(res);
    }

    // -- 6. lease the input devices ----------------------------------------
    let (lease_token, auto_lease) = match &req.lease {
        Some(t) => {
            broker.check_lease(t, &peer.peer_id).await?;
            (t.clone(), false)
        }
        None => {
            let grant = broker
                .acquire_lease(
                    &peer.peer_id,
                    "input",
                    Duration::from_millis(req.timeout_ms.max(5_000)),
                    Duration::ZERO,
                )
                .await?;
            (grant.token, true)
        }
    };

    // -- 7. baselines, then act, then look again ---------------------------
    let region = resolved.target.as_ref().and_then(|e| e.bbox);
    let window_rect = win.rect;

    // Three screen grabs for the whole action, not eight.
    //
    // Two here, 35ms apart: their difference is the ambient drift, and the
    // second one is also the "before" state -- a baseline measures t0 against
    // t1, and before *is* t1. Both rectangles are cropped from the same frame,
    // so the window and region numbers describe the same instant.
    let (window_baseline, region_baseline, shot_before) =
        tokio::task::spawn_blocking(move || {
            let first = kratos_shot::capture();
            std::thread::sleep(DRIFT_GAP);
            let second = kratos_shot::capture();

            let crop = |s: &Option<kratos_shot::Screenshot>| {
                (
                    s.as_ref().and_then(|f| f.region(window_rect, kratos_shot::FULL_W, kratos_shot::FULL_H)),
                    region.and_then(|r| {
                        s.as_ref()
                            .and_then(|f| f.region(Some(r), kratos_shot::REGION_N, kratos_shot::REGION_N))
                    }),
                )
            };
            let (wa, ra) = crop(&first);
            let (wb, rb) = crop(&second);
            (
                kratos_shot::changed(wa.as_ref(), wb.as_ref()),
                kratos_shot::changed(ra.as_ref(), rb.as_ref()),
                (wb, rb),
            )
        })
        .await
        .unwrap_or((None, None, (None, None)));

    let dispatch = execute(broker, effective_class, None, payload).await;

    if auto_lease {
        broker.release_lease(&lease_token).await;
    }

    let (act_source, backend_res) = match dispatch {
        Ok(v) => v,
        Err(e) => {
            journal_line(broker, peer, &req, &win, &ActRes::new("error", elapsed()), Some(&resolved.decision), None)
                .await;
            return Err(e);
        }
    };

    // -- 7b. wait for the screen to move, not for a fixed interval ---------
    //
    // The old build slept 350ms here whatever happened, which was 350ms of a
    // 484ms action and was paid in full by a button that responded in 40ms.
    // Now the target's own pixels are polled against the drift measured a
    // moment ago, and the wait ends when they move. An inert control still
    // costs the ceiling -- that case has to be waited out before `no_effect`
    // can honestly be claimed.
    let budget = Duration::from_millis(broker.config.read().await.settle_max_ms);
    let before_w = shot_before.0.clone();
    let before_r = shot_before.1.clone();
    let (shot_after, settle_ms, settled) = tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        let crop = |s: &Option<kratos_shot::Screenshot>| {
            (
                s.as_ref().and_then(|f| f.region(window_rect, kratos_shot::FULL_W, kratos_shot::FULL_H)),
                region.and_then(|r| {
                    s.as_ref()
                        .and_then(|f| f.region(Some(r), kratos_shot::REGION_N, kratos_shot::REGION_N))
                }),
            )
        };
        loop {
            let shot = kratos_shot::capture();
            let last = crop(&shot);
            let w = kratos_shot::assess(
                kratos_shot::changed(before_w.as_ref(), last.0.as_ref()),
                window_baseline,
                kratos_shot::FULL_THRESHOLD,
            );
            let r = kratos_shot::assess(
                kratos_shot::changed(before_r.as_ref(), last.1.as_ref()),
                region_baseline,
                kratos_shot::REGION_THRESHOLD,
            );
            if w.is_changed() || r.is_changed() {
                // Let it finish, then take the frame the evidence is built from.
                std::thread::sleep(SETTLE_CONFIRM);
                let shot = kratos_shot::capture();
                let settled = crop(&shot);
                return (settled, started.elapsed().as_millis() as u64, true);
            }
            if started.elapsed() >= budget {
                return (last, started.elapsed().as_millis() as u64, false);
            }
            std::thread::sleep(SETTLE_POLL);
        }
    })
    .await
    .unwrap_or(((None, None), 0, false));

    let after = observe_with_fallback(broker, class, None, req.window.as_deref(), Duration::from_millis(8_000))
        .await
        .map(|(_, o)| o)
        .unwrap_or_default();

    // -- 8. prove it ---------------------------------------------------------
    let expected = extract::quoted(&req.instruction);
    let effect = assemble_effect(
        &before,
        &after,
        (shot_before.0.as_ref(), shot_after.0.as_ref(), window_baseline),
        (shot_before.1.as_ref(), shot_after.1.as_ref(), region_baseline),
        backend_res.readback.clone(),
        expected.as_deref(),
    );

    let mut effect = effect;
    effect.settle_ms = Some(settle_ms);
    effect.settled = Some(settled);
    let proved = effect.evidence != "none" && effect.evidence != "inconclusive";
    let status = if resolved.kind.is_mutating() && !proved { st::NO_EFFECT } else { st::EXECUTED };

    let mut res = ActRes::new(status, elapsed());
    res.gen = Some(generation);
    res.gate = Some(gate_report);
    res.decision = Some(resolved.decision.clone());
    res.effect = Some(effect.clone());
    res.backend = Some(act_source);
    res.method = backend_res.method.clone();
    if status == st::NO_EFFECT {
        res.detail = Some(if effect.evidence == "inconclusive" {
            "the action was dispatched, and the screen was too busy for the pixels to say whether \
             anything changed. Nothing in the accessibility tree changed either."
                .into()
        } else {
            "the action was dispatched but nothing changed -- the click probably missed, or the \
             control was inert"
                .into()
        });
        res.next = Some(
            "do not assume this worked. Call verify with a claim about what should now be true, \
             or observe again and pick the target more specifically"
                .into(),
        );
    }
    broker.counters.record(status);
    journal_line(broker, peer, &req, &win, &res, Some(&resolved.decision), Some(&effect)).await;
    Ok(res)
}

/// The one window this request is about, or an error that says why not.
///
/// A `window` string that matches two different applications is refused rather
/// than resolved. Several windows of one program are fine -- they are the same
/// program. Windows of different programs mean the string did not say which
/// was meant, and the failure that matters there is not "no match", it is
/// acting on the wrong one: a search for "Upwork" once resolved to an editor
/// whose session was named after the job rather than to the browser showing
/// the site, and nothing in the result said so.
fn target_window(window: Option<&str>) -> Result<WindowRef, WireError> {
    match classify::resolve_window(window) {
        classify::Resolution::One(w) => Ok(w),
        classify::Resolution::Ambiguous(all) => {
            let listed: Vec<String> =
                all.iter().take(6).map(|w| format!("{:?} ({})", w.title, w.exe)).collect();
            Err(WireError::with_detail(
                codes::AMBIGUOUS_WINDOW,
                format!(
                    "{:?} matches windows of {} different applications, so it does not say \
                     which one you mean: {}",
                    window.unwrap_or(""),
                    all.iter()
                        .map(|w| w.exe.to_ascii_lowercase())
                        .collect::<std::collections::BTreeSet<_>>()
                        .len(),
                    listed.join("; ")
                ),
                json!({ "candidates": all.iter().map(|w| json!({
                    "title": w.title, "exe": w.exe, "class": w.class().as_str()
                })).collect::<Vec<_>>() }),
            ))
        }
        classify::Resolution::NotFound => Err(WireError::new(
            codes::NO_TARGET,
            match window {
                Some(w) => format!("no visible window matches {w:?}"),
                None => "there is no foreground window".to_string(),
            },
        )),
    }
}

/// Collect the evidence and name which sources agreed.
fn assemble_effect(
    before: &BackendObservation,
    after: &BackendObservation,
    window: (Option<&kratos_shot::Fingerprint>, Option<&kratos_shot::Fingerprint>, Option<f64>),
    region: (Option<&kratos_shot::Fingerprint>, Option<&kratos_shot::Fingerprint>, Option<f64>),
    readback: Option<String>,
    expected_text: Option<&str>,
) -> Effect {
    let structural = crate::diff::diff(before, after);

    let window_changed = kratos_shot::changed(window.0, window.1);
    let region_changed = kratos_shot::changed(region.0, region.1);
    let window_verdict = kratos_shot::assess(window_changed, window.2, kratos_shot::FULL_THRESHOLD);
    let region_verdict = kratos_shot::assess(region_changed, region.2, kratos_shot::REGION_THRESHOLD);

    let readback_matches = match (&readback, expected_text) {
        (Some(actual), Some(want)) if !want.is_empty() => {
            let a = actual.trim();
            let w = want.trim();
            // A long paste may be truncated by the control, so a prefix match
            // counts. An unreadable value is `None` and proves nothing either
            // way.
            a.contains(w) || (!a.is_empty() && w.starts_with(a) && a.len() >= 8)
        }
        _ => false,
    };

    let mut sources: Vec<&str> = Vec::new();
    if structural.touched() {
        sources.push("structural");
    }
    if readback_matches {
        sources.push("readback");
    }
    if window_verdict.is_changed() {
        sources.push("window");
    }
    if region_verdict.is_changed() {
        sources.push("region");
    }

    let evidence = if !sources.is_empty() {
        sources.join("+")
    } else if window_verdict == kratos_shot::PixelEvidence::Inconclusive
        && region_verdict == kratos_shot::PixelEvidence::Inconclusive
    {
        "inconclusive".to_string()
    } else {
        "none".to_string()
    };

    Effect {
        structural,
        window_changed,
        window_baseline: window.2,
        screen_changed: None,
        region_changed,
        region_baseline: region.2,
        // Filled in by the caller, which is the only place that knows how
        // long it waited.
        settle_ms: None,
        settled: None,
        readback,
        evidence,
    }
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

pub async fn verify(broker: &Arc<Broker>, req: VerifyReq) -> Result<VerifyRes, WireError> {
    let started = Instant::now();
    broker.mark_agent_activity();

    let win = target_window(req.window.as_deref())?;
    let class = win.class();
    let (_, obs) = observe_with_fallback(
        broker,
        class,
        None,
        req.window.as_deref(),
        Duration::from_millis(8_000),
    )
    .await?;

    // Screen text enters the model here as *data* inside a state object, to be
    // matched against a claim. It is never treated as instructions, and no
    // question asks the model what to do next.
    let visible: Vec<String> = obs
        .elements
        .iter()
        .take(120)
        .map(|e| {
            let v = e.value.as_deref().unwrap_or("");
            if v.is_empty() {
                e.criterion()
            } else {
                format!("{} = {:?}", e.criterion(), truncate(v, 80))
            }
        })
        .collect();

    let state = json!({
        "window": win.title,
        "url": obs.url,
        "visible": visible,
    });
    let mut questions: BTreeMap<String, Question> = BTreeMap::new();
    questions.insert(
        "holds".to_string(),
        Question::Noul {
            instructions: format!(
                "Does what is currently on screen confirm this claim: {:?}",
                req.claim
            ),
        },
    );

    let decider = broker.decider.read().await.clone();
    let answers = tokio::task::spawn_blocking(move || decider.ask(&state, &questions))
        .await
        .map_err(|e| WireError::new(codes::INTERNAL, format!("decider task failed: {e}")))?
        .map_err(|e| WireError::new(e.code(), e.to_string()))?;

    let confidence = answers.noul("holds").unwrap_or(0.0);
    Ok(VerifyRes {
        holds: confidence >= 0.5,
        confidence,
        decider: answers.model,
        took_ms: started.elapsed().as_millis() as u64,
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "\u{2026}"
}

// ---------------------------------------------------------------------------
// Journal
// ---------------------------------------------------------------------------

async fn journal_line(
    broker: &Arc<Broker>,
    peer: &Arc<PeerHandle>,
    req: &ActReq,
    win: &WindowRef,
    res: &ActRes,
    decision: Option<&Decision>,
    effect: Option<&Effect>,
) {
    let sensitive = decision
        .and_then(|d| d.target_id.as_ref())
        .and_then(|id| {
            broker
                .last_observation
                .try_read()
                .ok()
                .and_then(|o| o.as_ref().and_then(|r| r.element(id).map(|e| e.sensitive)))
        })
        .unwrap_or(false);

    let mut record = json!({
        "peer_id": peer.peer_id,
        "agent": peer.name,
        "op": "act",
        "instruction": kratos_gate::redact_for_log(&req.instruction, sensitive),
        "window": win.title,
        "exe": win.exe,
        "target_class": win.class().as_str(),
        "gen": res.gen,
        "status": res.status,
        "backend": res.backend,
        "took_ms": res.took_ms,
    });
    let obj = record.as_object_mut().expect("object");
    // How it was delivered and, when refused, which kind of refusal. Both are
    // things you want to count across a session: a run of `physical` says a
    // backend is not finding patterns, and a run of `ambiguous` says the
    // screen has look-alike controls rather than that the agent is vague.
    if let Some(m) = &res.method {
        obj.insert("method".into(), json!(m));
    }
    if let Some(r) = &res.refusal {
        obj.insert("refusal".into(), json!(r));
    }
    if let Some(g) = &res.gate {
        obj.insert("gate".into(), json!(g.rule));
        obj.insert("gate_category".into(), json!(g.category));
    }
    if let Some(d) = decision {
        if let Some(tied) = &d.ambiguous_with {
            obj.insert("ambiguous_with".into(), json!(tied));
        }
        obj.insert("kind".into(), json!(d.kind));
        obj.insert("target_id".into(), json!(d.target_id));
        obj.insert("target_label".into(), json!(d.target_label));
        obj.insert("confidence".into(), json!(d.confidence));
        obj.insert("kind_confidence".into(), json!(d.kind_confidence));
        obj.insert("target_confidence".into(), json!(d.target_confidence));
        obj.insert("threshold".into(), json!(d.threshold));
        obj.insert("decider".into(), json!(d.decider));
        obj.insert("decide_ms".into(), json!(d.decide_ms));
    }
    if let Some(e) = effect {
        obj.insert("structural".into(), json!(e.structural));
        obj.insert("window_changed".into(), json!(e.window_changed));
        obj.insert("window_baseline".into(), json!(e.window_baseline));
        obj.insert("region_changed".into(), json!(e.region_changed));
        obj.insert("evidence".into(), json!(e.evidence));
        obj.insert("settle_ms".into(), json!(e.settle_ms));
        obj.insert("settled".into(), json!(e.settled));
    }
    broker.journal.read().await.append(record);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(id: &str, role: &str, label: &str) -> Element {
        Element { id: id.into(), role: role.into(), label: label.into(), ..Default::default() }
    }

    // -- build_action -------------------------------------------------------

    #[test]
    fn typing_without_quotes_is_refused_not_guessed() {
        let target = el("e1", "textbox", "Subject");
        let err = build_action(Kind::Type, "put something sensible in the subject", Some(&target))
            .unwrap_err();
        assert!(err.contains("quotes"), "{err}");
    }

    #[test]
    fn typing_with_quotes_carries_the_literal() {
        let target = el("e1", "textbox", "Subject");
        let p = build_action(Kind::Type, "type \"Hello there\" in Subject", Some(&target)).unwrap();
        assert_eq!(p["kind"], json!("type"));
        assert_eq!(p["text"], json!("Hello there"));
        assert_eq!(p["target"]["id"], json!("e1"));
    }

    #[test]
    fn a_click_needs_a_target_and_says_so_when_it_has_none() {
        let err = build_action(Kind::Click, "click the Send button", None).unwrap_err();
        assert!(err.contains("needs an element"), "{err}");
    }

    #[test]
    fn navigate_and_open_need_no_target_so_an_empty_screen_cannot_block_them() {
        // The short-circuit bug: an unreadable screen used to stop a launch
        // that never needed an element in the first place.
        let p = build_action(Kind::Navigate, "go to https://instagram.com", None).unwrap();
        assert_eq!(p["url"], json!("https://instagram.com"));
        let p = build_action(Kind::Open, "open instagram in brave", None).unwrap();
        assert_eq!(p["text"], json!("instagram"));
    }

    #[test]
    fn a_key_action_normalises_the_shortcut() {
        let p = build_action(Kind::Key, "press Ctrl+Shift+N", None).unwrap();
        assert_eq!(p["key"], json!("ctrl+shift+n"));
    }

    #[test]
    fn scroll_carries_a_signed_amount() {
        let t = el("e1", "list", "Inbox");
        assert_eq!(build_action(Kind::Scroll, "scroll up 4", Some(&t)).unwrap()["amount"], json!(-4));
        assert_eq!(build_action(Kind::Scroll, "scroll down", Some(&t)).unwrap()["amount"], json!(3));
    }

    #[test]
    fn the_target_payload_carries_selector_and_bbox_together() {
        // The backend picks which to trust: dom-ext must use the selector and
        // never the bbox, uia the other way round.
        let mut t = el("brave:w3:t17:e42", "textbox", "To recipients");
        t.selector = Some("div[role=dialog] input".into());
        t.bbox = Some([412, 268, 300, 22]);
        let p = build_action(Kind::Click, "click the To field", Some(&t)).unwrap();
        assert_eq!(p["target"]["selector"], json!("div[role=dialog] input"));
        assert_eq!(p["target"]["bbox"], json!([412, 268, 300, 22]));
        assert_eq!(p["target"]["id"], json!("brave:w3:t17:e42"));
    }

    // -- how options are named to the model ---------------------------------

    fn cands() -> Vec<kratos_decide::Candidate> {
        vec![
            kratos_decide::Candidate { id: "brave:w1:t2:f0:e7".into(), criterion: "combobox \"Search\" in form".into() },
            kratos_decide::Candidate { id: "brave:w1:t2:f0:e9".into(), criterion: "button \"Search\" in search".into() },
            kratos_decide::Candidate {
                id: kratos_decide::NONE_OF_THESE.into(),
                criterion: "None of the elements listed above".into(),
            },
        ]
    }

    #[test]
    fn options_are_short_ordinals_not_element_ids() {
        let c = cands();
        assert_eq!(option_key(0, &c[0]), "1");
        assert_eq!(option_key(1, &c[1]), "2");
        assert_eq!(option_key(2, &c[2]), "none");
        // The thing that broke it live: no routing plumbing in an option name.
        for (i, cand) in c.iter().enumerate() {
            let key = option_key(i, cand);
            assert!(key.len() <= 4, "{key:?} is too long to be a clean option");
            assert!(!key.contains(':'), "{key:?} leaks the id namespace");
        }
    }

    #[test]
    fn an_ordinal_maps_back_to_the_right_element() {
        let c = cands();
        assert_eq!(resolve_option("1", &c).as_deref(), Some("brave:w1:t2:f0:e7"));
        assert_eq!(resolve_option("2", &c).as_deref(), Some("brave:w1:t2:f0:e9"));
        assert_eq!(resolve_option(" 2 ", &c).as_deref(), Some("brave:w1:t2:f0:e9"));
    }

    #[test]
    fn the_escape_hatch_resolves_to_no_target() {
        let c = cands();
        assert_eq!(resolve_option("none", &c), None);
        assert_eq!(resolve_option("NONE", &c), None);
        assert_eq!(resolve_option("3", &c), None, "the hatch by its ordinal is still the hatch");
        assert_eq!(resolve_option(kratos_decide::NONE_OF_THESE, &c), None);
    }

    #[test]
    fn an_answer_that_names_nothing_offered_is_not_a_target() {
        // Guessing the nearest option is how a confident answer lands on the
        // wrong control.
        let c = cands();
        assert_eq!(resolve_option("99", &c), None);
        assert_eq!(resolve_option("0", &c), None);
        assert_eq!(resolve_option("-1", &c), None);
        assert_eq!(resolve_option("the search box", &c), None);
        assert_eq!(resolve_option("", &c), None);
    }

    // -- advice on a refusal ------------------------------------------------

    #[tokio::test]
    async fn a_refusal_mentions_the_window_hint_only_when_it_would_help() {
        use crate::state::Broker;
        let mut cfg = kratos_protocol::config::Config::default();
        cfg.decider.provider = "mock".into();
        let broker = Broker::new(cfg, String::new());

        // No browser backend connected: the generic advice is all there is.
        let plain = targeting_advice(&broker, "uia").await;
        assert!(plain.contains("Name the target more specifically"));
        assert!(!plain.contains("pass `window`"), "{plain}");

        // With one connected and unused, say so -- rewording would not have
        // fixed the real problem.
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        Box::leak(Box::new(rx));
        broker
            .add_peer(std::sync::Arc::new(crate::state::PeerHandle {
                peer_id: "b-1".into(),
                role: kratos_protocol::Role::Backend,
                name: "ext".into(),
                source: Some(Source::DomExt),
                handles: vec!["browser".into()],
                out: tx,
                pending: tokio::sync::Mutex::new(std::collections::HashMap::new()),
                connected_at: std::time::Instant::now(),
                last_seen: std::sync::atomic::AtomicU64::new(0),
            }))
            .await;

        let hinted = targeting_advice(&broker, "uia").await;
        assert!(hinted.contains("pass `window`"), "{hinted}");

        // It did answer, so there is nothing to suggest.
        let used = targeting_advice(&broker, "dom-ext").await;
        assert!(!used.contains("pass `window`"), "{used}");
        // A fallback chain is judged by whoever answered last.
        let fell_back = targeting_advice(&broker, "dom-ext>uia").await;
        assert!(fell_back.contains("pass `window`"), "{fell_back}");
    }

    // -- whose window is being judged ---------------------------------------

    fn win_ref(title: &str, exe: &str) -> WindowRef {
        WindowRef { hwnd: 1, title: title.into(), exe: exe.into(), pid: 1, rect: None }
    }

    #[test]
    fn a_dom_ext_answer_is_gated_against_the_browser_that_answered() {
        // The live failure: foreground was the Windows Notification Center
        // while dom-ext reported a Brave tab.
        let foreground = win_ref("Notification Center", "explorer.exe");
        let observed = BackendObservation {
            window: "Inbox - Gmail".into(),
            url: Some("https://mail.google.com/".into()),
            elements: vec![Element { id: "brave:w3:t17:f0:e4".into(), ..Default::default() }],
            truncated: false,
        };
        let (target, class) = effective_target(&foreground, &observed, "dom-ext");

        assert_eq!(class, TargetClass::Browser, "a browser answered, so it is a browser target");
        assert_eq!(target.brand.as_deref(), Some("brave"));
        assert_eq!(target.exe.as_deref(), Some("brave.exe"));
        assert_eq!(target.window_title, "Inbox - Gmail");
        // The thing that was broken: a kill scope on Brave must now cover it.
        assert!(target.scopes().contains(&"browser:brave".to_string()));

        let mut kill = kratos_gate::KillSwitch::default();
        kill.engage("browser:brave");
        assert_eq!(kill.covers(&target), Some("browser:brave".to_string()));
    }

    #[test]
    fn a_native_answer_still_uses_the_foreground_window() {
        let foreground = win_ref("Untitled - Notepad", "notepad.exe");
        let observed = BackendObservation {
            window: "Untitled - Notepad".into(),
            url: None,
            elements: vec![Element { id: "uia:1a2b:3".into(), ..Default::default() }],
            truncated: false,
        };
        let (target, class) = effective_target(&foreground, &observed, "uia");
        assert_eq!(class, TargetClass::Native);
        assert_eq!(target.exe.as_deref(), Some("notepad.exe"));
        assert_eq!(target.brand, None);
    }

    #[test]
    fn a_fallback_chain_is_judged_by_whoever_answered_last() {
        let foreground = win_ref("Notification Center", "explorer.exe");
        let observed = BackendObservation {
            window: "whatever".into(),
            url: None,
            elements: vec![Element { id: "uia:ff:1".into(), ..Default::default() }],
            truncated: false,
        };
        // dom-ext was tried and failed; uia answered. Judge uia's world.
        let (target, class) = effective_target(&foreground, &observed, "dom-ext>uia");
        assert_eq!(class, TargetClass::Native);
        assert_eq!(target.exe.as_deref(), Some("explorer.exe"));
    }

    #[test]
    fn a_dom_ext_answer_with_no_elements_keeps_the_foreground_title() {
        let foreground = win_ref("Brave", "brave.exe");
        let observed = BackendObservation {
            window: "   ".into(),
            url: Some("https://example.com/".into()),
            elements: vec![],
            truncated: false,
        };
        let (target, class) = effective_target(&foreground, &observed, "dom-ext");
        assert_eq!(class, TargetClass::Browser);
        assert_eq!(target.window_title, "Brave");
        // No elements means no id to read a brand out of; the URL still gates.
        assert_eq!(target.brand, None);
        assert_eq!(target.domain().as_deref(), Some("example.com"));
    }

    #[test]
    fn a_malformed_id_prefix_does_not_become_a_brand() {
        let foreground = win_ref("x", "x.exe");
        let observed = BackendObservation {
            window: "t".into(),
            url: None,
            elements: vec![Element { id: "../..:w1:t1:f0:e0".into(), ..Default::default() }],
            truncated: false,
        };
        let (target, _) = effective_target(&foreground, &observed, "dom-ext");
        assert_eq!(target.brand, None, "only a plain alphanumeric prefix is a brand");
    }

    // -- effect assembly ----------------------------------------------------

    fn obs(window: &str, els: Vec<Element>) -> BackendObservation {
        BackendObservation { window: window.into(), url: None, elements: els, truncated: false }
    }

    fn fp(v: u8) -> kratos_shot::Fingerprint {
        kratos_shot::Fingerprint { cells: vec![v; 2304], w: 64, h: 36 }
    }

    #[test]
    fn structural_change_alone_proves_the_action() {
        let before = obs("G", vec![el("e1", "button", "Send")]);
        let after = obs("G", vec![]);
        let e = assemble_effect(&before, &after, (None, None, None), (None, None, None), None, None);
        assert_eq!(e.evidence, "structural");
    }

    #[test]
    fn with_no_pixels_at_all_the_answer_is_inconclusive_not_none() {
        // "We did not look" and "we looked and nothing moved" are different
        // statements, and only the second is evidence.
        let same = obs("G", vec![el("e1", "button", "Send")]);
        let e = assemble_effect(&same, &same, (None, None, None), (None, None, None), None, None);
        assert_eq!(e.evidence, "inconclusive");
        assert!(e.window_changed.is_none());
    }

    #[test]
    fn nothing_anywhere_is_none_and_that_means_no_effect() {
        let same = obs("G", vec![el("e1", "button", "Send")]);
        let quiet_before = fp(100);
        let quiet_after = fp(100);
        let e = assemble_effect(
            &same,
            &same,
            (Some(&quiet_before), Some(&quiet_after), Some(0.0)),
            (None, None, None),
            None,
            None,
        );
        assert_eq!(e.evidence, "none");
    }

    #[test]
    fn a_busy_screen_with_no_structural_change_is_inconclusive_not_success() {
        // The measured case: ambient drift above the ceiling. Reporting
        // `executed` here is how a missed click becomes a lie.
        let same = obs("G", vec![el("e1", "button", "Send")]);
        let a = fp(10);
        let b = fp(200);
        let e = assemble_effect(
            &same,
            &same,
            (Some(&a), Some(&b), Some(0.4)),
            (None, None, None),
            None,
            None,
        );
        assert_eq!(e.evidence, "inconclusive");
    }

    #[test]
    fn readback_proves_a_type_even_with_no_visible_change() {
        let same = obs("G", vec![el("e1", "textbox", "Subject")]);
        let e = assemble_effect(
            &same,
            &same,
            (None, None, None),
            (None, None, None),
            Some("Order-taking AI agents".into()),
            Some("Order-taking AI agents"),
        );
        assert_eq!(e.evidence, "readback");
    }

    #[test]
    fn readback_of_the_wrong_text_proves_nothing() {
        // The failure this exists to catch: text landed, but somewhere else.
        let same = obs("G", vec![el("e1", "textbox", "Subject")]);
        let quiet = fp(100);
        let e = assemble_effect(
            &same,
            &same,
            (Some(&quiet), Some(&quiet), Some(0.0)),
            (None, None, None),
            Some("something entirely different".into()),
            Some("Order-taking AI agents"),
        );
        assert_eq!(e.evidence, "none");
        assert!(!e.evidence.contains("readback"));
    }

    #[test]
    fn an_unreadable_value_is_not_counted_against_the_action() {
        let same = obs("G", vec![el("e1", "textbox", "Subject")]);
        let quiet = fp(100);
        let e = assemble_effect(
            &same,
            &same,
            (Some(&quiet), Some(&quiet), Some(0.0)),
            (None, None, None),
            None,
            Some("whatever"),
        );
        // No readback evidence, but no contradiction either.
        assert_eq!(e.evidence, "none");
        assert!(e.readback.is_none());
    }

    #[test]
    fn region_evidence_alone_is_enough_for_the_canvas_case() {
        let same = obs("G", vec![el("e1", "canvas", "Drawing")]);
        let mut before = kratos_shot::Fingerprint { cells: vec![235; 9216], w: 96, h: 96 };
        let mut after = before.clone();
        // ~20 cells changed: one glyph.
        for c in after.cells.iter_mut().take(20) {
            *c = 0;
        }
        before.cells[0] = 235;
        let e = assemble_effect(
            &same,
            &same,
            (None, None, None),
            (Some(&before), Some(&after), Some(0.0)),
            None,
            None,
        );
        assert_eq!(e.evidence, "region");
        assert!(e.region_changed.unwrap() >= kratos_shot::REGION_THRESHOLD);
    }

    #[test]
    fn several_agreeing_sources_are_all_named() {
        let before = obs("G", vec![el("e1", "button", "Send")]);
        let after = obs("Notepad", vec![]);
        let a = fp(10);
        let b = fp(200);
        let e = assemble_effect(
            &before,
            &after,
            (Some(&a), Some(&b), Some(0.0)),
            (None, None, None),
            None,
            None,
        );
        assert_eq!(e.evidence, "structural+window");
    }

    #[test]
    fn baselines_are_reported_so_a_reader_can_judge_the_delta() {
        let same = obs("G", vec![]);
        let a = fp(100);
        let e = assemble_effect(
            &same,
            &same,
            (Some(&a), Some(&a), Some(0.0123)),
            (None, None, None),
            None,
            None,
        );
        assert_eq!(e.window_baseline, Some(0.0123));
        assert_eq!(e.window_changed, Some(0.0));
    }

    #[test]
    fn truncate_keeps_short_strings_whole() {
        assert_eq!(truncate("short", 80), "short");
        assert_eq!(truncate(&"x".repeat(100), 10).chars().count(), 11);
    }
}
