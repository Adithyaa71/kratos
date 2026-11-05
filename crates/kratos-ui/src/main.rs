//! kratos settings.
//!
//! One window for the things an operator needs to be able to change without
//! editing JSON: which decision model and its key, how long the broker stays
//! up when idle, how certain it has to be before acting, what the gate
//! escalates, and -- the one that matters most -- the kill switch, globally or
//! per browser.
//!
//! It also shows the action log, because a tool that can click things on your
//! behalf should be able to show you what it clicked.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod worker;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use serde_json::{json, Value};
use worker::{Cmd, Shared};

const BROWSERS: [&str; 5] = ["brave", "chrome", "edge", "vivaldi", "opera"];
const KINDS: [&str; 8] =
    ["click", "type", "key", "scroll", "select", "navigate", "open", "focus"];

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([940.0, 700.0])
            .with_min_inner_size([680.0, 480.0])
            .with_title("kratos"),
        ..Default::default()
    };
    eframe::run_native(
        "kratos",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Status,
    KillSwitch,
    Decider,
    Thresholds,
    Gate,
    Log,
}

/// Editable copies of the settings.
///
/// Separate from the polled snapshot on purpose: the worker refreshes status
/// every second, and if the edit fields read straight from that, anything
/// half-typed would be overwritten mid-keystroke.
#[derive(Default)]
struct Draft {
    epoch: u64,
    idle_exit_ms: String,
    hotkey: String,
    hotkey_scope: String,
    provider: String,
    model: String,
    endpoint: String,
    timeout_ms: String,
    /// Write-only. Never populated from the broker, which does not expose it.
    api_key: String,
    key_present: bool,
    thresholds: Vec<(String, f32)>,
    rate_limit: String,
    block_keywords: String,
    review_keywords: String,
    review_apps: String,
    review_domains: String,
    deny_domains: String,
    allow_recipients: String,
    allowed_browsers: String,
    per_browser: Vec<(String, bool)>,
}

struct App {
    shared: Arc<Mutex<Shared>>,
    tx: tokio::sync::mpsc::Sender<Cmd>,
    tab: Tab,
    draft: Draft,
    log_lines: usize,
}

impl App {
    fn new() -> Self {
        let pipe_base =
            std::env::var("KRATOS_PIPE").unwrap_or_else(|_| "kratos.v1".to_string());
        let (shared, tx) = worker::spawn(pipe_base);
        App { shared, tx, tab: Tab::Status, draft: Draft::default(), log_lines: 0 }
    }

    fn send(&self, cmd: Cmd) {
        // try_send, not send: a full queue means the worker is busy, and
        // dropping a redundant refresh is better than stalling the redraw.
        let _ = self.tx.try_send(cmd);
    }

    /// Refill the edit buffers from a newly-arrived config.
    fn sync_draft(&mut self, config: &Value, epoch: u64) {
        if self.draft.epoch == epoch {
            return;
        }
        self.draft.epoch = epoch;
        let d = &mut self.draft;
        d.idle_exit_ms = config["idle_exit_ms"].as_u64().unwrap_or(300_000).to_string();
        d.hotkey = string_of(&config["kill"]["hotkey"], "ctrl+alt+k");
        d.hotkey_scope = string_of(&config["kill"]["hotkey_scope"], "global");
        d.provider = string_of(&config["decider"]["provider"], "jev");
        d.model = string_of(&config["decider"]["model"], "jev-latest");
        d.endpoint = string_of(&config["decider"]["endpoint"], "");
        d.timeout_ms = config["decider"]["timeout_ms"].as_u64().unwrap_or(8_000).to_string();
        d.key_present = config["decider"]["key_present"].as_bool().unwrap_or(false);
        d.api_key.clear();

        d.thresholds = KINDS
            .iter()
            .map(|k| {
                (k.to_string(), config["thresholds"][*k].as_f64().unwrap_or(0.7) as f32)
            })
            .collect();

        d.rate_limit = config["gate"]["rate_limit_per_minute"].as_u64().unwrap_or(30).to_string();
        d.block_keywords = lines_of(&config["gate"]["block_keywords"]);
        d.review_keywords = lines_of(&config["gate"]["review_keywords"]);
        d.review_apps = lines_of(&config["gate"]["review_apps"]);
        d.review_domains = lines_of(&config["gate"]["review_domains"]);
        d.deny_domains = lines_of(&config["gate"]["deny_domains"]);
        d.allow_recipients = lines_of(&config["gate"]["allow_recipients"]);
        d.allowed_browsers = lines_of(&config["gate"]["allowed_browsers"]);

        d.per_browser = BROWSERS
            .iter()
            .map(|b| {
                (b.to_string(), config["kill"]["per_browser"][*b].as_bool().unwrap_or(true))
            })
            .collect();
    }
}

fn string_of(v: &Value, fallback: &str) -> String {
    v.as_str().unwrap_or(fallback).to_string()
}

fn lines_of(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("\n")
        })
        .unwrap_or_default()
}

fn list_from(text: &str) -> Vec<String> {
    text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (connected, status, config, config_epoch, error, notice, log) = {
            let s = self.shared.lock().expect("worker mutex");
            (
                s.connected,
                s.status.clone(),
                s.config.clone(),
                s.config_epoch,
                s.error.clone(),
                s.notice.clone(),
                s.log.clone(),
            )
        };
        if let Some(cfg) = &config {
            self.sync_draft(cfg, config_epoch);
        }

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("kratos");
                ui.separator();
                for (tab, label) in [
                    (Tab::Status, "Status"),
                    (Tab::KillSwitch, "Kill switch"),
                    (Tab::Decider, "Decision model"),
                    (Tab::Thresholds, "Confidence"),
                    (Tab::Gate, "Gate"),
                    (Tab::Log, "Log"),
                ] {
                    if ui.selectable_label(self.tab == tab, label).clicked() {
                        self.tab = tab;
                        if tab == Tab::Log && self.log_lines == 0 {
                            self.log_lines = 100;
                            self.send(Cmd::TailLog(100));
                        }
                    }
                }
            });
        });

        egui::TopBottomPanel::bottom("statusbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                let (dot, text) = if connected {
                    (egui::Color32::from_rgb(70, 170, 90), "broker connected")
                } else {
                    (egui::Color32::from_rgb(170, 90, 70), "broker not running")
                };
                ui.colored_label(dot, "\u{25CF}");
                ui.label(text);
                if let Some(e) = &error {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(200, 90, 70), e);
                } else if let Some(n) = &notice {
                    ui.separator();
                    ui.weak(n);
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
                Tab::Status => self.status_tab(ui, connected, status.as_ref()),
                Tab::KillSwitch => self.kill_tab(ui, status.as_ref()),
                Tab::Decider => self.decider_tab(ui),
                Tab::Thresholds => self.thresholds_tab(ui),
                Tab::Gate => self.gate_tab(ui),
                Tab::Log => self.log_tab(ui, &log),
            });
        });

        // Poll-driven UI, so ask for a redraw rather than spinning at the
        // display's refresh rate.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl App {
    fn status_tab(&mut self, ui: &mut egui::Ui, connected: bool, status: Option<&Value>) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!connected, egui::Button::new("Start broker"))
                .on_hover_text(
                    "The broker is normally started automatically by the first agent tool call, \
                     and exits again when idle. This is for looking at it directly.",
                )
                .clicked()
            {
                self.send(Cmd::StartBroker);
            }
            if ui.add_enabled(connected, egui::Button::new("Stop broker")).clicked() {
                self.send(Cmd::StopBroker);
            }
            if ui.add_enabled(connected, egui::Button::new("Reload settings")).clicked() {
                self.send(Cmd::ReloadConfig);
            }
        });
        ui.add_space(8.0);

        let Some(s) = status else {
            ui.weak(
                "Nothing is running. That is the normal resting state: the broker is not \
                 autostarted, and nothing is registered at logon.",
            );
            return;
        };

        egui::Grid::new("status").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.strong(k);
                ui.label(v);
                ui.end_row();
            };
            row("Build", string_of(&s["broker"], "?"));
            row("Process", s["pid"].as_u64().unwrap_or(0).to_string());
            row("Up for", human_ms(s["uptime_ms"].as_u64().unwrap_or(0)));
            row(
                "Idle",
                format!(
                    "{} of {}",
                    human_ms(s["idle_ms"].as_u64().unwrap_or(0)),
                    human_ms(s["idle_exit_ms"].as_u64().unwrap_or(0))
                ),
            );
            row("Decision model", string_of(&s["decider"]["name"], "?"));
            row(
                "API key",
                if s["decider"]["key_present"].as_bool().unwrap_or(false) {
                    "configured".into()
                } else {
                    "missing -- actions will be refused".to_string()
                },
            );
            row("Observations", s["generation"].as_u64().unwrap_or(0).to_string());
            row("Journal", string_of(&s["journal"], "?"));
        });

        ui.add_space(12.0);
        ui.strong("Perception backends");
        let backends = s["backends"].as_array().cloned().unwrap_or_default();
        if backends.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(200, 140, 70),
                "None connected. Without a backend the broker can decide nothing, because it \
                 cannot see the screen.",
            );
        } else {
            for b in backends {
                ui.label(format!(
                    "  {}  handles {}  (last seen {} ago)",
                    string_of(&b["source"], "?"),
                    b["handles"].as_array().map(|h| h.len()).unwrap_or(0),
                    human_ms(b["last_seen_ms"].as_u64().unwrap_or(0))
                ));
            }
        }

        ui.add_space(12.0);
        ui.strong("Input lease");
        if s["lease"].is_null() {
            ui.weak("  free");
        } else {
            ui.label(format!(
                "  held by {} for another {}",
                string_of(&s["lease"]["holder"], "?"),
                human_ms(s["lease"]["expires_in_ms"].as_u64().unwrap_or(0))
            ));
        }

        ui.add_space(12.0);
        ui.strong("Actions");
        let c = &s["counters"];
        ui.label(format!(
            "  {} asked -- {} executed, {} no effect, {} refused, {} awaiting approval, \
             {} blocked, {} killed",
            c["act"].as_u64().unwrap_or(0),
            c["executed"].as_u64().unwrap_or(0),
            c["no_effect"].as_u64().unwrap_or(0),
            c["refused"].as_u64().unwrap_or(0),
            c["needs_confirmation"].as_u64().unwrap_or(0),
            c["blocked"].as_u64().unwrap_or(0),
            c["killed"].as_u64().unwrap_or(0),
        ));
    }

    fn kill_tab(&mut self, ui: &mut egui::Ui, status: Option<&Value>) {
        let engaged: Vec<String> = status
            .map(|s| {
                s["kill_engaged"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let global = engaged.iter().any(|e| e == "global");

        ui.label(
            "Scopes, not a mode: one browser can be halted while everything else keeps working.",
        );
        ui.add_space(10.0);

        ui.group(|ui| {
            ui.horizontal(|ui| {
                if global {
                    ui.colored_label(egui::Color32::from_rgb(200, 70, 70), "\u{25A0} EVERYTHING HALTED");
                    if ui.button("Resume everything").clicked() {
                        self.send(Cmd::ResetKill("global".into()));
                    }
                } else {
                    ui.label("All targets running");
                    if ui
                        .add(egui::Button::new("STOP EVERYTHING").fill(egui::Color32::from_rgb(150, 50, 50)))
                        .clicked()
                    {
                        self.send(Cmd::Kill("global".into()));
                    }
                }
            });
        });

        ui.add_space(14.0);
        ui.strong("Per browser");
        ui.weak("Halts actions on that browser's windows only.");
        ui.add_space(6.0);
        for (brand, enabled) in self.draft.per_browser.clone() {
            if !enabled {
                continue;
            }
            let scope = format!("browser:{brand}");
            let is_on = engaged.contains(&scope);
            ui.horizontal(|ui| {
                ui.add_sized([90.0, 20.0], egui::Label::new(&brand));
                if is_on {
                    ui.colored_label(egui::Color32::from_rgb(200, 70, 70), "halted");
                    if ui.button("Resume").clicked() {
                        self.send(Cmd::ResetKill(scope.clone()));
                    }
                } else {
                    ui.weak("running");
                    if ui.button("Halt").clicked() {
                        self.send(Cmd::Kill(scope.clone()));
                    }
                }
            });
        }

        ui.add_space(16.0);
        ui.strong("Hotkey");
        ui.weak(
            "Works anywhere, even while every agent is mid-action. An agent can engage the kill \
             switch but can never clear one -- only this window or the hotkey can.",
        );
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.draft.hotkey).desired_width(140.0));
            egui::ComboBox::from_label("engages")
                .selected_text(&self.draft.hotkey_scope)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.draft.hotkey_scope, "global".into(), "everything");
                    for b in BROWSERS {
                        ui.selectable_value(
                            &mut self.draft.hotkey_scope,
                            format!("browser:{b}"),
                            format!("{b} only"),
                        );
                    }
                });
        });
        ui.weak("Needs a modifier, e.g. ctrl+alt+k. Takes effect on the next broker start.");

        ui.add_space(8.0);
        ui.strong("Browsers offered above");
        ui.horizontal_wrapped(|ui| {
            for (brand, enabled) in self.draft.per_browser.iter_mut() {
                ui.checkbox(enabled, brand.as_str());
            }
        });

        ui.add_space(12.0);
        if ui.button("Save kill-switch settings").clicked() {
            let per: serde_json::Map<String, Value> = self
                .draft
                .per_browser
                .iter()
                .map(|(b, e)| (b.clone(), json!(e)))
                .collect();
            self.send(Cmd::ApplyConfig(json!({
                "kill": {
                    "hotkey": self.draft.hotkey.trim(),
                    "hotkey_scope": self.draft.hotkey_scope.trim(),
                    "per_browser": per,
                }
            })));
        }
    }

    fn decider_tab(&mut self, ui: &mut egui::Ui) {
        ui.label(
            "The broker is the only process that holds this key. It is sealed with Windows DPAPI \
             for this account, never written to the log, and never readable back over the wire.",
        );
        ui.add_space(10.0);

        egui::Grid::new("decider").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.strong("Provider");
            egui::ComboBox::from_id_salt("provider")
                .selected_text(&self.draft.provider)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.draft.provider, "jev".into(), "jev (TypeSafe)");
                    ui.selectable_value(
                        &mut self.draft.provider,
                        "cloudflare-clef".into(),
                        "cloudflare-clef (open weights)",
                    );
                    ui.selectable_value(&mut self.draft.provider, "mock".into(), "mock (offline)");
                });
            ui.end_row();

            ui.strong("Model");
            ui.add(egui::TextEdit::singleline(&mut self.draft.model).desired_width(260.0));
            ui.end_row();

            ui.strong("Endpoint");
            ui.add(egui::TextEdit::singleline(&mut self.draft.endpoint).desired_width(420.0));
            ui.end_row();

            ui.strong("API key");
            ui.vertical(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.api_key)
                        .password(true)
                        .hint_text(if self.draft.key_present {
                            "a key is stored -- type here only to replace it"
                        } else {
                            "no key stored; actions will be refused"
                        })
                        .desired_width(420.0),
                );
            });
            ui.end_row();

            ui.strong("Timeout (ms)");
            ui.add(egui::TextEdit::singleline(&mut self.draft.timeout_ms).desired_width(100.0));
            ui.end_row();

            ui.strong("Broker idle exit (ms)");
            ui.vertical(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.idle_exit_ms).desired_width(100.0),
                );
                ui.weak("0 keeps it running. Idle counts agent calls only.");
            });
            ui.end_row();
        });

        ui.add_space(6.0);
        if self.draft.provider == "cloudflare-clef" {
            ui.weak(
                "Clef speaks the same System One request shape, so only the endpoint differs: \
                 https://api.cloudflare.com/client/v4/accounts/<id>/ai/run/@cf/cloudflare/<model>",
            );
        }
        ui.weak(
            "Any provider works here as long as it returns a label with a probability \
             distribution and a calibrated confidence. One that returns only a label is refused, \
             because the confidence bars would then be decorative.",
        );

        ui.add_space(12.0);
        if ui.button("Save").clicked() {
            let mut decider = json!({
                "provider": self.draft.provider.trim(),
                "model": self.draft.model.trim(),
                "endpoint": self.draft.endpoint.trim(),
                "timeout_ms": self.draft.timeout_ms.trim().parse::<u64>().unwrap_or(8_000),
            });
            // Only sent when the operator actually typed one, so saving other
            // settings never clears a stored key.
            if !self.draft.api_key.trim().is_empty() {
                decider["api_key"] = json!(self.draft.api_key.trim());
            }
            self.send(Cmd::ApplyConfig(json!({
                "decider": decider,
                "idle_exit_ms": self.draft.idle_exit_ms.trim().parse::<u64>().unwrap_or(300_000),
            })));
            self.draft.api_key.clear();
        }
    }

    fn thresholds_tab(&mut self, ui: &mut egui::Ui) {
        ui.label(
            "How certain the model must be before an action runs. The bar is applied to the \
             weaker of two answers -- which action, and which element -- because an action it is \
             sure about, aimed at an element it is guessing at, is the failure that matters.",
        );
        ui.weak(
            "Measured on the previous build: executed decisions ran at 0.96 and above, escalated \
             ones at 0.65 and below, with nothing in between.",
        );
        ui.add_space(12.0);

        for (kind, value) in self.draft.thresholds.iter_mut() {
            ui.horizontal(|ui| {
                ui.add_sized([80.0, 20.0], egui::Label::new(kind.as_str()));
                ui.add(egui::Slider::new(value, 0.0..=1.0).fixed_decimals(2));
            });
        }

        ui.add_space(12.0);
        if ui.button("Save").clicked() {
            let thresholds: serde_json::Map<String, Value> = self
                .draft
                .thresholds
                .iter()
                .map(|(k, v)| (k.clone(), json!((*v as f64 * 100.0).round() / 100.0)))
                .collect();
            self.send(Cmd::ApplyConfig(json!({"thresholds": thresholds})));
        }
    }

    fn gate_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("One rule per line.");
        ui.add_space(8.0);

        let area = |ui: &mut egui::Ui, title: &str, help: &str, buf: &mut String, rows: usize| {
            ui.strong(title);
            ui.weak(help);
            ui.add(egui::TextEdit::multiline(buf).desired_rows(rows).desired_width(f32::INFINITY));
            ui.add_space(10.0);
        };

        area(
            ui,
            "Never allowed",
            "Matched against the whole instruction, quoted text included. No confirmation path.",
            &mut self.draft.block_keywords,
            4,
        );
        area(
            ui,
            "Needs approval -- keywords",
            "Matched as whole words, and NOT against text being typed: a subject line containing \
             \"order\" is not an order.",
            &mut self.draft.review_keywords,
            6,
        );
        area(ui, "Needs approval -- applications", "Matched against the window title.", &mut self.draft.review_apps, 4);
        area(
            ui,
            "Needs approval -- sites",
            "Subdomains included. These additionally require a click in a native dialog that no \
             agent can reach.",
            &mut self.draft.review_domains,
            4,
        );
        area(ui, "Blocked sites", "", &mut self.draft.deny_domains, 3);
        area(
            ui,
            "Allowed recipients",
            "When non-empty, any email address outside this list escalates to a native dialog, \
             even with no keyword match. A full address, or a bare domain for all of it.",
            &mut self.draft.allow_recipients,
            3,
        );

        area(
            ui,
            "Browsers this agent may drive",
            "By brand -- brave, chrome, edge, vivaldi, opera. One per line. Empty means any. \
             Matched against the real browser, never against a page title.",
            &mut self.draft.allowed_browsers,
            3,
        );

        ui.horizontal(|ui| {
            ui.strong("Actions per minute");
            ui.add(egui::TextEdit::singleline(&mut self.draft.rate_limit).desired_width(70.0));
        });

        ui.add_space(12.0);
        if ui.button("Save gate rules").clicked() {
            self.send(Cmd::ApplyConfig(json!({"gate": {
                "block_keywords": list_from(&self.draft.block_keywords),
                "review_keywords": list_from(&self.draft.review_keywords),
                "review_apps": list_from(&self.draft.review_apps),
                "review_domains": list_from(&self.draft.review_domains),
                "deny_domains": list_from(&self.draft.deny_domains),
                "allow_recipients": list_from(&self.draft.allow_recipients),
                "allowed_browsers": list_from(&self.draft.allowed_browsers),
                "rate_limit_per_minute": self.draft.rate_limit.trim().parse::<u32>().unwrap_or(30),
            }})));
        }
    }

    fn log_tab(&mut self, ui: &mut egui::Ui, log: &[Value]) {
        ui.horizontal(|ui| {
            ui.strong("Action log");
            for n in [50usize, 200, 1000] {
                if ui.selectable_label(self.log_lines == n, format!("last {n}")).clicked() {
                    self.log_lines = n;
                    self.send(Cmd::TailLog(n));
                }
            }
        });
        ui.add_space(8.0);

        if log.is_empty() {
            ui.weak("Nothing logged yet.");
            return;
        }

        for line in log.iter().rev() {
            let status = string_of(&line["status"], line["op"].as_str().unwrap_or("?"));
            let colour = match status.as_str() {
                "executed" => egui::Color32::from_rgb(90, 170, 110),
                "no_effect" => egui::Color32::from_rgb(210, 160, 70),
                "refused" | "needs_confirmation" => egui::Color32::from_rgb(190, 180, 90),
                "blocked" | "killed" => egui::Color32::from_rgb(200, 90, 80),
                _ => ui.visuals().text_color(),
            };
            ui.horizontal_wrapped(|ui| {
                ui.weak(short_time(line["ts"].as_str().unwrap_or("")));
                ui.colored_label(colour, format!("{status:<18}"));
                ui.weak(string_of(&line["peer_id"], "?"));
                ui.label(truncate(&string_of(&line["instruction"], ""), 90));
            });
            let mut detail = Vec::new();
            if let Some(k) = line["kind"].as_str() {
                detail.push(format!("kind={k}"));
            }
            if let Some(c) = line["confidence"].as_f64() {
                detail.push(format!("conf={c:.2}/{:.2}", line["threshold"].as_f64().unwrap_or(0.0)));
            }
            if let Some(t) = line["target_label"].as_str() {
                detail.push(format!("target={}", truncate(t, 48)));
            }
            if let Some(e) = line["evidence"].as_str() {
                detail.push(format!("evidence={e}"));
            }
            if let Some(g) = line["gate"].as_str() {
                detail.push(format!("gate={g}"));
            }
            if let Some(b) = line["backend"].as_str() {
                detail.push(format!("via={b}"));
            }
            if let Some(ms) = line["took_ms"].as_u64() {
                detail.push(format!("{ms}ms"));
            }
            if !detail.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(70.0);
                    ui.weak(detail.join("  "));
                });
            }
            ui.separator();
        }
    }
}

fn human_ms(ms: u64) -> String {
    if ms == 0 {
        return "never".to_string();
    }
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    }
}

fn short_time(ts: &str) -> String {
    // "2026-11-04T09:21:44.182Z" -> "09:21:44"
    ts.split('T').nth(1).map(|t| t.chars().take(8).collect()).unwrap_or_default()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "\u{2026}"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_draft_loads_from_a_redacted_config_without_a_key() {
        // The broker never sends the key, so the field must come up empty and
        // the UI must know one is stored.
        let config = json!({
            "idle_exit_ms": 120000,
            "kill": {"hotkey": "ctrl+alt+j", "hotkey_scope": "browser:brave",
                     "per_browser": {"brave": true, "chrome": false}},
            "decider": {"provider": "cloudflare-clef", "model": "clef-flash",
                        "endpoint": "https://example", "timeout_ms": 5000, "key_present": true},
            "thresholds": {"click": 0.8, "type": 0.9},
            "gate": {"review_keywords": ["send", "pay"], "rate_limit_per_minute": 12,
                     "allow_recipients": []}
        });
        let mut holder = App {
            shared: Arc::new(Mutex::new(Shared::default())),
            tx: tokio::sync::mpsc::channel(1).0,
            tab: Tab::Status,
            draft: Draft::default(),
            log_lines: 0,
        };
        holder.sync_draft(&config, 1);
        let d = &holder.draft;
        assert_eq!(d.idle_exit_ms, "120000");
        assert_eq!(d.hotkey, "ctrl+alt+j");
        assert_eq!(d.hotkey_scope, "browser:brave");
        assert_eq!(d.provider, "cloudflare-clef");
        assert_eq!(d.model, "clef-flash");
        assert!(d.key_present);
        assert!(d.api_key.is_empty(), "the key field must never be prefilled");
        assert_eq!(d.review_keywords, "send\npay");
        assert_eq!(d.rate_limit, "12");
        assert!(!d.per_browser.iter().find(|(b, _)| b == "chrome").unwrap().1);
        assert_eq!(d.thresholds.iter().find(|(k, _)| k == "click").unwrap().1, 0.8);
    }

    #[test]
    fn syncing_twice_with_the_same_epoch_does_not_clobber_edits() {
        let mut holder = App {
            shared: Arc::new(Mutex::new(Shared::default())),
            tx: tokio::sync::mpsc::channel(1).0,
            tab: Tab::Status,
            draft: Draft::default(),
            log_lines: 0,
        };
        holder.sync_draft(&json!({"idle_exit_ms": 1000}), 1);
        holder.draft.idle_exit_ms = "half typed".into();
        // A status poll arrives; the config has not changed.
        holder.sync_draft(&json!({"idle_exit_ms": 1000}), 1);
        assert_eq!(holder.draft.idle_exit_ms, "half typed");
        // A genuine config change does refresh.
        holder.sync_draft(&json!({"idle_exit_ms": 2000}), 2);
        assert_eq!(holder.draft.idle_exit_ms, "2000");
    }

    #[test]
    fn missing_config_fields_fall_back_rather_than_blanking_the_form() {
        let mut holder = App {
            shared: Arc::new(Mutex::new(Shared::default())),
            tx: tokio::sync::mpsc::channel(1).0,
            tab: Tab::Status,
            draft: Draft::default(),
            log_lines: 0,
        };
        holder.sync_draft(&json!({}), 1);
        assert_eq!(holder.draft.hotkey, "ctrl+alt+k");
        assert_eq!(holder.draft.provider, "jev");
        assert_eq!(holder.draft.idle_exit_ms, "300000");
        assert_eq!(holder.draft.thresholds.len(), KINDS.len());
        assert_eq!(holder.draft.per_browser.len(), BROWSERS.len());
    }

    #[test]
    fn list_parsing_drops_blanks_and_trims() {
        assert_eq!(list_from("send\n\n  pay  \n"), vec!["send", "pay"]);
        assert!(list_from("   \n\n").is_empty());
        // Round-trips with the renderer.
        assert_eq!(lines_of(&json!(["a", "b"])), "a\nb");
        assert_eq!(list_from(&lines_of(&json!(["a", "b"]))), vec!["a", "b"]);
    }

    #[test]
    fn durations_read_as_english() {
        assert_eq!(human_ms(0), "never");
        assert_eq!(human_ms(4_200), "4s");
        assert_eq!(human_ms(95_000), "1m 35s");
        assert_eq!(human_ms(7_380_000), "2h 3m");
    }

    #[test]
    fn timestamps_shorten_to_a_clock_time() {
        assert_eq!(short_time("2026-11-04T09:21:44.182Z"), "09:21:44");
        assert_eq!(short_time("nonsense"), "");
    }

    #[test]
    fn truncation_is_character_safe() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("\u{201c}quoted and long\u{201d}", 6).chars().count(), 7);
    }
}
