//! The UI's connection to the broker, on a thread of its own.
//!
//! egui redraws on the main thread and must never wait on I/O, so everything
//! that touches the pipe happens here and the two sides meet at a mutex
//! holding the latest snapshot. A settings window that freezes while a broker
//! is slow is a settings window nobody trusts to stop anything.
//!
//! The worker *attaches*; it does not start a broker unless asked. Opening the
//! settings window should not bring up a background process, and a UI that
//! auto-started one would also keep it alive forever, because an attached UI
//! suppresses the idle timeout.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kratos_protocol::{ops, Role};
use serde_json::{json, Value};
use tokio::sync::mpsc;

const BUILD: &str = concat!("kratos-ui ", env!("CARGO_PKG_VERSION"));

#[derive(Debug)]
pub enum Cmd {
    /// Bring a broker up if none is running.
    StartBroker,
    /// Ask the running broker to exit.
    StopBroker,
    ApplyConfig(Value),
    /// Pair or unpair a backend instance with a browser profile.
    ///
    /// Its own command rather than a config patch, because `instance.bind`
    /// is the op the broker exposes for it and it validates the brand --
    /// writing the list by hand from here would be a second implementation
    /// of a rule that already exists.
    BindInstance(Value),
    ReloadConfig,
    Kill(String),
    ResetKill(String),
    TailLog(usize),
}

#[derive(Default)]
pub struct Shared {
    pub connected: bool,
    pub status: Option<Value>,
    pub config: Option<Value>,
    /// What `lane.list` says: the browsers on this machine, their profiles,
    /// the attached instances and every session's lane.
    ///
    /// Read from the broker rather than from this process, because the broker
    /// is the one on the machine being driven -- and because two readers of
    /// `Local State` would be two things to keep in step.
    pub lanes: Option<Value>,
    pub log: Vec<Value>,
    /// Last thing that went wrong, for the status bar.
    pub error: Option<String>,
    /// Last thing that went right, so an Apply visibly did something.
    pub notice: Option<String>,
    /// Bumped whenever `config` is replaced, so the UI knows to reload its
    /// edit buffers rather than clobbering them on every poll.
    pub config_epoch: u64,
}

pub fn spawn(pipe_base: String) -> (Arc<Mutex<Shared>>, mpsc::Sender<Cmd>) {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let (tx, rx) = mpsc::channel::<Cmd>(32);
    let shared_for_thread = shared.clone();

    std::thread::Builder::new()
        .name("kratos-ui-worker".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build()
            else {
                if let Ok(mut s) = shared_for_thread.lock() {
                    s.error = Some("could not start the background runtime".into());
                }
                return;
            };
            runtime.block_on(run(pipe_base, shared_for_thread, rx));
        })
        .ok();

    (shared, tx)
}

async fn run(pipe_base: String, shared: Arc<Mutex<Shared>>, mut rx: mpsc::Receiver<Cmd>) {
    let mut peer: Option<kratos_ipc::PipePeer> = None;
    let mut log_lines = 0usize;

    loop {
        // Attach if we are not connected. Never start: that is an explicit
        // command, because attaching pins the broker's idle timer open.
        if peer.is_none() {
            peer = match kratos_ipc::PipePeer::attach(&pipe_base, Role::Ui, "settings", BUILD).await {
                Ok(p) => {
                    set(&shared, |s| {
                        s.connected = true;
                        s.error = None;
                    });
                    Some(p)
                }
                Err(_) => {
                    set(&shared, |s| {
                        s.connected = false;
                        s.status = None;
                    });
                    None
                }
            };
            if let Some(p) = peer.as_mut() {
                refresh_config(p, &shared).await;
            }
        }

        let tick = tokio::time::sleep(Duration::from_millis(1_000));
        tokio::pin!(tick);

        tokio::select! {
            command = rx.recv() => {
                let Some(command) = command else { return };  // the window closed
                handle(command, &pipe_base, &mut peer, &shared, &mut log_lines).await;
            }
            _ = &mut tick => {
                if let Some(p) = peer.as_mut() {
                    match p.call(ops::STATUS, json!({})).await {
                        Ok(v) => set(&shared, |s| s.status = Some(v)),
                        Err(_) => {
                            peer = None;
                            set(&shared, |s| { s.connected = false; s.status = None; });
                            continue;
                        }
                    }
                    // The machine's browsers, profiles and attached
                    // instances, on the same tick: the broker caches its
                    // Local State reads, so this costs a round trip rather
                    // than a megabyte of JSON per poll.
                    if let Ok(v) = p.call(ops::LANE_LIST, json!({})).await {
                        set(&shared, |s| s.lanes = Some(v));
                    }
                    if log_lines > 0 {
                        if let Ok(v) = p.call(ops::LOG_TAIL, json!({"n": log_lines})).await {
                            let lines = v["lines"].as_array().cloned().unwrap_or_default();
                            set(&shared, |s| s.log = lines);
                        }
                    }
                }
            }
        }
    }
}

async fn handle(
    command: Cmd,
    pipe_base: &str,
    peer: &mut Option<kratos_ipc::PipePeer>,
    shared: &Arc<Mutex<Shared>>,
    log_lines: &mut usize,
) {
    match command {
        Cmd::StartBroker => {
            match kratos_ipc::PipePeer::connect_or_start(pipe_base, Role::Ui, "settings", BUILD).await {
                Ok(mut p) => {
                    refresh_config(&mut p, shared).await;
                    *peer = Some(p);
                    set(shared, |s| {
                        s.connected = true;
                        s.error = None;
                        s.notice = Some("broker started".into());
                    });
                }
                Err(e) => set(shared, |s| s.error = Some(format!("could not start the broker: {e}"))),
            }
        }
        Cmd::StopBroker => {
            if let Some(p) = peer.as_mut() {
                let _ = p.call(ops::SHUTDOWN, json!({})).await;
            }
            *peer = None;
            set(shared, |s| {
                s.connected = false;
                s.status = None;
                s.notice = Some("broker asked to stop".into());
            });
        }
        Cmd::ApplyConfig(patch) => {
            let Some(p) = peer.as_mut() else {
                set(shared, |s| s.error = Some("not connected: start the broker first".into()));
                return;
            };
            match p.call(ops::CONFIG_SET, json!({"patch": patch})).await {
                Ok(v) => {
                    let restart = v["restart_required"].as_bool().unwrap_or(false);
                    let config = v["config"].clone();
                    set(shared, |s| {
                        s.config = Some(config);
                        s.config_epoch += 1;
                        s.error = None;
                        s.notice = Some(if restart {
                            "saved -- the pipe name changes on the next broker start".into()
                        } else {
                            "saved and live".into()
                        });
                    });
                }
                Err(e) => set(shared, |s| s.error = Some(format!("{}: {}", e.code, e.message))),
            }
        }
        Cmd::BindInstance(payload) => {
            let Some(p) = peer.as_mut() else {
                set(shared, |s| s.error = Some("not connected: start the broker first".into()));
                return;
            };
            let unpair = payload.get("profile_dir").is_none();
            match p.call(ops::INSTANCE_BIND, payload).await {
                Ok(v) => {
                    set(shared, |s| {
                        s.error = None;
                        s.notice = Some(if unpair {
                            "unpaired".into()
                        } else {
                            format!(
                                "paired with {}/{}",
                                v["brand"].as_str().unwrap_or("?"),
                                v["profile_dir"].as_str().unwrap_or("?")
                            )
                        });
                    });
                    refresh_config(p, shared).await;
                }
                Err(e) => set(shared, |s| s.error = Some(format!("{}: {}", e.code, e.message))),
            }
        }
        Cmd::ReloadConfig => {
            if let Some(p) = peer.as_mut() {
                refresh_config(p, shared).await;
            }
        }
        Cmd::Kill(scope) => {
            if let Some(p) = peer.as_mut() {
                match p.call(ops::KILL_ENGAGE, json!({"scope": scope, "reason": "settings window"})).await {
                    Ok(_) => set(shared, |s| s.notice = Some(format!("kill engaged for {scope}"))),
                    Err(e) => set(shared, |s| s.error = Some(e.message.clone())),
                }
            }
        }
        Cmd::ResetKill(scope) => {
            if let Some(p) = peer.as_mut() {
                match p.call(ops::KILL_RESET, json!({"scope": scope})).await {
                    Ok(_) => set(shared, |s| s.notice = Some(format!("kill cleared for {scope}"))),
                    Err(e) => set(shared, |s| s.error = Some(e.message.clone())),
                }
            }
        }
        Cmd::TailLog(n) => {
            *log_lines = n;
            if let Some(p) = peer.as_mut() {
                if let Ok(v) = p.call(ops::LOG_TAIL, json!({"n": n})).await {
                    let lines = v["lines"].as_array().cloned().unwrap_or_default();
                    set(shared, |s| s.log = lines);
                }
            }
        }
    }
}

async fn refresh_config(peer: &mut kratos_ipc::PipePeer, shared: &Arc<Mutex<Shared>>) {
    if let Ok(v) = peer.call(ops::CONFIG_GET, json!({})).await {
        let config = v["config"].clone();
        set(shared, |s| {
            s.config = Some(config);
            s.config_epoch += 1;
        });
    }
}

fn set(shared: &Arc<Mutex<Shared>>, f: impl FnOnce(&mut Shared)) {
    if let Ok(mut s) = shared.lock() {
        f(&mut s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_shared_state_is_disconnected_and_quiet() {
        let s = Shared::default();
        assert!(!s.connected);
        assert!(s.status.is_none());
        assert!(s.error.is_none());
        assert_eq!(s.config_epoch, 0);
    }

    #[test]
    fn the_worker_starts_without_a_broker_and_reports_disconnected() {
        // Opening the settings window must not require, or cause, a broker.
        let (shared, tx) = spawn("kratos-ui-test-absent-pipe".into());
        std::thread::sleep(Duration::from_millis(250));
        let s = shared.lock().unwrap();
        assert!(!s.connected);
        drop(s);
        drop(tx);
    }

    #[test]
    fn applying_config_while_disconnected_is_an_error_not_a_hang() {
        let (shared, tx) = spawn("kratos-ui-test-absent-pipe-2".into());
        tx.try_send(Cmd::ApplyConfig(json!({"idle_exit_ms": 1000}))).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let s = shared.lock().unwrap();
        assert!(s.error.is_some(), "the UI must be told, not left waiting");
        assert!(s.error.as_ref().unwrap().contains("not connected"));
    }
}
