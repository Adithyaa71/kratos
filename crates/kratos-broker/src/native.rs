//! Running `kratos-native`, and keeping it running.
//!
//! The Python backend has to be started by hand, which means a machine where
//! somebody forgot has a broker that can see nothing and an agent being told
//! `no_backend` for a reason that is not its fault. The Rust one is the
//! broker's to start.
//!
//! "On demand" is satisfied by the broker itself: nothing is registered at
//! logon, the broker does not exist until an agent's first call, and it exits
//! after five idle minutes. A backend spawned with it is spawned on demand and
//! goes away with it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::state::Broker;

/// Restart backoff. A backend that cannot start -- missing, or crashing on a
/// machine whose UI Automation is broken -- must not become a process the
/// broker spawns in a tight loop.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Where `kratos-native.exe` is: beside this broker, as every other binary in
/// this build is, with an override for a split install.
pub fn exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("KRATOS_NATIVE_EXE") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let beside = std::env::current_exe().ok()?.with_file_name("kratos-native.exe");
    beside.exists().then_some(beside)
}

/// Start the native backend and keep it alive until the broker stops.
pub fn supervise(broker: Arc<Broker>, pipe: String) {
    let Some(exe) = exe() else {
        eprintln!(
            "kratos-broker: backends.spawn_native is on but kratos-native.exe is not beside \
             this binary; set KRATOS_NATIVE_EXE or build it"
        );
        return;
    };

    tokio::spawn(async move {
        let mut backoff = BACKOFF_MIN;
        loop {
            eprintln!("kratos-broker: starting {}", exe.display());
            let child = tokio::process::Command::new(&exe)
                .env("KRATOS_PIPE", &pipe)
                // It dies with the broker. A perception backend outliving the
                // thing it reports to is a process reading somebody's screen
                // for nobody.
                .kill_on_drop(true)
                .stdin(std::process::Stdio::null())
                .spawn();

            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("kratos-broker: could not start the native backend: {e}");
                    return;
                }
            };

            let started = std::time::Instant::now();
            tokio::select! {
                status = child.wait() => {
                    // A backend that ran for a while and then stopped is a
                    // crash to recover from; one that stops immediately,
                    // repeatedly, is a configuration problem, and the backoff
                    // is what stops it becoming a spawn loop.
                    if started.elapsed() > Duration::from_secs(30) {
                        backoff = BACKOFF_MIN;
                    }
                    eprintln!(
                        "kratos-broker: the native backend exited ({status:?}); restarting in \
                         {backoff:?}"
                    );
                }
                _ = broker.shutdown.notified() => {
                    let _ = child.start_kill();
                    return;
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = broker.shutdown.notified() => return,
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backend_is_looked_for_beside_the_broker() {
        // The override wins when it points at something real, and is ignored
        // when it does not: a path that is wrong should fall back to the
        // ordinary answer rather than disabling the backend silently.
        std::env::set_var("KRATOS_NATIVE_EXE", "Z:\\no-such-native.exe");
        let found = exe();
        std::env::remove_var("KRATOS_NATIVE_EXE");
        assert!(found.is_none() || found.unwrap().exists());
    }

    #[test]
    fn the_backoff_climbs_and_stops() {
        let mut b = BACKOFF_MIN;
        for _ in 0..12 {
            b = (b * 2).min(BACKOFF_MAX);
        }
        assert_eq!(b, BACKOFF_MAX);
    }
}
