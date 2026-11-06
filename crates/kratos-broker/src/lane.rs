//! Which browser and which profile a session works in, and whether it may.
//!
//! The rules are spec §3, and they are deliberately boring: two lookups in the
//! config file, a fallback to what the machine already prefers, and no
//! guessing anywhere. The agent's part in this is a *name*; everything the
//! name maps to is the user's to set.
//!
//! Resolution happens per connection and is never stored against a session,
//! so an allocation changed in the settings window takes effect on the next
//! connection rather than needing anything to be invalidated first.

use kratos_protocol::{config::Config, Lane, LaneAssignment, LaneState, Source};

use crate::browsers;

/// What a session said about itself. All three are claims, and all three are
/// optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub harness: Option<String>,
    pub agent: Option<String>,
    pub session: Option<String>,
}

impl Identity {
    pub fn from_hello(hello: &kratos_protocol::Hello) -> Identity {
        Identity {
            harness: hello.harness.clone(),
            agent: hello.agent.clone(),
            session: hello.session.clone(),
        }
    }

    /// What to call this harness in the config file and the settings window.
    ///
    /// An agent that sends no harness name is still a harness: it is whatever
    /// `name` it introduced itself with, which for an MCP shim is its client's
    /// name and otherwise is a self-reported label. Both are claims; neither
    /// grants anything.
    pub fn harness_id(&self, fallback_name: &str) -> String {
        self.harness
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback_name)
            .to_string()
    }
}

/// An attached backend instance, reduced to what lane resolution needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedInstance {
    pub token: String,
    /// What the host reported, which it read from the process tree.
    pub brand: Option<String>,
    /// What the config pairs this token with, if the user or a launch has
    /// paired it.
    pub bound_to: Option<(String, String)>,
}

/// Resolve the lane for one session.
///
/// `attached` is every live `dom-ext` instance. It decides the lane's *state*
/// and nothing else: which browser and profile a session belongs to is a
/// question about the user's allocation, not about what happens to be running,
/// and answering it from what is running would make a lane move when a browser
/// was closed.
pub fn resolve(config: &Config, id: &Identity, attached: &[AttachedInstance]) -> Lane {
    let mut warning: Option<String> = None;

    // -- browser ------------------------------------------------------------
    //
    // harnesses[harness].browser, else defaults.browser, else the OS default.
    let assigned_harness = id.harness.as_deref().and_then(|h| config.harness(h));
    let wanted = assigned_harness
        .map(|h| h.browser.trim())
        .filter(|b| !b.is_empty())
        .or_else(|| {
            Some(config.defaults.browser.trim()).filter(|b| !b.is_empty() && *b != "os-default")
        });

    let browser = match wanted {
        Some(name) => match browsers::normalise(name) {
            Some(brand) => brand,
            None => {
                // A brand nobody has heard of is the user's typo, and silently
                // substituting another browser is how an agent ends up driving
                // the wrong one.
                let fallback = browsers::fallback_brand().unwrap_or("brave");
                warning = Some(format!(
                    "{name:?} is not a browser this build knows; using {fallback}"
                ));
                fallback
            }
        },
        None => match browsers::fallback_brand() {
            Some(brand) => brand,
            None => {
                return Lane {
                    browser: String::new(),
                    profile_dir: String::new(),
                    profile_name: None,
                    assigned: assignment(id),
                    state: LaneState::Unavailable,
                    warning: Some(
                        "no Chromium browser was found on this machine, so there is no lane to \
                         resolve. Allocate one in the settings window, or install one."
                            .to_string(),
                    ),
                }
            }
        },
    };

    let located = browsers::locate(browser);
    let installed = located.as_ref().is_some_and(|b| b.installed());
    let known = browsers::profiles(browser);

    // -- profile ------------------------------------------------------------
    //
    // agents[agent].profile_dir if it is present in that browser, else the
    // browser's last-used profile.
    let fallback_profile = || {
        let d = config.defaults.profile.trim();
        if d.is_empty() || d == "last-used" {
            browsers::last_used(browser)
        } else {
            d.to_string()
        }
    };

    let assigned_profile =
        id.agent.as_deref().and_then(|a| config.agent(a)).map(|a| a.profile_dir.trim().to_string());

    let profile_dir = match assigned_profile.filter(|p| !p.is_empty()) {
        Some(want) => {
            // Present means listed in that browser's own Local State. A
            // profile directory that is not there would open a *new* profile
            // with none of the user's logins in it, which is the one outcome
            // worth warning about rather than doing quietly.
            if known.is_empty() || known.iter().any(|p| p.dir == want) {
                want
            } else {
                let fallback = fallback_profile();
                warning = Some(format!(
                    "profile {want:?} is not one of {browser}'s profiles ({}), so this session is \
                     using {fallback:?} instead. Pick a profile that exists in the settings \
                     window.",
                    known.iter().map(|p| p.dir.as_str()).collect::<Vec<_>>().join(", ")
                ));
                fallback
            }
        }
        None => fallback_profile(),
    };

    let profile_name =
        known.iter().find(|p| p.dir == profile_dir).and_then(|p| p.name.clone());

    // -- state --------------------------------------------------------------
    let state = if !installed {
        if warning.is_none() {
            warning = Some(format!(
                "{browser} is not installed on this machine, so nothing can be opened for this \
                 lane"
            ));
        }
        LaneState::Unavailable
    } else if attached.iter().any(|i| i.bound_to.as_ref().is_some_and(|(b, p)| {
        b.eq_ignore_ascii_case(browser) && *p == profile_dir
    })) {
        LaneState::Ready
    } else {
        let unbound_same_brand = attached
            .iter()
            .filter(|i| i.bound_to.is_none())
            .any(|i| i.brand.as_deref().is_some_and(|b| b.eq_ignore_ascii_case(browser)));
        if unbound_same_brand && warning.is_none() {
            warning = Some(format!(
                "a {browser} extension is attached but is not paired with a profile yet, so this \
                 lane is driven through UI Automation. Pair it under Profiles in the settings \
                 window."
            ));
        }
        LaneState::Degraded
    };

    Lane {
        browser: browser.to_string(),
        profile_dir,
        profile_name,
        assigned: assignment(id),
        state,
        warning,
    }
}

fn assignment(id: &Identity) -> LaneAssignment {
    LaneAssignment { harness: id.harness.clone(), agent: id.agent.clone() }
}

/// Every live `dom-ext` instance, as lane resolution wants to see it.
pub async fn attached_instances(broker: &crate::state::Broker) -> Vec<AttachedInstance> {
    let config = broker.config.read().await;
    broker
        .backends_for(Source::DomExt)
        .await
        .iter()
        .filter_map(|p| p.instance.as_ref())
        .map(|i| AttachedInstance {
            token: i.token.clone(),
            brand: i.brand.clone(),
            bound_to: config
                .instance(&i.token)
                .filter(|c| !c.brand.trim().is_empty() && !c.profile_dir.trim().is_empty())
                .map(|c| (c.brand.clone(), c.profile_dir.clone())),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kratos_protocol::config::{AgentConfig, HarnessConfig};

    fn cfg() -> Config {
        let mut c = Config::default();
        c.decider.provider = "mock".into();
        c
    }

    fn id(harness: &str, agent: &str) -> Identity {
        Identity {
            harness: Some(harness.to_string()),
            agent: (!agent.is_empty()).then(|| agent.to_string()),
            session: Some("s-1".to_string()),
        }
    }

    /// A brand that is certainly not installed, so "unavailable" is testable
    /// without depending on what this machine happens to have.
    const ABSENT: &str = "opera";

    fn harness(id: &str, browser: &str, approved: bool) -> HarnessConfig {
        HarnessConfig {
            id: id.into(),
            label: String::new(),
            browser: browser.into(),
            approved,
            gate_overrides: serde_json::Value::Null,
        }
    }

    fn agent(id: &str, profile_dir: &str) -> AgentConfig {
        AgentConfig {
            id: id.into(),
            label: String::new(),
            profile_dir: profile_dir.into(),
            gate_overrides: serde_json::Value::Null,
        }
    }

    #[test]
    fn a_harness_gets_the_browser_the_user_gave_it() {
        let mut c = cfg();
        c.harnesses.push(harness("claude", "vivaldi", true));
        c.harnesses.push(harness("bluee", "chrome", true));
        assert_eq!(resolve(&c, &id("claude", ""), &[]).browser, "vivaldi");
        assert_eq!(resolve(&c, &id("bluee", ""), &[]).browser, "chrome");
    }

    #[test]
    fn an_agent_gets_the_profile_the_user_gave_it() {
        let mut c = cfg();
        c.harnesses.push(harness("claude", "chrome", true));
        c.agents.push(agent("agent-a", "Profile 7"));
        let lane = resolve(&c, &id("claude", "agent-a"), &[]);
        // Accepted when the browser lists no profiles at all (nothing to
        // contradict it); warned about when it lists some and this is not one.
        if browsers::profiles("chrome").is_empty() {
            assert_eq!(lane.profile_dir, "Profile 7");
            assert!(lane.warning.is_none() || lane.warning.as_deref().unwrap().contains("installed"));
        } else {
            assert_ne!(lane.profile_dir, "Profile 7");
            assert!(lane.warning.unwrap().contains("Profile 7"));
        }
    }

    #[test]
    fn a_missing_profile_falls_back_and_says_so() {
        let mut c = cfg();
        c.harnesses.push(harness("claude", "chrome", true));
        c.agents.push(agent("agent-a", "Profile does not exist 42"));
        if browsers::profiles("chrome").is_empty() {
            eprintln!("chrome has no profiles on this machine; nothing to contradict");
            return;
        }
        let lane = resolve(&c, &id("claude", "agent-a"), &[]);
        assert_eq!(lane.profile_dir, browsers::last_used("chrome"));
        let w = lane.warning.expect("a fallback must be said out loud");
        assert!(w.contains("Profile does not exist 42"), "{w}");
        assert!(w.contains("settings"), "the warning must say where to fix it: {w}");
    }

    #[test]
    fn an_unassigned_harness_gets_the_default_browser_and_the_last_used_profile() {
        let c = cfg();
        let lane = resolve(&c, &id("never-seen-before", ""), &[]);
        match browsers::fallback_brand() {
            Some(brand) => {
                assert_eq!(lane.browser, brand);
                assert_eq!(lane.profile_dir, browsers::last_used(brand));
            }
            None => assert_eq!(lane.state, LaneState::Unavailable),
        }
    }

    #[test]
    fn a_pinned_default_browser_overrides_the_os_default() {
        let mut c = cfg();
        c.defaults.browser = "chrome".into();
        assert_eq!(resolve(&c, &id("anything", ""), &[]).browser, "chrome");
    }

    #[test]
    fn a_browser_nobody_has_heard_of_warns_rather_than_being_substituted_silently() {
        let mut c = cfg();
        c.harnesses.push(harness("claude", "netscape", true));
        let lane = resolve(&c, &id("claude", ""), &[]);
        let w = lane.warning.expect("a typo must be reported");
        assert!(w.contains("netscape"), "{w}");
    }

    #[test]
    fn a_browser_that_is_not_installed_is_unavailable_not_ready() {
        if browsers::locate(ABSENT).is_some_and(|b| b.installed()) {
            eprintln!("{ABSENT} is installed here; skipping");
            return;
        }
        let mut c = cfg();
        c.harnesses.push(harness("claude", ABSENT, true));
        let lane = resolve(&c, &id("claude", ""), &[]);
        assert_eq!(lane.state, LaneState::Unavailable);
        assert!(lane.warning.unwrap().contains("not installed"));
    }

    #[test]
    fn a_lane_is_ready_only_when_a_bound_instance_matches_both_halves() {
        let Some(brand) = browsers::fallback_brand() else {
            eprintln!("no browser here; skipping");
            return;
        };
        let mut c = cfg();
        c.harnesses.push(harness("claude", brand, true));
        let profile = browsers::last_used(brand);

        let bound = |b: &str, p: &str| AttachedInstance {
            token: "tok".into(),
            brand: Some(b.to_string()),
            bound_to: Some((b.to_string(), p.to_string())),
        };

        assert_eq!(resolve(&c, &id("claude", ""), &[bound(brand, &profile)]).state, LaneState::Ready);
        // The right browser, the wrong profile: not this lane.
        assert_eq!(
            resolve(&c, &id("claude", ""), &[bound(brand, "Profile 99")]).state,
            LaneState::Degraded
        );
    }

    #[test]
    fn an_unpaired_instance_leaves_the_lane_degraded_and_explains_itself() {
        let Some(brand) = browsers::fallback_brand() else {
            eprintln!("no browser here; skipping");
            return;
        };
        let mut c = cfg();
        c.harnesses.push(harness("claude", brand, true));
        let unbound =
            AttachedInstance { token: "tok".into(), brand: Some(brand.to_string()), bound_to: None };
        let lane = resolve(&c, &id("claude", ""), &[unbound]);
        assert_eq!(lane.state, LaneState::Degraded);
        assert!(lane.warning.unwrap().contains("paired"));
    }

    #[test]
    fn resolution_follows_the_config_rather_than_remembering_anything() {
        // "Every new session re-resolves" is the whole of the mechanism: there
        // is no per-session store to go stale, so the same identity resolved
        // twice across a config change gives two different answers.
        let mut c = cfg();
        c.harnesses.push(harness("claude", "chrome", true));
        let before = resolve(&c, &id("claude", ""), &[]);
        c.harnesses.clear();
        c.harnesses.push(harness("claude", "edge", true));
        let after = resolve(&c, &id("claude", ""), &[]);
        assert_eq!(before.browser, "chrome");
        assert_eq!(after.browser, "edge");
    }

    #[test]
    fn the_lane_carries_who_it_was_resolved_for() {
        let c = cfg();
        let lane = resolve(&c, &id("claude", "agent-a"), &[]);
        assert_eq!(lane.assigned.harness.as_deref(), Some("claude"));
        assert_eq!(lane.assigned.agent.as_deref(), Some("agent-a"));
    }
}
