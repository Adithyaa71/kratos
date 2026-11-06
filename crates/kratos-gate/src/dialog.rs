//! The native approval dialog.
//!
//! This exists so that some approvals cannot be granted by the thing asking
//! for them. `MessageBoxW` is owned by the broker process and driven by the
//! real mouse and keyboard; an agent speaking the wire protocol has no path to
//! click it, which is the entire point. An agent that could answer its own
//! confirmation prompt has no confirmation prompt.

use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDNO, IDYES, MB_ICONQUESTION, MB_ICONWARNING, MB_SETFOREGROUND, MB_SYSTEMMODAL,
    MB_YESNO, MB_YESNOCANCEL,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// What a human did about an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Approved,
    Declined,
    /// Nobody answered in time. Treated as "not approved" for this request --
    /// never as approved, and never as a reason to retry automatically.
    TimedOut,
}

impl Answer {
    pub fn approved(self) -> bool {
        matches!(self, Answer::Approved)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Answer::Approved => "approved",
            Answer::Declined => "declined",
            Answer::TimedOut => "timed_out",
        }
    }
}

/// Show a modal dialog and wait up to `timeout` for a click.
///
/// Blocking: call it from a blocking context, never from an async task that
/// other agents' requests are queued behind.
///
/// On timeout the dialog is left open on purpose -- tearing it down would mean
/// a human reaching for "Yes" a moment later could click a window that no
/// longer means anything, or worse, click through to whatever appeared
/// underneath. The late click simply no longer counts for this request.
pub fn ask(description: &str, timeout: Duration) -> Answer {
    let text = format!(
        "An agent wants to do this:\n\n{description}\n\nAllow it? (treated as No in {}s)",
        timeout.as_secs()
    );
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("kratos-approval-dialog".into())
        .spawn(move || {
            let r = unsafe {
                MessageBoxW(
                    std::ptr::null_mut(),
                    wide(&text).as_ptr(),
                    wide("kratos - action needs your approval").as_ptr(),
                    MB_YESNO | MB_ICONWARNING | MB_SYSTEMMODAL | MB_SETFOREGROUND,
                )
            };
            let _ = tx.send(r == IDYES);
        })
        .ok();

    match rx.recv_timeout(timeout) {
        Ok(true) => Answer::Approved,
        Ok(false) => Answer::Declined,
        Err(_) => Answer::TimedOut,
    }
}

/// What a human decided about a client that wants to drive the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Allow it, and remember.
    Always,
    /// Allow it for this session only.
    Once,
    /// No.
    Deny,
    /// Nobody answered. Not approval, and not remembered either: the question
    /// is asked again next time, because a user who was away from the machine
    /// has not decided anything.
    TimedOut,
}

impl Approval {
    pub fn allowed(self) -> bool {
        matches!(self, Approval::Always | Approval::Once)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Approval::Always => "always",
            Approval::Once => "once",
            Approval::Deny => "deny",
            Approval::TimedOut => "timed_out",
        }
    }
}

/// Ask whether a client may drive this machine at all.
///
/// Three answers on a two-answer API. `MessageBoxW`'s buttons cannot be
/// relabelled, so the text says which is which, and `MB_YESNOCANCEL` is used
/// rather than a `TaskDialog`: a task dialog needs version 6 common controls,
/// which needs an application manifest, and a dialog that fails to appear is
/// an approval that can never be given.
///
/// Blocking. Call it from a blocking context.
pub fn ask_approval(
    harness: &str,
    detail: &str,
    timeout: Duration,
) -> Approval {
    let text = format!(
        "\"{harness}\" wants to see and drive this machine.\n\n{detail}\n\n\
         Yes      allow it, and remember\n\
         No       allow it just for now\n\
         Cancel   do not allow it\n\n\
         (no answer in {}s is treated as: do not allow)",
        timeout.as_secs()
    );
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("kratos-harness-approval".into())
        .spawn(move || {
            let r = unsafe {
                MessageBoxW(
                    std::ptr::null_mut(),
                    wide(&text).as_ptr(),
                    wide("kratos - a new client wants access").as_ptr(),
                    MB_YESNOCANCEL | MB_ICONQUESTION | MB_SYSTEMMODAL | MB_SETFOREGROUND,
                )
            };
            let _ = tx.send(r);
        })
        .ok();

    match rx.recv_timeout(timeout) {
        Ok(r) if r == IDYES => Approval::Always,
        Ok(r) if r == IDNO => Approval::Once,
        Ok(_) => Approval::Deny,
        Err(_) => Approval::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_click_is_approval() {
        assert!(Approval::Always.allowed());
        assert!(Approval::Once.allowed());
        assert!(!Approval::Deny.allowed());
        // The case that matters: the user is asleep and the agent is asking.
        assert!(!Approval::TimedOut.allowed());
    }

    #[test]
    fn an_approval_dialog_nobody_clicks_times_out() {
        let started = std::time::Instant::now();
        let answer = ask_approval("unit-test-harness", "nothing is happening", Duration::from_millis(600));
        assert_eq!(answer, Approval::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn timeout_is_not_approval() {
        assert!(!Answer::TimedOut.approved());
        assert!(!Answer::Declined.approved());
        assert!(Answer::Approved.approved());
    }

    #[test]
    fn a_dialog_nobody_clicks_times_out_rather_than_hanging() {
        // Runs a real MessageBoxW with nobody to click it. The requirement
        // under test is that the caller regains control, not that a window
        // appeared: a gate that blocks forever is how 89% of a session's wall
        // clock went missing once already.
        let started = std::time::Instant::now();
        let answer = ask("unit test: no click expected", Duration::from_millis(600));
        assert_eq!(answer, Answer::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }
}
