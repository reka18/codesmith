//! Browser attachments: the permission side of OpenCode's browser plugin
//! (`@opencode/plugin-browser`, MIT), where a desktop client attaches its
//! browser pane to a session.
//!
//! While a session has a browser attached, the attachment grants it
//! OpenCode's browser rules ([`rules`]): `browser *` and the `browser`
//! skill, as allows. The grant lives exactly as long as the attachment:
//!
//! - one attachment per session; attaching again replaces it, and the old
//!   handle's revoke (its drop) is a no-op against the new grant;
//! - dropping the handle ([`BrowserAttachment`]) revokes the grant;
//! - disposing the registry ([`Browsers::dispose`], the plugin's teardown)
//!   revokes every grant before it returns, so whatever closes after it
//!   (and every holder hearing [`Ended::Disposed`]) finds them gone;
//! - an attachment idle for the registry's time to live is evicted, and
//!   its grant stops counting at that moment, even before a sweep removes
//!   it; a fresh attachment afterwards works.
//!
//! The grant is read by the session's permission gate at each request
//! ([`crate::permission::Granted`]) and judged like a saved approval: after
//! the configured, built-in and policy denies and under the secret guards,
//! none of which it can lift. It never reaches another session.
//!
//! CodeSmith is a terminal agent with no browser pane, so no browser driver
//! is built in: [`Driver`] is where one plugs in, and the default
//! ([`NoDriver`]) refuses to open anything. Only the attachment lifecycle and
//! the permission checks in front of a driver are implemented.

use crate::permission::{Denied, Effect, Gate, Granted, Request, Rule};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// The permission action a browser's own operations are checked under.
pub const ACTION: &str = "browser";

/// The skill an attachment allows (OpenCode's `skill browser allow`).
pub const SKILL: &str = "browser";

/// How long an attachment may go without activity before it is evicted.
pub const DEFAULT_TIME_TO_LIVE: Duration = Duration::from_secs(30 * 60);

/// How often the registry looks for idle attachments.
pub const DEFAULT_SWEEP_EVERY: Duration = Duration::from_secs(30);

/// What an attachment grants its session while it lasts (OpenCode's
/// browser plugin's `session.permissions` additions): only allows.
pub fn rules() -> Vec<Rule> {
    vec![
        Rule::new(ACTION, "*", Effect::Allow),
        Rule::new("skill", SKILL, Effect::Allow),
    ]
}

/// The time idleness is measured on: monotonic, from any start.
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
}

/// The process's monotonic clock.
pub struct SystemClock(Instant);

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock(Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// A clock that moves only when told to (for tests and embedders that
/// drive idleness themselves).
#[derive(Default)]
pub struct ManualClock(Mutex<Duration>);

impl ManualClock {
    pub fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap()
    }
}

/// Why an attachment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// Another attachment to the same session took its place (OpenCode's
    /// `"replaced"`).
    Replaced,
    /// It was idle for the registry's time to live.
    Evicted,
    /// The registry was disposed (or dropped).
    Disposed,
    /// Its holder let it go.
    Detached,
}

/// Why a browser request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserError {
    /// The registry was disposed, or the attachment has ended (OpenCode's
    /// `rpc.unavailable`).
    Unavailable,
    /// The session has no browser attached.
    NotAttached(String),
    /// Not a URL or workspace path a browser may open.
    Invalid(String),
    /// A permission rule or the user refused it.
    Denied(String),
    /// The driver failed (or there is none).
    Driver(String),
}

impl std::fmt::Display for BrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowserError::Unavailable => f.write_str("The browser is unavailable"),
            BrowserError::NotAttached(s) => write!(f, "Session {s} has no browser attached"),
            BrowserError::Invalid(m) | BrowserError::Denied(m) | BrowserError::Driver(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for BrowserError {}

/// What actually drives a browser, once the permission checks passed.
pub trait Driver: Send + Sync {
    /// Open `url` (http(s) or file://) as a tab in `session`'s browser.
    fn open(&self, session: &str, url: &str) -> Result<(), String>;
}

/// The built-in driver: CodeSmith has no browser, so it opens nothing.
pub struct NoDriver;

/// What [`NoDriver`] answers.
pub const NO_DRIVER: &str =
    "CodeSmith has no built-in browser; a browser driver must be plugged in";

impl Driver for NoDriver {
    fn open(&self, _: &str, _: &str) -> Result<(), String> {
        Err(NO_DRIVER.into())
    }
}

/// How a registry behaves.
pub struct Options {
    pub time_to_live: Duration,
    /// How often a background task evicts idle attachments (started with
    /// the first attachment, inside a Tokio runtime); `None` leaves it to
    /// [`Browsers::sweep`]. An idle attachment's grant stops counting at
    /// its deadline either way.
    pub sweep_every: Option<Duration>,
    pub clock: Arc<dyn Clock>,
    pub driver: Arc<dyn Driver>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            time_to_live: DEFAULT_TIME_TO_LIVE,
            sweep_every: Some(DEFAULT_SWEEP_EVERY),
            clock: Arc::new(SystemClock::default()),
            driver: Arc::new(NoDriver),
        }
    }
}

/// Every session's browser attachment.
pub struct Browsers {
    state: Mutex<State>,
    time_to_live: Duration,
    sweep_every: Option<Duration>,
    clock: Arc<dyn Clock>,
    driver: Arc<dyn Driver>,
    next: AtomicU64,
    sweeping: AtomicBool,
}

#[derive(Default)]
struct State {
    disposed: bool,
    slots: HashMap<String, Slot>,
}

/// A session's attachment.
struct Slot {
    id: u64,
    connection: String,
    last: Duration,
    ended: watch::Sender<Option<Ended>>,
}

impl Slot {
    fn end(self, why: Ended) {
        let _ = self.ended.send(Some(why));
    }
}

impl Browsers {
    pub fn new(opts: Options) -> Arc<Browsers> {
        Arc::new(Browsers {
            state: Mutex::default(),
            time_to_live: opts.time_to_live,
            sweep_every: opts.sweep_every,
            clock: opts.clock,
            driver: opts.driver,
            next: AtomicU64::new(1),
            sweeping: AtomicBool::new(false),
        })
    }

    /// The process's registry, which every session's gate reads.
    pub fn global() -> &'static Arc<Browsers> {
        static GLOBAL: OnceLock<Arc<Browsers>> = OnceLock::new();
        GLOBAL.get_or_init(|| Browsers::new(Options::default()))
    }

    /// Attach a browser (its client connection `connection`) to `session`,
    /// granting it [`rules`] until the returned handle is dropped, another
    /// attachment replaces it, it is evicted as idle, or the registry is
    /// disposed. The replaced attachment hears [`Ended::Replaced`]; the new
    /// grant is in place before it does.
    pub fn attach(
        self: &Arc<Self>,
        session: &str,
        connection: &str,
    ) -> Result<BrowserAttachment, BrowserError> {
        let (tx, rx) = watch::channel(None);
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        {
            let mut st = self.state.lock().unwrap();
            if st.disposed {
                return Err(BrowserError::Unavailable);
            }
            let slot = Slot {
                id,
                connection: connection.to_string(),
                last: self.clock.now(),
                ended: tx,
            };
            if let Some(old) = st.slots.insert(session.to_string(), slot) {
                old.end(Ended::Replaced);
            }
        }
        self.start_sweeper();
        Ok(BrowserAttachment {
            browsers: Arc::downgrade(self),
            session: session.to_string(),
            connection: connection.to_string(),
            id,
            ended: rx,
        })
    }

    /// The session's live attachment, evicting it first when it has been
    /// idle for the time to live.
    fn live<'a>(&self, st: &'a mut State, session: &str) -> Option<&'a mut Slot> {
        let now = self.clock.now();
        let expired = st
            .slots
            .get(session)
            .is_some_and(|s| now.saturating_sub(s.last) >= self.time_to_live);
        if expired {
            if let Some(slot) = st.slots.remove(session) {
                slot.end(Ended::Evicted);
            }
        }
        st.slots.get_mut(session)
    }

    /// The session has a browser attached (OpenCode's `attached`).
    pub fn attached(&self, session: &str) -> bool {
        let mut st = self.state.lock().unwrap();
        self.live(&mut st, session).is_some()
    }

    /// The client connection attached to the session.
    pub fn connection(&self, session: &str) -> Option<String> {
        let mut st = self.state.lock().unwrap();
        self.live(&mut st, session).map(|s| s.connection.clone())
    }

    /// The rules the session's attachment grants now: none without one.
    pub fn granted(&self, session: &str) -> Vec<Rule> {
        let mut st = self.state.lock().unwrap();
        match self.live(&mut st, session) {
            Some(_) => rules(),
            None => vec![],
        }
    }

    /// The session's grants as its gate reads them: always this registry's
    /// current attachment for `session`, and nothing once the registry is
    /// gone.
    pub fn grants_for(self: &Arc<Self>, session: &str) -> Arc<dyn Granted> {
        Arc::new(SessionGrants {
            browsers: Arc::downgrade(self),
            session: session.to_string(),
        })
    }

    /// Activity on attachment `id` (the client reporting its state, a tab
    /// opened): its idle time starts over. Fails once it has ended.
    fn touch(&self, session: &str, id: u64) -> Result<(), BrowserError> {
        let now = self.clock.now();
        let mut st = self.state.lock().unwrap();
        match self.live(&mut st, session) {
            Some(slot) if slot.id == id => {
                slot.last = now;
                Ok(())
            }
            _ => Err(BrowserError::Unavailable),
        }
    }

    /// End attachment `id` if it is still the session's: a replaced
    /// attachment's revoke leaves its replacement alone. The check and the
    /// removal happen under one lock, so no grant can land between them.
    fn revoke(&self, session: &str, id: u64) {
        let mut st = self.state.lock().unwrap();
        if st.slots.get(session).is_some_and(|s| s.id == id) {
            if let Some(slot) = st.slots.remove(session) {
                slot.end(Ended::Detached);
            }
        }
    }

    /// Evict every attachment idle for the time to live: how many.
    pub fn sweep(&self) -> usize {
        let now = self.clock.now();
        let mut st = self.state.lock().unwrap();
        let idle: Vec<String> = st
            .slots
            .iter()
            .filter(|(_, s)| now.saturating_sub(s.last) >= self.time_to_live)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &idle {
            if let Some(slot) = st.slots.remove(k) {
                slot.end(Ended::Evicted);
            }
        }
        idle.len()
    }

    /// End every attachment and refuse new ones (the plugin's teardown).
    /// Every grant is gone before this returns, and before any holder
    /// hears [`Ended::Disposed`]. Disposing again does nothing.
    pub fn dispose(&self) {
        let ended: Vec<Slot> = {
            let mut st = self.state.lock().unwrap();
            st.disposed = true;
            st.slots.drain().map(|(_, s)| s).collect()
        };
        for slot in ended {
            slot.end(Ended::Disposed);
        }
    }

    pub fn is_disposed(&self) -> bool {
        self.state.lock().unwrap().disposed
    }

    /// One background sweep per registry, holding it weakly: it stops when
    /// the registry is disposed or dropped. When its runtime shuts down
    /// under it, the next attachment starts another.
    fn start_sweeper(self: &Arc<Self>) {
        let Some(every) = self.sweep_every else {
            return;
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.sweeping.swap(true, Ordering::SeqCst) {
            return;
        }
        /// Lets another sweep start once this one is gone, however it ends.
        struct Running(Weak<Browsers>);
        impl Drop for Running {
            fn drop(&mut self) {
                if let Some(b) = self.0.upgrade() {
                    b.sweeping.store(false, Ordering::SeqCst);
                }
            }
        }
        let weak = Arc::downgrade(self);
        let running = Running(weak.clone());
        rt.spawn(async move {
            let _running = running;
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(b) = weak.upgrade() else { return };
                if b.is_disposed() {
                    return;
                }
                b.sweep();
            }
        });
    }

    /// Open `url` (http(s) or file://) in the session's browser (OpenCode's
    /// `open`): the session must have one attached, and the gate must allow
    /// `browser` for the URL; an attachment's own grant does, but never
    /// over a configured or policy deny.
    pub fn open(&self, gate: &Gate, session: &str, url: &str) -> Result<(), BrowserError> {
        let lower = url.to_ascii_lowercase();
        if !["http://", "https://", "file://"]
            .iter()
            .any(|p| lower.starts_with(p))
        {
            return Err(BrowserError::Invalid(format!(
                "Not an http(s) or file:// URL: {url}"
            )));
        }
        let id = self.current(session)?;
        assert(gate, ACTION, url)?;
        self.touch(session, id)?;
        self.driver.open(session, url).map_err(BrowserError::Driver)
    }

    /// Open workspace file `path` (relative to `root`) as a file:// tab
    /// (OpenCode's `openFile`). Showing a file reads it, so the gate must
    /// allow `read` for it too: the secret guards apply, and an
    /// attachment's grant does not lift them.
    pub fn open_file(
        &self,
        gate: &Gate,
        session: &str,
        root: &Path,
        path: &str,
    ) -> Result<(), BrowserError> {
        let rel = Path::new(path);
        if path.is_empty()
            || rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(BrowserError::Invalid(format!(
                "Not a workspace-relative path: {path}"
            )));
        }
        self.current(session)?;
        let full = root.join(rel);
        assert(gate, "read", &full.display().to_string())?;
        self.open(gate, session, &format!("file://{}", full.display()))
    }

    /// The id of the session's live attachment.
    fn current(&self, session: &str) -> Result<u64, BrowserError> {
        let mut st = self.state.lock().unwrap();
        if st.disposed {
            return Err(BrowserError::Unavailable);
        }
        self.live(&mut st, session)
            .map(|s| s.id)
            .ok_or_else(|| BrowserError::NotAttached(session.to_string()))
    }
}

fn assert(gate: &Gate, action: &str, resource: &str) -> Result<(), BrowserError> {
    let req = Request {
        action: action.into(),
        resources: vec![resource.into()],
        save: vec![],
        title: format!("{action} {resource}"),
    };
    gate.assert(&req)
        .map_err(|e: Denied| BrowserError::Denied(e.to_string()))
}

/// A session's grants, read through a registry it does not keep alive.
struct SessionGrants {
    browsers: Weak<Browsers>,
    session: String,
}

impl Granted for SessionGrants {
    fn rules(&self) -> Vec<Rule> {
        match self.browsers.upgrade() {
            Some(b) => b.granted(&self.session),
            None => vec![],
        }
    }
}

/// A browser attached to a session, scoped to whoever holds it: dropping
/// it (or [`BrowserAttachment::detach`]) revokes its grant, unless another
/// attachment already replaced it, in which case it does nothing.
pub struct BrowserAttachment {
    browsers: Weak<Browsers>,
    session: String,
    connection: String,
    id: u64,
    ended: watch::Receiver<Option<Ended>>,
}

impl BrowserAttachment {
    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn connection(&self) -> &str {
        &self.connection
    }

    /// Why it ended, if it has (an idle one is evicted when asked).
    pub fn ended_now(&self) -> Option<Ended> {
        match self.browsers.upgrade() {
            Some(b) => {
                b.attached(&self.session);
            }
            None => return Some((*self.ended.borrow()).unwrap_or(Ended::Disposed)),
        }
        *self.ended.borrow()
    }

    /// Wait until it ends: why.
    pub async fn ended(&mut self) -> Ended {
        loop {
            if let Some(e) = *self.ended.borrow_and_update() {
                return e;
            }
            if self.ended.changed().await.is_err() {
                return (*self.ended.borrow()).unwrap_or(Ended::Disposed);
            }
        }
    }

    /// The client is still there (OpenCode's state updates): the idle time
    /// starts over. Fails once the attachment has ended.
    pub fn touch(&self) -> Result<(), BrowserError> {
        match self.browsers.upgrade() {
            Some(b) => b.touch(&self.session, self.id),
            None => Err(BrowserError::Unavailable),
        }
    }

    pub fn detach(self) {}
}

impl Drop for BrowserAttachment {
    fn drop(&mut self) {
        if let Some(b) = self.browsers.upgrade() {
            b.revoke(&self.session, self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{Fixed, Reply};

    fn registry() -> (Arc<Browsers>, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::default());
        let b = Browsers::new(Options {
            time_to_live: Duration::from_secs(60),
            sweep_every: None,
            clock: clock.clone(),
            driver: Arc::new(NoDriver),
        });
        (b, clock)
    }

    #[test]
    fn a_grant_is_only_allows() {
        assert!(rules()
            .iter()
            .all(|r| r.effect == Effect::Allow && !r.builtin && !r.guard && r.policy.is_none()));
    }

    #[test]
    fn a_dropped_registry_grants_nothing_and_ends_its_attachments() {
        let (b, _) = registry();
        let a = b.attach("s", "c").unwrap();
        let grants = b.grants_for("s");
        assert_eq!(grants.rules(), rules());
        drop(b);
        assert!(grants.rules().is_empty());
        assert_eq!(a.ended_now(), Some(Ended::Disposed));
        assert_eq!(a.touch(), Err(BrowserError::Unavailable));
    }

    #[test]
    fn the_built_in_driver_opens_nothing() {
        let (b, _) = registry();
        let mut gate = Gate::new(vec![], Arc::new(Fixed(Reply::Reject(None))));
        gate.set_granted(Some(b.grants_for("s")));
        let _a = b.attach("s", "c").unwrap();
        assert_eq!(
            b.open(&gate, "s", "https://example.test/"),
            Err(BrowserError::Driver(NO_DRIVER.into()))
        );
    }

    #[test]
    fn a_sweep_whose_runtime_ended_is_started_again() {
        let clock = Arc::new(ManualClock::default());
        let b = Browsers::new(Options {
            time_to_live: Duration::from_secs(60),
            sweep_every: Some(Duration::from_millis(5)),
            clock: clock.clone(),
            driver: Arc::new(NoDriver),
        });
        let rt = || {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap()
        };
        rt().block_on(async {
            let _a = b.attach("s1", "c").unwrap();
            assert!(b.sweeping.load(Ordering::SeqCst));
        });
        // The first runtime is gone, and its sweep with it.
        assert!(!b.sweeping.load(Ordering::SeqCst));
        rt().block_on(async {
            let mut a = b.attach("s2", "c").unwrap();
            clock.advance(Duration::from_secs(60));
            let why = tokio::time::timeout(Duration::from_secs(10), a.ended())
                .await
                .expect("the new sweep evicted it");
            assert_eq!(why, Ended::Evicted);
        });
    }

    #[tokio::test]
    async fn a_plugin_permission_check_sees_the_grant_while_it_lasts() {
        use crate::plugin::host::{assert_live, live_gate, AssertPermission, PermissionError};
        let (b, _) = registry();
        let id = "ses_browser_live_gate";
        let mut gate = Gate::new(
            vec![Rule::new(ACTION, "*", Effect::Ask)],
            Arc::new(Fixed(Reply::Reject(None))),
        );
        gate.set_granted(Some(b.grants_for(id)));
        let _live = live_gate(id, &gate);
        let input = || AssertPermission {
            session_id: id.into(),
            action: ACTION.into(),
            resources: vec!["https://example.test/".into()],
            ..Default::default()
        };
        let a = b.attach(id, "c").unwrap();
        assert_eq!(assert_live(input()).await, Ok(()));
        drop(a);
        assert_eq!(assert_live(input()).await, Err(PermissionError::Declined));
    }
}
