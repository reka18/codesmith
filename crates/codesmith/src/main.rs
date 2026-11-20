//! `codesmith`: a terminal coding agent (a Rust port of OpenCode).

mod render;
mod serve;
mod tui;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use codesmith_core::permission::{Asker, Fixed, Reply, Request};
use codesmith_core::session::{self, Options, Session, Sink};
use codesmith_core::snapshot::Snapshots;
use codesmith_core::store::{self, Store};
use codesmith_core::{agent, command, config, instructions, paths, prompt};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "codesmith", version, about = "A terminal coding agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Print logs to stderr.
    #[arg(long, global = true)]
    print_logs: bool,
    /// Start in a new git worktree (optionally with this name) (interactive UI).
    #[arg(short = 'w', long, num_args = 0..=1, default_missing_value = "", value_name = "NAME")]
    worktree: Option<String>,
    /// Continue the most recent session in this directory (interactive UI).
    #[arg(short, long)]
    r#continue: bool,
    /// Continue the session with this id (interactive UI).
    #[arg(short, long)]
    session: Option<String>,
    /// Model as provider/model (interactive UI).
    #[arg(short, long)]
    model: Option<String>,
    /// Agent to start with (interactive UI).
    #[arg(long)]
    agent: Option<String>,
    /// Send this prompt as soon as the UI opens.
    #[arg(long)]
    prompt: Option<String>,
    /// Approve every permission request that isn't denied (interactive UI).
    #[arg(long)]
    auto: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run one prompt non-interactively and print the result.
    Run(RunArgs),
    /// List providers and configured models, or ask one provider for its models.
    Models {
        /// A provider id: list the models its API reports.
        provider: Option<String>,
    },
    /// Review changes with parallel per-dimension reviewers and independent verification.
    Review {
        /// Commit, range (a..b), branch, or pull request (#12); default: uncommitted changes.
        target: Vec<String>,
        /// Post the review to the pull request.
        #[arg(long)]
        comment: bool,
        /// Model for this review, as provider/model.
        #[arg(short, long)]
        model: Option<String>,
    },
    /// Survey the codebase and choose the dimensions `review` uses.
    Calibrate {
        #[arg(short, long)]
        model: Option<String>,
    },
    /// List, create or remove git worktrees for isolated sessions.
    Worktree {
        #[command(subcommand)]
        what: WorktreeCmd,
    },
    /// Serve a local HTTP API for driving sessions from other programs.
    Serve {
        /// Address to listen on. Anything but loopback requires --token.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 4096)]
        port: u16,
        /// Require `Authorization: Bearer <token>` (also $CODESMITH_SERVER_TOKEN).
        #[arg(long, env = "CODESMITH_SERVER_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Approve every permission request that isn't denied instead of asking the client.
        #[arg(long)]
        auto: bool,
    },
    /// Save, list or remove API keys stored on this machine.
    Auth {
        #[command(subcommand)]
        what: AuthCmd,
    },
    /// List commands available as `codesmith run /name ...`.
    Commands,
    /// Show MCP servers and their tools.
    Mcp {
        #[command(subcommand)]
        what: McpCmd,
    },
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
    /// Run in a new git worktree, optionally with this name.
    #[arg(short = 'w', long, num_args = 0..=1, default_missing_value = "", value_name = "NAME")]
    worktree: Option<String>,
}

#[derive(Subcommand)]
enum WorktreeCmd {
    /// List this repository's worktrees.
    List,
    /// Create a worktree and print its path.
    Create {
        name: Option<String>,
        /// Commit or branch to start from (default HEAD).
        #[arg(long)]
        from: Option<String>,
        /// Create this new branch in it instead of a detached HEAD.
        #[arg(short, long)]
        branch: Option<String>,
    },
    /// Remove a worktree by name or path.
    Remove {
        name: String,
        /// Remove even with uncommitted changes.
        #[arg(long)]
        force: bool,
    },
}

/// Create a worktree for `--worktree` and return the folder to work in.
fn enter_worktree(cwd: &std::path::Path, name: &str) -> Result<PathBuf> {
    let loaded = config::load(cwd)?;
    let opts = codesmith_core::worktree::Create {
        name: (!name.is_empty()).then(|| name.to_string()),
        ..Default::default()
    };
    let (wt, sub) = codesmith_core::worktree::create(&loaded.config.worktree, cwd, &opts)?;
    eprintln!("\x1b[2mworking in new worktree {}\x1b[0m", wt.display());
    Ok(sub)
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Save an API key for a provider (read from the terminal without echo, or from stdin).
    Login { provider: String },
    /// List providers with saved keys and where each provider's key comes from.
    List,
    /// Remove a saved key.
    Logout { provider: String },
}

#[derive(Subcommand)]
enum McpCmd {
    /// Connect to each configured server and list its status and tools.
    List,
    /// Log in to a remote server with OAuth (opens the browser).
    Auth { name: String },
    /// Forget a remote server's saved OAuth tokens.
    Logout { name: String },
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
    /// Undo the latest turn: restore the files it changed and hide it.
    Undo {
        /// Session id (default: the latest in this directory).
        id: Option<String>,
    },
    /// Redo the latest undo.
    Redo { id: Option<String> },
    /// Show the file changes made since the latest turn began.
    Diff { id: Option<String> },
}

impl RunArgs {
    /// Arguments for running a native command as a new session.
    fn native(message: String, model: Option<String>) -> RunArgs {
        RunArgs {
            message: vec![message],
            model,
            agent: None,
            files: vec![],
            format: Format::Default,
            auto: true,
            thinking: false,
            r#continue: false,
            session: None,
            title: None,
            worktree: None,
        }
    }
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Format {
    Default,
    Json,
}

#[derive(Subcommand)]
enum Debug {
    /// Show where codesmith reads and writes files.
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

impl codesmith_core::question::Questioner for Terminal {
    fn ask(&self, prompts: &[codesmith_core::question::Prompt]) -> Option<Vec<Vec<String>>> {
        let mut tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        let mut answers = Vec::new();
        for p in prompts {
            let _ = write!(
                tty,
                "\n\x1b[35m{}\x1b[0m {}\n",
                if p.header.is_empty() {
                    "Question"
                } else {
                    &p.header
                },
                p.question
            );
            for (i, o) in p.options.iter().enumerate() {
                let _ = writeln!(
                    tty,
                    "  {}. {}\x1b[2m{}\x1b[0m",
                    i + 1,
                    o.label,
                    if o.description.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", o.description)
                    }
                );
            }
            let _ = write!(
                tty,
                "  {}or type your own answer; empty to skip, q to dismiss all\n> ",
                if p.multiple {
                    "numbers separated by commas, "
                } else {
                    "a number, "
                }
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
            if line.trim() == "q" {
                return None;
            }
            answers.push(codesmith_core::question::interpret(&line, p));
        }
        Some(answers)
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
            if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
                return Err(anyhow!("the interactive UI needs a terminal; use `codesmith run \"your message\"` for scripts"));
            }
            let cwd = match &cli.worktree {
                Some(name) => enter_worktree(&cwd, name)?,
                None => cwd,
            };
            let resume = match (cli.session, cli.r#continue) {
                (Some(id), _) => Some(id),
                (None, true) => Some(
                    open_store()?
                        .latest(&cwd.display().to_string())?
                        .ok_or_else(|| anyhow!("no session to continue in {}", cwd.display()))?
                        .id,
                ),
                (None, false) => None,
            };
            tui::run(
                cwd,
                tui::Opts {
                    resume,
                    agent: cli.agent,
                    model: cli.model,
                    prompt: cli.prompt,
                    auto: cli.auto,
                },
            )
            .await
        }
        Some(Command::Run(args)) => run_prompt(cwd, args).await,
        Some(Command::Mcp {
            what: McpCmd::Auth { name },
        }) => {
            let loaded = config::load(&cwd)?;
            let Some(codesmith_core::mcp::ServerConfig::Remote { url, oauth, .. }) =
                loaded.config.mcp.servers.get(&name)
            else {
                return Err(anyhow!(
                    "{name} is not a remote MCP server in codesmith.json"
                ));
            };
            if !codesmith_core::mcp::oauth::Setting::enabled(oauth.as_ref()) {
                return Err(anyhow!("OAuth is turned off for {name} (\"oauth\": false)"));
            }
            use codesmith_core::mcp::oauth;
            let e = oauth::login(&reqwest::Client::new(), &oauth::store_path(), &name, url, oauth.as_ref(), |u| {
                eprintln!("Opening the browser to authorize {name}. If it doesn't open, visit:\n{u}\n");
                oauth::open_browser(u);
            })
            .await?;
            println!(
                "Authorized {name}{}",
                if e.refresh_token.is_some() {
                    " (with a refresh token)"
                } else {
                    ""
                }
            );
            Ok(())
        }
        Some(Command::Mcp {
            what: McpCmd::Logout { name },
        }) => {
            use codesmith_core::mcp::oauth;
            if oauth::remove(&oauth::store_path(), &name)? {
                println!("Removed the saved tokens for {name}");
            } else {
                println!("No saved tokens for {name}");
            }
            Ok(())
        }
        Some(Command::Mcp { what: McpCmd::List }) => {
            let loaded = config::load(&cwd)?;
            if loaded.config.mcp.servers.is_empty() {
                println!("No MCP servers configured (add them under \"mcp\": {{\"servers\": ...}} in codesmith.json).");
                return Ok(());
            }
            let mcp = codesmith_core::mcp::Mcp::connect(&loaded.config.mcp, &loaded.root).await;
            for (name, s) in &mcp.statuses {
                match s {
                    codesmith_core::mcp::Status::Connected { tools } => {
                        println!("{name}: connected, {tools} tool(s)")
                    }
                    codesmith_core::mcp::Status::Disabled => println!("{name}: disabled"),
                    codesmith_core::mcp::Status::NeedsAuth => {
                        println!("{name}: needs authorization; run `codesmith mcp auth {name}`")
                    }
                    codesmith_core::mcp::Status::Failed(e) => println!("{name}: failed: {e}"),
                }
                for t in mcp.tools.iter().filter(|t| &t.server == name) {
                    let d: String = t
                        .description
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(70)
                        .collect();
                    println!("  {}  {d}", t.name);
                }
            }
            Ok(())
        }
        Some(Command::Commands) => {
            let loaded = config::load(&cwd)?;
            for (name, c) in all_commands(&cwd, &loaded) {
                println!("/{name:<16} {}", c.description.as_deref().unwrap_or(""));
            }
            Ok(())
        }
        Some(Command::Models {
            provider: Some(pid),
        }) => {
            let loaded = config::load(&cwd)?;
            let m = loaded.config.resolve_model(&format!("{pid}/_"))?;
            let ids = m
                .provider
                .list_models(&reqwest::Client::new())
                .await
                .map_err(|e| anyhow!("listing {pid} models: {e}"))?;
            for id in ids {
                println!("{pid}/{id}");
            }
            Ok(())
        }
        Some(Command::Models { provider: None }) => {
            let loaded = config::load(&cwd)?;
            let c = &loaded.config;
            let mut ids: Vec<String> = config::BUILTIN.iter().map(|b| b.0.to_string()).collect();
            ids.extend(
                c.providers
                    .keys()
                    .filter(|k| !ids.contains(k))
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            for id in ids {
                let p = c.provider(&id).unwrap_or_default();
                let name = p.name.clone().unwrap_or_else(|| id.clone());
                if p.models.is_empty() {
                    println!("{id}/*  ({name}; `codesmith models {id}` lists its models)");
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
        Some(Command::Auth { what }) => auth_cmd(&cwd, what),
        Some(Command::Worktree { what }) => {
            use codesmith_core::worktree;
            let loaded = config::load(&cwd)?;
            let cfg = &loaded.config.worktree;
            match what {
                WorktreeCmd::List => {
                    for e in worktree::list(&cwd)? {
                        let what = match (&e.branch, &e.head) {
                            (Some(b), _) => b.clone(),
                            (None, Some(h)) => format!("detached at {h}"),
                            _ => String::new(),
                        };
                        println!(
                            "{}{}  {what}",
                            e.directory.display(),
                            if e.main { "  (main)" } else { "" }
                        );
                    }
                }
                WorktreeCmd::Create { name, from, branch } => {
                    let (wt, _) =
                        worktree::create(cfg, &cwd, &worktree::Create { name, from, branch })?;
                    println!("{}", wt.display());
                }
                WorktreeCmd::Remove { name, force } => {
                    let d = worktree::remove(cfg, &cwd, &name, force)?;
                    println!("Removed {}", d.display());
                }
            }
            Ok(())
        }
        Some(Command::Review {
            target,
            comment,
            model,
        }) => {
            let mut msg = format!("/review {}", target.join(" "));
            if comment {
                msg.push_str(" --comment");
            }
            run_prompt(cwd, RunArgs::native(msg.trim_end().to_string(), model)).await
        }
        Some(Command::Calibrate { model }) => {
            run_prompt(cwd, RunArgs::native("/calibrate".into(), model)).await
        }
        Some(Command::Serve {
            host,
            port,
            token,
            auto,
        }) => {
            let token = token.filter(|t| !t.is_empty());
            let loopback = host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback());
            if !loopback && token.is_none() {
                return Err(anyhow!("listening on {host} exposes this machine's files and shell; set --token (or $CODESMITH_SERVER_TOKEN) to do that"));
            }
            let loaded = config::load(&cwd)?;
            let mcp = connect_mcp(&loaded, true).await;
            let st = serve::Server::new(loaded, cwd, store::default_path(), auto, token, mcp);
            serve::run(st, &host, port).await
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
                    codesmith_core::compaction::ceiling(c)
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

/// Whether stdin is something to read a prompt from: a pipe or a file.
/// A terminal, a socket or a device left open by the parent may never reach
/// end-of-file, so reading it would hang.
fn stdin_has_input() -> bool {
    if std::io::stdin().is_terminal() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        match std::fs::metadata("/dev/stdin") {
            Ok(m) => m.file_type().is_fifo() || m.file_type().is_file(),
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    true
}

fn read_secret(prompt: &str) -> Result<String> {
    let mut line = String::new();
    if std::io::stdin().is_terminal() {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
        let _ = std::process::Command::new("stty")
            .arg("-echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        let r = std::io::stdin().read_line(&mut line);
        let _ = std::process::Command::new("stty")
            .arg("echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        eprintln!();
        r?;
    } else {
        std::io::stdin().read_to_string(&mut line)?;
    }
    Ok(line.trim().to_string())
}

fn auth_cmd(cwd: &std::path::Path, what: AuthCmd) -> Result<()> {
    use codesmith_core::auth;
    let file = paths::auth_file();
    match what {
        AuthCmd::Login { provider } => {
            let key = read_secret(&format!("API key for {provider}: "))?;
            if key.is_empty() {
                return Err(anyhow!("no key entered"));
            }
            auth::set_in(&file, &provider, &key)?;
            println!("Saved a key for {provider} in {}", file.display());
            let loaded = config::load(cwd)?;
            if loaded.config.provider(&provider).is_none() {
                println!("note: {provider} is not a built-in provider; add it under \"providers\" in codesmith.json to use it");
            }
        }
        AuthCmd::List => {
            let loaded = config::load(cwd)?;
            let c = &loaded.config;
            let saved = auth::providers_in(&file);
            let mut ids: Vec<String> = config::BUILTIN.iter().map(|b| b.0.to_string()).collect();
            ids.extend(
                c.providers
                    .keys()
                    .chain(saved.iter())
                    .filter(|k| !ids.contains(k))
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            ids.dedup();
            for id in ids {
                let p = c.provider(&id).unwrap_or_default();
                let source = if p.settings.api_key.as_deref().is_some_and(|k| !k.is_empty()) {
                    "config".to_string()
                } else if let Some(v) = p
                    .env
                    .iter()
                    .find(|v| std::env::var(v).is_ok_and(|k| !k.is_empty()))
                {
                    format!("${v}")
                } else if let Some(k) = auth::key_in(&file, &id) {
                    format!("saved {}", auth::mask(&k))
                } else if p.env.is_empty() {
                    "none needed".into()
                } else {
                    "—".into()
                };
                println!("{id:<12} {source}");
            }
        }
        AuthCmd::Logout { provider } => {
            if auth::remove_in(&file, &provider)? {
                println!("Removed the saved key for {provider}");
            } else {
                println!("No saved key for {provider}");
            }
        }
    }
    Ok(())
}

/// Connect to the configured MCP servers, warning about any that fail.
pub(crate) async fn connect_mcp(
    loaded: &config::Loaded,
    warn: bool,
) -> Arc<codesmith_core::mcp::Mcp> {
    let mcp = codesmith_core::mcp::Mcp::connect(&loaded.config.mcp, &loaded.root).await;
    if warn {
        for (name, s) in &mcp.statuses {
            match s {
                codesmith_core::mcp::Status::Failed(e) => eprintln!("\x1b[33mwarning:\x1b[0m MCP server {name} failed to start: {e}"),
                codesmith_core::mcp::Status::NeedsAuth => eprintln!("\x1b[33mwarning:\x1b[0m MCP server {name} needs authorization: run `codesmith mcp auth {name}`"),
                _ => {}
            }
        }
    }
    mcp
}

pub(crate) fn all_commands(
    cwd: &std::path::Path,
    loaded: &config::Loaded,
) -> std::collections::BTreeMap<String, command::Command> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    command::discover(
        command::builtins(&loaded.root),
        &command::roots(cwd, &loaded.root, &paths::config_dir(), &home),
        &loaded.config.commands,
    )
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
        SessionCmd::Undo { id } => {
            let (info, snaps) = snapshot_target(&db, &cwd, id)?;
            match session::undo(&db, &info.id, &snaps)? {
                Some(changes) => {
                    println!(
                        "Undid the latest turn of {}; restored {} file(s):",
                        info.id,
                        changes.len()
                    );
                    print_changes(&changes, false);
                }
                None => println!("Nothing to undo in {}.", info.id),
            }
        }
        SessionCmd::Redo { id } => {
            let (info, snaps) = snapshot_target(&db, &cwd, id)?;
            match session::redo(&db, &info.id, &snaps)? {
                Some(changes) => {
                    println!(
                        "Redid the turn in {}; reapplied {} file(s):",
                        info.id,
                        changes.len()
                    );
                    print_changes(&changes, true);
                }
                None => println!(
                    "Nothing to redo in {} (a new turn after an undo makes it permanent).",
                    info.id
                ),
            }
        }
        SessionCmd::Diff { id } => {
            let (info, snaps) = snapshot_target(&db, &cwd, id)?;
            match session::turn_diff(&db, &info.id, &snaps)? {
                Some(d) if d.trim().is_empty() => {
                    println!("No file changes since the latest turn began.")
                }
                Some(d) => print!("{d}"),
                None => println!("No snapshot recorded for {}.", info.id),
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
                serde_json::from_str(&text).context("not a codesmith session export")?;
            db.import(&e)?;
            println!("Imported {}", e.info.id);
        }
    }
    Ok(())
}

fn snapshot_target(
    db: &Store,
    cwd: &std::path::Path,
    id: Option<String>,
) -> Result<(store::Info, Snapshots)> {
    let info = match id {
        Some(id) => db.get(&id)?.ok_or_else(|| anyhow!("no session {id}"))?,
        None => db
            .latest(&cwd.display().to_string())?
            .ok_or_else(|| anyhow!("no session in {}", cwd.display()))?,
    };
    let root = config::project_root(std::path::Path::new(&info.directory));
    let snaps = Snapshots::for_project(&root)
        .ok_or_else(|| anyhow!("snapshots are not kept for {}", root.display()))?;
    Ok((info, snaps))
}

/// `changes` are relative to the tree being restored: `A` files exist now but
/// not there, `D` files exist there but not now.
fn print_changes(changes: &[codesmith_core::snapshot::Change], redo: bool) {
    for c in changes {
        let what = match (c.status, redo) {
            ('A', _) => "removed",
            ('D', false) => "restored",
            ('D', true) => "recreated",
            (_, false) => "reverted",
            (_, true) => "reapplied",
        };
        println!("  {what:9} {}", c.path);
    }
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
    if stdin_has_input() {
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
    // Only now, with a message to send, create the worktree.
    let cwd = match &args.worktree {
        Some(name) => enter_worktree(&cwd, name)?,
        None => cwd,
    };
    let loaded = config::load(&cwd)?;
    let (mut agent_arg, mut model_arg) = (args.agent, args.model);
    let mut native: Option<(String, String)> = None;
    if let Some((name, rest)) = command::parse_invocation(&message) {
        let all = all_commands(&cwd, &loaded);
        let c = all.get(&name).ok_or_else(|| {
            anyhow!(
                "unknown command /{name}; available: {}",
                all.keys()
                    .map(|k| format!("/{k}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        if c.native {
            native = Some((name.clone(), rest.clone()));
        } else {
            message = command::expand(&c.template, &rest);
        }
        agent_arg = agent_arg.or(c.agent.clone());
        model_arg = model_arg.or(c.model.clone());
    }
    let interactive = !args.auto
        && std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .is_ok();
    let questioner: Option<Arc<dyn codesmith_core::question::Questioner>> = if interactive {
        Some(Arc::new(Terminal))
    } else {
        None
    };
    let asker: Arc<dyn Asker> = if args.auto {
        Arc::new(Fixed(Reply::Once))
    } else {
        Arc::new(Terminal)
    };
    let mcp = connect_mcp(&loaded, args.format != Format::Json).await;
    let mut session = Session::new(
        &loaded,
        cwd,
        Options {
            agent: agent_arg,
            model: model_arg,
            resume,
            title: args.title,
            parent: None,
            questioner,
            mcp: Some(mcp),
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
    let result = match native {
        Some((name, rest)) => {
            let env = codesmith_core::review::Env::new(
                &loaded,
                session.cwd.clone(),
                Some(session.model_spec.clone()),
                Some(store::default_path()),
                Some(session.id.clone()),
                None,
            );
            let progress = |s: String| eprintln!("\x1b[2m{name}: {s}\x1b[0m");
            match codesmith_core::review::run_command(&name, &rest, &env, &progress).await {
                Ok((report, usage)) => {
                    sink.text(&report);
                    session.append_exchange(&message, &report, &usage)
                }
                Err(e) => Err(e),
            }
        }
        None => session.prompt(&message, &mut sink).await,
    };
    sink.done(
        &session.usage,
        result.as_ref().err().map(|e| e.to_string()),
        &session.id,
        session.title.as_deref(),
    );
    result
}
