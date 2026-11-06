//! MCP shim for kratos.
//!
//! Speaks JSON-RPC 2.0 over stdio to an agent harness, and the kratos
//! wire protocol over a named pipe to the broker. It holds no policy, no key
//! and no state: every decision, every gate check and every log line happens
//! in the broker, so running two harnesses against one machine cannot produce
//! two different sets of rules.
//!
//! `tools/list` deliberately does **not** touch the pipe. A harness listing
//! what it can do should not start a background process; the broker comes up
//! on the first `desktop_*` call that needs it and exits again when idle.

use std::io::Write;

use kratos_ipc::PipePeer;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

const MCP_PROTOCOL: &str = "2025-06-18";
const BUILD: &str = concat!("kratos-mcp ", env!("CARGO_PKG_VERSION"));
const SERVER_NAME: &str = "kratos";

fn main() {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kratos-mcp: could not start the runtime: {e}");
            std::process::exit(1);
        }
    };
    runtime.block_on(serve());
}

/// Trim a line from a client, tolerating a byte-order mark.
///
/// `str::trim` does not remove U+FEFF: a BOM is a format character, not
/// whitespace. Several runtimes write one at the head of their first stdio
/// write without being asked, and the result was that `initialize` -- always
/// the first message, so always the one carrying it -- came back -32700 while
/// every message after it parsed. A harness whose handshake is rejected
/// concludes the server is broken, and that is a hard thing to attribute from
/// the far end.
fn clean(line: &str) -> &str {
    line.trim_start_matches('\u{feff}').trim()
}

async fn serve() {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut broker: Option<PipePeer> = None;
    let pipe_base =
        std::env::var("KRATOS_PIPE").unwrap_or_else(|_| "kratos.v1".to_string());
    let agent_name = std::env::var("KRATOS_AGENT_NAME").unwrap_or_else(|_| "mcp-client".to_string());

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = clean(&line);
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                // -32700 parse error, with a null id as JSON-RPC requires when
                // the id could not be recovered.
                emit(&json!({
                    "jsonrpc": "2.0", "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                }));
                continue;
            }
        };

        let id = request.get("id").cloned();
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let params = request.get("params").cloned().unwrap_or(json!({}));

        // A notification has no id and must never be answered.
        let is_notification = id.is_none();

        let outcome = handle(&method, params, &mut broker, &pipe_base, &agent_name).await;

        if is_notification {
            continue;
        }
        let id = id.unwrap_or(Value::Null);
        match outcome {
            Ok(result) => emit(&json!({"jsonrpc": "2.0", "id": id, "result": result})),
            Err((code, message)) => {
                emit(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
            }
        }
    }
}

fn emit(value: &Value) {
    let mut out = std::io::stdout().lock();
    if let Ok(text) = serde_json::to_string(value) {
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }
}

async fn handle(
    method: &str,
    params: Value,
    broker: &mut Option<PipePeer>,
    pipe_base: &str,
    agent_name: &str,
) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": MCP_PROTOCOL,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        })),

        "notifications/initialized" | "initialized" => Ok(json!({})),

        "ping" => Ok(json!({})),

        "tools/list" => Ok(json!({"tools": tools()})),

        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            call_tool(name, args, broker, pipe_base, agent_name).await
        }

        "resources/list" => Ok(json!({"resources": []})),
        "prompts/list" => Ok(json!({"prompts": []})),

        other => Err((-32601, format!("method not found: {other}"))),
    }
}

/// Map a tool name to a wire op and payload.
fn translate(name: &str, args: Value) -> Option<(&'static str, Value)> {
    use kratos_protocol::ops;
    let payload = match name {
        "desktop_observe" => (
            ops::OBSERVE,
            json!({
                "window": args.get("window"),
                "prefer": args.get("prefer"),
                "timeout_ms": args.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(8_000),
            }),
        ),
        "desktop_act" => (
            ops::ACT,
            json!({
                "instruction": args.get("instruction").and_then(|v| v.as_str()).unwrap_or(""),
                "window": args.get("window"),
                "dry_run": args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false),
                "target_id": args.get("target_id"),
                "lease": args.get("lease"),
                "timeout_ms": args.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(20_000),
            }),
        ),
        "desktop_verify" => (
            ops::VERIFY,
            json!({
                "claim": args.get("claim").and_then(|v| v.as_str()).unwrap_or(""),
                "window": args.get("window"),
            }),
        ),
        "desktop_confirm" => (
            ops::CONFIRM,
            json!({"token": args.get("token").and_then(|v| v.as_str()).unwrap_or("")}),
        ),
        "desktop_status" => (ops::STATUS, json!({})),
        "desktop_kill" => (
            ops::KILL_ENGAGE,
            json!({
                "scope": args.get("scope").and_then(|v| v.as_str()).unwrap_or("global"),
                "reason": args.get("reason"),
            }),
        ),
        "desktop_log" => (
            ops::LOG_TAIL,
            json!({"n": args.get("n").and_then(|v| v.as_u64()).unwrap_or(50)}),
        ),
        _ => return None,
    };
    Some(payload)
}

async fn call_tool(
    name: &str,
    args: Value,
    broker: &mut Option<PipePeer>,
    pipe_base: &str,
    agent_name: &str,
) -> Result<Value, (i64, String)> {
    let Some((op, payload)) = translate(name, args) else {
        return Err((-32602, format!("unknown tool: {name}")));
    };

    // Connect here, not at startup: this is the lazy start.
    if broker.is_none() {
        match PipePeer::connect_or_start(
            pipe_base,
            kratos_protocol::Role::Agent,
            agent_name,
            BUILD,
        )
        .await
        {
            Ok(c) => *broker = Some(c),
            Err(e) => {
                return Ok(tool_error(format!(
                    "the kratos broker could not be started or reached: {e}\n\nCheck that \
                     kratos-broker.exe sits beside kratos-mcp.exe, or set KRATOS_BROKER_EXE."
                )))
            }
        }
    }

    let client = broker.as_mut().expect("connected above");
    match client.call(op, payload.clone()).await {
        Ok(result) => Ok(tool_ok(&result)),
        Err(e) => {
            // A lost connection is worth one transparent reconnect: the broker
            // may have hit its idle timeout between two tool calls, which is
            // normal operation rather than a failure.
            //
            // But only for a read. If the connection dropped during an `act`,
            // the action may already have been dispatched, and resending it
            // would click Send twice. An unanswered mutation is reported, not
            // retried -- the same asymmetry the broker applies to a backend
            // timeout.
            if e.code == kratos_protocol::codes::INTERNAL && is_read_only(op) {
                *broker = None;
                match PipePeer::connect_or_start(
            pipe_base,
            kratos_protocol::Role::Agent,
            agent_name,
            BUILD,
        )
        .await
        {
                    Ok(mut fresh) => {
                        let retried = fresh.call(op, payload).await;
                        *broker = Some(fresh);
                        match retried {
                            Ok(result) => return Ok(tool_ok(&result)),
                            Err(again) => {
                                return Ok(tool_error(format!(
                                    "{}: {}",
                                    again.code, again.message
                                )))
                            }
                        }
                    }
                    Err(reconnect) => {
                        return Ok(tool_error(format!(
                            "lost the broker and could not reconnect: {reconnect}"
                        )))
                    }
                }
            }
            Ok(tool_error(format!("{}: {}", e.code, e.message)))
        }
    }
}

/// Whether an op can be safely repeated after a dropped connection.
fn is_read_only(op: &str) -> bool {
    use kratos_protocol::ops;
    matches!(op, ops::OBSERVE | ops::VERIFY | ops::STATUS | ops::LOG_TAIL | ops::KILL_STATUS)
}

fn tool_ok(value: &Value) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    json!({"content": [{"type": "text", "text": text}], "isError": false})
}

fn tool_error(message: String) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

const INSTRUCTIONS: &str = "\
Drive this Windows machine one step at a time. The loop is observe, then act \
with a single specific sentence, then verify if success is not obvious.

Write each instruction as you would to a person looking at the same screen: \
name the target specifically, say which dialog or pane it is in when that is \
ambiguous, and put any literal text to be typed in quotes. Text that is not \
quoted will not be typed -- the action is refused instead, because nothing \
here invents text.

Read `status` on every result. `executed` means something verifiably changed. \
`no_effect` means the action was dispatched and nothing changed, so do not \
assume it worked. `refused` means the decision model was not sure enough: \
name the target more precisely. `needs_confirmation` means a human must \
approve -- do something else and come back with desktop_confirm rather than \
waiting.";

fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "desktop_observe",
            "description": "List what is on screen now: the window, its URL if it is a browser, \
                            and every interactive element with its role, label, containing dialog \
                            or pane, and current value. Call this before acting on an unfamiliar \
                            screen.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {"type": "string", "description": "Part of a window title. Omit for the window in front."},
                    "prefer": {"type": "string", "enum": ["dom-ext", "uia", "ocr", "pixel"],
                               "description": "Force one perception backend. Omit to let the broker route."},
                    "timeout_ms": {"type": "integer"}
                }
            }
        }),
        json!({
            "name": "desktop_act",
            "description": "Carry out one step, described in plain language. A decision model \
                            chooses the action kind and the target element from what is actually \
                            on screen; literal text, keys and URLs are taken from your sentence, \
                            never invented. Put text to type in quotes. Returns what it decided, \
                            what the gate said, and mechanical evidence that something changed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instruction": {"type": "string",
                                    "description": "One step, e.g. click the \"Send\" button, or type \"hello\" into the Subject field"},
                    "window": {"type": "string"},
                    "dry_run": {"type": "boolean", "description": "Decide and gate, but dispatch nothing."},
                    "target_id": {"type": "string",
                                  "description": "Pin the element by an id from a recent desktop_observe, instead of letting the model choose."},
                    "lease": {"type": "string"},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["instruction"]
            }
        }),
        json!({
            "name": "desktop_verify",
            "description": "Ask whether the screen now supports a claim, as a calibrated \
                            probability. Use after a step whose success is not mechanically \
                            obvious, and always after a no_effect.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "claim": {"type": "string", "description": "e.g. the message was sent and the compose window is closed"},
                    "window": {"type": "string"}
                },
                "required": ["claim"]
            }
        }),
        json!({
            "name": "desktop_confirm",
            "description": "Resume an action that returned needs_confirmation, after a human has \
                            approved it. Single-use, expires, and refused if the foreground window \
                            changed. Some rules additionally require a human to click a native \
                            dialog that no agent can reach.",
            "inputSchema": {
                "type": "object",
                "properties": {"token": {"type": "string"}},
                "required": ["token"]
            }
        }),
        json!({
            "name": "desktop_status",
            "description": "Broker health: connected perception backends, who holds the input \
                            lease, engaged kill scopes, the decision model in use, and action \
                            counters.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "desktop_kill",
            "description": "Engage the kill switch and stop further actions. Scope it to \
                            everything (global), one browser (browser:brave), or one application \
                            (app:excel.exe). An agent can engage but cannot clear it -- only the \
                            operator can, from the settings window or the hotkey.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": {"type": "string", "description": "global, browser:<brand>, or app:<exe>"},
                    "reason": {"type": "string"}
                }
            }
        }),
        json!({
            "name": "desktop_log",
            "description": "Recent entries from the action log: what was asked, what was decided, \
                            what the gate said, and what changed.",
            "inputSchema": {
                "type": "object",
                "properties": {"n": {"type": "integer", "description": "How many lines, newest last."}}
            }
        }),
    ]
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_byte_order_mark_does_not_break_the_handshake() {
        // What a runtime that "helpfully" marks its first write sends.
        let line = "\u{feff}{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}";
        assert!(serde_json::from_str::<Value>(line).is_err(), "precondition");
        let v: Value = serde_json::from_str(clean(line)).expect("parses once cleaned");
        assert_eq!(v["method"], "initialize");
    }

    #[test]
    fn ordinary_lines_are_unchanged_and_blank_ones_stay_blank() {
        assert_eq!(clean("  {\"a\":1}\r\n"), "{\"a\":1}");
        assert_eq!(clean(""), "");
        assert_eq!(clean("\u{feff}"), "");
        assert_eq!(clean("   \t "), "");
        // A mark inside a string is data, not framing: leave it alone.
        assert_eq!(clean("{\"a\":\"x\u{feff}y\"}"), "{\"a\":\"x\u{feff}y\"}");
        // Several can stack when writes are concatenated.
        assert_eq!(clean("\u{feff}\u{feff}{}"), "{}");
    }

    use super::*;

    #[test]
    fn every_tool_has_a_schema_and_a_translation() {
        for t in tools() {
            let name = t["name"].as_str().unwrap();
            assert!(name.starts_with("desktop_"), "{name}");
            assert!(t["description"].as_str().unwrap().len() > 40, "{name} needs a real description");
            assert_eq!(t["inputSchema"]["type"], json!("object"), "{name}");
            assert!(translate(name, json!({})).is_some(), "{name} has no wire translation");
        }
    }

    #[test]
    fn required_arguments_are_declared() {
        let all = tools();
        let act = all.iter().find(|t| t["name"] == json!("desktop_act")).unwrap();
        assert_eq!(act["inputSchema"]["required"], json!(["instruction"]));
        let verify = all.iter().find(|t| t["name"] == json!("desktop_verify")).unwrap();
        assert_eq!(verify["inputSchema"]["required"], json!(["claim"]));
    }

    #[test]
    fn translation_maps_tools_onto_wire_ops() {
        use kratos_protocol::ops;
        let (op, payload) =
            translate("desktop_act", json!({"instruction": "click the \"Send\" button"})).unwrap();
        assert_eq!(op, ops::ACT);
        assert_eq!(payload["instruction"], json!("click the \"Send\" button"));
        assert_eq!(payload["dry_run"], json!(false));
        assert_eq!(payload["timeout_ms"], json!(20_000));

        let (op, payload) = translate("desktop_kill", json!({})).unwrap();
        assert_eq!(op, ops::KILL_ENGAGE);
        assert_eq!(payload["scope"], json!("global"), "an unscoped kill halts everything");

        assert!(translate("desktop_reset_kill", json!({})).is_none(), "agents cannot reset");
        assert!(translate("rm_rf", json!({})).is_none());
    }

    #[tokio::test]
    async fn listing_tools_does_not_touch_the_pipe() {
        // The lazy-start contract: a harness enumerating capabilities must not
        // cause a background process to appear.
        let mut broker = None;
        let result = handle("tools/list", json!({}), &mut broker, "kratos-test-never", "t")
            .await
            .unwrap();
        assert_eq!(result["tools"].as_array().unwrap().len(), 7);
        assert!(broker.is_none(), "tools/list must not connect");
    }

    #[tokio::test]
    async fn initialize_reports_the_protocol_and_instructions_without_connecting() {
        let mut broker = None;
        let r = handle("initialize", json!({}), &mut broker, "kratos-test-never", "t").await.unwrap();
        assert_eq!(r["protocolVersion"], json!(MCP_PROTOCOL));
        assert_eq!(r["serverInfo"]["name"], json!(SERVER_NAME));
        assert!(r["instructions"].as_str().unwrap().contains("quotes"));
        assert!(broker.is_none());
    }

    #[tokio::test]
    async fn an_unknown_method_is_a_jsonrpc_error_not_a_panic() {
        let mut broker = None;
        let err = handle("tools/explode", json!({}), &mut broker, "kratos-test-never", "t")
            .await
            .unwrap_err();
        assert_eq!(err.0, -32601);
    }

    #[tokio::test]
    async fn an_unknown_tool_is_rejected_before_any_connection_attempt() {
        let mut broker = None;
        let err = call_tool("desktop_nope", json!({}), &mut broker, "kratos-test-never", "t")
            .await
            .unwrap_err();
        assert_eq!(err.0, -32602);
        assert!(broker.is_none());
    }

    #[tokio::test]
    async fn an_unreachable_broker_is_reported_as_a_tool_error_not_a_crash() {
        // KRATOS_BROKER_EXE points at nothing, so the spawn fails and the shim must
        // still answer the harness.
        std::env::set_var("KRATOS_BROKER_EXE", "Z:\\no-such-broker.exe");
        let mut broker = None;
        let r = call_tool("desktop_status", json!({}), &mut broker, "kratos-test-absent-q7", "t")
            .await
            .unwrap();
        assert_eq!(r["isError"], json!(true));
        assert!(r["content"][0]["text"].as_str().unwrap().contains("could not be started"));
        std::env::remove_var("KRATOS_BROKER_EXE");
    }

    #[test]
    fn only_reads_are_retried_after_a_dropped_connection() {
        use kratos_protocol::ops;
        // Repeating an unanswered mutation could click Send twice.
        assert!(!is_read_only(ops::ACT));
        assert!(!is_read_only(ops::CONFIRM));
        assert!(!is_read_only(ops::KILL_ENGAGE));
        assert!(!is_read_only(ops::LEASE_ACQUIRE));
        assert!(is_read_only(ops::OBSERVE));
        assert!(is_read_only(ops::VERIFY));
        assert!(is_read_only(ops::STATUS));
        assert!(is_read_only(ops::LOG_TAIL));
    }

    #[test]
    fn tool_results_carry_mcp_content_blocks() {
        let ok = tool_ok(&json!({"status": "executed"}));
        assert_eq!(ok["isError"], json!(false));
        assert_eq!(ok["content"][0]["type"], json!("text"));
        assert!(ok["content"][0]["text"].as_str().unwrap().contains("executed"));

        let bad = tool_error("nope".into());
        assert_eq!(bad["isError"], json!(true));
    }
}
