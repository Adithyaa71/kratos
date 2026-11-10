//! One test at a time on a desktop there is only one of.
//!
//! The live tests in this crate open a window, bring it to the front and
//! drive the real mouse and keyboard. Cargo runs test binaries in parallel,
//! so two of them did this at once: two Notepads up together, a title search
//! landing on the other one, and a capture read while another test was typing
//! into the window it had just found. It failed once in about a dozen runs,
//! which is the worst frequency a test can have -- often enough to mislead,
//! rare enough to dismiss.
//!
//! The guard is the broker's own machine-wide input semaphore, not a new one,
//! so these tests also serialise against a broker that is running on this
//! machine rather than fighting it for the pointer.

use std::time::Duration;

/// Held for as long as a test needs the desktop to itself.
pub struct Desktop {
    guard: Option<kratos_ipc::win::InputGuard>,
}

impl Desktop {
    /// The desktop, or `None` because there is not one to drive.
    ///
    /// `None` means the session is locked. These tests click, type, bring
    /// windows forward and read pixels, and a locked session refuses all of
    /// it silently -- `SendInput` sends nothing, `SetForegroundWindow`
    /// returns false, `PrintWindow` hands back a frame that was never drawn.
    /// Failing then is a red suite that means "your screen was locked", which
    /// is a suite people learn to ignore. Skipping and saying so is the
    /// honest answer, and the caller prints which test it was.
    pub fn take_or_skip(wait: Duration, test: &str) -> Option<Desktop> {
        if !kratos_ipc::win::session_is_unlocked() {
            eprintln!("{test}: skipped -- this session is locked, so there is no desktop to drive");
            return None;
        }
        Some(Desktop::take(wait))
    }

    /// Wait for the desktop, for up to `wait`.
    ///
    /// Generously: the queue is every live test in this crate, each of which
    /// opens a window and drives it, and a wait shorter than the sum of them
    /// lets the last one through unguarded -- which is how this was first
    /// written, and it still raced.
    ///
    /// A timeout returns a `Desktop` holding nothing rather than failing: the
    /// test then runs anyway and may be flaky, which is strictly better than a
    /// red suite because something else on the machine held the guard. It
    /// says so on stderr so a failure afterwards is explicable.
    pub fn take(wait: Duration) -> Desktop {
        let name = match kratos_ipc::input_guard_name() {
            Ok(n) => n,
            Err(e) => {
                eprintln!("live test: no input guard name ({e}); running unguarded");
                return Desktop { guard: None };
            }
        };
        match kratos_ipc::win::InputGuard::open(&name) {
            Ok(g) => {
                if g.try_take(wait.as_millis() as u32) {
                    Desktop { guard: Some(g) }
                } else {
                    eprintln!(
                        "live test: the input guard was held for {}ms; running unguarded",
                        wait.as_millis()
                    );
                    Desktop { guard: None }
                }
            }
            Err(e) => {
                eprintln!("live test: could not open the input guard ({e}); running unguarded");
                Desktop { guard: None }
            }
        }
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        if let Some(g) = self.guard.take() {
            g.release();
        }
    }
}
