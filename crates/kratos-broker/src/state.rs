//! Everything the broker knows, and the rules for changing it.
//!
//! One `Arc<Broker>` is shared by every connection task. The locks are
//! deliberately small and never held across an await that does I/O: the gate
//! and the lease are on the path of every action, and a lock held across an
//! HTTP round trip to the decision model would serialise every agent behind
//! the slowest one.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kratos_protocol::{codes, config::Config, Element, Source, WireError};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex, Notify, RwLock};

use crate::journal::Journal;

pub const BROKER_NAME: &str = concat!("kratos-broker ", env!("CARGO_PKG_VERSION"));

fn config_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(kratos_protocol::paths::config_path()).ok()?.modified().ok()
}

// ---------------------------------------------------------------------------
// Peers
// ---------------------------------------------------------------------------

/// What the backend registry is keyed on.
///
/// `source` alone was the key until a second browser profile attached and
/// evicted the first. One `dom-ext` source has as many instances as there are
/// browser profiles open, and a tab in one cannot be reached through another,
/// so they are different peers for the same source rather than one peer
/// replacing another.
///
/// `None` for the instance means "the only instance of this source". That is
/// not a placeholder: `uia`, `ocr` and `pixel` really are one per machine, and
/// a restarted one still supersedes itself, which is the self-healing the
/// protocol has always promised.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RegistryKey {
    pub source: Source,
    pub instance: Option<String>,
}

pub struct PeerHandle {
    pub peer_id: String,
    pub role: kratos_protocol::Role,
    pub name: String,
    pub source: Option<Source>,
    /// Which instance of `source` this is, when it said.
    pub instance: Option<kratos_protocol::Instance>,
    /// Who this peer says it is: harness, agent, session. Claims, all three.
    pub identity: crate::lane::Identity,
    /// The harness name this peer is accounted under, which is its claimed
    /// harness or, failing that, the name it introduced itself with. Stored
    /// rather than recomputed so approval and the journal agree.
    pub harness_id: String,
    /// The lane resolved at handshake. Re-resolved by `whoami`, because the
    /// user may have changed the allocation since and the answer to "where am
    /// I" should be the current one.
    pub lane: Mutex<Option<kratos_protocol::Lane>>,
    pub handles: Vec<String>,
    /// Outbound queue. A task drains this into the socket, so no caller ever
    /// holds a writer lock and a slow peer cannot stall the broker.
    pub out: mpsc::Sender<kratos_protocol::Frame>,
    /// Replies the broker is waiting for, when it is the one asking
    /// (`backend.*`).
    pub pending: Mutex<HashMap<String, oneshot::Sender<kratos_protocol::Frame>>>,
    pub connected_at: Instant,
    pub last_seen: AtomicU64,
}

impl PeerHandle {
    pub fn touch(&self, now_ms: u64) {
        self.last_seen.store(now_ms, Ordering::Relaxed);
    }

    /// The slot this peer occupies in the backend registry, if it is a backend.
    pub fn registry_key(&self) -> Option<RegistryKey> {
        self.source.map(|source| RegistryKey {
            source,
            instance: self
                .instance
                .as_ref()
                .filter(|i| i.usable())
                .map(|i| i.token.clone()),
        })
    }

    pub fn info(&self, now_ms: u64) -> kratos_protocol::BackendInfo {
        kratos_protocol::BackendInfo {
            source: self.source.map(|s| s.as_str().to_string()).unwrap_or_default(),
            handles: self.handles.clone(),
            last_seen_ms: Some(now_ms.saturating_sub(self.last_seen.load(Ordering::Relaxed))),
            instance: self.instance.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Lease
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Lease {
    pub token: String,
    pub holder: String,
    pub scope: String,
    pub expires: Instant,
}

#[derive(Default)]
pub struct LeaseState {
    current: Option<Lease>,
    /// Tickets waiting, in arrival order. This is what makes the queue FIFO
    /// rather than a scramble: without it, whichever task the runtime happens
    /// to wake first wins, and an unlucky agent can starve indefinitely.
    queue: VecDeque<u64>,
    next_ticket: u64,
}

impl LeaseState {
    /// Drop an expired lease. Returns whether one was dropped, because the
    /// machine-wide guard has to be given back exactly once per acquisition
    /// and the caller is the one holding the lock.
    #[must_use]
    fn expire(&mut self, now: Instant) -> bool {
        if self.current.as_ref().is_some_and(|l| l.expires <= now) {
            self.current = None;
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Parked confirmations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Parked {
    pub instruction: String,
    pub window: Option<String>,
    pub rule: Option<String>,
    pub requires_os_dialog: bool,
    pub peer_id: String,
    /// The window that was in front when this was parked. A token is refused
    /// if it has changed, so an approval for "click Send in Gmail" cannot be
    /// redeemed against whatever happens to be in front two minutes later.
    pub foreground_hwnd: isize,
    pub created: Instant,
}

// ---------------------------------------------------------------------------
// Observations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ObservationRecord {
    pub elements: Vec<Element>,
}

impl ObservationRecord {
    pub fn element(&self, id: &str) -> Option<&Element> {
        self.elements.iter().find(|e| e.id == id)
    }
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Counters {
    pub act: AtomicU64,
    pub executed: AtomicU64,
    pub no_effect: AtomicU64,
    pub refused: AtomicU64,
    pub needs_confirmation: AtomicU64,
    pub blocked: AtomicU64,
    pub killed: AtomicU64,
    pub planned: AtomicU64,
}

impl Counters {
    pub fn record(&self, status: &str) {
        use kratos_protocol::status as st;
        let c = match status {
            st::EXECUTED => &self.executed,
            st::NO_EFFECT => &self.no_effect,
            st::REFUSED => &self.refused,
            st::NEEDS_CONFIRMATION => &self.needs_confirmation,
            st::BLOCKED => &self.blocked,
            st::KILLED => &self.killed,
            st::PLANNED => &self.planned,
            _ => return,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Value {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        json!({
            "act": g(&self.act),
            "executed": g(&self.executed),
            "no_effect": g(&self.no_effect),
            "refused": g(&self.refused),
            "needs_confirmation": g(&self.needs_confirmation),
            "blocked": g(&self.blocked),
            "killed": g(&self.killed),
            "planned": g(&self.planned),
        })
    }
}

// ---------------------------------------------------------------------------
// Broker
// ---------------------------------------------------------------------------

/// Whether a harness may drive the machine, as decided for this broker's
/// lifetime.
///
/// The config file holds the durable answer (`harnesses[].approved`); this
/// holds what was decided since, which is how "allow just for now" exists at
/// all. A deny is kept here too: asking again on the agent's next request
/// would turn a refusal into a dialog every few seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Allowed,
    Denied,
    /// A dialog is on screen and nobody has answered yet. Held so an agent in
    /// a retry loop raises one dialog rather than one per request.
    Asking,
}

pub struct Broker {
    pub config: RwLock<Config>,
    /// Per-harness decisions made since this broker started.
    pub approvals: Mutex<HashMap<String, Approval>>,
    /// Which session first acted on which browser tab: `brand:wN:tN` to a
    /// `peer_id`.
    ///
    /// "Tabs a session opened are owned by it" (spec §5). Nothing here can see
    /// a tab being *opened* -- the extension reports tabs, not their history --
    /// so first use is the proxy, and it is the same answer in every case that
    /// matters: the session that opened a tab is the first to act on it.
    tab_owners: Mutex<HashMap<String, String>>,
    /// The modification time of the config file as this broker last saw it,
    /// so a reload fires on somebody else's edit and not on its own write.
    config_seen: Mutex<Option<std::time::SystemTime>>,
    /// One approval dialog at a time, machine-wide.
    ///
    /// Two clients connecting at once would otherwise stack two system-modal
    /// dialogs on top of each other, and the user would answer whichever
    /// landed on top for whichever client they thought they were answering.
    pub approval_gate: Mutex<()>,
    pub gate: Mutex<kratos_gate::SafetyGate>,
    pub decider: RwLock<Arc<dyn kratos_decide::DecisionProvider>>,
    pub journal: RwLock<Journal>,
    pub peers: RwLock<HashMap<String, Arc<PeerHandle>>>,
    pub lease: Mutex<LeaseState>,
    pub lease_notify: Notify,
    /// The machine-wide input guard: one mouse, one holder, across every
    /// broker this Windows user is running.
    ///
    /// `None` only if the kernel object could not be created at all, in which
    /// case the broker still works and the lease is per-broker as it was --
    /// refusing to start over this would trade a rare, narrow race for a
    /// desktop that cannot be driven.
    input_guard: Option<kratos_ipc::win::InputGuard>,
    pub parked: Mutex<HashMap<String, Parked>>,
    pub last_observation: RwLock<Option<ObservationRecord>>,
    pub counters: Counters,
    pub started: Instant,
    /// Milliseconds since `started` at the last *agent* request. A UI polling
    /// `status` deliberately does not move this, or the idle timer could never
    /// fire while a settings window was open.
    last_agent_ms: AtomicU64,
    generation: AtomicU64,
    peer_seq: AtomicU64,
    pub shutdown: Notify,
}

impl Broker {
    pub fn new(config: Config, api_key: String) -> Arc<Self> {
        Broker::with_input_guard(config, api_key, None)
    }

    /// For tests: a broker whose machine-wide input guard has a name of its
    /// own, so two brokers in one test process do not contend for the real
    /// one -- and neither contends with the broker the operator may have
    /// running while the suite runs.
    pub fn with_input_guard(
        config: Config,
        api_key: String,
        guard: Option<&str>,
    ) -> Arc<Self> {
        let gate = kratos_gate::SafetyGate::new(config.gate.clone());
        let decider = kratos_decide::build(&config.decider, api_key);
        let journal = Journal::from_config(&config.log);
        Arc::new(Broker {
            config: RwLock::new(config),
            approvals: Mutex::new(HashMap::new()),
            tab_owners: Mutex::new(HashMap::new()),
            config_seen: Mutex::new(config_mtime()),
            approval_gate: Mutex::new(()),
            gate: Mutex::new(gate),
            decider: RwLock::new(decider),
            journal: RwLock::new(journal),
            peers: RwLock::new(HashMap::new()),
            lease: Mutex::new(LeaseState::default()),
            lease_notify: Notify::new(),
            input_guard: match guard
                .map(|g| Ok(format!("kratos.input.{g}")))
                .unwrap_or_else(kratos_ipc::input_guard_name)
                .and_then(|n| kratos_ipc::win::InputGuard::open(&n))
            {
                Ok(g) => Some(g),
                Err(e) => {
                    eprintln!(
                        "kratos-broker: no machine-wide input guard ({e}); the input lease is \
                         per-broker only"
                    );
                    None
                }
            },
            parked: Mutex::new(HashMap::new()),
            last_observation: RwLock::new(None),
            counters: Counters::default(),
            started: Instant::now(),
            last_agent_ms: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            peer_seq: AtomicU64::new(0),
            shutdown: Notify::new(),
        })
    }

    pub fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn next_peer_id(&self, role: kratos_protocol::Role) -> String {
        let n = self.peer_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let prefix = match role {
            kratos_protocol::Role::Agent => "a",
            kratos_protocol::Role::Backend => "b",
            kratos_protocol::Role::Ui => "u",
        };
        format!("{prefix}-{n}")
    }

    pub fn mark_agent_activity(&self) {
        self.last_agent_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    pub fn idle_ms(&self) -> u64 {
        self.now_ms().saturating_sub(self.last_agent_ms.load(Ordering::Relaxed))
    }

    // -- peers --------------------------------------------------------------

    pub async fn add_peer(&self, peer: Arc<PeerHandle>) -> Option<Arc<PeerHandle>> {
        let mut peers = self.peers.write().await;
        // A backend re-registering **the same instance of** its source
        // supersedes the old one, so a crashed-and-restarted backend heals
        // itself instead of leaving a dead entry that routing would keep
        // choosing.
        //
        // Only the same key. Keying this on `source` alone meant a second
        // browser profile attaching evicted the first, so one harness opening
        // its browser silently unplugged another harness's -- with the
        // displaced one told `evt superseded`, which read like a restart and
        // was not one.
        let key = peer.registry_key();
        let superseded = key.and_then(|key| {
            peers
                .values()
                .find(|p| p.registry_key().as_ref() == Some(&key) && p.peer_id != peer.peer_id)
                .cloned()
        });
        if let Some(old) = &superseded {
            peers.remove(&old.peer_id);
        }
        peers.insert(peer.peer_id.clone(), peer);
        superseded
    }

    pub async fn remove_peer(&self, peer_id: &str) {
        self.peers.write().await.remove(peer_id);
        self.release_tabs(peer_id).await;
        // Anyone holding the input lease who disconnects must not keep it.
        let mut lease = self.lease.lock().await;
        if lease.current.as_ref().is_some_and(|l| l.holder == peer_id) {
            lease.current = None;
            drop(lease);
            self.give_back_input();
            self.lease_notify.notify_waiters();
        }
    }

    /// Every live instance of a source, newest connection first.
    ///
    /// The order is the tie-break, not a preference: lanes decide which
    /// instance a session reaches (P3), and until they do, "newest" is the
    /// only answer that preserves what this broker did before instances
    /// existed, when the newest had evicted every older one.
    pub async fn backends_for(&self, source: Source) -> Vec<Arc<PeerHandle>> {
        let mut found: Vec<Arc<PeerHandle>> = self
            .peers
            .read()
            .await
            .values()
            .filter(|p| p.source == Some(source))
            .cloned()
            .collect();
        found.sort_by_key(|p| std::cmp::Reverse(p.connected_at));
        found
    }

    pub async fn backend_for(&self, source: Source) -> Option<Arc<PeerHandle>> {
        self.backends_for(source).await.into_iter().next()
    }

    pub async fn has_ui(&self) -> bool {
        self.peers.read().await.values().any(|p| p.role == kratos_protocol::Role::Ui)
    }

    pub async fn backend_infos(&self) -> Vec<kratos_protocol::BackendInfo> {
        let now = self.now_ms();
        let mut v: Vec<_> = self
            .peers
            .read()
            .await
            .values()
            .filter(|p| p.source.is_some())
            .map(|p| p.info(now))
            .collect();
        // By source, then by instance token: with two instances of one source
        // the order has to be stable or a status read looks different each
        // time for no reason.
        v.sort_by(|a, b| {
            let token = |i: &Option<kratos_protocol::Instance>| {
                i.as_ref().map(|x| x.token.clone()).unwrap_or_default()
            };
            a.source.cmp(&b.source).then_with(|| token(&a.instance).cmp(&token(&b.instance)))
        });
        v
    }

    /// Backends to try for a target class, in preference order, filtered to
    /// those actually connected and enabled in config.
    pub async fn route(&self, class: kratos_protocol::TargetClass, prefer: Option<Source>) -> Vec<Source> {
        let enabled = self.config.read().await.backends.enabled.clone();
        let is_enabled = |s: Source| enabled.iter().any(|e| e == s.as_str());
        let peers = self.peers.read().await;
        let connected = |s: Source| peers.values().any(|p| p.source == Some(s));

        if let Some(p) = prefer {
            // An explicit preference is honoured or refused, never silently
            // downgraded: a caller that asked for the DOM backend and got
            // pixels would draw wrong conclusions from the result.
            return if is_enabled(p) && connected(p) { vec![p] } else { vec![] };
        }
        class
            .preference()
            .iter()
            .copied()
            .filter(|s| is_enabled(*s) && connected(*s))
            .collect()
    }

    // -- lease --------------------------------------------------------------

    pub async fn acquire_lease(
        &self,
        holder: &str,
        scope: &str,
        ttl: Duration,
        wait: Duration,
    ) -> Result<kratos_protocol::LeaseGrant, WireError> {
        let ticket = {
            let mut st = self.lease.lock().await;
            st.next_ticket += 1;
            let t = st.next_ticket;
            st.queue.push_back(t);
            t
        };
        let deadline = Instant::now() + wait;

        loop {
            // Register interest *before* checking, so a release that happens
            // between the check and the wait cannot be missed.
            let notified = self.lease_notify.notified();
            {
                let mut st = self.lease.lock().await;
                if st.expire(Instant::now()) {
                    self.give_back_input();
                }
                if st.current.is_none() && st.queue.front() == Some(&ticket) {
                    // The machine-wide guard decides, not this broker's own
                    // bookkeeping: another broker for this user may hold the
                    // mouse, and its lease is invisible from here.
                    let left = deadline.saturating_duration_since(Instant::now());
                    if !self.take_input(left) {
                        st.queue.retain(|t| *t != ticket);
                        return Err(WireError::with_detail(
                            codes::LEASE_HELD,
                            "another kratos broker on this machine holds the input devices",
                            json!({"holder": "another-broker", "expires_in_ms": 0}),
                        ));
                    }
                    st.queue.pop_front();
                    let token = kratos_ipc::next_id("l");
                    st.current = Some(Lease {
                        token: token.clone(),
                        holder: holder.to_string(),
                        scope: scope.to_string(),
                        expires: Instant::now() + ttl,
                    });
                    return Ok(kratos_protocol::LeaseGrant {
                        token,
                        scope: scope.to_string(),
                        expires_in_ms: ttl.as_millis() as u64,
                    });
                }
                if Instant::now() >= deadline {
                    st.queue.retain(|t| *t != ticket);
                    let (h, ms) = match &st.current {
                        Some(l) => (
                            l.holder.clone(),
                            l.expires.saturating_duration_since(Instant::now()).as_millis() as u64,
                        ),
                        None => ("queue".to_string(), 0),
                    };
                    return Err(WireError::with_detail(
                        codes::LEASE_HELD,
                        "another agent holds the input devices",
                        json!({"holder": h, "expires_in_ms": ms}),
                    ));
                }
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep_until(deadline.into()) => {}
            }
        }
    }

    pub async fn release_lease(&self, token: &str) {
        let mut st = self.lease.lock().await;
        if st.current.as_ref().is_some_and(|l| l.token == token) {
            st.current = None;
            drop(st);
            self.give_back_input();
            self.lease_notify.notify_waiters();
        }
    }

    /// Take the machine-wide guard, if there is one.
    ///
    /// Waiting here rather than only polling is what makes the FIFO queue mean
    /// anything across brokers: the second broker's request waits out the
    /// first's lease instead of failing immediately.
    fn take_input(&self, wait: Duration) -> bool {
        match &self.input_guard {
            Some(g) => {
                // Clamped: a wait budget is milliseconds in a u32, and an
                // absurd one would wrap rather than wait.
                let ms = wait.as_millis().min(60_000) as u32;
                g.try_take(ms)
            }
            None => true,
        }
    }

    fn give_back_input(&self) {
        if let Some(g) = &self.input_guard {
            g.release();
        }
    }

    /// Validate a lease token supplied with an action.
    pub async fn check_lease(&self, token: &str, holder: &str) -> Result<(), WireError> {
        let mut st = self.lease.lock().await;
        if st.expire(Instant::now()) {
            self.give_back_input();
        }
        match &st.current {
            Some(l) if l.token == token && l.holder == holder => Ok(()),
            Some(l) if l.token == token => Err(WireError::new(
                codes::LEASE_INVALID,
                "that lease belongs to another peer",
            )),
            _ => Err(WireError::new(
                codes::LEASE_INVALID,
                "lease token is unknown or has expired",
            )),
        }
    }

    pub async fn lease_snapshot(&self) -> Value {
        let mut st = self.lease.lock().await;
        if st.expire(Instant::now()) {
            self.give_back_input();
        }
        match &st.current {
            Some(l) => json!({
                "scope": l.scope,
                "holder": l.holder,
                "expires_in_ms": l.expires.saturating_duration_since(Instant::now()).as_millis() as u64,
                "waiting": st.queue.len(),
            }),
            None => json!(null),
        }
    }

    // -- parked confirmations ----------------------------------------------

    pub async fn park(&self, p: Parked) -> (String, u64) {
        let ttl = self.config.read().await.gate.confirm_ttl_ms;
        let token = kratos_ipc::next_id("c");
        let mut parked = self.parked.lock().await;
        parked.retain(|_, v| v.created.elapsed() < Duration::from_millis(ttl));
        parked.insert(token.clone(), p);
        (token, ttl)
    }

    /// Redeem a token. Single-use: taken out of the map whatever the outcome,
    /// so a replay cannot run the action twice.
    pub async fn redeem(&self, token: &str, peer_id: &str) -> Result<Parked, WireError> {
        let ttl = self.config.read().await.gate.confirm_ttl_ms;
        let mut parked = self.parked.lock().await;
        let Some(p) = parked.remove(token) else {
            return Err(WireError::new(
                codes::STALE_CONFIRM,
                "unknown, already-used or expired confirmation token",
            ));
        };
        if p.created.elapsed() >= Duration::from_millis(ttl) {
            return Err(WireError::new(codes::STALE_CONFIRM, "confirmation token expired"));
        }
        if p.peer_id != peer_id {
            return Err(WireError::new(
                codes::UNAUTHORIZED,
                "that confirmation belongs to another agent",
            ));
        }
        let current = crate::classify::foreground().map(|w| w.hwnd).unwrap_or(0);
        if current != p.foreground_hwnd {
            return Err(WireError::with_detail(
                codes::STALE_CONFIRM,
                "the foreground window changed since this action was parked",
                json!({"parked_hwnd": p.foreground_hwnd, "current_hwnd": current}),
            ));
        }
        Ok(p)
    }

    // -- tab ownership ------------------------------------------------------

    /// Claim a tab for a session, or report who already has it.
    ///
    /// `Ok(())` means it is this session's to act on; `Err(owner)` names the
    /// session that got there first.
    pub async fn claim_tab(&self, tab: &str, peer_id: &str) -> Result<(), String> {
        let mut owners = self.tab_owners.lock().await;
        match owners.get(tab) {
            Some(owner) if owner != peer_id => Err(owner.clone()),
            Some(_) => Ok(()),
            None => {
                owners.insert(tab.to_string(), peer_id.to_string());
                Ok(())
            }
        }
    }

    /// Forget a disconnected session's tabs.
    ///
    /// Otherwise a crashed agent would own its tabs until the broker exited,
    /// and the agent that replaced it could not touch the work it left behind.
    pub async fn release_tabs(&self, peer_id: &str) {
        self.tab_owners.lock().await.retain(|_, owner| owner != peer_id);
    }

    // -- hot reload ---------------------------------------------------------

    /// Pick up an edit somebody else made to the config file.
    ///
    /// Section 9 says the file is hot-reloaded and the settings window is the
    /// intended editor, but the file is the source of truth -- and until this
    /// existed, an edit made with a text editor was read only if the broker
    /// happened to restart afterwards. Polled rather than watched: this runs
    /// on the idle tick, which is already there, and a file watcher would be a
    /// thread and an API for a question asked every five seconds.
    ///
    /// The kill switch is untouched, as with any other config change.
    /// Returns whether anything was reloaded.
    pub async fn reload_if_changed(&self) -> bool {
        let path = kratos_protocol::paths::config_path();
        let now = config_mtime();
        {
            let mut seen = self.config_seen.lock().await;
            if *seen == now {
                return false;
            }
            // Recorded before the load, so a file that fails to parse is not
            // retried every tick -- it is reported once and left alone until
            // it changes again.
            *seen = now;
        }

        let fresh = match Config::load_from(&path) {
            Ok(c) => c,
            Err(kratos_protocol::config::LoadError::Missing) => return false,
            Err(e) => {
                eprintln!("kratos-broker: {} changed and will not parse: {e}", path.display());
                return false;
            }
        };
        let key = crate::seal::read_key(&fresh.decider.api_key_enc);
        let decider = kratos_decide::build(&fresh.decider, key);
        let journal = Journal::from_config(&fresh.log);
        self.gate.lock().await.reconfigure(fresh.gate.clone());
        *self.decider.write().await = decider;
        *self.journal.write().await = journal;
        *self.config.write().await = fresh;
        eprintln!("kratos-broker: reloaded {}", path.display());
        true
    }

    // -- harnesses and approval --------------------------------------------

    /// Record a harness the broker has not seen before, unassigned and
    /// unapproved, so the settings window can list it.
    ///
    /// Writing to the config file on sight is deliberate: a harness that only
    /// existed in memory would vanish from the UI the moment the broker idled
    /// out, and the user would have nothing to allocate a browser to. It is
    /// also the only thing an unapproved client's connection achieves.
    pub async fn remember_harness(&self, id: &str) -> bool {
        if id.trim().is_empty() {
            return false;
        }
        if self.config.read().await.harness(id).is_some() {
            return false;
        }
        let patch = {
            let cfg = self.config.read().await;
            let mut list = cfg.harnesses.clone();
            list.push(kratos_protocol::config::HarnessConfig {
                id: id.to_string(),
                label: String::new(),
                browser: String::new(),
                approved: false,
                gate_overrides: Value::Null,
            });
            json!({"harnesses": list})
        };
        if let Err(e) = self.apply_config(&patch, None).await {
            eprintln!("kratos-broker: could not record harness {id:?}: {}", e.message);
            return false;
        }
        true
    }

    /// Whether this harness is allowed to drive the machine right now.
    ///
    /// The config's `approved` wins when it is set, because that is the user's
    /// standing decision; otherwise whatever was decided since this broker
    /// started; otherwise nothing is decided yet and the caller has to ask.
    pub async fn approval(&self, harness: &str) -> Option<Approval> {
        if self.config.read().await.harness(harness).is_some_and(|h| h.approved) {
            return Some(Approval::Allowed);
        }
        self.approvals.lock().await.get(harness).copied()
    }

    /// Claim the right to ask about this harness. `false` means somebody else
    /// already has a dialog up for it.
    pub async fn begin_asking(&self, harness: &str) -> bool {
        let mut map = self.approvals.lock().await;
        match map.get(harness) {
            Some(_) => false,
            None => {
                map.insert(harness.to_string(), Approval::Asking);
                true
            }
        }
    }

    /// Remember a decision for this broker's lifetime, and durably when the
    /// user said "always".
    pub async fn record_approval(&self, harness: &str, answer: kratos_gate::dialog::Approval) {
        use kratos_gate::dialog::Approval as A;
        let decided = match answer {
            A::Always | A::Once => Approval::Allowed,
            A::Deny => Approval::Denied,
            // Not a decision. The claim is released so the question can be
            // asked again, because a user who was not at the machine has not
            // refused anything.
            A::TimedOut => {
                self.approvals.lock().await.remove(harness);
                return;
            }
        };
        self.approvals.lock().await.insert(harness.to_string(), decided);

        if answer == A::Always {
            let patch = {
                let cfg = self.config.read().await;
                let mut list = cfg.harnesses.clone();
                match list.iter_mut().find(|h| h.id.eq_ignore_ascii_case(harness)) {
                    Some(h) => h.approved = true,
                    None => list.push(kratos_protocol::config::HarnessConfig {
                        id: harness.to_string(),
                        label: String::new(),
                        browser: String::new(),
                        approved: true,
                        gate_overrides: Value::Null,
                    }),
                }
                json!({"harnesses": list})
            };
            if let Err(e) = self.apply_config(&patch, None).await {
                eprintln!(
                    "kratos-broker: approved {harness:?} for this session but could not save it: {}",
                    e.message
                );
            }
        }
    }

    // -- config -------------------------------------------------------------

    /// Apply a config patch and rebuild whatever it affects.
    ///
    /// The kill switch is untouched on purpose: an engaged kill switch that a
    /// config write could clear is not a kill switch.
    pub async fn apply_config(&self, patch: &Value, api_key: Option<String>) -> Result<Value, WireError> {
        let merged = {
            let current = self.config.read().await;
            current
                .merge_patch(patch)
                .map_err(|e| WireError::new(codes::BAD_REQUEST, format!("invalid config: {e}")))?
        };

        let key = match api_key {
            Some(k) => k,
            None => crate::seal::read_key(&merged.decider.api_key_enc),
        };
        let new_decider = kratos_decide::build(&merged.decider, key);
        let new_journal = Journal::from_config(&merged.log);

        self.gate.lock().await.reconfigure(merged.gate.clone());
        *self.decider.write().await = new_decider;
        *self.journal.write().await = new_journal;

        let path = kratos_protocol::paths::config_path();
        if let Err(e) = merged.save_to(&path) {
            return Err(WireError::new(
                codes::INTERNAL,
                format!("config applied in memory but not saved to {}: {e}", path.display()),
            ));
        }
        let redacted = merged.redacted();
        *self.config.write().await = merged;
        // Our own write must not look like somebody else's edit, or the next
        // idle tick reloads what we just applied.
        *self.config_seen.lock().await = config_mtime();
        Ok(redacted)
    }

    pub async fn status(&self) -> Value {
        let cfg = self.config.read().await;
        let now = self.now_ms();
        let peers: Vec<Value> = self
            .peers
            .read()
            .await
            .values()
            .map(|p| {
                json!({"peer_id": p.peer_id, "role": p.role.as_str(), "name": p.name,
                       "source": p.source.map(|s| s.as_str()),
                       "instance": p.instance.as_ref().map(|i| i.token.clone()),
                       "connected_ms": p.connected_at.elapsed().as_millis() as u64})
            })
            .collect();

        // Resolved rather than read back from each session, so `status` says
        // where a session *would* be now -- which is the question somebody
        // asks after changing an allocation.
        let attached = crate::lane::attached_instances_with(self, &cfg).await;
        let agents: Vec<Arc<PeerHandle>> = self
            .peers
            .read()
            .await
            .values()
            .filter(|p| p.role == kratos_protocol::Role::Agent)
            .cloned()
            .collect();
        let lanes: Vec<Value> = agents
            .iter()
            .map(|p| {
                json!({
                    "peer_id": p.peer_id,
                    "harness": p.harness_id,
                    "agent": p.identity.agent,
                    "session": p.identity.session,
                    "lane": crate::lane::resolve_opt(&cfg, &p.identity, &attached),
                })
            })
            .collect();

        let decider = self.decider.read().await;
        json!({
            "broker": BROKER_NAME,
            "v": kratos_protocol::V,
            "pid": std::process::id(),
            "uptime_ms": now,
            "idle_ms": self.idle_ms(),
            "idle_exit_ms": cfg.idle_exit_ms,
            "generation": self.current_generation(),
            "peers": peers,
            "backends": self.backend_infos().await,
            "lease": self.lease_snapshot().await,
            "kill_engaged": self.gate.lock().await.kill.engaged(),
            "decider": {
                "provider": cfg.decider.provider,
                "model": cfg.decider.model,
                "name": decider.name(),
                "key_present": decider.configured(),
            },
            "counters": self.counters.snapshot(),
            // Which lane every session is in. Reported from `status` and not
            // only from the UI's `lane.list`, because the first symptom of a
            // misallocated lane is an agent reading a window it did not
            // expect, and the agent can read this.
            "lanes": lanes,
            "pause": {
                "on_user_input": cfg.defaults.pause_on_user_input,
                "user_idle_ms": cfg.defaults.user_idle_ms,
                "pause_max_ms": cfg.defaults.pause_max_ms,
                "since_real_input_ms": crate::activity::since_real_input_ms(),
                "watching": crate::activity::watching(),
            },
            "journal": self.journal.read().await.path().display().to_string(),
            // Which config this broker actually read. A broker started by
            // hand, from a shell with KRATOS_CONFIG set, is reading a
            // different policy than the settings window is editing -- and the
            // symptom of that is a key that appears to have vanished. Saying
            // so costs one string.
            "config": kratos_protocol::paths::config_path().display().to_string(),
        })
    }

    /// Broadcast an event to every peer, dropping it for any peer whose queue
    /// is full rather than blocking the sender.
    pub async fn broadcast(&self, frame: kratos_protocol::Frame) {
        let peers: Vec<Arc<PeerHandle>> = self.peers.read().await.values().cloned().collect();
        for p in peers {
            let _ = p.out.try_send(frame.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kratos_protocol::Role;

    fn broker() -> Arc<Broker> {
        let mut cfg = Config::default();
        cfg.decider.provider = "mock".into();
        // A guard of its own per broker: these tests are about the lease's
        // own bookkeeping, they run in parallel, and the machine-wide guard
        // would make them take turns -- or fight the operator's broker.
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let name = format!(
            "unit-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        Broker::with_input_guard(cfg, String::new(), Some(&name))
    }

    fn peer(id: &str, role: Role, source: Option<Source>) -> Arc<PeerHandle> {
        instanced(id, role, source, None)
    }

    /// A peer that is one named instance of its source.
    fn instanced(
        id: &str,
        role: Role,
        source: Option<Source>,
        token: Option<&str>,
    ) -> Arc<PeerHandle> {
        let (tx, rx) = mpsc::channel(8);
        // Keep the receiver alive for the life of the handle so try_send works.
        Box::leak(Box::new(rx));
        Arc::new(PeerHandle {
            peer_id: id.into(),
            role,
            name: id.into(),
            source,
            instance: token.map(kratos_protocol::Instance::new),
            identity: crate::lane::Identity::default(),
            harness_id: id.into(),
            lane: Mutex::new(None),
            handles: vec!["browser".into(), "native".into()],
            out: tx,
            pending: Mutex::new(HashMap::new()),
            connected_at: Instant::now(),
            last_seen: AtomicU64::new(0),
        })
    }

    #[tokio::test]
    async fn a_backend_reregistering_supersedes_the_old_one() {
        let b = broker();
        b.add_peer(peer("b-1", Role::Backend, Some(Source::Uia))).await;
        let superseded = b.add_peer(peer("b-2", Role::Backend, Some(Source::Uia))).await;
        assert_eq!(superseded.map(|p| p.peer_id.clone()), Some("b-1".to_string()));
        // Exactly one uia backend remains, and it is the new one.
        assert_eq!(b.backend_for(Source::Uia).await.unwrap().peer_id, "b-2");
        assert_eq!(b.peers.read().await.len(), 1);
    }

    #[tokio::test]
    async fn two_instances_of_one_source_coexist() {
        let b = broker();
        b.add_peer(instanced("b-1", Role::Backend, Some(Source::DomExt), Some("brave-tok")))
            .await;
        let superseded = b
            .add_peer(instanced("b-2", Role::Backend, Some(Source::DomExt), Some("vivaldi-tok")))
            .await;
        // Two browser profiles are two instances, not one replacing the other.
        assert!(superseded.is_none(), "a different instance must not be evicted");
        assert_eq!(b.backends_for(Source::DomExt).await.len(), 2);
        assert_eq!(b.peers.read().await.len(), 2);
    }

    #[tokio::test]
    async fn the_same_instance_reconnecting_replaces_itself() {
        let b = broker();
        b.add_peer(instanced("b-1", Role::Backend, Some(Source::DomExt), Some("brave-tok")))
            .await;
        let superseded = b
            .add_peer(instanced("b-2", Role::Backend, Some(Source::DomExt), Some("brave-tok")))
            .await;
        assert_eq!(superseded.map(|p| p.peer_id.clone()), Some("b-1".to_string()));
        assert_eq!(b.backends_for(Source::DomExt).await.len(), 1);
    }

    #[tokio::test]
    async fn an_instance_never_supersedes_a_different_source() {
        let b = broker();
        b.add_peer(instanced("b-1", Role::Backend, Some(Source::Uia), Some("same-tok"))).await;
        let superseded = b
            .add_peer(instanced("b-2", Role::Backend, Some(Source::DomExt), Some("same-tok")))
            .await;
        // The key is the pair. A token colliding across sources is not a
        // reconnection of anything.
        assert!(superseded.is_none());
        assert_eq!(b.peers.read().await.len(), 2);
    }

    #[tokio::test]
    async fn backend_for_picks_the_newest_instance() {
        let b = broker();
        b.add_peer(instanced("b-1", Role::Backend, Some(Source::DomExt), Some("older"))).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        b.add_peer(instanced("b-2", Role::Backend, Some(Source::DomExt), Some("newer"))).await;
        // Until lanes choose (P3), the newest instance is the one that answers:
        // that is what this broker did before instances existed, when the
        // newest had evicted every older one.
        assert_eq!(b.backend_for(Source::DomExt).await.unwrap().peer_id, "b-2");
        let order: Vec<String> = b
            .backends_for(Source::DomExt)
            .await
            .iter()
            .map(|p| p.peer_id.clone())
            .collect();
        assert_eq!(order, vec!["b-2".to_string(), "b-1".to_string()]);
    }

    #[tokio::test]
    async fn a_source_with_no_instance_is_still_one_per_source() {
        let b = broker();
        b.add_peer(peer("b-1", Role::Backend, Some(Source::Uia))).await;
        let superseded = b.add_peer(peer("b-2", Role::Backend, Some(Source::Uia))).await;
        assert_eq!(superseded.map(|p| p.peer_id.clone()), Some("b-1".to_string()));
        assert_eq!(b.backends_for(Source::Uia).await.len(), 1);
    }

    #[test]
    fn a_blank_token_is_no_instance_at_all() {
        // Otherwise two peers would both key on the empty string and evict one
        // another, which is the defect wearing a different hat.
        assert!(!kratos_protocol::Instance::new("   ").usable());
        let p = instanced("b-1", Role::Backend, Some(Source::DomExt), Some("  "));
        assert_eq!(
            p.registry_key(),
            Some(RegistryKey { source: Source::DomExt, instance: None })
        );
    }

    #[tokio::test]
    async fn status_names_the_instance_behind_each_backend() {
        let b = broker();
        b.add_peer(instanced("b-1", Role::Backend, Some(Source::DomExt), Some("brave-tok")))
            .await;
        b.add_peer(instanced("b-2", Role::Backend, Some(Source::DomExt), Some("alpha-tok")))
            .await;
        let s = b.status().await;
        let tokens: Vec<&str> = s["backends"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["instance"]["token"].as_str().unwrap())
            .collect();
        // Sorted, so a status read does not reshuffle between calls.
        assert_eq!(tokens, vec!["alpha-tok", "brave-tok"]);
    }

    #[tokio::test]
    async fn routing_prefers_dom_ext_for_browsers_and_skips_absent_backends() {
        let b = broker();
        b.add_peer(peer("b-1", Role::Backend, Some(Source::Uia))).await;
        // Only uia is connected, so a browser target falls to it.
        assert_eq!(b.route(kratos_protocol::TargetClass::Browser, None).await, vec![Source::Uia]);

        b.add_peer(peer("b-2", Role::Backend, Some(Source::DomExt))).await;
        assert_eq!(
            b.route(kratos_protocol::TargetClass::Browser, None).await,
            vec![Source::DomExt, Source::Uia]
        );
        // A native window never routes to the DOM backend.
        assert_eq!(b.route(kratos_protocol::TargetClass::Native, None).await, vec![Source::Uia]);
    }

    #[tokio::test]
    async fn an_explicit_preference_is_honoured_or_empty_never_downgraded() {
        let b = broker();
        b.add_peer(peer("b-1", Role::Backend, Some(Source::Uia))).await;
        assert_eq!(
            b.route(kratos_protocol::TargetClass::Browser, Some(Source::Uia)).await,
            vec![Source::Uia]
        );
        // Asked for dom-ext, which is not connected: empty, not a silent uia.
        assert!(b
            .route(kratos_protocol::TargetClass::Browser, Some(Source::DomExt))
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_disabled_backend_is_not_routed_to_even_when_connected() {
        let b = broker();
        b.add_peer(peer("b-1", Role::Backend, Some(Source::Uia))).await;
        b.config.write().await.backends.enabled = vec!["dom-ext".into()];
        assert!(b.route(kratos_protocol::TargetClass::Native, None).await.is_empty());
    }

    // -- lease --------------------------------------------------------------

    #[tokio::test]
    async fn the_lease_is_exclusive_and_releases() {
        let b = broker();
        let g = b
            .acquire_lease("a-1", "input", Duration::from_secs(5), Duration::ZERO)
            .await
            .unwrap();
        let err = b
            .acquire_lease("a-2", "input", Duration::from_secs(5), Duration::ZERO)
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::LEASE_HELD);
        assert_eq!(err.detail["holder"], json!("a-1"));

        b.release_lease(&g.token).await;
        assert!(b
            .acquire_lease("a-2", "input", Duration::from_secs(5), Duration::ZERO)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn two_agents_take_turns_in_arrival_order() {
        let b = broker();
        let first = b
            .acquire_lease("a-1", "input", Duration::from_millis(120), Duration::ZERO)
            .await
            .unwrap();

        // Two waiters queue behind the holder; the earlier one must win.
        let b2 = b.clone();
        let second =
            tokio::spawn(async move {
                b2.acquire_lease("a-2", "input", Duration::from_millis(80), Duration::from_secs(3))
                    .await
            });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let b3 = b.clone();
        let third =
            tokio::spawn(async move {
                b3.acquire_lease("a-3", "input", Duration::from_millis(80), Duration::from_secs(3))
                    .await
            });

        tokio::time::sleep(Duration::from_millis(20)).await;
        b.release_lease(&first.token).await;

        let g2 = second.await.unwrap().expect("a-2 should get it first");
        assert_eq!(g2.scope, "input");
        b.release_lease(&g2.token).await;
        let g3 = third.await.unwrap().expect("a-3 should follow");
        assert_ne!(g2.token, g3.token);
    }

    #[tokio::test]
    async fn an_expired_lease_is_reclaimed_and_its_token_stops_working() {
        let b = broker();
        let g = b
            .acquire_lease("a-1", "input", Duration::from_millis(40), Duration::ZERO)
            .await
            .unwrap();
        assert!(b.check_lease(&g.token, "a-1").await.is_ok());
        tokio::time::sleep(Duration::from_millis(70)).await;
        let err = b.check_lease(&g.token, "a-1").await.unwrap_err();
        assert_eq!(err.code, codes::LEASE_INVALID);
        // And the devices are free again.
        assert!(b
            .acquire_lease("a-2", "input", Duration::from_secs(1), Duration::ZERO)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_lease_token_cannot_be_used_by_another_peer() {
        let b = broker();
        let g = b
            .acquire_lease("a-1", "input", Duration::from_secs(5), Duration::ZERO)
            .await
            .unwrap();
        assert!(b.check_lease(&g.token, "a-2").await.is_err());
    }

    #[tokio::test]
    async fn disconnecting_releases_the_lease() {
        let b = broker();
        b.add_peer(peer("a-1", Role::Agent, None)).await;
        let _g = b
            .acquire_lease("a-1", "input", Duration::from_secs(30), Duration::ZERO)
            .await
            .unwrap();
        b.remove_peer("a-1").await;
        // Otherwise a crashed agent would hold the mouse for 30 seconds.
        assert!(b
            .acquire_lease("a-2", "input", Duration::from_secs(1), Duration::ZERO)
            .await
            .is_ok());
    }

    // -- parked confirmations ----------------------------------------------

    fn parked(peer_id: &str) -> Parked {
        Parked {
            instruction: "click the \"Send\" button".into(),
            window: None,
            rule: Some("review_keyword:\"send\"".into()),
            requires_os_dialog: false,
            peer_id: peer_id.into(),
            foreground_hwnd: crate::classify::foreground().map(|w| w.hwnd).unwrap_or(0),
            created: Instant::now(),
        }
    }

    #[tokio::test]
    async fn a_confirmation_token_is_single_use() {
        let b = broker();
        let (token, ttl) = b.park(parked("a-1")).await;
        assert!(ttl > 0);
        assert!(b.redeem(&token, "a-1").await.is_ok());
        let err = b.redeem(&token, "a-1").await.unwrap_err();
        assert_eq!(err.code, codes::STALE_CONFIRM);
    }

    #[tokio::test]
    async fn a_confirmation_cannot_be_redeemed_by_a_different_agent() {
        let b = broker();
        let (token, _) = b.park(parked("a-1")).await;
        let err = b.redeem(&token, "a-9").await.unwrap_err();
        assert_eq!(err.code, codes::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_confirmation_is_refused_if_the_foreground_moved() {
        let b = broker();
        let mut p = parked("a-1");
        p.foreground_hwnd = 0x7FFF_FFFF; // a window that is certainly not in front
        let (token, _) = b.park(p).await;
        let err = b.redeem(&token, "a-1").await.unwrap_err();
        assert_eq!(err.code, codes::STALE_CONFIRM);
        assert!(err.message.contains("foreground"));
    }

    #[tokio::test]
    async fn an_expired_confirmation_is_refused() {
        let b = broker();
        b.config.write().await.gate.confirm_ttl_ms = 30;
        let (token, _) = b.park(parked("a-1")).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(b.redeem(&token, "a-1").await.unwrap_err().code, codes::STALE_CONFIRM);
    }

    // -- idle ---------------------------------------------------------------

    #[tokio::test]
    async fn only_agent_activity_resets_the_idle_timer() {
        let b = broker();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let before = b.idle_ms();
        assert!(before >= 25, "{before}");
        // A UI calling status must not keep the broker alive forever.
        let _ = b.status().await;
        assert!(b.idle_ms() >= before, "status() must not reset idle");
        b.mark_agent_activity();
        assert!(b.idle_ms() < 20);
    }

    // -- config -------------------------------------------------------------

    #[tokio::test]
    async fn a_config_patch_cannot_clear_the_kill_switch() {
        let b = broker();
        b.gate.lock().await.kill.engage("global");
        let dir = std::env::temp_dir().join(format!("kratos-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("KRATOS_CONFIG", dir.join("config.json"));

        b.apply_config(&json!({"idle_exit_ms": 120000}), None).await.unwrap();
        assert_eq!(b.config.read().await.idle_exit_ms, 120_000);
        assert!(b.gate.lock().await.kill.is_engaged(), "kill must survive a config write");
    }

    #[tokio::test]
    async fn an_invalid_config_patch_is_rejected_without_changing_anything() {
        let b = broker();
        let before = b.config.read().await.idle_exit_ms;
        let err = b
            .apply_config(&json!({"idle_exit_ms": "not a number"}), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::BAD_REQUEST);
        assert_eq!(b.config.read().await.idle_exit_ms, before);
    }

    #[tokio::test]
    async fn status_reports_the_shape_the_protocol_documents() {
        let b = broker();
        b.add_peer(peer("b-1", Role::Backend, Some(Source::DomExt))).await;
        let s = b.status().await;
        assert_eq!(s["v"], json!(1));
        assert!(s["broker"].as_str().unwrap().starts_with("kratos-broker "));
        assert_eq!(s["backends"][0]["source"], json!("dom-ext"));
        assert_eq!(s["lease"], json!(null));
        assert_eq!(s["kill_engaged"], json!([]));
        assert_eq!(s["counters"]["act"], json!(0));
        assert_eq!(s["decider"]["provider"], json!("mock"));
    }

    #[test]
    fn counters_only_count_known_statuses() {
        let c = Counters::default();
        c.record(kratos_protocol::status::EXECUTED);
        c.record(kratos_protocol::status::EXECUTED);
        c.record(kratos_protocol::status::NO_EFFECT);
        c.record("something-new-from-a-later-build");
        let s = c.snapshot();
        assert_eq!(s["executed"], json!(2));
        assert_eq!(s["no_effect"], json!(1));
        assert_eq!(s["refused"], json!(0));
    }
}
