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

#[tokio::test]
async fn a_real_window_is_read_and_driven_through_its_own_patterns() {
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

    // It attaches once per source; the tree is the one that acts.
    let mut uia = None;
    for _ in 0..2 {
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
