//! Driving a real window, over the real protocol.
//!
//! The test opens a window of its own — one text box, one checkbox, one
//! button, one label that records what happened — and then speaks to
//! `kratos-native` exactly as the broker does: a named pipe, a hello, a
//! `backend.observe`, a `backend.act`. Nothing is faked below the protocol.
//!
//! A window of its own, because the alternative is typing into whatever the
//! operator has open. A test that edits somebody's document is a test that
//! gets deleted.
//!
//! It skips, loudly, on a session with no desktop.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use kratos_protocol::{ops, Frame};

mod common;
use serde_json::{json, Value};

const GRACE: Duration = Duration::from_secs(10);

/// The window under test. WinForms via PowerShell: no build step, no
/// dependency, and every control has the UI Automation patterns this phase is
/// about — `ValuePattern` on the box, `InvokePattern` on the button,
/// `TogglePattern` on the checkbox.
const SCRATCH: &str = r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$form = New-Object System.Windows.Forms.Form
$form.Text = "Kratos Actuation Test"
$form.Size = New-Object System.Drawing.Size(560, 240)
$form.StartPosition = "CenterScreen"
$form.TopMost = $true
$box = New-Object System.Windows.Forms.TextBox
$box.AccessibleName = "Recipient"
$box.Location = New-Object System.Drawing.Point(130, 22)
$box.Size = New-Object System.Drawing.Size(380, 24)
$check = New-Object System.Windows.Forms.CheckBox
$check.AccessibleName = "Urgent"
$check.Text = "Urgent"
$check.Location = New-Object System.Drawing.Point(130, 60)
$send = New-Object System.Windows.Forms.Button
$send.AccessibleName = "Send"
$send.Text = "Send"
$send.Location = New-Object System.Drawing.Point(130, 100)
$status = New-Object System.Windows.Forms.Label
$status.Text = "waiting"
$status.Location = New-Object System.Drawing.Point(130, 150)
$status.Size = New-Object System.Drawing.Size(380, 24)
$send.Add_Click({ $status.Text = "sent to " + $box.Text })
$form.Controls.AddRange(@($box, $check, $send, $status))
[void]$form.ShowDialog()
"#;

const TITLE: &str = "Kratos Actuation Test";

struct Scratch {
    child: Option<Child>,
    script: std::path::PathBuf,
}

impl Scratch {
    fn open() -> Option<Scratch> {
        let script = std::env::temp_dir().join(format!("kratos-scratch-{}.ps1", std::process::id()));
        std::fs::write(&script, SCRATCH).ok()?;
        let child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-WindowStyle",
                "Hidden",
                "-File",
            ])
            .arg(&script)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // The form takes a moment to appear; without this the first observe
        // races it.
        std::thread::sleep(Duration::from_millis(2500));
        Some(Scratch { child: Some(child), script })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_file(&self.script);
    }
}

/// Stand in for the broker: own the pipe and speak to the backend.
struct Broker {
    listener: kratos_ipc::PipeListener,
}

struct Conn {
    reader: kratos_ipc::FrameReader<
        tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeServer>,
    >,
    writer: kratos_ipc::FrameWriter<
        tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeServer>,
    >,
    source: String,
    next: u32,
}

impl Conn {
    async fn call(&mut self, op: &str, payload: Value) -> Result<Value, String> {
        self.next += 1;
        let id = format!("b-{}", self.next);
        self.writer.send(&Frame::req(&id, op, payload)).await.map_err(|e| e.to_string())?;
        loop {
            let frame = match self.reader.next().await {
                Ok(Some(f)) => f,
                Ok(None) => return Err("the backend closed the connection".into()),
                Err(e) if e.is_fatal() => return Err(e.to_string()),
                Err(_) => continue,
            };
            if frame.id() != Some(id.as_str()) {
                continue;
            }
            return if frame.is_ok() {
                Ok(frame.payload().clone())
            } else {
                Err(frame
                    .wire_error()
                    .map(|e| format!("{}: {}", e.code, e.message))
                    .unwrap_or_else(|| "refused".into()))
            };
        }
    }
}

impl Broker {
    async fn accept(&mut self) -> Conn {
        let conn = self.listener.accept().await.expect("accept");
        let (mut reader, mut writer) = kratos_ipc::split(conn);
        let hello = reader.next().await.expect("read").expect("a hello");
        let source =
            hello.payload()["source"].as_str().unwrap_or_default().to_string();
        let id = hello.id().unwrap_or("1").to_string();
        writer
            .send(&Frame::ok(
                &id,
                json!({"v": 1, "peer_id": "b-1", "broker": "test", "decider": "none",
                       "kill_engaged": [], "backends": []}),
            ))
            .await
            .expect("ack");
        Conn { reader, writer, source, next: 0 }
    }
}

fn elements(obs: &Value) -> Vec<Value> {
    obs["elements"].as_array().cloned().unwrap_or_default()
}

fn find<'a>(els: &'a [Value], role: &str, label: &str) -> Option<&'a Value> {
    els.iter().find(|e| e["role"] == json!(role) && e["label"] == json!(label))
}

/// Start the backend and hand back its `uia` connection.
///
/// The backend attaches three times, once per source. Only the tree acts, so
/// only that connection is useful here -- but all three have to be accepted or
/// the ones left waiting fill the listener's queue.
async fn attach_uia(pipe: &str) -> Option<(Conn, Child)> {
    let mut broker = Broker { listener: kratos_ipc::PipeListener::bind(pipe).expect("bind") };
    let backend = Command::new(env!("CARGO_BIN_EXE_kratos-native"))
        .env("KRATOS_PIPE", pipe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kratos-native");

    let mut uia = None;
    for _ in 0..3 {
        match tokio::time::timeout(GRACE, broker.accept()).await {
            Ok(conn) if conn.source == "uia" => uia = Some(conn),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    uia.map(|c| (c, backend))
}

/// The three kinds that are not input: starting something, going somewhere,
/// and moving the focus.
///
/// These were the Python backend's and not this one's, so retiring it without
/// them would have taken away the ability to start anything. The refusals are
/// as much the point as the successes: every literal comes from the operator's
/// sentence, so a missing one is a refusal and never a guess, and a name this
/// machine does not have comes back with what it does have.
#[tokio::test]
async fn starting_things_and_moving_the_focus() {
    let Some(_desktop) =
        common::Desktop::take_or_skip(std::time::Duration::from_secs(180), "starting things")
    else {
        return;
    };
    let pipe = format!("kratos-native-launch-{}", std::process::id());
    let Some((mut uia, mut backend)) = attach_uia(&pipe).await else {
        panic!("kratos-native never attached as uia");
    };

    // -- the refusals, which do not depend on this machine at all -----------
    let res = uia.call(ops::BACKEND_ACT, json!({"kind": "open"})).await.expect("open");
    assert_eq!(res["applied"], json!(false), "{res}");
    assert!(
        res["detail"].as_str().unwrap_or_default().contains("no application is named"),
        "{res}"
    );

    let res = uia.call(ops::BACKEND_ACT, json!({"kind": "navigate"})).await.expect("navigate");
    assert_eq!(res["applied"], json!(false), "{res}");
    assert!(res["detail"].as_str().unwrap_or_default().contains("no URL is named"), "{res}");

    let res = uia.call(ops::BACKEND_ACT, json!({"kind": "focus"})).await.expect("focus");
    assert_eq!(res["applied"], json!(false), "{res}");
    assert!(res["detail"].as_str().unwrap_or_default().contains("no window is named"), "{res}");

    // A name nothing on this machine answers to: refused, with what it does
    // have. "No" on its own is not actionable.
    let res = uia
        .call(ops::BACKEND_ACT, json!({"kind": "open", "text": "qzxvbnmlkjhgfdsa"}))
        .await
        .expect("open nonsense");
    assert_eq!(res["applied"], json!(false), "{res}");
    let detail = res["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("qzxvbnmlkjhgfdsa"), "{res}");
    assert!(
        detail.contains("Nearest:") || detail.contains("catalogued"),
        "a refusal should say what this machine has: {res}"
    );

    // -- and the one application every Windows machine has ------------------
    let before = std::process::Command::new("tasklist")
        .args(["/fi", "imagename eq notepad.exe"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("notepad.exe"))
        .unwrap_or(false);

    let res = uia
        .call(ops::BACKEND_ACT, json!({"kind": "open", "text": "notepad"}))
        .await
        .expect("open notepad");
    assert_eq!(res["applied"], json!(true), "{res}");
    let detail = res["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("notepad"), "the detail should name what ran: {res}");

    // It really started: the process exists afterwards even if the window has
    // not drawn, and a locked session draws nothing.
    let running = std::process::Command::new("tasklist")
        .args(["/fi", "imagename eq notepad.exe"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("notepad.exe"))
        .unwrap_or(false);
    assert!(running, "notepad was started and is not running; before={before}");

    // -- focus: never reported as done when Windows refused it --------------
    let res = uia
        .call(ops::BACKEND_ACT, json!({"kind": "focus", "text": "Notepad"}))
        .await
        .expect("focus notepad");
    let detail = res["detail"].as_str().unwrap_or_default().to_string();
    match res["applied"].as_bool() {
        Some(true) => {
            // Claimed: then the window it settled on is named, so a wrong
            // match is visible rather than silent.
            assert!(
                res["readback"].as_str().unwrap_or_default().to_lowercase().contains("notepad"),
                "{res}"
            );
        }
        _ => {
            // Refused: it must say why, because the next step is a keypress
            // and a keypress goes to whatever has focus.
            assert!(
                detail.contains("refused") || detail.contains("no visible window"),
                "a focus that did not happen must say so: {res}"
            );
        }
    }

    // Tidy up: this test started it.
    let _ = std::process::Command::new("taskkill").args(["/f", "/im", "notepad.exe"]).output();
    let _ = backend.kill();
    let _ = backend.wait();
}

#[tokio::test]
async fn a_real_window_is_read_and_driven_through_its_own_patterns() {
    let Some(_desktop) =
        common::Desktop::take_or_skip(std::time::Duration::from_secs(180), "patterns")
    else {
        return;
    };
    let Some(_scratch) = Scratch::open() else {
        eprintln!("could not open the scratch window; skipping");
        return;
    };

    let pipe = format!("kratos-native-it-{}", std::process::id());
    let mut broker = Broker { listener: kratos_ipc::PipeListener::bind(&pipe).expect("bind") };
    let mut backend = Command::new(env!("CARGO_BIN_EXE_kratos-native"))
        .env("KRATOS_PIPE", &pipe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kratos-native");

    // It attaches once per source -- three of them -- and the tree is the one
    // that acts. All three are accepted even though two are unused: a
    // connection nobody accepts leaves the backend waiting on the listener,
    // and a backend waiting on a connection is a backend not answering this
    // one.
    let mut uia = None;
    for _ in 0..3 {
        match tokio::time::timeout(GRACE, broker.accept()).await {
            Ok(conn) if conn.source == "uia" => uia = Some(conn),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let Some(mut uia) = uia else {
        let _ = backend.kill();
        let _ = backend.wait();
        panic!("kratos-native never attached as uia");
    };

    // -- read ---------------------------------------------------------------
    let obs = uia
        .call(ops::BACKEND_OBSERVE, json!({"window": TITLE, "class": "native"}))
        .await
        .expect("observe");
    let els = elements(&obs);
    assert!(!els.is_empty(), "nothing was read from {TITLE:?}: {obs}");
    let edit = find(&els, "edit", "Recipient")
        .unwrap_or_else(|| panic!("no Recipient box among {els:?}"))
        .clone();
    let send = find(&els, "button", "Send")
        .unwrap_or_else(|| panic!("no Send button among {els:?}"))
        .clone();
    let urgent = find(&els, "checkbox", "Urgent").cloned();

    // -- type ---------------------------------------------------------------
    let typed = "someone@example.com";
    let res = uia
        .call(
            ops::BACKEND_ACT,
            json!({"kind": "type", "target": {"id": edit["id"], "bbox": edit["bbox"]},
                   "text": typed}),
        )
        .await
        .expect("type");
    assert_eq!(res["applied"], json!(true), "{res}");
    assert_eq!(
        res["method"],
        json!("semantic"),
        "a text box has a ValuePattern; typing at it with the keyboard is the fallback: {res}"
    );
    assert_eq!(
        res["readback"],
        json!(typed),
        "the field must read back what was asked for: {res}"
    );

    // -- click --------------------------------------------------------------
    let res = uia
        .call(
            ops::BACKEND_ACT,
            json!({"kind": "click", "target": {"id": send["id"], "bbox": send["bbox"]}}),
        )
        .await
        .expect("click");
    assert_eq!(res["applied"], json!(true), "{res}");
    assert_eq!(res["method"], json!("semantic"), "a button has an InvokePattern: {res}");

    // The application itself says what happened, which is the only evidence
    // that cannot be produced by the thing under test.
    let after = uia
        .call(ops::BACKEND_OBSERVE, json!({"window": TITLE, "class": "native"}))
        .await
        .expect("observe again");
    let labels: Vec<String> = elements(&after)
        .iter()
        .map(|e| e["label"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        labels.iter().any(|l| l == &format!("sent to {typed}")),
        "the window should say what it did; labels were {labels:?}"
    );

    // -- a real keypress ----------------------------------------------------
    //
    // Through SendInput, into the window that has focus, with an element
    // attached that this backend must ignore: the broker passes along
    // whatever the decision model picked as the place the key is expected to
    // land, and resolving it is how `press Enter` fails because something
    // unrelated moved. The id here is deliberately one that cannot resolve.
    let res = uia
        .call(
            ops::BACKEND_ACT,
            json!({"kind": "key", "key": "enter",
                   "target": {"id": "uia:dead:9.9.9", "bbox": [0, 0, 1, 1]}}),
        )
        .await
        .expect("keypress");
    assert_eq!(
        res["applied"],
        json!(true),
        "a keypress must not fail because an element it never needed is gone: {res}"
    );
    assert_eq!(res["method"], json!("physical"), "there is no pattern for a keypress: {res}");
    assert!(
        res["detail"].as_str().unwrap_or_default().to_lowercase().contains("enter"),
        "the detail should say which key: {res}"
    );

    // A key this build does not know is refused by name rather than pressed
    // as something else.
    let res = uia
        .call(ops::BACKEND_ACT, json!({"kind": "key", "key": "nosuchkey"}))
        .await
        .expect("bad keypress");
    assert_eq!(res["applied"], json!(false), "{res}");

    // -- toggle -------------------------------------------------------------
    if let Some(urgent) = urgent {
        let res = uia
            .call(
                ops::BACKEND_ACT,
                json!({"kind": "click", "target": {"id": urgent["id"], "bbox": urgent["bbox"]}}),
            )
            .await
            .expect("toggle");
        assert_eq!(res["applied"], json!(true), "{res}");
        assert_eq!(
            res["method"],
            json!("semantic"),
            "a checkbox has no Invoke; clicking it is a Toggle: {res}"
        );
    }

    // -- a keypress has no element -----------------------------------------
    let res = uia
        .call(ops::BACKEND_ACT, json!({"kind": "key", "key": "Tab"}))
        .await
        .expect("key");
    assert_eq!(res["applied"], json!(true), "a keypress goes to whatever has focus: {res}");
    assert_eq!(res["method"], json!("physical"), "there is no pattern for a keystroke: {res}");

    // -- and an id from another backend is refused, not aimed at ------------
    let res = uia
        .call(
            ops::BACKEND_ACT,
            json!({"kind": "click", "target": {"id": "brave:w1:t1:f0:e9", "bbox": [0, 0, 10, 10]}}),
        )
        .await;
    assert!(res.is_err(), "another backend's id is not this one's to act on: {res:?}");

    let _ = backend.kill();
    // Reaped, not just killed: a test that leaves zombies behind is a test
    // that slowly fills the process table of the machine it runs on.
    let _ = backend.wait();
}
