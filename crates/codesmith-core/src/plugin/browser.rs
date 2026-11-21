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

use crate::config::ToolOutputConfig;
use crate::permission::{Denied, Effect, Gate, Granted, Request, Rule};
use crate::tool::read::authorize_target;
use crate::tool::webfetch::{check_url, deny_forms, normalize};
use crate::tool::{real_path, Ctx, Target};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use url::Url;

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
    /// over a configured or policy deny. A file:// URL is a file to read,
    /// so it is checked as the read tool checks a path ([`authorize_file`],
    /// against the project at `root`): `external_directory` outside the
    /// project, then `read` with the configured denies and the secret
    /// guards, none of which the grant lifts. The browser is handed the
    /// file:// URL of the file's real path.
    pub fn open(
        &self,
        gate: &Gate,
        session: &str,
        root: &Path,
        url: &str,
    ) -> Result<(), BrowserError> {
        let invalid = || BrowserError::Invalid(format!("Not an http(s) or file:// URL: {url}"));
        let parsed = Url::parse(url).map_err(|_| invalid())?;
        if !matches!(parsed.scheme(), "http" | "https" | "file") {
            return Err(invalid());
        }
        self.current(session)?;
        if parsed.scheme() != "file" {
            return self.show(gate, session, Some(url), web_url(url)?);
        }
        let path = parsed.to_file_path().map_err(|_| invalid())?;
        let mut file = authorize_file(gate, session, root, &path, Within::Anywhere)?;
        file.set_fragment(parsed.fragment());
        self.show(gate, session, Some(url), file)
    }

    /// Open `url`, checked already as a URL or file a browser may open
    /// ([`web_url`], [`authorize_file`]), once the gate allows `browser` for
    /// it. The rules see it without its fragment, and its other forms (as
    /// `written`, percent-decoded) must pass the deny rules too, as for
    /// webfetch.
    fn show(
        &self,
        gate: &Gate,
        session: &str,
        written: Option<&str>,
        url: Url,
    ) -> Result<(), BrowserError> {
        let id = self.current(session)?;
        let mut checked = url.clone();
        checked.set_fragment(None);
        gate.assert_not_denied(ACTION, &deny_forms(written, &checked))
            .map_err(|e| BrowserError::Denied(e.to_string()))?;
        assert(gate, ACTION, checked.as_str())?;
        self.touch(session, id)?;
        self.driver
            .open(session, url.as_str())
            .map_err(BrowserError::Driver)
    }

    /// Open workspace file `path` (relative to `root`) as a file:// tab
    /// (OpenCode's `openFile`). Showing a file reads it, so it is checked
    /// as the read tool checks a file ([`authorize_file`]): the configured
    /// denies and secret guards apply under every name it goes by, and an
    /// attachment's grant does not lift them. `path` must name a file in
    /// the workspace: not absolute, no `.` or `..`, no `%`, `#` or `?`, and
    /// not a symlink that leads out of `root`.
    pub fn open_file(
        &self,
        gate: &Gate,
        session: &str,
        root: &Path,
        path: &str,
    ) -> Result<(), BrowserError> {
        let rel = Path::new(path);
        if path.is_empty()
            || path.contains(['%', '#', '?', '\0'])
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
        let url = authorize_file(gate, session, root, &root.join(rel), Within::Workspace)?;
        self.show(gate, session, None, url)
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

/// `raw`, an http(s) URL, as the `browser` rules see it: parsed as
/// webfetch parses it ([`normalize`]: the scheme and host lowercased, a
/// default port dropped, `.` segments resolved), its host without trailing
/// dots, and refused with credentials or a `*` or `?` in the host
/// ([`check_url`]), so `HTTPS://u@EVIL.test.:443/x` meets a rule for
/// `https://evil.test/*`. The fragment is kept for the browser.
fn web_url(raw: &str) -> Result<Url, BrowserError> {
    let invalid =
        |why: String| BrowserError::Invalid(format!("Not a URL a browser may open: {why}"));
    let fragment = Url::parse(raw)
        .ok()
        .and_then(|u| u.fragment().map(String::from));
    let mut url = normalize(raw).map_err(|e| invalid(e.to_string()))?;
    check_url(&url).map_err(invalid)?;
    if let Some(url::Host::Domain(host)) = url.host() {
        let bare = host.trim_end_matches('.').to_string();
        if bare.len() != host.len() {
            if bare.is_empty() {
                return Err(invalid(format!("no host in {raw}")));
            }
            url.set_host(Some(&bare))
                .map_err(|e| invalid(e.to_string()))?;
        }
    }
    url.set_fragment(fragment.as_deref());
    Ok(url)
}

/// Where a file a browser shows may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Within {
    /// Only under the workspace root, symlinks followed.
    Workspace,
    /// Anywhere, outside the project with `external_directory` approval.
    Anywhere,
}

/// The checks the read tool makes before it reads `path` (absolute), for
/// showing it in `session`'s browser: resolved exactly as `read` resolves
/// it ([`crate::tool::Ctx::resolve`]: normalized, symlinks followed), then
/// `external_directory` when its real path is outside the project, then
/// `read` under every name it goes by (project-relative, as named and as
/// resolved), so the configured denies and the secret guards meet each
/// spelling ([`crate::tool::read::authorize_target`]). With
/// [`Within::Workspace`] a real path outside `root` (a symlink leading out)
/// is refused before anything is asked. The result is the file:// URL of
/// the real path, percent-encoded, so the browser opens the file that was
/// checked. The questions are asked as a tool asks them, so call this from
/// outside a runtime or from a multi-thread runtime's worker.
fn authorize_file(
    gate: &Gate,
    session: &str,
    root: &Path,
    path: &Path,
    within: Within,
) -> Result<Url, BrowserError> {
    let invalid =
        || BrowserError::Invalid(format!("Not a file a browser may open: {}", path.display()));
    let name = path
        .to_str()
        .filter(|n| path.is_absolute() && root.is_absolute() && !n.contains('\0'))
        .ok_or_else(invalid)?;
    let ctx = file_ctx(gate, session, root);
    let workspace = real_path(root);
    let outside = |t: &Target| within == Within::Workspace && !t.real.starts_with(&workspace);
    if outside(&ctx.resolve(name)) {
        return Err(BrowserError::Invalid(format!(
            "Not a workspace file (it leads outside the workspace): {}",
            path.display()
        )));
    }
    let (target, _) = authorize_target(&ctx, name).map_err(BrowserError::Denied)?;
    if outside(&target) {
        return Err(invalid());
    }
    Url::from_file_path(&target.real).map_err(|_| invalid())
}

/// A tool context for checking a file the way the tools do: the project
/// and the working directory are `root`. It runs nothing and fetches
/// nothing.
fn file_ctx<'a>(gate: &'a Gate, session: &str, root: &Path) -> Ctx<'a> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    Ctx {
        cwd: root.to_path_buf(),
        root: root.to_path_buf(),
        gate,
        shell: String::new(),
        session_id: session.to_string(),
        limits: ToolOutputConfig::default(),
        client: CLIENT.get_or_init(reqwest::Client::new).clone(),
        jobs: None,
        websearch: None,
        format: None,
        lsp: None,
        progress: None,
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

    /// A driver that records every URL it was asked to open.
    #[derive(Default)]
    struct Opened(Mutex<Vec<String>>);

    impl Driver for Opened {
        fn open(&self, _: &str, url: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(url.to_string());
            Ok(())
        }
    }

    impl Opened {
        fn urls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// Records every question asked and declines it.
    #[derive(Default)]
    struct Declines(Mutex<Vec<(String, Vec<String>)>>);

    impl crate::permission::Asker for Declines {
        fn ask(&self, req: &Request) -> Reply {
            let mut asked = self.0.lock().unwrap();
            asked.push((req.action.clone(), req.resources.clone()));
            Reply::Reject(None)
        }
    }

    impl Declines {
        fn requests(&self) -> Vec<(String, Vec<String>)> {
            self.0.lock().unwrap().clone()
        }
        fn actions(&self) -> Vec<String> {
            self.requests().into_iter().map(|r| r.0).collect()
        }
    }

    /// A stand-in home (with an SSH key), a workspace with a `.env`, a
    /// `secrets/key` and a page, and a folder outside both; nothing real is
    /// read.
    struct Files {
        _dir: tempfile::TempDir,
        home: std::path::PathBuf,
        root: std::path::PathBuf,
        outside: std::path::PathBuf,
    }

    fn files() -> Files {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (home, root, outside) = (base.join("home"), base.join("work"), base.join("outside"));
        for p in [
            home.join(".ssh/id_rsa"),
            root.join(".env"),
            root.join("secrets/key"),
            root.join("index.html"),
            root.join("a b.html"),
            outside.join("page.html"),
        ] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
        }
        Files {
            _dir: dir,
            home,
            root,
            outside,
        }
    }

    /// An attached session `s` whose gate holds `extra`, `read * allow` and
    /// the secret guards for `f.home`, with every question declined by a
    /// recorder; and the driver that records what reached it.
    fn attached(
        f: &Files,
        extra: Vec<Rule>,
    ) -> (
        Arc<Browsers>,
        BrowserAttachment,
        Gate,
        Arc<Declines>,
        Arc<Opened>,
    ) {
        let driver = Arc::new(Opened::default());
        let b = Browsers::new(Options {
            time_to_live: Duration::from_secs(60),
            sweep_every: None,
            clock: Arc::new(ManualClock::default()),
            driver: driver.clone(),
        });
        let mut rules = vec![Rule::new("read", "*", Effect::Allow)];
        rules.extend(crate::permission::secret_guards_for(
            std::slice::from_ref(&f.home),
            &[],
            &[],
            &[],
        ));
        rules.extend(extra);
        let rec = Arc::new(Declines::default());
        let mut gate = Gate::new(rules, rec.clone());
        gate.set_granted(Some(b.grants_for("s")));
        let a = b.attach("s", "c").unwrap();
        (b, a, gate, rec, driver)
    }

    fn file_url(p: &std::path::Path) -> String {
        Url::from_file_path(p).unwrap().to_string()
    }

    fn symlink(target: &std::path::Path, link: &std::path::Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    fn opening_a_workspace_file_opens_its_real_path_percent_encoded() {
        let f = files();
        let (b, _a, gate, rec, driver) = attached(&f, vec![]);
        b.open_file(&gate, "s", &f.root, "index.html").unwrap();
        b.open_file(&gate, "s", &f.root, "a b.html").unwrap();
        assert_eq!(
            driver.urls(),
            [
                file_url(&f.root.join("index.html")),
                format!(
                    "{}a%20b.html",
                    file_url(&f.root.join("x")).trim_end_matches('x')
                ),
            ]
        );
        assert!(rec.requests().is_empty(), "{:?}", rec.requests());
    }

    /// A project-relative deny is met however the file is spelled: the
    /// check names it as the read tool does, relative to the project.
    #[test]
    fn opening_a_file_meets_a_project_relative_deny_under_every_spelling() {
        let f = files();
        let deny = vec![Rule::new("read", "secrets/*", Effect::Deny)];
        let (b, _a, gate, _rec, driver) = attached(&f, deny);
        for name in ["secrets/key", "secrets/./key", "secrets//key"] {
            assert!(
                matches!(
                    b.open_file(&gate, "s", &f.root, name),
                    Err(BrowserError::Denied(_))
                ),
                "{name}"
            );
        }
        assert!(driver.urls().is_empty(), "{:?}", driver.urls());
    }

    #[test]
    fn opening_a_file_meets_an_exact_deny_under_every_spelling() {
        let f = files();
        // Inside the project a `read` resource is project-relative, for the
        // read tool as here (docs/permissions.md).
        for deny in ["secrets/key", "secrets/k*"] {
            let rules = vec![Rule::new("read", deny, Effect::Deny)];
            let (b, _a, gate, _rec, driver) = attached(&f, rules);
            for name in ["secrets/key", "secrets/./key", "secrets//key"] {
                assert!(
                    matches!(
                        b.open_file(&gate, "s", &f.root, name),
                        Err(BrowserError::Denied(_))
                    ),
                    "{deny} {name}"
                );
            }
            assert!(driver.urls().is_empty(), "{:?}", driver.urls());
        }
    }

    #[test]
    fn opening_a_file_keeps_the_env_guard() {
        let f = files();
        let (b, _a, gate, rec, driver) = attached(&f, vec![]);
        assert!(matches!(
            b.open_file(&gate, "s", &f.root, ".env"),
            Err(BrowserError::Denied(_))
        ));
        assert_eq!(rec.actions(), ["read"]);
        assert!(driver.urls().is_empty());
    }

    /// A workspace symlink that leads out (to an ordinary file, or to the
    /// SSH folder) is refused before anything is asked or opened.
    #[cfg(unix)]
    #[test]
    fn opening_a_file_refuses_a_symlink_out_of_the_workspace() {
        let f = files();
        symlink(&f.home.join(".ssh"), &f.root.join("keys"));
        symlink(&f.outside.join("page.html"), &f.root.join("page.html"));
        let (b, _a, gate, rec, driver) = attached(&f, vec![]);
        for name in ["keys/id_rsa", "keys", "page.html"] {
            assert!(
                matches!(
                    b.open_file(&gate, "s", &f.root, name),
                    Err(BrowserError::Invalid(_))
                ),
                "{name}"
            );
        }
        assert!(rec.requests().is_empty(), "{:?}", rec.requests());
        assert!(driver.urls().is_empty(), "{:?}", driver.urls());
    }

    /// A symlink inside the workspace is opened by its real path, and
    /// checked under that name too.
    #[cfg(unix)]
    #[test]
    fn opening_a_file_checks_and_opens_a_workspace_symlink_by_its_target() {
        let f = files();
        symlink(&f.root.join("secrets/key"), &f.root.join("note.txt"));
        let deny = vec![Rule::new("read", "secrets/*", Effect::Deny)];
        let (b, _a, gate, _rec, driver) = attached(&f, deny);
        assert!(matches!(
            b.open_file(&gate, "s", &f.root, "note.txt"),
            Err(BrowserError::Denied(_))
        ));
        let (b, _a, gate, _rec, driver2) = attached(&f, vec![]);
        b.open_file(&gate, "s", &f.root, "note.txt").unwrap();
        assert!(driver.urls().is_empty());
        assert_eq!(driver2.urls(), [file_url(&f.root.join("secrets/key"))]);
    }

    #[test]
    fn opening_a_file_refuses_what_is_not_a_plain_workspace_path() {
        let f = files();
        let (b, _a, gate, rec, driver) = attached(&f, vec![]);
        let etc = "/etc/passwd".to_string();
        for name in [
            "",
            "../outside/page.html",
            "secrets/../../outside/page.html",
            ".",
            "./index.html",
            etc.as_str(),
            "%2e%2e/%2e%2e/etc/passwd",
            "%2e%2e%2f%2e%2e%2fetc%2fpasswd",
            "index.html#x",
            "index.html?x",
            "a#/../../etc/passwd",
            "index\0.html",
        ] {
            assert!(
                matches!(
                    b.open_file(&gate, "s", &f.root, name),
                    Err(BrowserError::Invalid(_))
                ),
                "{name:?}"
            );
        }
        assert!(rec.requests().is_empty(), "{:?}", rec.requests());
        assert!(driver.urls().is_empty(), "{:?}", driver.urls());
    }

    fn open(b: &Browsers, gate: &Gate, f: &Files, url: &str) -> Result<(), BrowserError> {
        b.open(gate, "s", &f.root, url)
    }

    fn outside_allowed() -> Vec<Rule> {
        vec![Rule::new("external_directory", "*", Effect::Allow)]
    }

    /// A file:// URL to the SSH folder asks for the outside folder and then
    /// meets the secret guard, grant or no grant; neither reaches the
    /// driver when declined.
    #[test]
    fn a_file_url_to_a_secret_outside_asks_for_the_folder_and_the_guard() {
        let f = files();
        let key = f.home.join(".ssh/id_rsa");
        let url = file_url(&key);
        let (b, _a, gate, rec, driver) = attached(&f, vec![]);
        assert!(matches!(
            open(&b, &gate, &f, &url),
            Err(BrowserError::Denied(_))
        ));
        assert_eq!(rec.actions(), ["external_directory"]);
        let (b, _a, gate, rec, driver2) = attached(&f, outside_allowed());
        assert!(matches!(
            open(&b, &gate, &f, &url),
            Err(BrowserError::Denied(_))
        ));
        assert_eq!(
            rec.requests(),
            [("read".to_string(), vec![key.display().to_string()])]
        );
        assert!(driver.urls().is_empty() && driver2.urls().is_empty());
    }

    #[test]
    fn a_file_url_meets_the_env_guard_and_a_project_relative_deny() {
        let f = files();
        let deny = vec![Rule::new("read", "secrets/*", Effect::Deny)];
        let (b, _a, gate, rec, driver) = attached(&f, deny);
        let root = file_url(&f.root);
        assert!(matches!(
            open(&b, &gate, &f, &format!("{root}/.env")),
            Err(BrowserError::Denied(_))
        ));
        assert_eq!(rec.actions(), ["read"]);
        for rest in [
            "secrets/key",
            "secrets/./key",
            "secrets//key",
            "secrets/%6Bey",
            "x/%2e%2e/secrets/key",
            "x/%2E%2E/secrets/key",
            "SECRETS/../secrets/key",
        ] {
            assert!(
                matches!(
                    open(&b, &gate, &f, &format!("{root}/{rest}")),
                    Err(BrowserError::Denied(_))
                ),
                "{rest}"
            );
        }
        assert_eq!(rec.actions(), ["read"], "the deny asks nothing");
        assert!(driver.urls().is_empty(), "{:?}", driver.urls());
    }

    /// `..` in a file:// URL is resolved before the checks, so a URL that
    /// climbs out of the project is checked (and opened) as the file it
    /// names.
    #[test]
    fn a_file_url_that_climbs_out_is_checked_where_it_lands() {
        let f = files();
        let page = f.outside.join("page.html");
        for url in [
            format!("{}/../outside/page.html", file_url(&f.root)),
            format!("{}/%2e%2e/outside/page.html", file_url(&f.root)),
        ] {
            let (b, _a, gate, rec, driver) = attached(&f, vec![]);
            assert!(matches!(
                open(&b, &gate, &f, &url),
                Err(BrowserError::Denied(_))
            ));
            assert_eq!(
                rec.requests(),
                [(
                    "external_directory".to_string(),
                    vec![page.display().to_string()]
                )],
                "{url}"
            );
            assert!(driver.urls().is_empty());
            let (b, _a, gate, _rec, driver) = attached(&f, outside_allowed());
            open(&b, &gate, &f, &url).unwrap();
            assert_eq!(driver.urls(), [file_url(&page)]);
        }
        let (b, _a, gate, rec, driver) = attached(&f, vec![]);
        assert!(matches!(
            open(&b, &gate, &f, "file:///w/../../etc/passwd"),
            Err(BrowserError::Denied(_))
        ));
        assert_eq!(rec.actions(), ["external_directory"]);
        assert!(driver.urls().is_empty());
    }

    /// A workspace symlink to the SSH folder is judged by where it points:
    /// outside the project, and guarded.
    #[cfg(unix)]
    #[test]
    fn a_file_url_through_a_symlink_is_checked_by_its_target() {
        let f = files();
        symlink(&f.home.join(".ssh"), &f.root.join("keys"));
        symlink(&f.outside.join("page.html"), &f.root.join("page.html"));
        let key = f.home.join(".ssh/id_rsa");
        let (b, _a, gate, rec, driver) = attached(&f, vec![]);
        for rel in ["keys/id_rsa", "page.html"] {
            let url = format!("{}/{rel}", file_url(&f.root));
            assert!(matches!(
                open(&b, &gate, &f, &url),
                Err(BrowserError::Denied(_))
            ));
        }
        assert_eq!(rec.actions(), ["external_directory", "external_directory"]);
        let (b, _a, gate, rec, driver2) = attached(&f, outside_allowed());
        let url = format!("{}/keys/id_rsa", file_url(&f.root));
        assert!(matches!(
            open(&b, &gate, &f, &url),
            Err(BrowserError::Denied(_))
        ));
        let asked = rec.requests();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].0, "read");
        assert!(asked[0].1.contains(&key.display().to_string()), "{asked:?}");
        let page = format!("{}/page.html", file_url(&f.root));
        open(&b, &gate, &f, &page).unwrap();
        assert!(driver.urls().is_empty());
        assert_eq!(driver2.urls(), [file_url(&f.outside.join("page.html"))]);
    }

    /// The browser is handed the file that was checked, encoded: `#` and
    /// `?` in a file's name stay part of it, and a URL's own query (which
    /// a file has none of) is dropped.
    #[test]
    fn a_file_url_is_opened_as_the_encoded_real_path() {
        let f = files();
        std::fs::write(f.root.join("a#b?c.html"), "x").unwrap();
        let (b, _a, gate, rec, driver) = attached(&f, vec![]);
        let root = file_url(&f.root);
        open(&b, &gate, &f, &format!("{root}/a%23b%3Fc.html#top")).unwrap();
        open(
            &b,
            &gate,
            &f,
            &format!("{root}/index.html?x=/../../etc/passwd"),
        )
        .unwrap();
        open(
            &b,
            &gate,
            &f,
            &format!("FILE://{}/index.html", f.root.display()),
        )
        .unwrap();
        open(
            &b,
            &gate,
            &f,
            &format!("file://localhost{}/index.html", f.root.display()),
        )
        .unwrap();
        assert_eq!(
            driver.urls(),
            [
                format!("{root}/a%23b%3Fc.html#top"),
                format!("{root}/index.html"),
                format!("{root}/index.html"),
                format!("{root}/index.html"),
            ]
        );
        assert!(rec.requests().is_empty(), "{:?}", rec.requests());
    }

    #[test]
    fn a_file_url_that_names_no_local_file_is_refused() {
        let f = files();
        let (b, _a, gate, rec, driver) = attached(&f, vec![]);
        let root = file_url(&f.root);
        for url in [
            "file://evil.test/etc/passwd".to_string(),
            format!("{root}/index%00.html"),
            format!("{root}/%FF.html"),
            "file://evil.test/".to_string(),
        ] {
            assert!(
                matches!(open(&b, &gate, &f, &url), Err(BrowserError::Invalid(_))),
                "{url}"
            );
        }
        assert!(rec.requests().is_empty(), "{:?}", rec.requests());
        assert!(driver.urls().is_empty(), "{:?}", driver.urls());
    }

    /// A configured `browser` deny meets the URL however it is spelled:
    /// the rules see it as it would be requested.
    #[test]
    fn a_browser_deny_is_not_dodged_by_spelling() {
        let f = files();
        let deny = vec![
            Rule::new(ACTION, "https://evil.test/*", Effect::Deny),
            Rule::new(ACTION, "https://good.test/secret", Effect::Deny),
        ];
        let (b, _a, gate, rec, driver) = attached(&f, deny);
        for url in [
            "https://evil.test/x",
            "HTTPS://evil.test/x",
            "https://EVIL.test/x",
            "https://evil.test:443/x",
            "https://evil.test./x",
            "https://evil.test../x",
            "HtTpS://EvIl.TeSt.:443/a/../x#top",
            "https://evil.test",
            "https://evil.te\tst/x",
            "https://good.test/%73ecret",
            "https://good.test/a/../secret",
            "https://good.test/secret#f",
        ] {
            assert!(
                matches!(open(&b, &gate, &f, url), Err(BrowserError::Denied(_))),
                "{url}"
            );
        }
        assert!(rec.requests().is_empty(), "{:?}", rec.requests());
        assert!(driver.urls().is_empty(), "{:?}", driver.urls());
    }

    /// Credentials would put the host somewhere a rule does not look; they
    /// are refused before anything is asked, like other schemes.
    #[test]
    fn a_browser_url_with_credentials_or_another_scheme_is_refused() {
        let f = files();
        let deny = vec![Rule::new(ACTION, "https://evil.test/*", Effect::Deny)];
        let (b, _a, gate, rec, driver) = attached(&f, deny);
        for url in [
            "https://u@evil.test/x",
            "https://u:p@evil.test/x",
            "https://:p@good.test/",
            "https://*.test/",
            "https://.../",
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "ftp://good.test/",
            "about:blank",
            "chrome://settings",
            "blob:https://good.test/x",
            "view-source:https://evil.test/",
            "//evil.test/x",
            "evil.test/x",
            "",
        ] {
            assert!(
                matches!(open(&b, &gate, &f, url), Err(BrowserError::Invalid(_))),
                "{url}"
            );
        }
        assert!(rec.requests().is_empty(), "{:?}", rec.requests());
        assert!(driver.urls().is_empty(), "{:?}", driver.urls());
    }

    /// The browser is handed the URL the rules saw, with its fragment.
    #[test]
    fn a_browser_opens_the_url_the_rules_saw() {
        let f = files();
        let (b, _a, gate, rec, driver) = attached(&f, vec![]);
        open(&b, &gate, &f, "HTTPS://Good.Test.:443/a/../b?q=1#frag").unwrap();
        open(&b, &gate, &f, "http://good.test:80").unwrap();
        open(&b, &gate, &f, "http://good.test:8080/").unwrap();
        assert_eq!(
            driver.urls(),
            [
                "https://good.test/b?q=1#frag",
                "http://good.test/",
                "http://good.test:8080/",
            ]
        );
        assert!(rec.requests().is_empty(), "{:?}", rec.requests());
    }

    /// Like an "always" approval, an attachment turns a configured or
    /// built-in `browser` ask into an allow while it lasts (documented in
    /// docs/permissions.md); a `deny` is what forbids.
    #[test]
    fn an_attachment_answers_a_browser_ask_as_a_saved_approval_would() {
        let f = files();
        let url = "https://good.test/";
        for rule in [
            Rule::new(ACTION, "*", Effect::Ask),
            Rule::new(ACTION, "*", Effect::Ask).into_builtin(),
        ] {
            let (b, a, gate, rec, driver) = attached(&f, vec![rule.clone()]);
            open(&b, &gate, &f, url).unwrap();
            assert!(rec.requests().is_empty(), "{rule:?}");
            assert_eq!(driver.urls(), [url]);
            drop(a);
            assert!(!gate.allows(ACTION, &[url.to_string()]), "{rule:?}");
        }
        let deny = vec![Rule::new(ACTION, "*", Effect::Deny)];
        let (b, _a, gate, rec, driver) = attached(&f, deny);
        assert!(matches!(
            open(&b, &gate, &f, url),
            Err(BrowserError::Denied(_))
        ));
        assert!(rec.requests().is_empty() && driver.urls().is_empty());
    }

    #[test]
    fn a_grant_is_only_allows() {
        assert!(rules()
            .iter()
            .all(|r| r.effect == Effect::Allow && !r.builtin && !r.guard && r.policy.is_none()));
    }

    /// Every rule an attachment grants is one the gate keeps.
    #[test]
    fn a_grant_names_only_grantable_actions() {
        let (b, _) = registry();
        let mut gate = Gate::new(vec![], Arc::new(Fixed(Reply::Reject(None))));
        gate.set_granted(Some(b.grants_for("s")));
        let _a = b.attach("s", "c").unwrap();
        assert_eq!(gate.granted(), rules());
        assert!(rules()
            .iter()
            .all(|r| crate::permission::GRANTABLE.contains(&r.action.as_str())));
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
            b.open(&gate, "s", Path::new("/"), "https://example.test/"),
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
