//! When the user last touched the machine — and only the user.
//!
//! An agent that paused itself would never act, so the one thing this must get
//! right is telling *real* input from input the agent injected. Windows says
//! so directly: a low-level hook receives `LLKHF_INJECTED` on a keystroke and
//! `LLMHF_INJECTED` on a mouse event that came from `SendInput` rather than
//! from hardware.
//!
//! `GetLastInputInfo` is the obvious shortcut and is wrong here: it counts
//! injected input too, so every action the agent took would look like the user
//! arriving, and the agent would pause after its own first click.
//!
//! The hooks live on their own thread with its own message loop, because
//! `SetWindowsHookEx` delivers to the thread that installed the hook and
//! nothing on a tokio worker pumps messages.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Milliseconds since boot (`GetTickCount64`) at the last real input.
///
/// Zero means "no real input seen since this broker started", which is treated
/// as "long ago": a broker that has just started must not pause an agent over
/// input it never saw.
static LAST_REAL: AtomicU64 = AtomicU64::new(0);
static WATCHING: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
fn tick() -> u64 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
}

#[cfg(not(windows))]
fn tick() -> u64 {
    0
}

/// Record an input event. `injected` events are ignored on purpose.
///
/// Public so the pause can be tested without a hardware keyboard: a test calls
/// this with `false` to stand in for the user and with `true` to stand in for
/// the agent, which is exactly the distinction that matters.
pub fn note_input(injected: bool) {
    if injected {
        return;
    }
    LAST_REAL.store(tick().max(1), Ordering::Relaxed);
}

/// How long since the user last typed or moved the mouse.
///
/// `u64::MAX` when nothing has been seen, so a caller comparing against an
/// idle threshold reads "the user is not here" rather than "the user is here
/// right now".
pub fn since_real_input_ms() -> u64 {
    let last = LAST_REAL.load(Ordering::Relaxed);
    if last == 0 {
        return u64::MAX;
    }
    tick().saturating_sub(last)
}

/// Whether the hooks are installed. Reported in `status`, because a pause
/// setting that silently does nothing is worse than one that is off.
pub fn watching() -> bool {
    WATCHING.load(Ordering::Relaxed)
}

/// Pretend the user just acted. Tests only.
#[cfg(test)]
pub fn note_real_input_now() {
    note_input(false);
}

/// Forget any input seen. Tests only.
#[cfg(test)]
pub fn forget() {
    LAST_REAL.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// The hooks
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod hooks {
    use super::*;
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
        KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL,
        WH_MOUSE_LL,
    };

    unsafe extern "system" fn keyboard(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
        if code >= 0 && l != 0 {
            let info = &*(l as *const KBDLLHOOKSTRUCT);
            note_input(info.flags & LLKHF_INJECTED != 0);
        }
        CallNextHookEx(std::ptr::null_mut(), code, w, l)
    }

    unsafe extern "system" fn mouse(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
        if code >= 0 && l != 0 {
            let info = &*(l as *const MSLLHOOKSTRUCT);
            // Movement counts. A user who nudges the mouse to see what the
            // agent is doing has arrived, and taking the pointer out from
            // under them is the thing being prevented.
            note_input(info.flags & LLMHF_INJECTED != 0);
        }
        CallNextHookEx(std::ptr::null_mut(), code, w, l)
    }

    /// Install the hooks and pump messages until the process ends.
    pub fn watch() {
        std::thread::Builder::new()
            .name("kratos-user-activity".into())
            .spawn(|| unsafe {
                let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), std::ptr::null_mut(), 0);
                let ms = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), std::ptr::null_mut(), 0);
                if kb.is_null() && ms.is_null() {
                    eprintln!(
                        "kratos-broker: could not watch for user input; the pause is off and \
                         status says so"
                    );
                    return;
                }
                WATCHING.store(true, Ordering::Relaxed);

                // A low-level hook is only called while the installing thread
                // pumps messages. Without this loop the hooks exist and never
                // fire, which looks exactly like a user who is never there.
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                WATCHING.store(false, Ordering::Relaxed);
            })
            .ok();
    }
}

#[cfg(windows)]
pub fn watch() {
    hooks::watch();
}

#[cfg(not(windows))]
pub fn watch() {}

// ---------------------------------------------------------------------------
// The pause itself
// ---------------------------------------------------------------------------

/// What happened when an action wanted the input devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    /// The user is not using the machine; carry on.
    Clear,
    /// The user was active and stopped; this is how long was spent waiting.
    Waited(u64),
    /// The user is still using the machine. Nothing ran.
    Busy(u64),
}

/// Wait for the user to stop, up to `max_ms`.
///
/// Polls rather than being woken: the hook runs on another thread and the
/// condition is "nothing for `idle_ms`", so there is no single event to wait
/// for. A 50ms poll on a path that is already measured in hundreds costs
/// nothing to speak of.
pub async fn wait_for_user_to_stop(idle_ms: u64, max_ms: u64) -> Pause {
    if since_real_input_ms() >= idle_ms {
        return Pause::Clear;
    }
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let waited = started.elapsed().as_millis() as u64;
        if since_real_input_ms() >= idle_ms {
            return Pause::Waited(waited);
        }
        if waited >= max_ms {
            return Pause::Busy(waited);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests share one static, so they run one at a time.
    ///
    /// An async mutex rather than a std one: half of them await, and a
    /// std guard held across an await is a deadlock waiting for a runtime
    /// that schedules them on one thread.
    static ORDER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn nothing_seen_reads_as_the_user_being_away() {
        let _g = ORDER.lock().await;
        forget();
        assert_eq!(since_real_input_ms(), u64::MAX);
        // A broker that has just started must not pause an agent over input
        // it never saw.
        assert!(since_real_input_ms() >= 1500);
    }

    #[tokio::test]
    async fn injected_input_is_not_the_user() {
        let _g = ORDER.lock().await;
        forget();
        // This is the whole point: every action the agent takes is injected,
        // and an agent that paused itself would never act.
        for _ in 0..10 {
            note_input(true);
        }
        assert_eq!(since_real_input_ms(), u64::MAX, "injected input must not count");

        note_input(false);
        assert!(since_real_input_ms() < 1000, "real input must count");
    }

    #[tokio::test]
    async fn an_action_waits_for_the_user_and_then_gives_up() {
        let _g = ORDER.lock().await;
        forget();
        note_real_input_now();

        // The user is mid-keystroke: a short budget runs out and nothing runs.
        match wait_for_user_to_stop(5_000, 120).await {
            Pause::Busy(waited) => assert!(waited >= 120, "{waited}"),
            other => panic!("expected Busy, got {other:?}"),
        }

        // And with a threshold the user has already cleared, there is no wait
        // at all rather than a poll's worth.
        assert_eq!(wait_for_user_to_stop(0, 1_000).await, Pause::Clear);
    }

    /// The claim that matters, tested against the real hooks on real
    /// hardware: input this process injects must not look like the user.
    ///
    /// Everything else here is arithmetic over an atomic. This is the part
    /// that would silently invert -- a wrong flag constant, a struct laid out
    /// differently than assumed -- and leave an agent that pauses itself
    /// after its own first click.
    #[tokio::test]
    #[cfg(windows)]
    async fn input_this_process_injects_is_not_the_user() {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_MOVE,
        };

        let _g = ORDER.lock().await;
        watch();
        // The hooks install on their own thread; give it a moment to exist.
        for _ in 0..40 {
            if watching() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        if !watching() {
            eprintln!("no input hooks in this session (headless?); skipping");
            return;
        }
        // A locked session refuses SendInput and returns zero sent, with no
        // error to read. The test below would then fail with "SendInput
        // refused", which says nothing about the actual cause.
        if !kratos_ipc::win::session_is_unlocked() {
            eprintln!("this session is locked, so SendInput is refused; skipping");
            return;
        }

        // Only run while nobody is using the machine, so a real hand on the
        // mouse cannot be mistaken for a failure.
        forget();
        std::thread::sleep(std::time::Duration::from_millis(250));
        if since_real_input_ms() != u64::MAX {
            eprintln!("somebody is using this machine; skipping");
            return;
        }

        // A zero-pixel move: it is an injected mouse event and it moves
        // nothing.
        let mut input: INPUT = unsafe { std::mem::zeroed() };
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi.dx = 0;
        input.Anonymous.mi.dy = 0;
        input.Anonymous.mi.dwFlags = MOUSEEVENTF_MOVE;
        let sent = unsafe {
            SendInput(1, &input, std::mem::size_of::<INPUT>() as i32)
        };
        assert_eq!(sent, 1, "SendInput refused; the test proves nothing");

        std::thread::sleep(std::time::Duration::from_millis(250));
        assert_eq!(
            since_real_input_ms(),
            u64::MAX,
            "injected input registered as the user. Either LLMHF_INJECTED is not being read, \
             or somebody touched the mouse during this test"
        );
    }

    /// Installing the hooks is on the broker's startup path, which an agent
    /// waits through on its first tool call.
    #[tokio::test]
    #[cfg(windows)]
    async fn watching_the_user_costs_nothing_to_start() {
        let _g = ORDER.lock().await;
        let started = std::time::Instant::now();
        watch();
        for _ in 0..200 {
            if watching() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let took = started.elapsed();
        eprintln!("hooks installed in {took:?} (watching={})", watching());
        assert!(
            took < std::time::Duration::from_millis(100),
            "the broker is spawned on an agent's first call and this is on that path: {took:?}"
        );
    }

    #[tokio::test]
    async fn an_action_proceeds_once_the_user_stops() {
        let _g = ORDER.lock().await;
        forget();
        note_real_input_now();
        // 80ms of quiet is enough for this threshold, so the wait ends early
        // rather than running out the budget.
        match wait_for_user_to_stop(80, 5_000).await {
            Pause::Waited(waited) => assert!(waited < 2_000, "{waited}"),
            other => panic!("expected Waited, got {other:?}"),
        }
    }
}
