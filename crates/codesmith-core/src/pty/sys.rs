//! The operating-system side of a terminal: open a pseudo-terminal, start
//! a program on it as the leader of a new session, resize it, write to it,
//! and stop everything the program started.
//!
//! The program gets the terminal's slave side as its controlling terminal
//! (`setsid` and `TIOCSCTTY` before `exec`), so it and everything it starts
//! share one session; its process group is its pid. Stopping a terminal
//! signals that group and the terminal's foreground group (where an
//! interactive shell runs its jobs), first with `SIGHUP` (a shell passes it
//! on to its jobs), then with `SIGKILL`.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// A program running on a new pseudo-terminal.
pub(super) struct Spawned {
    pub master: OwnedFd,
    pub child: Child,
}

/// How long a write may wait for the program to make room before the rest
/// of the input is dropped.
const WRITE_WAIT: Duration = Duration::from_secs(2);

fn check(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn set_flags(fd: RawFd, nonblocking: bool) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor this process owns; no pointers.
    unsafe {
        let fl = check(libc::fcntl(fd, libc::F_GETFD))?;
        check(libc::fcntl(fd, libc::F_SETFD, fl | libc::FD_CLOEXEC))?;
        if nonblocking {
            let fl = check(libc::fcntl(fd, libc::F_GETFL))?;
            check(libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK))?;
        }
    }
    Ok(())
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Start `program args` in `cwd` with exactly `env`, on a new terminal of
/// `cols` × `rows`.
pub(super) fn spawn(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    cols: u16,
    rows: u16,
) -> io::Result<Spawned> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    // openpty is not thread-safe everywhere (macOS's uses ptsname's static
    // buffer, so two at once can open the same slave), and until the flags
    // below are set its descriptors would leak into a concurrent spawn: one
    // terminal is opened and started at a time.
    static OPENING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = OPENING.lock().unwrap_or_else(|e| e.into_inner());
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut ws = winsize(cols, rows);
    // SAFETY: openpty writes the two descriptors; the name and termios
    // pointers may be null, and `ws` outlives the call.
    check(unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // `*const` on Linux, `*mut` on macOS.
            std::ptr::addr_of_mut!(ws) as _,
        )
    })?;
    // SAFETY: openpty just returned these descriptors; nothing else owns them.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // Neither side may leak into other programs this process starts.
    set_flags(master.as_raw_fd(), true)?;
    set_flags(slave.as_raw_fd(), false)?;

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: between fork and exec only async-signal-safe calls: setsid
    // and ioctl on the child's own stdin (the slave side).
    unsafe {
        cmd.pre_exec(|| {
            check(libc::setsid())?;
            check(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    // `cmd` holds the parent's copies of the slave side; close them so the
    // program's exit is seen on the master.
    drop(cmd);
    Ok(Spawned { master, child })
}

/// Tell the terminal its new size (the kernel sends the foreground group
/// `SIGWINCH`).
pub(super) fn resize(master: &OwnedFd, cols: u16, rows: u16) -> io::Result<()> {
    let ws = winsize(cols, rows);
    // SAFETY: TIOCSWINSZ reads a winsize that outlives the call.
    check(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) })?;
    Ok(())
}

/// Write all of `data`, waiting up to [`WRITE_WAIT`] in all for the
/// program to read; what does not fit by then is dropped.
pub(super) fn write_all(master: &OwnedFd, mut data: &[u8]) -> io::Result<()> {
    let fd = master.as_raw_fd();
    let until = Instant::now() + WRITE_WAIT;
    while !data.is_empty() {
        // SAFETY: writes from a live slice into a descriptor we own.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n >= 0 {
            data = &data[n as usize..];
            continue;
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the terminal is not reading its input",
                    ));
                }
                poll(fd, libc::POLLOUT, left.min(Duration::from_millis(50)));
            }
            _ => return Err(e),
        }
    }
    Ok(())
}

/// Wait up to `wait` for `events` on `fd`; the events that came.
pub(super) fn poll(fd: RawFd, events: libc::c_short, wait: Duration) -> libc::c_short {
    let mut p = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let ms = wait.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: one pollfd that outlives the call.
    let n = unsafe { libc::poll(&mut p, 1, ms) };
    if n > 0 {
        p.revents
    } else {
        0
    }
}

/// What one read of the master gave.
pub(super) enum Read {
    Data(usize),
    /// Nothing yet.
    Empty,
    /// Every holder of the slave side closed it.
    Closed,
}

/// Wait up to `wait` for output and read what there is into `buf`.
pub(super) fn read(master: &OwnedFd, buf: &mut [u8], wait: Duration) -> Read {
    let fd = master.as_raw_fd();
    let ev = poll(fd, libc::POLLIN, wait);
    if ev == 0 {
        return Read::Empty;
    }
    // SAFETY: reads into a live, writable buffer of its length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n > 0 {
        return Read::Data(n as usize);
    }
    if n == 0 {
        return Read::Closed;
    }
    match io::Error::last_os_error().kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Read::Empty,
        // EIO: the slave side is closed.
        _ => Read::Closed,
    }
}

/// The groups to signal to stop a terminal started as `pid`: its own and
/// the terminal's foreground group, never this process's.
fn groups(master: &OwnedFd, pid: u32) -> Vec<libc::pid_t> {
    let mut out = Vec::new();
    // SAFETY: getpgrp and tcgetpgrp take no pointers.
    let (ours, fg) = unsafe { (libc::getpgrp(), libc::tcgetpgrp(master.as_raw_fd())) };
    for g in [libc::pid_t::try_from(pid).unwrap_or(0), fg] {
        if g > 1 && g != ours && !out.contains(&g) {
            out.push(g);
        }
    }
    out
}

fn signal(groups: &[libc::pid_t], sig: libc::c_int) {
    for &g in groups {
        // SAFETY: kill(2) with a negative pid signals a group; no pointers.
        unsafe { libc::kill(-g, sig) };
    }
}

/// Kill whatever is left of the terminal's groups at once (its program has
/// exited, or is being stopped after its grace).
pub(super) fn kill_groups(master: &OwnedFd, pid: u32) {
    signal(&groups(master, pid), libc::SIGKILL);
}

/// Stop a running terminal: `SIGHUP` to its groups, up to `grace` for the
/// program to exit, then `SIGKILL`. Returns once the program is reaped (or
/// after a further second if it cannot be).
pub(super) fn stop(master: &OwnedFd, child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    let pid = child.id();
    let gs = groups(master, pid);
    signal(&gs, libc::SIGHUP);
    let until = Instant::now() + grace;
    loop {
        if let Ok(Some(s)) = child.try_wait() {
            signal(&gs, libc::SIGKILL);
            return Some(s);
        }
        if Instant::now() >= until {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    signal(&gs, libc::SIGKILL);
    let _ = child.kill();
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        if let Ok(Some(s)) = child.try_wait() {
            return Some(s);
        }
        if Instant::now() >= until {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The exit code OpenCode reports: the program's, or 128 + the signal
/// that ended it.
pub(super) fn exit_code(s: ExitStatus) -> u32 {
    if let Some(c) = s.code() {
        return c as u32 & 0xff;
    }
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = s.signal() {
            return 128 + sig as u32;
        }
    }
    1
}
