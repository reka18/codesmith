//! `codesmith bus`: look at the local bus (`peers`, `inbox`), and `relay`,
//! which carries one message from a tailbus bridge to a session shared
//! with `/bus share` and prints its answer. See `docs/bus.md` and
//! `docs/tailbus.md`.

use anyhow::{anyhow, Context, Result};
use codesmith_core::bus::{self, client, Joined, Kind, Member, Role};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

#[derive(clap::Subcommand)]
pub enum BusCmd {
    /// List the sessions on the bus.
    Peers {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the messages waiting for a session (default: the newest
    /// session in this folder).
    Inbox {
        /// The session's id.
        session: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Carry the message on stdin to the session shared as NAME and print
    /// its answer (what a tailbus bridge runs).
    Relay {
        /// The name the session is shared as (`/bus share NAME`).
        #[arg(long)]
        to: String,
        /// Who the message is from, shown to the session (`{SENDER}`).
        #[arg(long, default_value = "")]
        from: String,
        /// The tailbus message id (`{ID}`): a repeat gets the first answer.
        #[arg(long)]
        id: Option<String>,
        /// When the shared session isn't running: refuse, or continue it
        /// in a headless run.
        #[arg(long, value_enum, default_value_t = Offline::Refuse)]
        offline: Offline,
        /// Seconds to wait for the answer.
        #[arg(long, default_value_t = 3600)]
        timeout: u64,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Offline {
    Refuse,
    Run,
}

fn observer() -> Member {
    Member {
        role: Role::Observer,
        session: client::transient_id("observer"),
        name: None,
        title: None,
        project: String::new(),
        agent: String::new(),
        model: String::new(),
        pid: std::process::id(),
        busy: false,
        accepts: false,
    }
}

/// An observer on the bus, or `None` when no broker runs.
async fn look() -> Option<bus::Bus> {
    let b = bus::Bus::join(
        observer(),
        client::Options {
            start: false,
            reconnect: false,
            ..Default::default()
        },
    );
    match b.ready(Duration::from_secs(2)).await {
        Joined::On(_) => Some(b),
        _ => None,
    }
}

pub async fn run(cwd: PathBuf, what: BusCmd) -> Result<()> {
    match what {
        BusCmd::Peers { json } => {
            let peers = match look().await {
                Some(b) => b.peers().await.map_err(|e| anyhow!(e))?,
                None => Vec::new(),
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&peers)?);
            } else if peers.is_empty() {
                println!("No session is on the bus.");
            } else {
                for p in peers {
                    let shared = p
                        .shared
                        .map(|s| format!(" · shared as {s}"))
                        .unwrap_or_default();
                    println!(
                        "{}  {}  {} · {}{}{}{}",
                        p.name,
                        p.project,
                        p.agent,
                        p.model,
                        if p.busy { " · busy" } else { "" },
                        if p.paused { " · paused" } else { "" },
                        shared
                    );
                }
            }
            Ok(())
        }
        BusCmd::Inbox { session, json } => {
            let session = match session {
                Some(s) => s,
                None => {
                    let db = crate::cli::open_store()?;
                    db.latest(&cwd.display().to_string())?
                        .ok_or_else(|| anyhow!("no session in {}", cwd.display()))?
                        .id
                }
            };
            let list = match look().await {
                Some(b) => b.inbox(Some(&session)).await.map_err(|e| anyhow!(e))?,
                None => Vec::new(),
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                println!("{}", codesmith_core::tool::bus::describe_inbox(&list));
            }
            Ok(())
        }
        BusCmd::Relay {
            to,
            from,
            id,
            offline,
            timeout,
        } => {
            relay(
                &to,
                &from,
                id.as_deref(),
                offline,
                Duration::from_secs(timeout),
            )
            .await
        }
    }
}

async fn relay(
    to: &str,
    from: &str,
    key: Option<&str>,
    offline: Offline,
    timeout: Duration,
) -> Result<()> {
    let mut body = String::new();
    std::io::stdin()
        .take(bus::protocol::MAX_BODY as u64 + 1)
        .read_to_string(&mut body)
        .context("reading the message on stdin")?;
    if body.len() > bus::protocol::MAX_BODY {
        return Err(anyhow!(
            "the message is over {} KiB, the most the bus carries",
            bus::protocol::MAX_BODY / 1024
        ));
    }
    if body.trim().is_empty() {
        return Err(anyhow!("no message on stdin"));
    }
    // The broker and any headless run are the user's own, whatever HOME a
    // bridge set.
    let home = bus::user_home().ok_or_else(|| anyhow!("can't find your home folder"))?;
    let member = Member {
        role: Role::Relay,
        session: client::transient_id("relay"),
        name: None,
        title: None,
        project: String::new(),
        agent: String::new(),
        model: String::new(),
        pid: std::process::id(),
        busy: false,
        accepts: false,
    };
    let b = bus::Bus::join(
        member,
        client::Options {
            home: Some(home.clone()),
            ..Default::default()
        },
    );
    match b.ready(Duration::from_secs(10)).await {
        Joined::On(_) => {}
        Joined::Off(why) => return Err(anyhow!("couldn't reach the bus: {why}")),
        Joined::Connecting => return Err(anyhow!("couldn't reach the bus: it didn't answer")),
    }
    let via = if from.trim().is_empty() {
        "tailbus".to_string()
    } else {
        from.trim().to_string()
    };
    let r = b
        .relay(to, &body, &via, key)
        .await
        .map_err(|e| anyhow!(e))?;
    if !r.live {
        return match offline {
            Offline::Refuse => Err(anyhow!(
                "{to} isn't running: open its session ({}) in CodeSmith, or relay with --offline run",
                r.session
            )),
            Offline::Run => headless(&home, &r.env, &r.session, &r.project, &via, &body).await,
        };
    }
    let id =
        r.id.ok_or_else(|| anyhow!("the bus took the message without an id"))?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(anyhow!(
                "{to} didn't answer within {} seconds (request {id})",
                timeout.as_secs()
            ));
        }
        match b.wait(left, Some(&id)).await {
            Some(m) if m.kind == Kind::Response => {
                print!("{}", m.body);
                if !m.body.ends_with('\n') {
                    println!();
                }
                return Ok(());
            }
            Some(m) => eprintln!("{to}: {}", m.body.lines().next().unwrap_or("")),
            None => {}
        }
    }
}

/// `--offline run`: continue the shared session in a headless run, as the
/// user (their home), with the message framed as another session's.
async fn headless(
    home: &std::path::Path,
    env: &std::collections::BTreeMap<String, String>,
    session: &str,
    project: &str,
    via: &str,
    body: &str,
) -> Result<()> {
    let exe = std::env::current_exe()?;
    let dir = if project.is_empty() || !std::path::Path::new(project).is_dir() {
        home.to_path_buf()
    } else {
        PathBuf::from(project)
    };
    let framed = bus::framed(&bus::Message {
        id: bus::protocol::new_id(),
        kind: Kind::Request,
        from: String::new(),
        from_name: via.to_string(),
        to: session.to_string(),
        to_name: String::new(),
        reply_to: None,
        body: body.to_string(),
        sent: bus::protocol::now_ms(),
        hop: 0,
        project: String::new(),
        via: Some("tailbus".into()),
    })
    .replace(
        "answer it once with bus_send (kind \"response\", reply_to",
        "your final answer is the response (ignore reply_to",
    );
    let mut cmd = tokio::process::Command::new(exe);
    // The session started in this folder with someone there to confirm
    // it; nobody is now, so the folder warning doesn't ask again.
    cmd.args(["run", "--any-folder", "--session", session])
        .current_dir(&dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    // Where the sharing process kept its config, keys and sessions; the
    // user's home for what it didn't set. Never the bridge's.
    let env = bus::protocol::share_env(env);
    for k in bus::protocol::SHARE_ENV {
        match env.get(k) {
            Some(v) => {
                cmd.env(k, v);
            }
            None if k == "HOME" => {
                cmd.env("HOME", home);
            }
            None => {
                cmd.env_remove(k);
            }
        }
    }
    let mut child = cmd.spawn().context("starting a headless run")?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        stdin.write_all(framed.as_bytes()).await?;
    }
    let status = child.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("the headless run failed ({status})"))
    }
}
