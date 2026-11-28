//! `codesmith remote`: pair a phone, list and revoke devices, and start,
//! stop or look at the gateway. See `docs/remote.md`.
//!
//! Only `pair` and `start` (the opt-ins) start the gateway or touch the
//! network; `id`, `status`, `devices` and `revoke` never do.

use crate::pair_tty::{self, Offer, Outcome, Tty};
use anyhow::{anyhow, Context, Result};
use codesmith_core::remote::{self, control, key, settings, store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(clap::Subcommand)]
pub enum RemoteCmd {
    /// Pair a phone: show a QR code for it to scan, then confirm here.
    /// Turns remote access on.
    Pair {
        /// The session to offer (its id); without one the phone is paired
        /// with no session.
        #[arg(long)]
        session: Option<String>,
        /// Offer every session on this machine, now and future.
        #[arg(long)]
        all: bool,
        /// Offer watching only.
        #[arg(long)]
        watch: bool,
        /// The session's title to show (as `/mobile` passes it), instead
        /// of reading it from the session store.
        #[arg(long, hide = true, requires = "session")]
        title: Option<String>,
    },
    /// List the paired devices.
    Devices {
        /// Include revoked devices.
        #[arg(long)]
        all: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Revoke a device: it's no longer paired and its connections close.
    Revoke {
        /// The device: its id, a prefix of it (4 characters or more), or
        /// its name.
        device: String,
    },
    /// Start the gateway, turning remote access on.
    Start,
    /// Stop the gateway.
    Stop,
    /// Print this machine's node id.
    Id,
    /// Whether the gateway runs, and what it's doing.
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(what: RemoteCmd) -> Result<()> {
    match what {
        RemoteCmd::Pair {
            session,
            all,
            watch,
            title,
        } => pair(session, title, all, watch).await,
        RemoteCmd::Devices { all, json } => devices(all, json),
        RemoteCmd::Revoke { device } => revoke(&device).await,
        RemoteCmd::Start => start().await,
        RemoteCmd::Stop => stop().await,
        RemoteCmd::Id => {
            let k = key::load_or_create(&remote::key_path())?;
            println!("{}", key::node_id(&k));
            Ok(())
        }
        RemoteCmd::Status { json } => status(json).await,
    }
}

fn launch() -> Result<control::Launch> {
    Ok(control::Launch {
        program: std::env::current_exe().context("finding this program")?,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// Turn `remote.enabled` on in the global config, saying so when it
/// changes; the settings after.
fn opt_in() -> Result<settings::Settings> {
    let dir = codesmith_core::paths::config_dir();
    let s = settings::load_from(&dir).map_err(|e| anyhow!(e))?;
    if codesmith_core::config::global_disabled() {
        anyhow::bail!(
            "remote access needs your global config, which $CODESMITH_DISABLE_GLOBAL_CONFIG turns off"
        );
    }
    if !s.enabled {
        let path = settings::save_enabled(&dir, true)?;
        eprintln!(
            "Remote access is on (\"remote\": {{\"enabled\": true}} in {}).",
            path.display()
        );
    }
    Ok(settings::Settings { enabled: true, ..s })
}

async fn ensure() -> Result<(PathBuf, control::Registration)> {
    let dir = remote::dir()?;
    let reg = control::ensure(&dir, &launch()?).await?;
    Ok((dir, reg))
}

async fn start() -> Result<()> {
    opt_in()?;
    let (dir, reg) = ensure().await?;
    println!("The gateway is running (pid {}).", reg.pid);
    println!("Node id: {}", reg.node_id);
    if let Some(s) = control::status(&dir).await {
        println!("Relay: {}", s.home_relay.unwrap_or(s.relay));
    }
    println!("It runs while a device is paired; pair one with `codesmith remote pair`.");
    Ok(())
}

async fn stop() -> Result<()> {
    let dir = remote::dir_path()?;
    if control::status(&dir).await.is_none() {
        println!("The gateway isn't running.");
        return Ok(());
    }
    match control::call(&dir, control::Request::Stop { version: None }).await? {
        control::Frame::Ok => {
            println!("The gateway stopped.");
            Ok(())
        }
        control::Frame::Error { message } => Err(anyhow!(message)),
        f => Err(anyhow!("the gateway said {f:?}")),
    }
}

async fn status(json: bool) -> Result<()> {
    let dir = remote::dir_path()?;
    let s = settings::load();
    let running = control::status(&dir).await;
    let devices = paired_devices()?.len();
    if json {
        let v = serde_json::json!({
            "enabled": s.as_ref().map(|s| s.enabled).unwrap_or(false),
            "settings_error": s.as_ref().err(),
            "running": running.is_some(),
            "gateway": running,
            "devices": devices,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    match &s {
        Ok(s) => println!(
            "Remote access: {}",
            if s.enabled { "on" } else { "off (the default)" }
        ),
        Err(e) => println!("Remote access: the settings are wrong: {e}"),
    }
    match running {
        Some(g) => {
            println!("Gateway: running (pid {}, CodeSmith {})", g.pid, g.version);
            println!("Node id: {}", g.node_id);
            match g.home_relay {
                Some(r) => println!("Relay: {r} (connected)"),
                None => println!("Relay: {} (not connected)", g.relay),
            }
            if !g.addrs.is_empty() {
                println!("Direct addresses: {}", g.addrs.join(", "));
            }
            println!("Connections: {}", g.connections);
            if g.pairing {
                println!("A pairing dialog is open.");
            }
        }
        None => println!("Gateway: not running"),
    }
    println!("Paired devices: {devices}");
    Ok(())
}

/// The paired devices, without creating `remote.db` when there is none.
fn paired_devices() -> Result<Vec<store::Device>> {
    let p = remote::db_path();
    if !p.exists() {
        return Ok(vec![]);
    }
    store::Store::open(&p)?.devices(false)
}

fn devices(all: bool, json: bool) -> Result<()> {
    let p = remote::db_path();
    let list = if p.exists() {
        store::Store::open(&p)?.devices(all)?
    } else {
        vec![]
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    if list.is_empty() {
        println!("No devices are paired. Pair one with `codesmith remote pair` or /mobile.");
        return Ok(());
    }
    for d in list {
        let when = chrono::DateTime::from_timestamp_millis(d.paired)
            .map(|t| t.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let state = match d.revoked {
            Some(_) => " (revoked)",
            None => "",
        };
        println!(
            "{}  {} ({}), paired {when}{state}",
            key::short(&d.id),
            d.name,
            d.platform
        );
        for g in d.grants {
            let what = if g.session == store::ALL_SESSIONS {
                "every session".to_string()
            } else {
                g.session
            };
            println!("    {}: {what}", g.scope.as_str());
        }
        if d.unlock_key.is_some() {
            println!("    unlock passkey registered");
        }
    }
    Ok(())
}

async fn revoke(query: &str) -> Result<()> {
    let dir = remote::dir_path()?;
    if control::status(&dir).await.is_some() {
        // The gateway revokes it and closes its connections.
        return match control::call(
            &dir,
            control::Request::Revoke {
                device: query.to_string(),
            },
        )
        .await?
        {
            control::Frame::Ok => {
                println!("Revoked. Its connections are closed.");
                Ok(())
            }
            control::Frame::Error { message } => Err(anyhow!(message)),
            f => Err(anyhow!("the gateway said {f:?}")),
        };
    }
    let p = remote::db_path();
    if !p.exists() {
        anyhow::bail!("no paired device is {query:?}");
    }
    let s = store::Store::open(&p)?;
    let d = s.find(query)?;
    s.revoke(&d.id)?;
    s.audit(Some(&d.id), "revoked", None, None)?;
    println!("Revoked {} ({}).", d.name, key::short(&d.id));
    Ok(())
}

/// The title of session `id`, from the session store.
fn session_title(id: &str) -> Result<Option<String>> {
    let s = crate::cli::open_store()?;
    match s.get(id)? {
        Some(info) => Ok(info.title),
        None => Err(anyhow!("there's no session {id}")),
    }
}

async fn pair(
    session: Option<String>,
    title: Option<String>,
    all: bool,
    watch: bool,
) -> Result<()> {
    // The terminal first: with none, nothing starts (P10).
    let mut tty = Tty::open().map_err(|_| {
        anyhow!(
            "pairing needs you at a terminal: its confirmation is read from the terminal, never from piped input. Run `codesmith remote pair` in one."
        )
    })?;
    let title = match (&session, title) {
        (_, Some(t)) => Some(t),
        (Some(id), None) => session_title(id)?,
        (None, None) => None,
    };
    opt_in()?;
    let (dir, _) = ensure().await?;
    let mut conn = control::connect(
        &dir,
        control::Request::Pair(control::PairRequest {
            session: session.clone(),
            title: title.clone(),
        }),
    )
    .await?;
    let mut out = tty.writer()?;
    let cols = pair_tty::tty_width(&out);
    tty.raw()?;
    tty.flush_input();
    let stop = Arc::new(AtomicBool::new(false));
    let (mut keys, reader) = pair_tty::key_thread(tty, stop.clone());
    let offer = Offer {
        session,
        title,
        all,
        watch,
    };
    let r = pair_tty::run(&mut conn, &offer, &mut keys, &mut out, cols).await;
    stop.store(true, Ordering::SeqCst);
    drop(keys);
    use std::io::Write;
    let _ = write!(out, "\r\x1b[2K\x1b[0m");
    let _ = out.flush();
    // The key thread puts the terminal back as it ends.
    let _ = reader.join();
    match r? {
        Outcome::Paired { name, grant } => {
            let what = match grant {
                Some(g) if g.session == store::ALL_SESSIONS => {
                    format!("{} every session", g.scope.as_str())
                }
                Some(g) => format!("{} {}", g.scope.as_str(), g.session),
                None => "no session yet".into(),
            };
            let msg = format!("Paired \"{}\": {what}.", remote::link::printable(&name));
            let _ = writeln!(out, "\x1b[2J\x1b[H{msg}");
            if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                println!("{msg}");
            }
        }
        Outcome::Closed => {
            let _ = writeln!(out, "\x1b[2J\x1b[HPairing closed.");
        }
        Outcome::Replaced => {
            let _ = writeln!(
                out,
                "\x1b[2J\x1b[HPairing moved to another window (only one can be open)."
            );
        }
    }
    Ok(())
}
