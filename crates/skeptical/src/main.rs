//! `skeptical`: a terminal coding agent (a Rust port of OpenCode).

mod render;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use skeptical_core::permission::{Asker, Fixed, Reply, Request};
use skeptical_core::session::{Options, Session};
use skeptical_core::store::{self, Store};
use skeptical_core::{agent, config, instructions, paths, prompt};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "skeptical", version, about = "A terminal coding agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Print logs to stderr.
    #[arg(long, global = true)]
    print_logs: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run one prompt non-interactively and print the result.
    Run(RunArgs),
    /// List configured providers and models.
    Models,
    /// Manage stored sessions.
    Session {
        #[command(subcommand)]
        what: SessionCmd,
    },
    /// Inspect configuration and internals.
    Debug {
        #[command(subcommand)]
        what: Debug,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    /// The message to send (piped stdin is appended).
    message: Vec<String>,
    /// Model as provider/model.
    #[arg(short, long)]
    model: Option<String>,
    /// Agent to use (default: build).
    #[arg(long)]
    agent: Option<String>,
    /// Attach a file's contents (repeatable).
    #[arg(short, long = "file")]
    files: Vec<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Default)]
    format: Format,
    /// Approve every permission request that isn't denied.
    #[arg(long)]
    auto: bool,
    /// Show the model's reasoning.
    #[arg(long)]
    thinking: bool,
    /// Continue the most recent session in this directory.
    #[arg(short, long)]
    r#continue: bool,
    /// Continue the session with this id.
    #[arg(short, long)]
    session: Option<String>,
    /// Title for a new session (otherwise one is generated).
    #[arg(long)]
    title: Option<String>,
}

#[derive(Subcommand)]
enum SessionCmd {
    /// List sessions, newest first.
    List {
        /// Include sessions from every directory.
        #[arg(long)]
        all: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Delete a session (and its subagent sessions).
    Delete { id: String },
    /// Print a session as JSON.
    Export { id: String },
    /// Import a session exported as JSON (a file path, or - for stdin).
    Import { file: String },
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Format {
    Default,
    Json,
}

#[derive(Subcommand)]
enum Debug {
    /// Show where skeptical reads and writes files.
    Paths,
    /// Show the merged config and the files it came from.
    Config,
    /// List agents and their permission rules.
    Agents,
    /// Show what a model resolves to: provider, URL, limits, extra body.
    Model {
        /// provider/model (default: the configured model).
        spec: Option<String>,
    },
    /// Print the system prompt the build agent would get here.
    Prompt {
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long)]
        agent: Option<String>,
    },
}

/// Asks on the controlling terminal.
struct Terminal;

impl Asker for Terminal {
    fn ask(&self, req: &Request) -> Reply {
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty");
        let Ok(mut tty) = tty else {
            return Reply::Reject(Some("No terminal is available to approve this request. Re-run with --auto to approve requests automatically.".into()));
        };
        let _ = write!(
            tty,
            "\n\x1b[33mPermission required\x1b[0m ({}): {}\n  [y] allow once   [a] always allow   [n] reject   (or type feedback to reject with it)\n> ",
            req.action, req.title
        );
        let _ = tty.flush();
        let mut line = String::new();
        let mut buf = [0u8; 1];
        while let Ok(1) = tty.read(&mut buf) {
            if buf[0] == b'\n' {
                break;
            }
            line.push(buf[0] as char);
        }
        match line.trim() {
            "y" | "Y" | "yes" | "" => Reply::Once,
            "a" | "A" | "always" => Reply::Always,
            "n" | "N" | "no" => Reply::Reject(None),
            feedback => Reply::Reject(Some(feedback.to_string())),
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match run(cli).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("\x1b[31merror:\x1b[0m {e:#}");
            1
        }
    };
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    match cli.command {
        None => {
            eprintln!("The interactive terminal UI is not built yet. Use `skeptical run \"your message\"`.");
            Ok(())
        }
        Some(Command::Run(args)) => run_prompt(cwd, args).await,
        Some(Command::Models) => {
            let loaded = config::load(&cwd)?;
            let c = &loaded.config;
            let mut ids: Vec<String> = vec!["anthropic".into(), "openai".into()];
            ids.extend(
                c.providers
                    .keys()
                    .filter(|k| !ids.contains(k))
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            for id in ids {
                let p = c.provider(&id).unwrap_or_default();
                if p.models.is_empty() {
                    println!("{id}/*");
                }
                for m in p.models.keys() {
                    println!("{id}/{m}");
                }
            }
            if let Some(m) = &c.model {
                println!("\ndefault: {m}");
            }
            Ok(())
        }
        Some(Command::Session { what }) => session_cmd(cwd, what),
        Some(Command::Debug { what }) => debug(cwd, what),
    }
}

fn debug(cwd: PathBuf, what: Debug) -> Result<()> {
    match what {
        Debug::Paths => {
            println!("config  {}", paths::config_dir().display());
            println!("data    {}", paths::data_dir().display());
            println!("output  {}", paths::tool_output_dir().display());
            println!("tmp     {}", paths::tmp_dir().display());
        }
        Debug::Config => {
            let loaded = config::load(&cwd)?;
            println!("root: {}", loaded.root.display());
            for f in &loaded.files {
                println!("file: {}", f.display());
            }
            if loaded.files.is_empty() {
                println!("(no config files found)");
            }
            let c = &loaded.config;
            println!("model: {}", c.model.as_deref().unwrap_or("(none)"));
            println!(
                "providers: {}",
                c.providers.keys().cloned().collect::<Vec<_>>().join(", ")
            );
            println!("permissions: {}", serde_json::to_string(&c.permissions)?);
        }
        Debug::Model { spec } => {
            let loaded = config::load(&cwd)?;
            let spec = spec
                .or(loaded.config.model.clone())
                .ok_or_else(|| anyhow!("no model given or configured"))?;
            let m = loaded.config.resolve_model(&spec)?;
            println!(
                "model:    {}/{} (sent as {})",
                m.provider_id,
                spec.split_once('/').map(|x| x.1).unwrap_or(""),
                m.model_id
            );
            println!("protocol: {:?}", m.provider.protocol);
            println!("url:      {}", m.provider.base_url);
            println!(
                "api key:  {}",
                if m.provider.api_key.is_some() {
                    "set"
                } else {
                    "none"
                }
            );
            println!(
                "context:  {}",
                m.context
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown (no automatic compaction)".into())
            );
            if let Some(c) = m.context {
                println!(
                    "compacts above ~{} prompt tokens",
                    skeptical_core::compaction::ceiling(c)
                );
            }
            println!(
                "output:   {}",
                m.max_output
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "provider default".into())
            );
            println!("body:     {}", serde_json::Value::Object(m.body.clone()));
        }
        Debug::Agents => {
            let loaded = config::load(&cwd)?;
            for id in agent::list(&loaded.config) {
                let a = agent::resolve(&loaded.config, &id).unwrap();
                let tools: Vec<&str> = agent::tools(&a).iter().map(|t| t.name()).collect();
                println!("{id} ({}): {}", a.mode, a.description);
                println!("  tools: {}", tools.join(", "));
                println!("  rules: {}", serde_json::to_string(&a.permissions)?);
            }
        }
        Debug::Prompt {
            model,
            agent: agent_id,
        } => {
            let loaded = config::load(&cwd)?;
            let id = agent_id.unwrap_or_else(|| "build".into());
            let a = agent::resolve(&loaded.config, &id)
                .ok_or_else(|| anyhow!("unknown agent {id:?}"))?;
            let model_id = model
                .or(loaded.config.model.clone())
                .unwrap_or_else(|| "none/none".into());
            let mid = model_id
                .split_once('/')
                .map(|x| x.1.to_string())
                .unwrap_or(model_id);
            let tools: Vec<&str> = agent::tools(&a).iter().map(|t| t.name()).collect();
            let env = prompt::environment(&prompt::Environment {
                cwd: &cwd,
                root: &loaded.root,
                is_git: loaded.root.join(".git").exists(),
                tmp: &paths::tmp_dir(),
                date: prompt::today(),
            });
            let files = instructions::discover(
                &cwd,
                &loaded.root,
                &paths::config_dir(),
                &loaded.config.instructions,
            );
            println!(
                "{}",
                prompt::assemble(
                    a.system.as_deref(),
                    &mid,
                    &tools,
                    &env,
                    &instructions::render(&files)
                )
            );
        }
    }
    Ok(())
}

fn open_store() -> Result<Store> {
    let path = store::default_path();
    Store::open(&path).with_context(|| format!("session store {}", path.display()))
}

fn session_cmd(cwd: PathBuf, what: SessionCmd) -> Result<()> {
    let mut db = open_store()?;
    match what {
        SessionCmd::List { all, json } => {
            let dir = cwd.display().to_string();
            let list = db.list(if all { None } else { Some(&dir) }, false)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            if list.is_empty() {
                println!(
                    "No sessions{}.",
                    if all {
                        ""
                    } else {
                        " in this directory (use --all for every directory)"
                    }
                );
            }
            for s in list {
                let when = chrono_like(s.updated);
                let title = s.title.as_deref().unwrap_or("(untitled)");
                if all {
                    println!("{}  {when}  {title}  \x1b[2m{}\x1b[0m", s.id, s.directory);
                } else {
                    println!("{}  {when}  {title}", s.id);
                }
            }
        }
        SessionCmd::Delete { id } => {
            if db.delete(&id)? {
                println!("Deleted {id}");
            } else {
                return Err(anyhow!("no session {id}"));
            }
        }
        SessionCmd::Export { id } => {
            println!("{}", serde_json::to_string_pretty(&db.export(&id)?)?)
        }
        SessionCmd::Import { file } => {
            let text = if file == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else {
                std::fs::read_to_string(&file).with_context(|| format!("reading {file}"))?
            };
            let e: store::Export =
                serde_json::from_str(&text).context("not a skeptical session export")?;
            db.import(&e)?;
            println!("Imported {}", e.info.id);
        }
    }
    Ok(())
}

/// "YYYY-MM-DD HH:MM" in local time for a millisecond timestamp.
fn chrono_like(ms: i64) -> String {
    let secs = ms / 1000;
    let out = std::process::Command::new("/bin/date")
        .arg("-r")
        .arg(secs.to_string())
        .arg("+%Y-%m-%d %H:%M")
        .output();
    out.ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| secs.to_string())
}

async fn run_prompt(cwd: PathBuf, args: RunArgs) -> Result<()> {
    let db = match open_store() {
        Ok(s) => Some(s),
        Err(e) => {
            if args.r#continue || args.session.is_some() {
                return Err(e);
            }
            eprintln!("\x1b[33mwarning:\x1b[0m sessions will not be saved: {e:#}");
            None
        }
    };
    let resume = match (&args.session, args.r#continue, &db) {
        (Some(id), _, _) => Some(id.clone()),
        (None, true, Some(s)) => Some(
            s.latest(&cwd.display().to_string())?
                .ok_or_else(|| anyhow!("no session to continue in {}", cwd.display()))?
                .id,
        ),
        _ => None,
    };
    let mut message = args.message.join(" ");
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped)?;
        if !piped.trim().is_empty() {
            if !message.is_empty() {
                message.push_str("\n\n");
            }
            message.push_str(piped.trim_end());
        }
    }
    for f in &args.files {
        let text =
            std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        message.push_str(&format!(
            "\n\n<file path=\"{}\">\n{}\n</file>",
            f.display(),
            text.trim_end()
        ));
    }
    if message.trim().is_empty() {
        return Err(anyhow!(
            "no message: pass one as an argument or pipe it on stdin"
        ));
    }
    let loaded = config::load(&cwd)?;
    let asker: Arc<dyn Asker> = if args.auto {
        Arc::new(Fixed(Reply::Once))
    } else {
        Arc::new(Terminal)
    };
    let mut session = Session::new(
        &loaded,
        cwd,
        Options {
            agent: args.agent,
            model: args.model,
            resume,
            title: args.title,
            parent: None,
        },
        asker,
        db,
    )?;
    let json = args.format == Format::Json;
    let mut sink = render::Printer::new(json, args.thinking);
    if !json {
        eprintln!(
            "\x1b[2m> {} · {}/{} · {}\x1b[0m",
            session.agent.name, session.model.provider_id, session.model.model_id, session.id
        );
    }
    let result = session.prompt(&message, &mut sink).await;
    sink.done(
        &session.usage,
        result.as_ref().err().map(|e| e.to_string()),
        &session.id,
        session.title.as_deref(),
    );
    result
}
