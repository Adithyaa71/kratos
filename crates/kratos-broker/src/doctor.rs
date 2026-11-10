//! `kratos doctor`: print the actual state, not a guess at it.
//!
//! Every long debugging session in this project's history ended at a fact
//! nobody had printed — a config the broker was not reading, a registry hive
//! written for one browser and not the next, a manifest with a relative path,
//! an extension registered against an id that had changed. None of those are
//! hard to find once somebody looks. This looks.
//!
//! It is read-only. It writes nothing, starts nothing and fixes nothing: a
//! diagnostic that changes the thing it is diagnosing is a diagnostic nobody
//! can trust twice.

use std::path::{Path, PathBuf};

use kratos_protocol::config::Config;
use serde_json::Value;

/// How a check came out.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Verdict {
    Ok,
    /// Works, but something about it will bite later.
    Warn,
    /// Will not work.
    Fail,
    /// Not configured, and that is a choice rather than a fault.
    Off,
}

impl Verdict {
    fn tag(self) -> &'static str {
        match self {
            Verdict::Ok => "ok  ",
            Verdict::Warn => "warn",
            Verdict::Fail => "FAIL",
            Verdict::Off => "off ",
        }
    }
}

struct Report {
    lines: Vec<(Verdict, String, String)>,
}

impl Report {
    fn new() -> Report {
        Report { lines: Vec::new() }
    }

    fn add(&mut self, v: Verdict, what: &str, detail: impl Into<String>) {
        self.lines.push((v, what.to_string(), detail.into()));
    }

    fn print(&self) -> i32 {
        let width = self.lines.iter().map(|(_, w, _)| w.len()).max().unwrap_or(0);
        for (v, what, detail) in &self.lines {
            println!("[{}] {what:width$}  {detail}", v.tag());
        }
        let fails = self.lines.iter().filter(|(v, _, _)| *v == Verdict::Fail).count();
        let warns = self.lines.iter().filter(|(v, _, _)| *v == Verdict::Warn).count();
        println!();
        match (fails, warns) {
            (0, 0) => println!("Nothing to fix."),
            (0, w) => println!("{w} thing(s) worth knowing about, nothing broken."),
            (f, _) => println!("{f} thing(s) will stop this working."),
        }
        i32::from(fails > 0)
    }
}

/// Run every check and print the result. Returns a process exit code.
pub fn run() -> i32 {
    let mut r = Report::new();

    // -- what this build is and where it lives ------------------------------
    let exe = std::env::current_exe().ok();
    let dir = exe.as_ref().and_then(|e| e.parent()).map(Path::to_path_buf);
    r.add(
        Verdict::Ok,
        "build",
        format!(
            "kratos-broker {} at {}",
            env!("CARGO_PKG_VERSION"),
            exe.as_ref().map(|e| e.display().to_string()).unwrap_or_else(|| "?".into())
        ),
    );

    // The binaries a working setup needs, and whether they are where
    // everything else expects them. A relative path in a manifest or an MCP
    // snippet is the single most common cause of "it works in my shell and
    // not in the harness".
    if let Some(dir) = &dir {
        for (name, needed) in [
            ("kratos-mcp.exe", true),
            ("kratos-host.exe", false),
            ("kratos-native.exe", false),
            ("kratos-ui.exe", false),
        ] {
            let p = dir.join(name);
            match (p.is_file(), needed) {
                (true, _) => r.add(Verdict::Ok, name, p.display().to_string()),
                (false, true) => r.add(
                    Verdict::Fail,
                    name,
                    format!("not beside the broker ({}); a harness cannot reach it", dir.display()),
                ),
                (false, false) => {
                    r.add(Verdict::Off, name, "not built; that feature is simply absent")
                }
            }
        }
    }

    // -- is anybody home ----------------------------------------------------
    //
    // Early, because every row below it is beside the point when the answer
    // is no. A locked session refuses SendInput and SetForegroundWindow with
    // no error to read and hands back window captures that were never drawn,
    // so a run on a locked machine produces a log full of `no_effect` and
    // nothing that says why.
    if kratos_ipc::win::session_is_unlocked() {
        r.add(Verdict::Ok, "session", "unlocked: there is a desktop to drive");
    } else {
        r.add(
            Verdict::Fail,
            "session",
            "locked. Nothing that needs the mouse, the keyboard or a visible window can run \
             until the person at this machine unlocks it; a `dom-ext` action through a browser \
             still can.",
        );
    }

    // -- config -------------------------------------------------------------
    let config_path = kratos_protocol::paths::config_path();
    let config = match Config::load_from(&config_path) {
        Ok(c) => {
            r.add(Verdict::Ok, "config", config_path.display().to_string());
            Some(c)
        }
        Err(kratos_protocol::config::LoadError::Missing) => {
            r.add(
                Verdict::Warn,
                "config",
                format!("{} does not exist yet; defaults are in use", config_path.display()),
            );
            Some(Config::default())
        }
        Err(e) => {
            r.add(
                Verdict::Fail,
                "config",
                format!("{} will not parse: {e}", config_path.display()),
            );
            None
        }
    };

    if let Some(cfg) = &config {
        // -- the decision model --------------------------------------------
        if cfg.decider.provider == "mock" {
            r.add(Verdict::Off, "decision model", "mock: offline, deterministic, no key needed");
        } else if cfg.decider.api_key_enc.is_empty() {
            r.add(
                Verdict::Fail,
                "decision model",
                format!(
                    "{} is configured with no key, so every act is refused \
                     decider_unconfigured. Settings window -> Decision model -> paste -> Save",
                    cfg.decider.provider
                ),
            );
        } else {
            r.add(
                Verdict::Ok,
                "decision model",
                format!("{}/{} with a sealed key", cfg.decider.provider, cfg.decider.model),
            );
        }

        // -- allocation -----------------------------------------------------
        let approved = cfg.harnesses.iter().filter(|h| h.approved).count();
        match cfg.harnesses.len() {
            0 => r.add(
                Verdict::Warn,
                "harnesses",
                "none seen yet; one appears here the first time a client connects",
            ),
            n => {
                let listed: Vec<String> = cfg
                    .harnesses
                    .iter()
                    .map(|h| {
                        format!(
                            "{}{}{}",
                            h.id,
                            if h.browser.is_empty() {
                                String::new()
                            } else {
                                format!("->{}", h.browser)
                            },
                            if h.approved { "" } else { " (not approved)" }
                        )
                    })
                    .collect();
                let v = if approved == 0 { Verdict::Warn } else { Verdict::Ok };
                r.add(v, "harnesses", format!("{n} known, {approved} approved: {}", listed.join(", ")));
            }
        }
        if !cfg.agents.is_empty() {
            let listed: Vec<String> = cfg
                .agents
                .iter()
                .map(|a| {
                    format!(
                        "{}{}",
                        a.id,
                        if a.profile_dir.is_empty() {
                            String::new()
                        } else {
                            format!("->{}", a.profile_dir)
                        }
                    )
                })
                .collect();
            r.add(Verdict::Ok, "agents", listed.join(", "));
        }
        r.add(
            Verdict::Ok,
            "defaults",
            format!(
                "browser={} profile={} open={} pause={}",
                cfg.defaults.browser,
                cfg.defaults.profile,
                cfg.defaults.open_method,
                if cfg.defaults.pause_on_user_input { "on" } else { "off" }
            ),
        );
    }

    // -- browsers -----------------------------------------------------------
    let installed = crate::browsers::installed();
    if installed.is_empty() {
        r.add(
            Verdict::Warn,
            "browsers",
            "no Chromium browser found; browser lanes cannot resolve and native windows still work",
        );
    }
    for b in &installed {
        let profiles = crate::browsers::profiles(b.brand);
        let names: Vec<String> = profiles
            .iter()
            .map(|p| match &p.name {
                Some(n) => format!("{} ({n})", p.dir),
                None => p.dir.clone(),
            })
            .collect();
        r.add(
            Verdict::Ok,
            b.brand,
            format!(
                "last used {:?}; profiles: {}",
                crate::browsers::last_used(b.brand),
                if names.is_empty() { "none listed".to_string() } else { names.join(", ") }
            ),
        );
    }
    match crate::browsers::os_default_brand() {
        Some(b) => r.add(Verdict::Ok, "default browser", format!("{b}, from UserChoice")),
        None => r.add(
            Verdict::Warn,
            "default browser",
            "not a Chromium browser this can drive; allocate one per harness instead",
        ),
    }

    // -- the browser bridge -------------------------------------------------
    check_bridge(&mut r, dir.as_deref());

    // -- the pixel path -----------------------------------------------------
    match crate::ensure::icon_weights() {
        Some(p) => r.add(Verdict::Ok, "icon detector", p.display().to_string()),
        None => r.add(
            Verdict::Off,
            "icon detector",
            "no weights; they are AGPL and not shipped -- see docs/pixel-path.md",
        ),
    }

    // -- is anything running ------------------------------------------------
    check_broker(&mut r);

    r.print()
}

/// The native-messaging manifest and the per-brand registry keys.
///
/// Both halves, because either one alone is a setup that looks correct and
/// does not work: a manifest with no registry key is never found, and a
/// registry key pointing at a missing manifest produces "Specified native
/// messaging host not found" against a registration that is provably there.
fn check_bridge(r: &mut Report, exe_dir: Option<&Path>) {
    let manifest = kratos_protocol::paths::state_dir().join("native-host.json");
    let parsed: Option<Value> = std::fs::read_to_string(&manifest)
        .ok()
        .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok());

    match &parsed {
        None => {
            r.add(
                Verdict::Off,
                "bridge manifest",
                format!(
                    "{} is absent, so no browser can start the host. The settings window's \
                     Browser tab writes it",
                    manifest.display()
                ),
            );
            return;
        }
        Some(m) => {
            let path = m["path"].as_str().unwrap_or_default();
            let origins = m["allowed_origins"].as_array().map(|a| a.len()).unwrap_or(0);
            let p = PathBuf::from(path);
            // Owned, because the policy note below names the ids it found.
            let mut notes: Vec<String> = Vec::new();
            if !p.is_absolute() {
                notes.push(
                    "the path is relative, and a browser resolves it against its own working \
                     directory"
                        .to_string(),
                );
            }
            if !p.is_file() {
                notes.push("the path does not point at a file".to_string());
            }
            if let (Some(dir), true) = (exe_dir, p.is_file()) {
                let expected = dir.join("kratos-host.exe");
                if expected.is_file() && expected != p {
                    notes.push(
                        "it points at a different build than the one beside this broker"
                            .to_string(),
                    );
                }
            }

            // The question that matters, and the one this did not ask.
            //
            // A browser loads the extension a policy force-installs, and
            // Chromium refuses `connectNative` when the caller's origin is
            // not in this manifest. On this machine the policy installed
            // `eefdlif...`, the packed .crx, and the manifest allowed only
            // `ldgfddp...`, the id an unpacked copy gets from its path. Both
            // files existed and pointed at real things, so every check here
            // passed while the bridge could not connect at all -- and the
            // only thing that worked was loading the unpacked copy by hand
            // from the extensions page, which produced the allowed id.
            let allowed: Vec<String> = m["allowed_origins"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|o| o.as_str())
                        .filter_map(|o| {
                            o.strip_prefix("chrome-extension://")
                                .map(|rest| rest.trim_end_matches('/').to_lowercase())
                        })
                        .collect()
                })
                .unwrap_or_default();
            let orphaned: Vec<String> =
                forced_extension_ids().into_iter().filter(|id| !allowed.contains(id)).collect();
            if !orphaned.is_empty() {
                notes.push(format!(
                    "a policy force-installs {} and the manifest does not allow it, so that \
                     extension cannot reach the host however the browser is started: press \
                     Register in the Browser tab",
                    orphaned.join(", ")
                ));
            }

            let v = if notes.is_empty() { Verdict::Ok } else { Verdict::Fail };
            r.add(
                v,
                "bridge manifest",
                format!(
                    "{path} ({origins} allowed origin(s)){}",
                    if notes.is_empty() {
                        String::new()
                    } else {
                        format!(" -- {}", notes.join("; "))
                    }
                ),
            );
        }
    }

    // Every brand, read back. A registry write has reported success here and
    // left nothing behind, and each Chromium brand reads its own hive -- so
    // the only answer worth having is what is in each one now.
    let mut registered = Vec::new();
    let mut missing = Vec::new();
    for (brand, hive) in BRIDGE_HIVES {
        match read_registry_sz(hive) {
            Some(value) => {
                let matches = Path::new(&value) == manifest.as_path();
                registered.push(format!("{brand}{}", if matches { "" } else { " (other path!)" }));
            }
            None => missing.push(*brand),
        }
    }
    if registered.is_empty() {
        r.add(
            Verdict::Fail,
            "bridge registry",
            "no brand has the host registered; the browser will refuse to start it",
        );
    } else {
        let v = if registered.iter().any(|s| s.contains("other path")) {
            Verdict::Fail
        } else {
            Verdict::Ok
        };
        r.add(
            v,
            "bridge registry",
            format!(
                "registered for {}{}",
                registered.join(", "),
                if missing.is_empty() {
                    String::new()
                } else {
                    format!("; absent for {}", missing.join(", "))
                }
            ),
        );
    }
}

/// `HKCU` paths the Chromium brands read their native-host registrations from.
///
/// Each brand reads its own, and writing one and testing in that browser is
/// how a registration comes to exist for exactly one of them.
const BRIDGE_HIVES: &[(&str, &str)] = &[
    ("brave", r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\ai.kratos.bridge"),
    ("chrome", r"Software\Google\Chrome\NativeMessagingHosts\ai.kratos.bridge"),
    ("edge", r"Software\Microsoft\Edge\NativeMessagingHosts\ai.kratos.bridge"),
    ("vivaldi", r"Software\Vivaldi\NativeMessagingHosts\ai.kratos.bridge"),
    ("chromium", r"Software\Chromium\NativeMessagingHosts\ai.kratos.bridge"),
];

#[cfg(windows)]
fn read_registry_sz(key: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

    let wide: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide.as_ptr(),
            std::ptr::null(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let chars = (size as usize / 2).min(buf.len());
    let len = buf[..chars].iter().position(|c| *c == 0).unwrap_or(chars);
    Some(String::from_utf16_lossy(&buf[..len]))
}

#[cfg(not(windows))]
fn read_registry_sz(_key: &str) -> Option<String> {
    None
}

/// Where each brand keeps `ExtensionInstallForcelist`, under HKCU.
const FORCELIST_KEYS: [&str; 5] = [
    r"Software\Policies\BraveSoftware\Brave-Browser\ExtensionInstallForcelist",
    r"Software\Policies\Google\Chrome\ExtensionInstallForcelist",
    r"Software\Policies\Microsoft\Edge\ExtensionInstallForcelist",
    r"Software\Policies\Vivaldi\ExtensionInstallForcelist",
    r"Software\Policies\Chromium\ExtensionInstallForcelist",
];

/// Every extension id a policy on this machine force-installs.
///
/// Entries are numbered named values holding `<id>;<update_url>`. Read by
/// number because that is the documented shape of the key.
///
/// The same list exists in `kratos-ui`'s `bridge.rs`, duplicated on purpose:
/// the broker does not depend on the settings window, and the alternative is
/// a registry reader in the one crate whose whole claim is that it does no
/// I/O. If one changes, change the other.
#[cfg(windows)]
fn forced_extension_ids() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in FORCELIST_KEYS {
        for n in 1..=20u32 {
            let Some(value) = read_registry_named(key, &n.to_string()) else { continue };
            let id = value.split(';').next().unwrap_or_default().trim().to_lowercase();
            let looks_like_an_id =
                id.len() == 32 && id.bytes().all(|b| b.is_ascii_lowercase() && b <= b'p');
            if looks_like_an_id && !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

#[cfg(not(windows))]
fn forced_extension_ids() -> Vec<String> {
    Vec::new()
}

/// One named registry value, where `read_registry_sz` reads a key's default.
#[cfg(windows)]
fn read_registry_named(key: &str, name: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

    let wide: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
    let value: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: both strings are NUL-terminated and outlive the call; buf and
    // size describe one buffer, and size is updated to the bytes written.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let chars = (size as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..chars]))
}

/// What a running broker says about itself.
///
/// Read over the pipe rather than inferred, and it deliberately does **not**
/// start one: a diagnostic that spawns a daemon has changed the machine it
/// was asked to describe.
fn check_broker(r: &mut Report) {
    let pipe = std::env::var("KRATOS_PIPE").unwrap_or_else(|_| "kratos.v1".to_string());
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            r.add(Verdict::Fail, "broker", format!("could not start a runtime to ask: {e}"));
            return;
        }
    };

    let status = runtime.block_on(async {
        let mut peer = kratos_ipc::PipePeer::attach(
            &pipe,
            kratos_protocol::Role::Ui,
            "kratos-doctor",
            concat!("kratos-doctor ", env!("CARGO_PKG_VERSION")),
        )
        .await
        .ok()?;
        peer.call(kratos_protocol::ops::STATUS, serde_json::json!({})).await.ok()
    });

    let Some(status) = status else {
        r.add(
            Verdict::Off,
            "broker",
            format!(
                "nothing listening on {pipe}. That is the resting state: a harness's first \
                 tool call starts one"
            ),
        );
        return;
    };

    r.add(
        Verdict::Ok,
        "broker",
        format!(
            "pid {} up {}s, {} peer(s)",
            status["pid"].as_u64().unwrap_or(0),
            status["uptime_ms"].as_u64().unwrap_or(0) / 1000,
            status["peers"].as_array().map(|a| a.len()).unwrap_or(0)
        ),
    );

    let backends = status["backends"].as_array().cloned().unwrap_or_default();
    if backends.is_empty() {
        r.add(
            Verdict::Fail,
            "backends",
            "none connected, so the broker can see nothing and every act fails no_backend",
        );
    } else {
        let listed: Vec<String> = backends
            .iter()
            .map(|b| {
                let source = b["source"].as_str().unwrap_or("?");
                match b["instance"]["token"].as_str() {
                    Some(t) => format!("{source}[{}]", &t[..t.len().min(8)]),
                    None => source.to_string(),
                }
            })
            .collect();
        r.add(Verdict::Ok, "backends", listed.join(", "));
    }

    // Lanes, which is the question this whole design exists to answer.
    let lanes = status["lanes"].as_array().cloned().unwrap_or_default();
    if lanes.is_empty() {
        r.add(Verdict::Off, "lanes", "no agent sessions connected");
    } else {
        for l in &lanes {
            let lane = &l["lane"];
            let state = lane["state"].as_str().unwrap_or("?");
            let v = match state {
                "ready" => Verdict::Ok,
                "unavailable" => Verdict::Fail,
                _ => Verdict::Warn,
            };
            r.add(
                v,
                "lane",
                format!(
                    "{} -> {}/{} ({state}){}",
                    l["harness"].as_str().unwrap_or("?"),
                    lane["browser"].as_str().unwrap_or("?"),
                    lane["profile_dir"].as_str().unwrap_or("?"),
                    lane["warning"].as_str().map(|w| format!(" -- {w}")).unwrap_or_default()
                ),
            );
        }
    }

    if let Some(engaged) = status["kill_engaged"].as_array() {
        if !engaged.is_empty() {
            r.add(
                Verdict::Fail,
                "kill switch",
                format!(
                    "engaged for {:?}; nothing will run until it is cleared from the settings \
                     window or the hotkey",
                    engaged
                ),
            );
        }
    }

    if status["pause"]["on_user_input"] == serde_json::json!(true)
        && status["pause"]["watching"] != serde_json::json!(true)
    {
        r.add(
            Verdict::Warn,
            "user-activity pause",
            "configured on, but the input hooks are not installed, so it will never pause",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_tag_is_fixed_width_so_the_output_lines_up() {
        for v in [Verdict::Ok, Verdict::Warn, Verdict::Fail, Verdict::Off] {
            assert_eq!(v.tag().len(), 4, "{:?}", v.tag());
        }
    }

    #[test]
    fn only_a_failure_is_a_failure() {
        let mut r = Report::new();
        r.add(Verdict::Ok, "a", "fine");
        r.add(Verdict::Warn, "b", "later");
        r.add(Verdict::Off, "c", "not configured");
        assert_eq!(r.print(), 0, "warnings and absences are not failures");

        r.add(Verdict::Fail, "d", "broken");
        assert_eq!(r.print(), 1);
    }

    #[test]
    fn every_brand_that_reads_its_own_hive_is_checked() {
        // Writing one hive and testing in that browser is how a registration
        // comes to exist for exactly one brand, so the check covers all of
        // them -- including plain Chromium, which reads Software\\Chromium.
        let brands: Vec<&str> = BRIDGE_HIVES.iter().map(|(b, _)| *b).collect();
        for expected in ["brave", "chrome", "edge", "vivaldi", "chromium"] {
            assert!(brands.contains(&expected), "{expected} reads its own hive and is not checked");
        }
        for (_, hive) in BRIDGE_HIVES {
            assert!(hive.ends_with("NativeMessagingHosts\\ai.kratos.bridge"), "{hive}");
        }
    }

    #[test]
    fn the_doctor_reads_and_never_writes() {
        // The guarantee that makes it worth running twice. Checked by
        // inspection of this module: nothing writes a file, nothing writes
        // the registry, nothing spawns a process, and the broker is attached
        // to rather than started. (Spelled around rather than out, because
        // the check below greps this very file.)
        let source = include_str!("doctor.rs");
        // Assembled rather than written out, or this very list would be the
        // match that fails the test.
        let forbidden = [
            concat!("fs", "::", "write"),
            concat!("connect", "_or_", "start"),
            concat!("RegSet", "ValueW"),
            concat!("Command", "::", "new"),
        ];
        for forbidden in forbidden {
            assert!(
                !source.contains(forbidden),
                "doctor must not {forbidden}: a diagnostic that changes the machine it \
                 describes cannot be trusted twice"
            );
        }
    }
}
