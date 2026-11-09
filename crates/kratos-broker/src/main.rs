//! kratos broker.
//!
//! Owns the named pipe, the gate, the kill switch, the input lease, the action
//! log and the decision model. Not autostarted: an MCP shim spawns it on the
//! first tool call that needs it, and it exits again after a configurable idle
//! period. Nothing is registered at logon, so a machine that is not being
//! driven has no process for this at all.

mod act;
mod activity;
mod browsers;
mod classify;
mod diff;
mod hotkey;
mod journal;
mod native;
mod lane;
mod peer;
mod seal;
mod state;
mod target;

use std::time::Duration;

use kratos_protocol::config::{Config, LoadError};
use serde_json::json;

/// How often the idle watchdog looks. Coarse on purpose: this is a
/// housekeeping timer, and waking five times a second to compare two integers
/// is exactly the sort of background cost that makes a daemon unwelcome.
const IDLE_TICK: Duration = Duration::from_secs(5);

fn main() {
    if let Err(e) = real_main() {
        eprintln!("kratos-broker: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), String> {
    // Two threads is enough: the work here is I/O and waiting, with the two
    // genuinely blocking things -- the decision HTTP call and the screen grab
    // -- on the blocking pool. A thread per core would mostly idle.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()
        .map_err(|e| format!("could not start the runtime: {e}"))?;
    runtime.block_on(run())
}

async fn run() -> Result<(), String> {
    let config_path = kratos_protocol::paths::config_path();
    let mut config = match Config::load_or_default(&config_path) {
        Ok(c) => c,
        Err(LoadError::Parse(m)) => {
            // Refusing beats overwriting someone's hand-edited gate policy.
            return Err(format!(
                "{} is malformed and was left untouched: {m}\nfix it, or move it aside to start \
                 from defaults",
                config_path.display()
            ));
        }
        Err(e) => return Err(format!("could not read {}: {e}", config_path.display())),
    };

    // Write the defaults out on first run, so there is something to edit and
    // the settings window has a file to point at.
    if !config_path.exists() {
        let _ = kratos_protocol::paths::ensure_state_dir();
        let _ = config.save_to(&config_path);
    }

    // One override, for the case of watching a broker by hand: `start-daemon.cmd`
    // sets this to 0 so the window does not close out from under you mid-task.
    // The config file stays the source of truth for everything else -- this is
    // not a general escape hatch, and in particular no gate or kill setting can
    // be changed from the environment.
    if let Some(raw) = std::env::var_os("KRATOS_IDLE_EXIT_MS") {
        match raw.to_string_lossy().trim().parse::<u64>() {
            Ok(ms) => config.idle_exit_ms = ms,
            Err(_) => eprintln!(
                "kratos-broker: KRATOS_IDLE_EXIT_MS is not a number, keeping {}",
                config.idle_exit_ms
            ),
        }
    }

    // One broker per OS user, and the guard is keyed on the *user*, not on the
    // pipe name.
    //
    // Keying it on the pipe meant a second `KRATOS_PIPE` was a second broker,
    // each with its own gate, journal, routing and input lease -- which is
    // exactly the broker-per-agent model this design removes. `KRATOS_PIPE`
    // survives as a test seam, and setting it is what puts the pipe name back
    // in the guard so the suite can run several isolated brokers at once.
    let pipe_override = std::env::var("KRATOS_PIPE").ok().filter(|p| !p.trim().is_empty());
    let pipe_base = pipe_override.clone().unwrap_or_else(|| config.pipe_name.clone());
    let guard_key = match &pipe_override {
        Some(p) => format!("kratos.broker.{p}"),
        None => "kratos.broker".to_string(),
    };
    let instance = kratos_ipc::win::SingleInstance::acquire(
        &kratos_ipc::instance_name(&guard_key).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("could not take the single-instance lock: {e}"))?;
    if instance.already_running {
        // The expected outcome of two shims racing to spawn a broker. Not an
        // error: the other one won, and it is serving the pipe.
        return Ok(());
    }

    let mut listener = match kratos_ipc::PipeListener::bind(&pipe_base) {
        Ok(l) => l,
        Err(e) => {
            // The second guard behind the mutex. Same conclusion.
            return if e.raw_os_error() == Some(5) || e.raw_os_error() == Some(231) {
                Ok(())
            } else {
                Err(format!("could not create the pipe: {e}"))
            };
        }
    };

    // Watch for the person at the keyboard before anything can act, so an
    // action that arrives a second from now already knows whether they are
    // mid-sentence. Injected input is ignored, or the agent would pause itself
    // after its own first click.
    if config.defaults.pause_on_user_input {
        activity::watch();
    }

    let spawn_native = config.backends.spawn_native;
    let api_key = seal::read_key(&config.decider.api_key_enc);
    let hotkey_spec = config.kill.hotkey.clone();
    let hotkey_scope = config.kill.hotkey_scope.clone();
    let broker = state::Broker::new(config, api_key);
    broker.mark_agent_activity(); // the idle clock starts now, not at epoch

    eprintln!(
        "kratos-broker {} listening on {} (pid {})",
        env!("CARGO_PKG_VERSION"),
        listener.path(),
        std::process::id()
    );

    // -- kill-switch hotkey -------------------------------------------------
    {
        let mut presses = hotkey::spawn(&hotkey_spec);
        let broker = broker.clone();
        let scope = hotkey_scope.clone();
        tokio::spawn(async move {
            while presses.recv().await.is_some() {
                broker.gate.lock().await.kill.engage(&scope);
                let engaged = broker.gate.lock().await.kill.engaged();
                eprintln!("kratos-broker: kill switch engaged by hotkey -> {engaged:?}");
                broker.journal.read().await.append(json!({
                    "op": "kill.engage", "peer_id": "hotkey", "agent": "hotkey",
                    "scope": scope, "reason": "hotkey", "engaged": engaged
                }));
                broker
                    .broadcast(kratos_protocol::Frame::evt(
                        kratos_protocol::ops::EVT_KILL,
                        json!({"engaged": engaged, "by": "hotkey"}),
                    ))
                    .await;
            }
        });
    }

    // -- the native backend -------------------------------------------------
    if spawn_native {
        native::supervise(broker.clone(), pipe_base.clone());
    }

    // -- idle watchdog ------------------------------------------------------
    {
        let broker = broker.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(IDLE_TICK).await;
                // Section 9 of the protocol says the config file is hot
                // reloaded, and the file is the source of truth even when the
                // settings window is the intended editor. This is that reload:
                // the tick is already here, and a file watcher would be a
                // thread and an API for a question asked every five seconds.
                broker.reload_if_changed().await;
                let limit = broker.config.read().await.idle_exit_ms;
                if limit == 0 {
                    continue; // 0 disables the timer entirely
                }
                // A settings window counts as someone watching; backends do
                // not, because they are expected to reconnect.
                if broker.idle_ms() >= limit && !broker.has_ui().await {
                    eprintln!(
                        "kratos-broker: idle for {}ms, exiting (set idle_exit_ms to 0 to stay up)",
                        broker.idle_ms()
                    );
                    broker.shutdown.notify_waiters();
                    return;
                }
            }
        });
    }

    // -- accept loop --------------------------------------------------------
    let shutdown = broker.shutdown.notified();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(conn) => {
                    let broker = broker.clone();
                    // One task per connection: a panic in a request handler
                    // takes that connection down and leaves every other
                    // agent, and the kill switch, running.
                    tokio::spawn(peer::serve(broker, conn));
                }
                Err(e) => {
                    eprintln!("kratos-broker: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    eprintln!("kratos-broker: stopped");
    drop(instance);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_idle_tick_is_coarse_enough_to_be_invisible() {
        // A daemon that wakes constantly is a daemon people kill.
        assert!(super::IDLE_TICK >= std::time::Duration::from_secs(1));
    }
}
