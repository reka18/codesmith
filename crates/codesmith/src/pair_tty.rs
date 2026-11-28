//! The pairing dialog in the terminal: the QR code, the link and a
//! countdown, then the confirmation when a phone claims the code.
//!
//! Everything is read from and written to the controlling terminal
//! (`/dev/tty`), never stdin or stdout: a piped `y` can't answer, and a
//! run with no terminal (the agent's own commands have none) refuses
//! before anything starts (P10, X3). The mouse is never captured.

use codesmith_core::remote::control::{self, Answer, Asking, Frame, GrantAnswer};
use codesmith_core::remote::key;
use codesmith_core::remote::store::{Scope, ALL_SESSIONS};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// A key the dialog understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Esc,
    /// Ctrl-C or Ctrl-D.
    Interrupt,
    /// Anything else (arrows, function keys): ignored.
    Other,
}

/// The controlling terminal, in raw mode while this lives.
pub struct Tty {
    file: std::fs::File,
    saved: Option<libc::termios>,
    pending: Vec<u8>,
}

impl Tty {
    /// The controlling terminal. Fails with no terminal, as in the agent's
    /// shell commands and anything started with no terminal.
    pub fn open() -> io::Result<Tty> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOCTTY)
            .open("/dev/tty")?;
        Tty::from_file(file)
    }

    /// A terminal from an open file (a pseudo-terminal, in tests).
    pub fn from_file(file: std::fs::File) -> io::Result<Tty> {
        // SAFETY: isatty on a descriptor this process owns.
        if unsafe { libc::isatty(file.as_raw_fd()) } != 1 {
            return Err(io::Error::other("not a terminal"));
        }
        Ok(Tty {
            file,
            saved: None,
            pending: Vec::new(),
        })
    }

    /// Single keys, no echo, no signals; output still turns `\n` into
    /// `\r\n`.
    pub fn raw(&mut self) -> io::Result<()> {
        let fd = self.file.as_raw_fd();
        // SAFETY: termios calls on a terminal descriptor this process owns,
        // with a struct tcgetattr filled.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            if self.saved.is_none() {
                self.saved = Some(t);
            }
            t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            t.c_iflag &= !(libc::IXON | libc::ICRNL | libc::INLCR);
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn restore(&mut self) {
        if let Some(t) = self.saved.take() {
            // SAFETY: restoring the attributes tcgetattr returned.
            unsafe {
                libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &t);
            }
        }
    }

    /// Drop whatever was typed ahead.
    pub fn flush_input(&mut self) {
        self.pending.clear();
        // SAFETY: tcflush on a terminal descriptor this process owns.
        unsafe {
            libc::tcflush(self.file.as_raw_fd(), libc::TCIFLUSH);
        }
    }

    pub fn writer(&self) -> io::Result<std::fs::File> {
        self.file.try_clone()
    }

    fn fill(&mut self, timeout: Duration) -> io::Result<bool> {
        if wait_readable(self.file.as_raw_fd(), timeout)? {
            let mut buf = [0u8; 256];
            let n = io::Read::read(&mut self.file, &mut buf)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the terminal closed",
                ));
            }
            self.pending.extend_from_slice(&buf[..n]);
            return Ok(true);
        }
        Ok(false)
    }

    /// The next key, or `None` when `timeout` passes first.
    pub fn key(&mut self, timeout: Duration) -> io::Result<Option<Key>> {
        if self.pending.is_empty() && !self.fill(timeout)? {
            return Ok(None);
        }
        // An escape alone is Esc; one followed at once by more is a
        // sequence (an arrow key), taken whole and ignored.
        if self.pending == [0x1b] {
            let _ = self.fill(Duration::from_millis(30))?;
        }
        Ok(Some(take_key(&mut self.pending)))
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Whether `fd` has input within `timeout`. `select`, not `poll`: macOS's
/// `poll` doesn't work on terminal devices.
fn wait_readable(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    if fd < 0 || fd as usize >= libc::FD_SETSIZE {
        return Err(io::Error::other(
            "the terminal's descriptor is out of range",
        ));
    }
    // SAFETY: an fd_set zeroed and filled with one in-range descriptor,
    // and a timeval, both valid for the call.
    unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
        let mut tv = libc::timeval {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_usec: timeout.subsec_micros() as libc::suseconds_t,
        };
        let r = libc::select(
            fd + 1,
            &mut set,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        );
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e);
        }
        Ok(r > 0 && libc::FD_ISSET(fd, &set))
    }
}

/// The first key in `buf`, taken out of it (`buf` must not be empty).
pub fn take_key(buf: &mut Vec<u8>) -> Key {
    let b = buf[0];
    let (key, used) = match b {
        0x03 | 0x04 => (Key::Interrupt, 1),
        b'\r' | b'\n' => (Key::Enter, 1),
        0x7f | 0x08 => (Key::Backspace, 1),
        // Alone, or before a control character: Esc itself.
        0x1b if buf.len() == 1 || buf[1] < 0x20 || buf[1] == 0x7f => (Key::Esc, 1),
        0x1b => {
            // ESC [ … final, or ESC O x, or ESC x.
            let mut i = 1;
            if matches!(buf.get(1), Some(b'[') | Some(b'O')) {
                i = 2;
                while i < buf.len() && !(0x40..=0x7e).contains(&buf[i]) {
                    i += 1;
                }
                i = (i + 1).min(buf.len());
            } else {
                i += 1;
            }
            (Key::Other, i)
        }
        0x20..=0x7e => (Key::Char(b as char), 1),
        0x80.. => {
            let len = match b {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            match buf.get(..len).and_then(|s| std::str::from_utf8(s).ok()) {
                Some(s) => (s.chars().next().map(Key::Char).unwrap_or(Key::Other), len),
                None => (Key::Other, len.min(buf.len())),
            }
        }
        _ => (Key::Other, 1),
    };
    buf.drain(..used.max(1).min(buf.len()));
    key
}

/// One line typed in raw mode.
#[derive(Debug, Default)]
pub struct LineEdit {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    Typing,
    Submit(String),
    Cancel,
    Interrupt,
}

impl LineEdit {
    pub fn feed(&mut self, key: &Key) -> Edit {
        match key {
            Key::Char(c) => {
                if self.text.chars().count() < 16 {
                    self.text.push(*c);
                }
                Edit::Typing
            }
            Key::Backspace => {
                self.text.pop();
                Edit::Typing
            }
            Key::Enter => Edit::Submit(std::mem::take(&mut self.text)),
            Key::Esc => Edit::Cancel,
            Key::Interrupt => Edit::Interrupt,
            Key::Other => Edit::Typing,
        }
    }
}

/// What the first answer chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Yes,
    Watch,
    All,
    No,
}

/// The answer to the confirmation: only `y` or `yes` allows; `w` and `a`
/// pick the other grants; anything else, Enter alone included, refuses.
pub fn choice(line: &str) -> Choice {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Choice::Yes,
        "w" | "watch" => Choice::Watch,
        "a" | "all" => Choice::All,
        _ => Choice::No,
    }
}

/// The second confirmation for every session: exactly `all`.
pub fn confirms_all(line: &str) -> bool {
    line.trim().eq_ignore_ascii_case("all")
}

/// What the dialog offers.
#[derive(Debug, Clone, Default)]
pub struct Offer {
    pub session: Option<String>,
    pub title: Option<String>,
    /// `--all`: every session.
    pub all: bool,
    /// `--watch`: watch only.
    pub watch: bool,
}

impl Offer {
    fn scope(&self) -> Scope {
        if self.watch {
            Scope::Watch
        } else {
            Scope::Control
        }
    }

    fn what(&self) -> String {
        let verb = if self.watch { "watch" } else { "control" };
        if self.all {
            return format!("to {verb} every session on this machine");
        }
        match (&self.session, &self.title) {
            (Some(_), Some(t)) => format!(
                "to {verb} \"{}\"",
                codesmith_core::remote::link::printable(t)
            ),
            (Some(s), None) => format!("to {verb} session {s}"),
            (None, _) => "to pair with this machine (no session yet)".into(),
        }
    }

    fn keys(&self) -> &'static str {
        if self.all {
            "[y] yes   [n] no"
        } else if self.session.is_none() || self.watch {
            "[y] yes   [a] all sessions on this machine   [n] no"
        } else {
            "[y] yes   [w] watch only   [a] all sessions on this machine   [n] no"
        }
    }

    /// The grant a first answer gives, or `Err(scope)` when it needs the
    /// second confirmation for every session; `None` to refuse.
    pub fn grant(&self, c: Choice) -> Option<Result<Option<GrantAnswer>, Scope>> {
        let one = |scope| {
            Some(Ok(self.session.as_ref().map(|s| GrantAnswer {
                session: s.clone(),
                scope,
            })))
        };
        match c {
            Choice::No => None,
            Choice::All => Some(Err(self.scope())),
            Choice::Yes if self.all => Some(Err(self.scope())),
            Choice::Yes => one(self.scope()),
            Choice::Watch if self.all => Some(Err(Scope::Watch)),
            Choice::Watch => one(Scope::Watch),
        }
    }
}

/// The confirmation's lines (P8).
pub fn prompt_lines(offer: &Offer, device: &Asking, check: &str) -> Vec<String> {
    let name = codesmith_core::remote::link::printable(&device.name);
    let platform = codesmith_core::remote::link::printable(&device.platform);
    let paired = if device.paired {
        " (paired device)"
    } else {
        ""
    };
    vec![
        format!(
            "Allow \"{name}\" ({platform}, {}){paired} {}?",
            key::short(&device.id),
            offer.what()
        ),
        format!("Check code on the phone: {check}"),
        offer.keys().to_string(),
    ]
}

/// The QR code for `text`, error correction L, drawn with half blocks in
/// black on white (whatever the terminal's colours) with a quiet zone of
/// `quiet` modules.
pub fn qr_lines(text: &str, quiet: usize) -> Option<Vec<String>> {
    use qrcode::{Color, EcLevel, QrCode};
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::L).ok()?;
    let w = code.width();
    let colors = code.to_colors();
    let size = w + 2 * quiet;
    let dark = |x: usize, y: usize| -> bool {
        if x < quiet || y < quiet || x >= quiet + w || y >= quiet + w {
            return false;
        }
        colors[(y - quiet) * w + (x - quiet)] == Color::Dark
    };
    let mut out = Vec::new();
    let mut y = 0;
    while y < size {
        let mut line = String::new();
        for x in 0..size {
            let top = dark(x, y);
            let bottom = y + 1 < size && dark(x, y + 1);
            // Foreground is the top half, background the bottom.
            let fg = if top { 30 } else { 97 };
            let bg = if bottom { 40 } else { 107 };
            line.push_str(&format!("\x1b[{fg};{bg}m▀"));
        }
        line.push_str("\x1b[0m");
        out.push(line);
        y += 2;
    }
    Some(out)
}

/// The terminal's width, when it says.
fn width(fd: RawFd) -> usize {
    // SAFETY: TIOCGWINSZ fills a winsize.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    80
}

/// How the dialog ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Paired {
        name: String,
        grant: Option<GrantAnswer>,
    },
    /// Closed here (`q`, Esc or Ctrl-C while no claim was asked).
    Closed,
    /// Another dialog took over.
    Replaced,
}

/// A key and when it was pressed.
pub type Pressed = (Instant, Key);

/// Read keys from `tty` on a thread of their own, until `stop` is set or
/// the receiver goes.
pub fn key_thread(
    mut tty: Tty,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> (
    mpsc::UnboundedReceiver<Pressed>,
    std::thread::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let h = std::thread::spawn(move || {
        use std::sync::atomic::Ordering;
        while !stop.load(Ordering::SeqCst) {
            match tty.key(Duration::from_millis(100)) {
                Ok(Some(k)) => {
                    if tx.send((Instant::now(), k)).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
        // `tty` drops here, putting the terminal back.
    });
    (rx, h)
}

/// Keys pressed sooner than this after a prompt appears answer nothing.
pub const SETTLE: Duration = Duration::from_millis(300);

enum Stage {
    /// Showing the code.
    Waiting {
        link: String,
        machine: String,
        expires: Instant,
        relay: Option<String>,
        relay_connected: bool,
    },
    /// Asking about a claim.
    Asking {
        id: u64,
        lines: Vec<String>,
        deadline: Instant,
        since: Instant,
        edit: LineEdit,
        /// After `a`: the scope every session would get.
        all: Option<Scope>,
    },
}

fn mmss(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// Draw the code screen; the row the countdown is on.
fn draw_code(out: &mut dyn Write, offer: &Offer, stage: &Stage, cols: usize) -> io::Result<()> {
    let Stage::Waiting {
        link,
        machine,
        relay,
        relay_connected,
        ..
    } = stage
    else {
        return Ok(());
    };
    write!(out, "\x1b[2J\x1b[H")?;
    let quiet = if cols >= 90 { 4 } else { 2 };
    match qr_lines(link, quiet) {
        Some(lines) => {
            for l in lines {
                writeln!(out, "{l}")?;
            }
        }
        None => writeln!(
            out,
            "(The link is too long for a QR code; copy it instead.)"
        )?,
    }
    writeln!(out)?;
    writeln!(
        out,
        "Scan this with CodeSmith Mobile to pair it with {}.",
        codesmith_core::remote::link::printable(machine)
    )?;
    match (&offer.session, &offer.title, offer.all) {
        (_, _, true) => writeln!(out, "It asks for every session on this machine.")?,
        (Some(s), Some(t), _) => writeln!(
            out,
            "Session: {} ({s})",
            codesmith_core::remote::link::printable(t)
        )?,
        (Some(s), None, _) => writeln!(out, "Session: {s}")?,
        (None, _, _) => writeln!(
            out,
            "No session: the phone is paired and gets a session later."
        )?,
    }
    match (relay, relay_connected) {
        (Some(r), true) => writeln!(out, "Relay: {r}")?,
        (Some(r), false) => writeln!(out, "Relay: {r} (not reached yet; the phone can still try)")?,
        (None, _) => writeln!(out, "Relay: none (direct addresses only)")?,
    }
    writeln!(out, "Link: {link}")?;
    writeln!(out)?;
    // The countdown goes on the next line.
    Ok(())
}

fn draw_countdown(out: &mut dyn Write, stage: &Stage) -> io::Result<()> {
    let now = Instant::now();
    match stage {
        Stage::Waiting { expires, .. } => write!(
            out,
            "\r\x1b[2KCode expires in {}, then a new one shows.  [c] copy link   [q] close",
            mmss(expires.saturating_duration_since(now))
        )?,
        Stage::Asking {
            deadline,
            edit,
            all,
            since,
            ..
        } => {
            if now < *since + SETTLE {
                // Keys answer nothing yet; say so by not asking.
                write!(
                    out,
                    "\r\x1b[2K({} left)",
                    mmss(deadline.saturating_duration_since(now))
                )?;
                return out.flush();
            }
            let ask = if all.is_some() {
                "Type \"all\" to confirm"
            } else {
                "Answer"
            };
            write!(
                out,
                "\r\x1b[2K({} left) {ask}: {}",
                mmss(deadline.saturating_duration_since(now)),
                edit.text
            )?
        }
    }
    out.flush()
}

fn draw_asking(out: &mut dyn Write, stage: &Stage) -> io::Result<()> {
    let Stage::Asking { lines, all, .. } = stage else {
        return Ok(());
    };
    write!(out, "\x1b[2J\x1b[H")?;
    for l in lines {
        writeln!(out, "{l}")?;
    }
    if all.is_some() {
        writeln!(
            out,
            "\nThat grants every session on this machine, now and future."
        )?;
    }
    writeln!(out)?;
    Ok(())
}

/// Copy `text` to the clipboard with OSC 52, where the terminal allows it.
fn copy(out: &mut dyn Write, text: &str) -> io::Result<()> {
    use base64::Engine;
    let b = base64::engine::general_purpose::STANDARD.encode(text);
    write!(out, "\x1b]52;c;{b}\x07")?;
    out.flush()
}

/// Run the dialog on `conn` (a `pair` connection), reading `keys` and
/// drawing on `out`, until a phone is paired or the dialog closes.
pub async fn run(
    conn: &mut control::Conn,
    offer: &Offer,
    keys: &mut mpsc::UnboundedReceiver<Pressed>,
    out: &mut (dyn Write + Send),
    cols: usize,
) -> anyhow::Result<Outcome> {
    let mut stage: Option<Stage> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut note: Option<String> = None;
    loop {
        let (deadline, settle) = match &stage {
            Some(Stage::Asking {
                deadline, since, ..
            }) => (
                Some(*deadline),
                (Instant::now() < *since + SETTLE).then_some(*since + SETTLE),
            ),
            _ => (None, None),
        };
        let far = Instant::now() + Duration::from_secs(3600);
        tokio::select! {
            f = conn.next() => {
                let Some(f) = f? else {
                    anyhow::bail!("the gateway closed the pairing");
                };
                match f {
                    Frame::Code { link, machine, expires_in_ms, relay, relay_connected, .. } => {
                        let s = Stage::Waiting {
                            link,
                            machine,
                            expires: Instant::now() + Duration::from_millis(expires_in_ms),
                            relay,
                            relay_connected,
                        };
                        draw_code(out, offer, &s, cols)?;
                        if let Some(n) = note.take() {
                            writeln!(out, "{n}")?;
                        }
                        draw_countdown(out, &s)?;
                        stage = Some(s);
                    }
                    Frame::Confirm { id, device, check, expires_in_ms } => {
                        let now = Instant::now();
                        let s = Stage::Asking {
                            id,
                            lines: prompt_lines(offer, &device, &check),
                            deadline: now + Duration::from_millis(expires_in_ms),
                            since: now,
                            edit: LineEdit::default(),
                            all: None,
                        };
                        write!(out, "\x07")?;
                        draw_asking(out, &s)?;
                        draw_countdown(out, &s)?;
                        stage = Some(s);
                    }
                    Frame::Paired { device, grant } => {
                        return Ok(Outcome::Paired { name: device.name, grant });
                    }
                    Frame::Refused { reason } => {
                        note = Some(format!("Not paired: {reason}."));
                        stage = None;
                    }
                    Frame::Replaced => return Ok(Outcome::Replaced),
                    Frame::Error { message } => anyhow::bail!(message),
                    Frame::Ok | Frame::Status(_) => {}
                }
            }
            k = keys.recv() => {
                let Some((at, k)) = k else {
                    anyhow::bail!("the terminal closed");
                };
                match &mut stage {
                    None => {
                        if k == Key::Interrupt {
                            return Ok(Outcome::Closed);
                        }
                    }
                    Some(s @ Stage::Waiting { .. }) => match k {
                        Key::Char('c') | Key::Char('C') => {
                            if let Stage::Waiting { link, .. } = &*s {
                                copy(out, link)?;
                            }
                            write!(out, "\r\x1b[2KLink copied (where the terminal allows it).")?;
                            out.flush()?;
                        }
                        Key::Char('q') | Key::Char('Q') | Key::Esc | Key::Interrupt => {
                            return Ok(Outcome::Closed);
                        }
                        _ => {}
                    },
                    Some(Stage::Asking { id, since, edit, all, .. }) => {
                        // A key pressed as the prompt appeared answers nothing.
                        if at < *since + SETTLE {
                            continue;
                        }
                        let id = *id;
                        let refuse = Answer { id, allow: false, grant: None };
                        let send = match edit.feed(&k) {
                            Edit::Typing => None,
                            Edit::Cancel => Some(refuse),
                            Edit::Interrupt => {
                                conn.send(&refuse).await?;
                                return Ok(Outcome::Closed);
                            }
                            Edit::Submit(line) => match all.take() {
                                Some(scope) if confirms_all(&line) => Some(Answer {
                                    id,
                                    allow: true,
                                    grant: Some(GrantAnswer {
                                        session: ALL_SESSIONS.into(),
                                        scope,
                                    }),
                                }),
                                Some(_) => Some(refuse),
                                None => match offer.grant(choice(&line)) {
                                    None => Some(refuse),
                                    Some(Ok(grant)) => Some(Answer { id, allow: true, grant }),
                                    Some(Err(scope)) => {
                                        *all = Some(scope);
                                        None
                                    }
                                },
                            },
                        };
                        match send {
                            Some(a) => {
                                // One answer per prompt: wait for the gateway's word.
                                conn.send(&a).await?;
                                stage = None;
                                write!(out, "\r\x1b[2K")?;
                                out.flush()?;
                            }
                            None => {
                                if let Some(s) = &stage {
                                    if matches!(s, Stage::Asking { all: Some(_), edit, .. } if edit.text.is_empty()) {
                                        draw_asking(out, s)?;
                                    }
                                    draw_countdown(out, s)?;
                                }
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(settle.unwrap_or(far).into()) => {
                // Settled: now it asks.
                if let Some(s) = &stage {
                    draw_countdown(out, s)?;
                }
            }
            _ = tick.tick() => {
                if let Some(s) = &stage {
                    draw_countdown(out, s)?;
                }
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or(far).into()) => {
                // Out of time here too; the gateway refuses it.
                if let Some(Stage::Asking { id, .. }) = &stage {
                    let id = *id;
                    conn.send(&Answer { id, allow: false, grant: None }).await?;
                    stage = None;
                }
            }
        }
    }
}

/// The terminal's width.
pub fn tty_width(t: &std::fs::File) -> usize {
    width(t.as_raw_fd())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Key> {
        let mut b = bytes.to_vec();
        let mut out = vec![];
        while !b.is_empty() {
            out.push(take_key(&mut b));
        }
        out
    }

    #[test]
    fn bytes_become_keys() {
        assert_eq!(
            keys(b"y\r\x7f\x1b\x03"),
            [
                Key::Char('y'),
                Key::Enter,
                Key::Backspace,
                Key::Esc,
                Key::Interrupt
            ]
        );
        // An arrow key is one ignored key, not an Esc and letters.
        assert_eq!(keys(b"\x1b[A"), [Key::Other]);
        assert_eq!(keys(b"\x1b[1;5Cy"), [Key::Other, Key::Char('y')]);
        assert_eq!(keys("é".as_bytes()), [Key::Char('é')]);
        assert_eq!(keys(b"\xff"), [Key::Other]);
        assert_eq!(keys(b"\x01"), [Key::Other]);
    }

    /// A pseudo-terminal's two ends: the master to type into, the slave
    /// as the dialog's terminal.
    fn pty() -> (std::fs::File, Tty) {
        use std::os::fd::FromRawFd;
        let (mut m, mut s) = (0, 0);
        // SAFETY: openpty fills two descriptors this test then owns.
        let r = unsafe {
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(r, 0);
        // SAFETY: fresh descriptors from openpty.
        let (m, s) = unsafe { (std::fs::File::from_raw_fd(m), std::fs::File::from_raw_fd(s)) };
        let mut t = Tty::from_file(s).unwrap();
        t.raw().unwrap();
        (m, t)
    }

    /// Regression: macOS's `poll` doesn't work on terminals, so a lone Esc
    /// waited for the next key and was eaten with it, and no wait timed
    /// out.
    #[test]
    fn a_real_terminal_gives_a_lone_esc_and_times_out() {
        let (mut m, mut t) = pty();
        let start = Instant::now();
        assert_eq!(t.key(Duration::from_millis(100)).unwrap(), None);
        assert!(start.elapsed() < Duration::from_secs(2));
        m.write_all(b"\x1b").unwrap();
        assert_eq!(t.key(Duration::from_secs(5)).unwrap(), Some(Key::Esc));
        m.write_all(b"y\r\x1b[A").unwrap();
        assert_eq!(t.key(Duration::from_secs(5)).unwrap(), Some(Key::Char('y')));
        assert_eq!(t.key(Duration::from_secs(5)).unwrap(), Some(Key::Enter));
        assert_eq!(t.key(Duration::from_secs(5)).unwrap(), Some(Key::Other));
        // Typed ahead, then flushed: gone.
        m.write_all(b"yes\r").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        t.flush_input();
        assert_eq!(t.key(Duration::from_millis(100)).unwrap(), None);
    }

    #[test]
    fn a_file_that_isnt_a_terminal_is_refused() {
        let f = tempfile::tempfile().unwrap();
        assert!(Tty::from_file(f).is_err());
    }

    #[test]
    fn only_y_or_yes_allows() {
        for y in ["y", "Y", "yes", " YES "] {
            assert_eq!(choice(y), Choice::Yes, "{y}");
        }
        for n in ["", "n", "no", "yy", "ye", "yes please", "1", "true", "ok"] {
            assert_eq!(choice(n), Choice::No, "{n:?}");
        }
        assert_eq!(choice("w"), Choice::Watch);
        assert_eq!(choice("a"), Choice::All);
        assert!(confirms_all("all") && confirms_all(" ALL "));
        assert!(!confirms_all("a") && !confirms_all("") && !confirms_all("yes"));
    }

    #[test]
    fn line_editing() {
        let mut e = LineEdit::default();
        assert_eq!(e.feed(&Key::Char('y')), Edit::Typing);
        assert_eq!(e.feed(&Key::Char('x')), Edit::Typing);
        assert_eq!(e.feed(&Key::Backspace), Edit::Typing);
        assert_eq!(e.feed(&Key::Other), Edit::Typing);
        assert_eq!(e.feed(&Key::Enter), Edit::Submit("y".into()));
        assert_eq!(e.feed(&Key::Enter), Edit::Submit(String::new()));
        assert_eq!(e.feed(&Key::Esc), Edit::Cancel);
        assert_eq!(e.feed(&Key::Interrupt), Edit::Interrupt);
        for _ in 0..100 {
            e.feed(&Key::Char('a'));
        }
        assert_eq!(e.text.len(), 16, "bounded");
    }

    #[test]
    fn what_each_answer_grants() {
        let s = Offer {
            session: Some("ses_1".into()),
            title: Some("t".into()),
            ..Default::default()
        };
        let g = |o: &Offer, c| o.grant(c);
        assert_eq!(
            g(&s, Choice::Yes),
            Some(Ok(Some(GrantAnswer {
                session: "ses_1".into(),
                scope: Scope::Control
            })))
        );
        assert_eq!(
            g(&s, Choice::Watch),
            Some(Ok(Some(GrantAnswer {
                session: "ses_1".into(),
                scope: Scope::Watch
            })))
        );
        assert_eq!(g(&s, Choice::All), Some(Err(Scope::Control)));
        assert_eq!(g(&s, Choice::No), None);
        // --watch caps every grant at watch.
        let w = Offer {
            watch: true,
            ..s.clone()
        };
        assert_eq!(
            g(&w, Choice::Yes),
            Some(Ok(Some(GrantAnswer {
                session: "ses_1".into(),
                scope: Scope::Watch
            })))
        );
        assert_eq!(g(&w, Choice::All), Some(Err(Scope::Watch)));
        // --all: yes still needs the second confirmation.
        let a = Offer {
            all: true,
            ..s.clone()
        };
        assert_eq!(g(&a, Choice::Yes), Some(Err(Scope::Control)));
        // No session: yes pairs the device alone.
        let none = Offer::default();
        assert_eq!(g(&none, Choice::Yes), Some(Ok(None)));
    }

    #[test]
    fn the_prompt_shows_name_platform_short_id_session_and_check_code() {
        let o = Offer {
            session: Some("ses_1".into()),
            title: Some("fix-the-login-page".into()),
            ..Default::default()
        };
        let d = Asking {
            id: "f8820d751e90d88eca3b848440efc98ae226f1552a6fb5c76c7ad09c96832dc6".into(),
            name: "Ray's iPhone".into(),
            platform: "ios".into(),
            paired: false,
        };
        let l = prompt_lines(&o, &d, "779 852 318 371");
        assert_eq!(
            l[0],
            "Allow \"Ray's iPhone\" (ios, f8820d751e) to control \"fix-the-login-page\"?"
        );
        assert_eq!(l[1], "Check code on the phone: 779 852 318 371");
        assert!(l[2].starts_with("[y] yes   [w] watch only   [a] all sessions"));
        let paired = Asking {
            paired: true,
            ..d.clone()
        };
        assert!(prompt_lines(&o, &paired, "x")[0].contains("(paired device)"));
        // A hostile name can't move the cursor or reorder the text.
        let evil = Asking {
            name: "\u{1b}[2J\u{202e}x".into(),
            ..d
        };
        let l = prompt_lines(&o, &evil, "x");
        assert!(
            !l[0].contains('\u{1b}') && !l[0].contains('\u{202e}'),
            "{}",
            l[0]
        );
    }

    #[test]
    fn the_qr_code_is_square_with_a_quiet_zone() {
        let link = "codesmith://pair?v=1&n=aa980150be84faa3788d7d170f02984f084bcaeb6871fe1fe7805ff2505a375c&m=mini&c=pn00cOpwBxfLmPR62H1EaQ";
        let lines = qr_lines(link, 4).unwrap();
        let cells = |l: &str| l.matches('▀').count();
        let w = cells(&lines[0]);
        assert!(lines.iter().all(|l| cells(l) == w));
        assert_eq!(lines.len(), w.div_ceil(2));
        // The quiet zone is light: the first row is light on light.
        assert!(lines[0]
            .split('▀')
            .all(|c| c.is_empty() || c.contains("97;107m") || c.contains("\x1b[0m")));
        // A version with error correction L.
        let code = qrcode::QrCode::with_error_correction_level(link, qrcode::EcLevel::L).unwrap();
        assert_eq!(code.error_correction_level(), qrcode::EcLevel::L);
        assert_eq!(w, code.width() + 8);
    }
}

/// The dialog against a real gateway (in process, loopback, relays off)
/// and a fake phone, with keys fed in as a terminal would send them.
#[cfg(test)]
mod dialog_tests {
    use super::*;
    use codesmith_core::remote::gateway::{self, Options};
    use codesmith_core::remote::phone::{self, Phone};
    use codesmith_core::remote::settings::Relay;
    use std::sync::{Arc, Mutex};

    const WAIT: Duration = Duration::from_secs(20);

    /// What the dialog draws, and how far the test has read it.
    #[derive(Clone, Default)]
    struct Screen(Arc<Mutex<(String, usize)>>);

    impl Write for Screen {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap()
                .0
                .push_str(&String::from_utf8_lossy(b));
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Screen {
        /// Wait for `text` after what was read so far, and read past it.
        async fn until(&self, text: &str) {
            let deadline = tokio::time::Instant::now() + WAIT;
            loop {
                {
                    let mut g = self.0.lock().unwrap();
                    let (s, pos) = &mut *g;
                    if let Some(i) = s[*pos..].find(text) {
                        *pos += i + text.len();
                        return;
                    }
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "{text:?} never showed: {}",
                    {
                        let g = self.0.lock().unwrap();
                        g.0[g.1..].to_string()
                    }
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        /// The newest link drawn.
        fn link(&self) -> Option<String> {
            let g = self.0.lock().unwrap();
            let i = g.0.rfind("Link: ")?;
            let rest = &g.0[i + 6..];
            Some(rest[..rest.find('\n')?].trim().to_string())
        }
    }

    struct World {
        _tmp: tempfile::TempDir,
        ready: gateway::Ready,
        keys: mpsc::UnboundedSender<Pressed>,
        screen: Screen,
        dialog: tokio::task::JoinHandle<anyhow::Result<Outcome>>,
        code: Arc<Mutex<Option<String>>>,
    }

    async fn world(offer: Offer, confirm: Duration) -> World {
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let mut o = Options::new(base.join("run"), Relay::None);
        o.key = base.join("remote-key");
        o.db = base.join("remote.db");
        o.config_dir = None;
        o.loopback = true;
        o.online_wait = Duration::ZERO;
        o.confirm_timeout = confirm;
        let dir = o.dir.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(gateway::serve(o, Some(tx)));
        let ready = rx.await.unwrap();
        let mut conn = control::connect(
            &dir,
            control::Request::Pair(control::PairRequest {
                session: offer.session.clone(),
                title: offer.title.clone(),
            }),
        )
        .await
        .unwrap();
        let (keys, mut rx) = mpsc::unbounded_channel();
        let screen = Screen::default();
        let mut out = screen.clone();
        let code = Arc::new(Mutex::new(None));
        let dialog =
            tokio::spawn(async move { run(&mut conn, &offer, &mut rx, &mut out, 100).await });
        World {
            _tmp: tmp,
            ready,
            keys,
            screen,
            dialog,
            code,
        }
    }

    impl World {
        /// The code in the newest link on the screen.
        async fn code(&self) -> String {
            let deadline = tokio::time::Instant::now() + WAIT;
            let link = loop {
                if let Some(l) = self.screen.link() {
                    break l;
                }
                assert!(tokio::time::Instant::now() < deadline, "no link");
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            let c = codesmith_core::remote::link::Link::parse(&link)
                .unwrap()
                .code;
            *self.code.lock().unwrap() = Some(c.clone());
            c
        }

        fn type_(&self, text: &str) {
            let mut b = text.as_bytes().to_vec();
            while !b.is_empty() {
                let k = take_key(&mut b);
                self.keys.send((Instant::now(), k)).unwrap();
            }
        }

        /// A phone claims the live code; the answer is typed once asked.
        async fn claim(&self, phone: &Phone, answers: &[&str]) -> u16 {
            let code = self.code().await;
            let conn = phone
                .connect(
                    &self.ready.node_id,
                    &self.ready.addrs,
                    codesmith_core::remote::PAIR_ALPN,
                )
                .await
                .unwrap();
            let body =
                serde_json::json!({"code": code, "device": {"name": "P", "platform": "ios"}});
            let claim = tokio::spawn(async move { phone::claim(&conn, &body).await.unwrap() });
            self.screen.until("Check code on the phone: ").await;
            for (i, a) in answers.iter().enumerate() {
                self.screen
                    .until(if i == 0 {
                        "Answer: "
                    } else {
                        "Type \"all\" to confirm: "
                    })
                    .await;
                self.type_(a);
            }
            tokio::time::timeout(WAIT, claim)
                .await
                .unwrap()
                .unwrap()
                .status
        }
    }

    fn offer() -> Offer {
        Offer {
            session: Some("ses_1".into()),
            title: Some("t".into()),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_answer_but_yes_refuses_and_yes_pairs() {
        let w = world(offer(), Duration::from_millis(1500)).await;
        let p = Phone::new().await.unwrap();
        for answers in [
            &["n\r"][..],
            &["\r"],
            &["\x1b"],
            &["a\r", "yes\r"],
            &["yy\r"],
            &[],
        ] {
            assert_eq!(w.claim(&p, answers).await, 403, "{answers:?}");
            w.screen.until("Not paired").await;
        }
        assert_eq!(w.claim(&p, &["y\r"]).await, 200);
        match tokio::time::timeout(WAIT, w.dialog)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Outcome::Paired { grant, .. } => assert_eq!(
                grant,
                Some(GrantAnswer {
                    session: "ses_1".into(),
                    scope: Scope::Control
                })
            ),
            o => panic!("{o:?}"),
        }
    }
}
