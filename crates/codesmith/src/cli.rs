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
    /// Start outside a repository without asking, even in the home folder
    /// or `/` with no terminal to confirm.
    #[arg(long, global = true, env = "CODESMITH_ANY_FOLDER", action = clap::ArgAction::SetTrue, value_parser = clap::builder::FalseyValueParser::new())]
    any_folder: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run one prompt non-interactively and print the result.
    Run(RunArgs),
    /// One model call outside any session's history (OpenCode's generate
    /// API): text from a chat model, text from a session's settled
    /// context (`--session`), or an image (`--image`). Nothing is stored
    /// but a session's usage.
    Generate {
        /// The prompt.
        prompt: Vec<String>,
        /// The model as provider/model[#variant] (default: the configured
        /// model; for `--image`, required).
        #[arg(short, long)]
        model: Option<String>,
        /// Generate from this session's context, adding only its usage.
        #[arg(long)]
        session: Option<String>,
        /// Make an image and save it here (its real extension replaces
        /// the one given).
        #[arg(long)]
        image: Option<PathBuf>,
        /// Ask a decision model the questions in this JSON file (an array
        /// of `predicate`, `choice` and `score` questions) about the
        /// prompt; prints the answers as JSON.
        #[arg(long, conflicts_with_all = ["image", "session"])]
        questions: Option<PathBuf>,
    },
    /// List providers and their models, or ask one provider for its models.
    /// `codesmith models swap PROVIDER MODEL` loads MODEL on the server of a
    /// provider with a `swap` setting.
    Models {
        /// A provider id: list the models its API reports, and the models its
        /// server can load when it has a `swap` setting. Or `swap`.
        provider: Option<String>,
        /// After `swap`: the provider and the model to load.
        #[arg(value_name = "PROVIDER MODEL")]
        args: Vec<String>,
        /// Fetch the models.dev catalog into the cache first (the one
        /// network request CodeSmith makes for it unless fetching is on).
        #[arg(long)]
        refresh: bool,
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
    /// List, edit or remove the approvals "always" answers saved.
    Permissions {
        #[command(subcommand)]
        what: Option<PermissionsCmd>,
    },
    /// Trust a project's config to use what it asks for ({env:} and
    /// {file:} values, provider and MCP URLs and headers, programs), or
    /// list and revoke the answers kept.
    Trust {
        /// The project folder (default: the current one), or with
        /// `--revoke` a remembered answer's id.
        target: Option<String>,
        /// List the remembered answers.
        #[arg(long, conflicts_with_all = ["revoke", "distrust", "show"])]
        list: bool,
        /// Forget the answer for the project (or with this id).
        #[arg(long, conflicts_with_all = ["distrust", "show"])]
        revoke: bool,
        /// Remember not to trust it.
        #[arg(long, conflicts_with = "show")]
        distrust: bool,
        /// Only show what the project's config asks for and the answer kept.
        #[arg(long)]
        show: bool,
        /// Print JSON (with --list or --show).
        #[arg(long)]
        json: bool,
    },
    /// Inspect configuration and internals.
    Debug {
        #[command(subcommand)]
        what: Debug,
    },
    /// The local bus between CodeSmith sessions on this machine.
    Bus {
        #[command(subcommand)]
        what: crate::busctl::BusCmd,
    },
    /// Set up a tailbus bridge that hands other agents' messages to
    /// CodeSmith: a headless `codesmith run`, or a relay to a session you
    /// have open.
    Bridge {
        #[command(subcommand)]
        what: crate::bridge_init::BridgeCmd,
    },
    /// Upgrade CodeSmith to the latest or a specific release.
    #[command(visible_alias = "update")]
    Upgrade {
        /// Version to upgrade to (with or without a leading v).
        target: Option<String>,
        /// Installation method to use (otherwise detected from where this copy lives).
        #[arg(short, long, value_enum)]
        method: Option<crate::upgrade::Method>,
        /// Only report whether another release is out; change nothing.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
enum PermissionsCmd {
    /// List this project's saved approvals (the default).
    List {
        /// Include every project's.
        #[arg(long)]
        all: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Change a saved approval's resource pattern or action.
    Edit {
        /// Saved approval id (see `permissions list`).
        id: String,
        /// The new resource pattern.
        #[arg(long)]
        resource: Option<String>,
        /// The new action.
        #[arg(long)]
        action: Option<String>,
    },
    /// Remove saved approvals.
    Remove {
        /// Saved approval ids (see `permissions list`).
        #[arg(required = true)]
        ids: Vec<String>,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    /// The message to send (piped stdin is appended).
    message: Vec<String>,
    /// Model as provider/model.
    #[arg(short, long)]
    model: Option<String>,
    /// Agent to use (default: `default_agent`, else build).
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
    /// Archive the session before its prompt runs, so it stays out of
    /// session lists (for scripted or CI runs).
    #[arg(long)]
    archived: bool,
    /// Reasoning effort for the model (one of its levels, or `default`).
    #[arg(long, value_name = "LEVEL")]
    effort: Option<String>,
    /// Offer only these built-in tools (comma-separated) and deny every
    /// other tool, MCP's included.
    #[arg(long, value_name = "LIST", conflicts_with = "no_tools")]
    tools: Option<String>,
    /// Offer no tools at all.
    #[arg(long)]
    no_tools: bool,
    /// Connect no MCP servers.
    #[arg(long)]
    no_mcp: bool,
    /// Append one JSON line per model step (tokens, cost) and tool call
    /// to this file while the run goes.
    #[arg(long, value_name = "PATH")]
    usage_log: Option<PathBuf>,
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
        /// Stop at this time ("in 8 hours", "2026-10-27T18:00"): no slot
        /// after it runs, and an event watch ends then.
        #[arg(long)]
        expires: Option<String>,
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
            expires,
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
            // A program's tools are read from it and checked now, as its
            // runs will be: each must exist and be allowed unattended.
            if let au::Action::Code { code, .. } = &action {
                let loaded = config::load(std::path::Path::new(&dir))?;
                let tools = au::authorize_code(&db, &loaded, creator.as_deref(), code).await?;
                if !tools.is_empty() {
                    println!(
                        "The program calls {}; its runs may call only these.",
                        tools.join(", ")
                    );
                }
            }
            let a = au::create_with(
                &db,
                au::New {
                    name,
                    directory: dir,
                    session_id: creator,
                    trigger,
                    action,
                },
                &au::Opts { expires },
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
                _ if a.status == "done" => println!("{id} has expired, so it is done"),
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
    /// Sign in to a provider: save an API key (read from the terminal without echo, or from stdin), or run its browser, device or Claude Code login.
    Login {
        /// Provider id, such as anthropic, openai, amazon-bedrock or
        /// github-copilot. Left out: pick one from a list of providers
        /// with how each signs in (in a terminal; without one, the list
        /// is printed).
        provider: Option<String>,
        /// amazon-bedrock: sign with this AWS profile (SSO or named) instead of saving a key.
        #[arg(long)]
        profile: Option<String>,
        /// github-copilot: log in to this GitHub Enterprise host (company.ghe.com).
        #[arg(long)]
        enterprise: Option<String>,
        /// The client id of your OAuth app: github-copilot (device flow
        /// enabled), or a provider's browser login with --oauth.
        #[arg(long)]
        client_id: Option<String>,
        /// Log in through the provider's OAuth instead of pasting a key
        /// (openai, xai, poe, gitlab, digitalocean, snowflake-cortex).
        #[arg(long)]
        oauth: bool,
        /// openai --oauth: opt in to OpenAI's agent registration, which
        /// registers a sign-in client of your own named CodeSmith (OpenAI
        /// may reject it). Without it, a client id of your own is needed.
        #[arg(long, requires = "oauth")]
        register: bool,
        /// azure: sign requests with Microsoft Entra ID tokens from the
        /// Azure CLI (`az login`) instead of saving a key.
        #[arg(long)]
        cli: bool,
        /// google-vertex: use Application Default Credentials in this project.
        #[arg(long)]
        project: Option<String>,
        /// google-vertex: the location with --project (default global).
        #[arg(long)]
        location: Option<String>,
        /// An answer the provider's login asks for beside the key, such as
        /// `accountId=…` for Cloudflare (asked in the terminal otherwise).
        #[arg(long = "field", value_name = "KEY=VALUE")]
        fields: Vec<String>,
        /// claude-agent: sign in with a Claude Code token instead of Claude
        /// Code's keychain login (works over SSH). At a terminal, CodeSmith
        /// runs `claude setup-token` and saves the token it prints;
        /// otherwise the token is read from standard input.
        #[arg(long)]
        token: bool,
        /// Save the login in another CodeSmith home's auth store (the
        /// `home` a tailbus bridge runs CodeSmith with), not your own. The
        /// folder must exist and can't be your own home.
        #[arg(long, value_name = "DIR", conflicts_with = "to_file")]
        home: Option<PathBuf>,
        /// Write the key, or a Claude Code token for claude-agent, to this
        /// file (created readable only by you) instead of an auth store: a
        /// tailbus bridge's `secrets` reads it. An existing file is
        /// replaced only when you say so at a terminal; a symlink never is.
        #[arg(long, value_name = "PATH")]
        to_file: Option<PathBuf>,
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
        /// The server's name in codesmith.json (asked for in a terminal
        /// when left out).
        name: Option<String>,
    },
    /// Forget a remote server's saved OAuth tokens.
    Logout {
        /// The server's name in codesmith.json.
        name: String,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// List sessions, newest first (archived ones with --archived).
    List {
        /// Include sessions from every directory.
        #[arg(long)]
        all: bool,
        /// List the archived sessions instead.
        #[arg(long)]
        archived: bool,
        /// Only sessions whose title contains this (case-insensitive; `%`
        /// and `_` match any run and any one character).
        #[arg(long)]
        search: Option<String>,
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
        /// Replace every text, tool input and output, path and title with
        /// a marker, keeping the session's shape (OpenCode's `--sanitize`).
        #[arg(long)]
        sanitize: bool,
    },
    /// Import a session exported as JSON (a file path, or - for stdin).
    Import {
        /// Path to the exported JSON, or - to read stdin.
        file: String,
    },
    /// Archive a session: it stays, out of session lists.
    Archive {
        /// Session id (see `session list`).
        id: String,
    },
    /// Bring an archived session back into session lists.
    Unarchive {
        /// Session id (see `session list --archived`).
        id: String,
    },
    /// List a session's messages with their numbers (for `session fork --before`).
    Messages {
        /// Session id (default: the latest in this directory).
        id: Option<String>,
    },
    /// Fork a session: a new session with its history through the latest
    /// message, or up to a message, that then goes its own way.
    Fork {
        /// Session id (default: the latest in this directory).
        id: Option<String>,
        /// Copy the history up to (not including) this message (see `session messages`).
        #[arg(long)]
        before: Option<i64>,
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
    /// Show the file changes made since the latest turn began, or by the
    /// turns from one prompt through another (`--from`, `--to`).
    Diff {
        /// Session id (default: the latest in this directory).
        id: Option<String>,
        /// The turn of this prompt (see `session messages`): its start
        /// snapshot against the range's last recorded end snapshot.
        #[arg(long)]
        from: Option<i64>,
        /// Through the turn of this later prompt (default: `--from`'s).
        #[arg(long, requires = "from")]
        to: Option<i64>,
    },
    /// Move media kept inline in session records (sessions from before the
    /// blob store, or with `media.blob` off) into the blob store.
    MigrateMedia {
        /// Session id (default: every session).
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
            archived: false,
            effort: None,
            tools: None,
            no_tools: false,
            no_mcp: false,
            usage_log: None,
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
    /// List every outside page CodeSmith shows or opens; with --check,
    /// request each and fail when one is gone (run before each release).
    Links {
        /// Request each link and report the ones that moved or are gone.
        #[arg(long)]
        check: bool,
    },
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

/// Run the interactive UI with the opt-in update check alongside; once the
/// terminal is restored, say if another release is out.
async fn with_update_notice(ui: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    let check = crate::upgrade::start(env!("CARGO_PKG_VERSION"));
    let result = ui.await;
    if let Some(check) = check {
        if let Some(notice) = check.notice().await {
            eprintln!("{notice}");
        }
    }
    result
}

/// `codesmith upgrade ...` parsed into (target, method, check).
#[cfg(test)]
pub(crate) fn parse_upgrade<'a>(
    argv: impl Iterator<Item = &'a str>,
) -> Result<(Option<String>, Option<crate::upgrade::Method>, bool), clap::Error> {
    match Cli::try_parse_from(argv)?.command {
        Some(Command::Upgrade {
            target,
            method,
            check,
        }) => Ok((target, method, check)),
        _ => unreachable!("upgrade parses as upgrade"),
    }
}

/// The long help of `codesmith` or of a subcommand path.
#[cfg(test)]
pub(crate) fn help(path: &[&str]) -> String {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    for name in path {
        cmd = cmd.find_subcommand(name).expect("subcommand").clone();
    }
    cmd.render_long_help().to_string()
}

/// Parse the command line and run it; returns the exit code.
pub async fn entry() -> i32 {
    let cli = Cli::parse();
    // A saved Claude Code token is what Claude Code signs in with.
    codesmith_core::providers::claude_agent::use_saved_token();
    let upgrading = matches!(cli.command, Some(Command::Upgrade { .. }));
    match run(cli).await {
        Ok(()) => 0,
        Err(e) => {
            // The error can carry a provider's or MCP server's text.
            eprintln!(
                "\x1b[31merror:\x1b[0m {}",
                crate::render::safe(&format!("{e:#}"))
            );
            // An upgrade is an install: it ends with the failure banner.
            if upgrading {
                let colour = crate::upgrade::colour_for(std::io::stderr().is_terminal());
                eprint!(
                    "{}",
                    crate::upgrade::banner(crate::upgrade::Ending::Failed, colour)
                );
            }
            1
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    // A project whose config asks for host secrets, URLs or programs is
    // asked about once, at a terminal; elsewhere (no terminal, `acp`,
    // `serve`, automations) an unanswered project is not trusted.
    if asks_trust(&cli.command) {
        trust_prompt(&cwd)?;
    }
    // The interactive UI shows the notice of an untrusted project itself.
    if cli.command.is_none() {
        codesmith_core::trust::set_announce(false);
    }
    // Sessions refresh the models.dev catalog in the background (never
    // waiting for it) when fetching is turned on; it is off by default.
    if matches!(
        cli.command,
        None | Some(Command::Run(_)) | Some(Command::Acp) | Some(Command::Serve { .. })
    ) {
        if let Some(c) = config::load(&cwd).ok().and_then(|l| l.config.models_dev) {
            c.spawn_refresh();
        }
    }
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
            with_update_notice(tui::run(
                cwd,
                tui::Opts {
                    any_folder: cli.any_folder,
                    resume,
                    agent: cli.agent,
                    model: cli.model,
                    prompt: cli.prompt,
                    auto: cli.auto,
                },
            ))
            .await
        }
        Some(Command::Run(args)) => {
            folder_check(&cwd, cli.any_folder)?;
            run_prompt(cwd, args).await
        }
        Some(Command::Mcp {
            what: McpCmd::Auth { name },
        }) => {
            let loaded = config::load(&cwd)?;
            let name = match name {
                Some(n) => n,
                None => pick_mcp_server(&loaded.config.mcp.servers)?,
            };
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
            let e = oauth::login_with(
                &reqwest::Client::new(),
                &oauth::store_path(),
                &name,
                url,
                oauth.as_ref(),
                |u| {
                    eprintln!(
                        "Opening the browser to authorize {name}. If it doesn't open, visit:\n{u}\n\
                         Waiting for the browser… If it runs on another machine, paste the URL it ends up on here."
                    );
                    oauth::open_browser(u);
                },
                Some(codesmith_core::providers::oauth::stdin_lines()),
                |why| eprintln!("{why}"),
            )
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
            for (name, s) in &mcp.statuses() {
                match s {
                    codesmith_core::mcp::Status::Connected { tools } => {
                        println!("{name}: connected, {tools} tool(s)")
                    }
                    codesmith_core::mcp::Status::Disabled => println!("{name}: disabled"),
                    codesmith_core::mcp::Status::NeedsAuth => {
                        println!("{name}: needs authorization; run `codesmith mcp auth {name}`")
                    }
                    codesmith_core::mcp::Status::Failed(e) => {
                        println!("{name}: failed: {}", crate::render::safe(e))
                    }
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
            provider: Some(p),
            args,
            ..
        }) if p == "swap" && !args.is_empty() => models_swap(&cwd, &args).await,
        Some(Command::Models { args, .. }) if !args.is_empty() => Err(anyhow!(
            "unexpected {:?}: `codesmith models PROVIDER` lists, `codesmith models swap PROVIDER MODEL` loads a model",
            args.join(" ")
        )),
        Some(Command::Models {
            provider,
            refresh: true,
            ..
        }) => {
            if codesmith_core::catalog::fetch_disabled() {
                return Err(anyhow!(
                    "fetching the model catalog is turned off by CODESMITH_DISABLE_MODELS_FETCH"
                ));
            }
            let loaded = config::load(&cwd)?;
            let c = loaded
                .config
                .models_dev
                .clone()
                .ok_or_else(|| anyhow!("no model catalog"))?;
            use codesmith_core::catalog::Refreshed;
            match c
                .try_refresh(true)
                .await
                .map_err(|e| anyhow!("refreshing the model catalog from {}: {e:#}", c.source()))?
            {
                Refreshed::Updated => eprintln!("Updated the model catalog from {}.", c.source()),
                _ => eprintln!("The model catalog from {} is unchanged.", c.source()),
            }
            Box::pin(run(Cli {
                command: Some(Command::Models {
                    provider,
                    args: vec![],
                    refresh: false,
                }),
                ..cli
            }))
            .await
        }
        Some(Command::Models {
            provider: Some(pid),
            ..
        }) => {
            let loaded = config::load(&cwd)?;
            let swap = codesmith_core::providers::swap::for_provider(&loaded.config, &pid).is_some();
            let listed = list_provider_models(&loaded, &pid).await;
            if !swap {
                return listed;
            }
            if let Err(e) = listed {
                eprintln!("{e:#}");
            }
            print_swappable(&loaded.config, &pid).await
        }
        Some(Command::Models { provider: None, .. }) => {
            let loaded = config::load(&cwd)?;
            let c = &loaded.config;
            // Local servers on this machine list what they serve now.
            codesmith_core::providers::local::discover_local(
                c,
                &codesmith_core::providers::live::file(),
            )
            .await;
            let mut ids: Vec<String> = config::BUILTIN.iter().map(|b| b.0.to_string()).collect();
            ids.extend(
                c.providers
                    .keys()
                    .filter(|k| !ids.contains(k))
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            // Other catalog providers are listed once they have a key, as
            // OpenCode lists the providers that are available; so are the
            // provider plugins that ambient credentials make usable.
            for (id, _, _) in codesmith_core::providers::PROVIDERS {
                let p = c.provider(id).unwrap_or_default();
                let env = codesmith_core::providers::process_env();
                let keyed = p
                    .env
                    .iter()
                    .any(|v| std::env::var(v).is_ok_and(|k| !k.is_empty()))
                    || codesmith_core::auth::entry(id).is_some();
                if (keyed || codesmith_core::providers::available(id, &p.settings.plugin, &env))
                    && !ids.iter().any(|i| i == id)
                {
                    ids.push(id.to_string());
                }
            }
            for id in c.keyed_catalog_providers() {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            for id in ids {
                let p = c.provider(&id).unwrap_or_default();
                let name = p.name.clone().unwrap_or_else(|| id.clone());
                // The configured models and the catalog's chat models.
                let models = c.list_models(&id, &[]);
                if models.is_empty() {
                    println!("{id}/*  ({name}; `codesmith models {id}` lists its models)");
                }
                for m in models {
                    if codesmith_core::providers::hidden(&id, &m) {
                        continue;
                    }
                    println!("{id}/{m}");
                }
            }
            if let Some(m) = &c.model {
                println!("\ndefault: {m}");
            }
            Ok(())
        }
        Some(Command::Auth { what }) => auth_cmd(&cwd, what).await,
        Some(Command::Acp) => acp::run().await,
        Some(Command::Generate {
            prompt,
            model,
            session,
            image,
            questions,
        }) => generate_cmd(&cwd, prompt.join(" "), model, session, image, questions).await,
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
                // `COLUMNS`, else the terminal's width, else 80.
                let width = std::env::var("COLUMNS")
                    .ok()
                    .and_then(|c| c.parse().ok())
                    .or_else(|| {
                        std::io::stdout()
                            .is_terminal()
                            .then(|| ratatui::crossterm::terminal::size().ok())
                            .flatten()
                            .map(|(w, _)| w as usize)
                    })
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
            let token = token
                .or_else(|| ENV_TOKEN.get().cloned())
                .filter(|t| !t.is_empty());
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
            #[cfg(unix)]
            let daemon = codesmith_core::daemon::enabled().then(|| {
                let inherited = SHELL_HANDOFF.get().cloned();
                std::sync::Arc::new(codesmith_core::daemon::for_server(inherited))
            });
            #[cfg(not(unix))]
            let daemon = None;
            let st = serve::Server::with_daemon(
                loaded,
                cwd,
                store::default_path(),
                auto,
                token,
                mcp,
                daemon,
            );
            serve::run(st, &host, port).await
        }
        Some(Command::Session { what }) => session_cmd(cwd, what),
        Some(Command::Permissions { what }) => permissions_cmd(
            &cwd,
            what.unwrap_or(PermissionsCmd::List {
                all: false,
                json: false,
            }),
        ),
        Some(Command::Trust {
            target,
            list,
            revoke,
            distrust,
            show,
            json,
        }) => trust_cmd(&cwd, target, list, revoke, distrust, show, json),
        Some(Command::Debug {
            what: Debug::Links { check },
        }) => crate::links::run(check).await,
        Some(Command::Debug { what }) => debug(cwd, what),
        Some(Command::Bus { what }) => crate::busctl::run(cwd, what).await,
        Some(Command::Bridge { what }) => crate::bridge_init::run_cmd(&cwd, what).await,
        Some(Command::Upgrade {
            target,
            method,
            check,
        }) => {
            let current = env!("CARGO_PKG_VERSION");
            let mut updater = crate::upgrade::Github::new(current);
            crate::upgrade::command(
                &mut updater,
                current,
                target.as_deref(),
                method,
                check,
                &mut std::io::stdout(),
            )
            .await
        }
    }
}

fn debug(cwd: PathBuf, what: Debug) -> Result<()> {
    match what {
        Debug::Paths => {
            println!("config  {}", paths::config_dir().display());
            println!("data    {}", paths::data_dir().display());
            println!("output  {}", paths::tool_output_dir().display());
            println!("tmp     {}", paths::tmp_dir().display());
            println!("cache   {}", paths::cache_dir().display());
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
                .or_else(|| loaded.config.default_model())
                .ok_or_else(|| anyhow!("no model given, configured or available"))?;
            let m = loaded.config.resolve_model(&spec)?;
            let base = config::split_variant(&spec).0;
            println!("model:    {base} (sent as {})", m.model_id);
            println!("protocol: {:?}", m.provider.protocol);
            println!("url:      {}", m.provider.base_url);
            if m.local {
                println!("local:    yes (a session's title waits for the turn's response)");
            }
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
                    codesmith_ai::Auth::BearerHeader(h) => {
                        format!(" (sent as a bearer token in {h})")
                    }
                    codesmith_ai::Auth::Bearer => " (sent as a bearer token)".into(),
                    codesmith_ai::Auth::Dynamic(a) => format!(" ({})", a.describe()),
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
            let lines = variant_lines(&m)?;
            if lines.is_empty() {
                println!("variants: none");
            } else {
                println!("variants: (select with {base}#name)");
                for line in lines {
                    println!("  {line}");
                }
            }
        }
        Debug::Links { .. } => unreachable!("handled before debug()"),
        Debug::Plugins => {
            let loaded = config::load(&cwd)?;
            let reg = codesmith_core::plugin::active();
            let builtin: Vec<String> = codesmith_core::plugin::builtin()
                .iter()
                .map(|p| p.id().to_string())
                .collect();
            if reg.ids().iter().all(|id| builtin.contains(id)) {
                println!("No plugins besides the built-in ones are compiled into this binary. See the README's Plugins section to build one with yours.");
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
                    "{id}{}: {}{}",
                    if builtin.contains(&id) {
                        " (built in)"
                    } else {
                        ""
                    },
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
                let hidden = if a.hidden { ", hidden" } else { "" };
                println!("{id} ({}{hidden}): {}", a.mode, a.description);
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
            // The baseline a new session would make, without MCP servers.
            let env = prompt::env_block(&prompt::Environment {
                cwd: &cwd,
                root: &loaded.root,
                is_git: loaded.root.join(".git").exists(),
                tmp: &paths::tmp_dir(),
                date: String::new(),
            });
            let files = instructions::discover(
                &cwd,
                &loaded.root,
                &paths::config_dir(),
                &loaded.config.instructions,
            );
            let skills = codesmith_core::skill::find(&cwd, &loaded.root, &loaded.config.skills);
            let mut lists = vec![codesmith_core::reference::source(
                &codesmith_core::reference::configured(&loaded.config, &loaded.root),
            )];
            if tools.contains(&"skill") && !skills.is_empty() {
                lists.push(codesmith_core::skill::source(&skills));
            }
            lists.push(instructions::source(instructions::read(&files)));
            lists.push(prompt::builtin_sources(prompt::today, env));
            let list = codesmith_core::instruction_state::combine(lists)?;
            let o = codesmith_core::instruction_state::observe(&list, None)?;
            let text = codesmith_core::instruction_state::join(
                &codesmith_core::instruction_state::render_initial(&list, &o.current)?,
            );
            let base = match &a.system {
                Some(s) => s.clone(),
                None => {
                    prompt::base_named(&mid, &prompt::model_name(&loaded.config, &model_id), &tools)
                }
            };
            let mut parts = vec![base.as_str()];
            if !text.is_empty() {
                parts.push(&text);
            }
            println!("{}", parts.join("\n\n"));
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

/// `--field KEY=VALUE` answers.
fn parse_fields(fields: &[String]) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut given = serde_json::Map::new();
    for f in fields {
        let (k, v) = f
            .split_once('=')
            .ok_or_else(|| anyhow!("--field takes KEY=VALUE, got {f:?}"))?;
        given.insert(k.trim().to_string(), v.trim().into());
    }
    Ok(given)
}

/// An MCP server `mcp auth` can sign in to (OpenCode's `mcpAuthChoices`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AuthChoice {
    pub value: String,
    pub label: String,
    /// It has saved tokens.
    pub connected: bool,
    pub hint: Option<&'static str>,
}

/// The servers that take an OAuth sign-in (remote, OAuth not turned off),
/// by name, with whether tokens for them are saved in `store` and a hint
/// when those tokens have expired with no way to refresh them.
pub(crate) fn mcp_auth_choices(
    servers: &std::collections::BTreeMap<String, codesmith_core::mcp::ServerConfig>,
    store: &std::path::Path,
) -> Vec<AuthChoice> {
    use codesmith_core::mcp::{oauth, ServerConfig};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    servers
        .iter()
        .filter_map(|(name, s)| match s {
            ServerConfig::Remote { url, oauth: o, .. } if oauth::Setting::enabled(o.as_ref()) => {
                let saved = oauth::load(store, name, url);
                let stale = saved.as_ref().is_some_and(|e| {
                    e.refresh_token.is_none() && e.expires_at.is_some_and(|t| t <= now)
                });
                Some(AuthChoice {
                    value: name.clone(),
                    label: name.clone(),
                    connected: saved.is_some(),
                    hint: stale.then_some("needs authentication"),
                })
            }
            _ => None,
        })
        .collect()
}

/// The server `mcp auth` was run for without a name: picked from a
/// numbered list in a terminal, else an error before anything connects.
fn pick_mcp_server(
    servers: &std::collections::BTreeMap<String, codesmith_core::mcp::ServerConfig>,
) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "Pass an MCP server name when running without an interactive terminal"
        ));
    }
    let choices = mcp_auth_choices(servers, &codesmith_core::mcp::oauth::store_path());
    if choices.is_empty() {
        return Err(anyhow!(
            "No remote MCP server in codesmith.json takes an OAuth sign-in"
        ));
    }
    for (i, c) in choices.iter().enumerate() {
        let state = match (c.connected, c.hint) {
            (_, Some(h)) => format!(" ({h})"),
            (true, None) => " (signed in)".into(),
            (false, None) => String::new(),
        };
        eprintln!("  {}. {}{state}", i + 1, c.label);
    }
    let answer = read_line("MCP server to sign in to (number): ")?.unwrap_or_default();
    answer
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| choices.get(i))
        .map(|c| c.value.clone())
        .ok_or_else(|| anyhow!("no server numbered {answer:?}"))
}

/// A line typed in the terminal; `None` when stdin is not one.
pub(crate) fn read_line(prompt: &str) -> Result<Option<String>> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(Some(line.trim().to_string()))
}

pub(crate) fn read_secret(prompt: &str) -> Result<String> {
    let mut line = String::new();
    if std::io::stdin().is_terminal() {
        // Echo goes off before the prompt shows, so a key typed or pasted
        // the moment it does is not echoed.
        let _ = std::process::Command::new("stty")
            .env_remove(codesmith_core::SERVER_TOKEN_VAR)
            .arg("-echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        eprint!("{prompt}");
        std::io::stderr().flush()?;
        let r = std::io::stdin().read_line(&mut line);
        let _ = std::process::Command::new("stty")
            .env_remove(codesmith_core::SERVER_TOKEN_VAR)
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

/// Logins that are not an API key: an AWS profile for Bedrock, GitHub's
/// device flow for Copilot. `None` for a provider that takes a key.
#[derive(Default)]
struct PluginLogin {
    profile: Option<String>,
    enterprise: Option<String>,
    project: Option<String>,
    location: Option<String>,
    client_id: Option<String>,
    oauth: bool,
    register: bool,
    cli: bool,
    fields: Vec<String>,
    cwd: PathBuf,
    /// Another home the login is for (`--home`): its config is read, not
    /// yours or the project's.
    home: Option<PathBuf>,
}

/// The config a login reads: another home's (`--home`, a bridge's), or
/// the one for `cwd`.
pub(crate) fn login_config(
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
) -> Result<config::Loaded> {
    match home {
        Some(h) => config::load_home(h),
        None => config::load(cwd),
    }
}

/// A key or code typed into a login: never shown, logged or printed with
/// `{:?}`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Redirects pasted back into a browser login.
pub(crate) enum Pasted {
    /// Lines typed in the terminal (read only once the login waits, so a
    /// question asked before it gets its own answer).
    Stdin,
    /// The sign-in picker's paste box.
    Channel(tokio::sync::mpsc::Receiver<String>),
    None,
}

impl Pasted {
    fn take(&mut self) -> Option<tokio::sync::mpsc::Receiver<String>> {
        match std::mem::replace(self, Pasted::None) {
            Pasted::Stdin => Some(codesmith_core::providers::oauth::stdin_lines()),
            Pasted::Channel(rx) => Some(rx),
            Pasted::None => None,
        }
    }
}

/// Shows the URL to sign in at, and opens it; the flag says whether a
/// pasted redirect completes the login.
pub(crate) type OpenUrl = Arc<dyn Fn(&str, bool) + Send + Sync>;
/// Shows a device code with its page (and a link with the code filled in).
pub(crate) type ShowCode = Arc<dyn Fn(&str, &str, Option<&str>) + Send + Sync>;

/// How a login talks to the person: in the terminal (`codesmith auth
/// login`), or through the sign-in picker of the interactive UI, which
/// must not write to the terminal or read from it.
pub(crate) struct LoginIo {
    /// Show the URL to sign in at, and open it. The flag: whether a
    /// redirect pasted back completes the login.
    pub open: OpenUrl,
    /// Show a device code and the page to enter it on (and open the page,
    /// with the code filled in when the provider gives such a link).
    pub device: ShowCode,
    /// Say something while the login runs.
    pub note: Arc<dyn Fn(&str) + Send + Sync>,
    pub pasted: Pasted,
    /// Whether what the flags left out may be asked in the terminal.
    pub terminal: bool,
}

impl LoginIo {
    /// The terminal's: messages on stderr, pastes and answers from stdin.
    pub(crate) fn terminal() -> LoginIo {
        LoginIo {
            open: Arc::new(|url, paste| {
                let tail = if paste {
                    " If it runs on another machine, paste the URL it ends up on here."
                } else {
                    ""
                };
                eprintln!(
                    "Open this URL to log in (it should open by itself):\n{url}\nWaiting for the browser…{tail}"
                );
                codesmith_core::mcp::oauth::open_browser(url);
            }),
            device: Arc::new(|uri, code, complete| {
                eprintln!(
                    "Open {uri} on any device and enter the code {code}\nWaiting for authorization…"
                );
                codesmith_core::mcp::oauth::open_browser(complete.unwrap_or(uri));
            }),
            note: Arc::new(|m| eprintln!("{m}")),
            pasted: Pasted::Stdin,
            terminal: true,
        }
    }

    fn read_line(&self, prompt: &str) -> Result<Option<String>> {
        if self.terminal {
            read_line(prompt)
        } else {
            Ok(None)
        }
    }
}

async fn plugin_login(
    file: &std::path::Path,
    provider: &str,
    login: PluginLogin,
) -> Result<Option<String>> {
    plugin_login_with(file, provider, login, LoginIo::terminal()).await
}

async fn plugin_login_with(
    file: &std::path::Path,
    provider: &str,
    login: PluginLogin,
    io: LoginIo,
) -> Result<Option<String>> {
    use codesmith_core::providers;
    let PluginLogin {
        profile,
        enterprise,
        project,
        location,
        client_id,
        oauth,
        register,
        cli,
        fields,
        cwd,
        home,
    } = login;
    // Claude Code keeps its own login: check it and save its models; no
    // credential is asked for or saved.
    if provider == providers::claude_agent::ID {
        if home.is_some() {
            return Err(anyhow!(
                "Claude Code's own login is yours, not another home's: sign it in with a Claude Code token (--token)"
            ));
        }
        let loaded = config::load(&cwd)?;
        let p = loaded.config.provider(provider).unwrap_or_default();
        let state = providers::claude_agent::check(&providers::claude_agent::connection(&p)).await;
        return providers::claude_agent::describe(&state).map(Some);
    }
    if cli {
        if provider != providers::azure::ID {
            return Err(anyhow!("--cli is for azure"));
        }
        let env = providers::process_env();
        if providers::azure::find_cli(&env).is_none() {
            return Err(anyhow!(
                "the Azure CLI is not installed (no `az` on PATH): install it and run `az login`, or log in with a key"
            ));
        }
        let loaded = login_config(&cwd, home.as_deref())?;
        let settings = loaded
            .config
            .provider(provider)
            .unwrap_or_default()
            .settings;
        let mut answers = parse_fields(&fields)?;
        let resource = match answers
            .remove("resourceName")
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|r| !r.is_empty())
        {
            Some(r) => Some(r),
            None if providers::azure::key_form(&settings).fields.is_empty() => None,
            None => io
                .read_line("Azure resource (your Azure OpenAI or AI Services resource name): ")?
                .filter(|r| !r.is_empty()),
        };
        providers::azure::save_cli_login(file, resource.as_deref())?;
        return Ok(Some(format!(
            "Saved an Azure CLI login{} in {}; requests use tokens from `az`",
            resource.map(|r| format!(" for {r}")).unwrap_or_default(),
            file.display()
        )));
    }
    if oauth {
        return browser_login(
            file,
            provider,
            client_id.as_deref(),
            register,
            &fields,
            &login_config(&cwd, home.as_deref())?,
            io,
        )
        .await
        .map(Some);
    }
    if client_id.is_some() && provider != providers::copilot::ID {
        return Err(anyhow!(
            "--client-id is for github-copilot, or goes with --oauth"
        ));
    }
    if location.is_some() && project.is_none() {
        return Err(anyhow!("--location goes with --project"));
    }
    if let Some(project) = project {
        if providers::login_id(provider) != providers::vertex::ID {
            return Err(anyhow!("--project is for google-vertex"));
        }
        providers::vertex::save_login(file, &project, location.as_deref())?;
        let env = providers::process_env();
        let creds = match codesmith_ai::google::adc_file(&env) {
            Some(f) => format!("Application Default Credentials from {}", f.display()),
            None => "the metadata server (no ADC file found; run `gcloud auth application-default login`)".into(),
        };
        return Ok(Some(format!(
            "Saved Google Vertex project {project} in {} for google-vertex, using {creds}",
            location.as_deref().unwrap_or("global")
        )));
    }
    if providers::login_id(provider) == providers::vertex::ID {
        let found = providers::vertex::suggested_projects(&providers::process_env());
        return Err(anyhow!(
            "google-vertex uses Application Default Credentials: run `codesmith auth login google-vertex --project <id> [--location <loc>]`{}; or set GOOGLE_VERTEX_API_KEY for Express Mode",
            if found.is_empty() { String::new() } else { format!(" (gcloud projects: {})", found.join(", ")) }
        ));
    }
    if let Some(profile) = profile {
        if provider != providers::bedrock::ID {
            return Err(anyhow!("--profile is for amazon-bedrock"));
        }
        let known = codesmith_ai::aws::profiles(&providers::process_env());
        if !known.contains(&profile) {
            (io.note)(&format!(
                "note: no profile {profile:?} in your AWS configuration ({})",
                if known.is_empty() {
                    "none found".to_string()
                } else {
                    known.join(", ")
                }
            ));
        }
        providers::bedrock::save_profile(file, &profile)?;
        return Ok(Some(format!(
            "Saved AWS profile {profile} for amazon-bedrock in {}",
            file.display()
        )));
    }
    if provider == providers::copilot::ID {
        let loaded = login_config(&cwd, home.as_deref())?;
        let settings = loaded
            .config
            .provider(provider)
            .unwrap_or_default()
            .settings
            .plugin;
        let id = providers::copilot::client_id(
            client_id.as_deref(),
            &settings,
            &providers::process_env(),
        )?;
        let device = io.device.clone();
        providers::copilot::login(
            &reqwest::Client::new(),
            file,
            &id,
            enterprise.as_deref(),
            |code| device(&code.verification_uri, &code.user_code, None),
        )
        .await?;
        return Ok(Some(format!(
            "Logged in to GitHub Copilot{}; the login is saved in {}",
            enterprise.map(|e| format!(" on {e}")).unwrap_or_default(),
            file.display()
        )));
    }
    if enterprise.is_some() {
        return Err(anyhow!("--enterprise is for github-copilot"));
    }
    Ok(None)
}

/// A provider's browser OAuth login (`--oauth`).
async fn browser_login(
    file: &std::path::Path,
    provider: &str,
    client_id: Option<&str>,
    register: bool,
    fields: &[String],
    loaded: &config::Loaded,
    mut io: LoginIo,
) -> Result<String> {
    if register && provider != codesmith_core::providers::chatgpt::ID {
        return Err(anyhow!("--register is for openai (Sign in with ChatGPT)"));
    }
    use codesmith_core::providers;
    let answers = parse_fields(fields)?;
    let answer = |key: &str, title: &str| -> Result<Option<String>> {
        if let Some(v) = answers.get(key).and_then(|v| v.as_str()) {
            return Ok(Some(v.to_string()).filter(|v| !v.is_empty()));
        }
        Ok(io
            .read_line(&format!("{title}: "))?
            .filter(|a| !a.is_empty()))
    };
    let settings = loaded
        .config
        .provider(provider)
        .unwrap_or_default()
        .settings;
    let env = providers::process_env();
    let opener = io.open.clone();
    let open = |paste: bool| {
        let opener = opener.clone();
        move |url: &str| opener(url, paste)
    };
    let note = io.note.clone();
    let wrong_paste = |m: &str| note(&format!("{m}; still waiting"));
    let client = reqwest::Client::new();
    match provider {
        providers::poe::ID => {
            let id = providers::poe::client_id(client_id, &settings.plugin, &env)?;
            let e = providers::poe::Endpoints::poe(&id);
            let pasted = io.pasted.take();
            providers::poe::login_with(&client, file, &e, open(true), pasted, wrong_paste).await?;
        }
        providers::gitlab::ID => {
            let instance = providers::gitlab::normalize_instance(
                answers.get("instanceUrl").and_then(|v| v.as_str()),
                &env,
            )?;
            let id = providers::gitlab::client_id(client_id, &settings.plugin, &env, &instance)?;
            let e = providers::gitlab::Endpoints {
                instance,
                client_id: id,
                port: providers::gitlab::PORT,
            };
            let pasted = io.pasted.take();
            providers::gitlab::login_with(&client, file, &e, open(true), pasted, wrong_paste)
                .await?;
        }
        providers::digitalocean::ID => {
            let id = providers::digitalocean::client_id(client_id, &settings.plugin, &env)?;
            let e = providers::digitalocean::Endpoints::digitalocean(&id);
            if let Some(found) =
                providers::digitalocean::login(&client, file, &e, open(false)).await?
            {
                (io.note)(&format!("Found {found} for the account."));
            }
        }
        providers::snowflake::ID => {
            let account = answer("account", "Snowflake account (myorg-myaccount)")?
                .or_else(|| env("SNOWFLAKE_ACCOUNT"))
                .or(settings.plugin.account.clone())
                .ok_or_else(|| anyhow!("Snowflake account is required: pass --field account=…"))?;
            let role = answer("role", "Snowflake role (optional)")?;
            let e = providers::snowflake::Endpoints::for_account(
                &account,
                client_id.or(settings.plugin.client_id.as_deref()),
            );
            let pasted = io.pasted.take();
            providers::snowflake::login_with(
                &client,
                file,
                &e,
                role.as_deref(),
                open(true),
                pasted,
                wrong_paste,
            )
            .await?;
        }
        providers::chatgpt::ID => {
            let saved = codesmith_core::auth::entry_in(file, provider)
                .filter(|e| providers::chatgpt::is_login(Some(e)))
                .and_then(|e| e["metadata"]["clientID"].as_str().map(str::to_string));
            let id = providers::chatgpt::client_id(
                saved.as_deref(),
                client_id,
                register,
                &settings.plugin,
                &env,
            )?;
            if providers::chatgpt::registers(&id) {
                (io.note)(providers::chatgpt::REGISTER_NOTICE);
            }
            let e = providers::chatgpt::Endpoints::default();
            let pasted = io.pasted.take();
            providers::chatgpt::login_with(
                &client,
                file,
                &e,
                &id,
                open(true),
                pasted,
                |m: &str| note(m),
            )
            .await?;
        }
        providers::xai::ID => {
            let id = providers::xai::client_id(client_id, &settings.plugin, &env)?;
            let e = providers::xai::Endpoints::xai(&id, settings.plugin.oauth_scope.as_deref());
            let device = io.device.clone();
            providers::xai::login(&client, file, &e, |code| {
                device(
                    &code.verification_uri,
                    &code.user_code,
                    code.verification_uri_complete.as_deref(),
                )
            })
            .await?;
        }
        p => {
            return Err(anyhow!(
                "{p} has no browser login; --oauth is for openai (Sign in with ChatGPT), poe, gitlab, digitalocean, snowflake-cortex and xai"
            ))
        }
    }
    Ok(format!(
        "Logged in to {provider}; the login is saved in {}",
        file.display()
    ))
}

/// Save `key` for `provider` with its key form's answers: those `given`,
/// the rest asked in the terminal when `terminal` (else a required one
/// left out is an error).
fn save_key_login(
    file: &std::path::Path,
    loaded: &config::Loaded,
    provider: &str,
    key: &str,
    mut given: serde_json::Map<String, serde_json::Value>,
    terminal: bool,
) -> Result<String> {
    use codesmith_core::auth;
    if key.is_empty() {
        return Err(anyhow!("no key entered"));
    }
    let form = codesmith_core::providers::key_form(
        provider,
        &loaded
            .config
            .provider(provider)
            .unwrap_or_default()
            .settings,
        &codesmith_core::providers::process_env(),
    );
    given.retain(|_, v| v.as_str().is_none_or(|s| !s.is_empty()));
    match form {
        Some(form) => {
            for field in &form.fields {
                if given.contains_key(field.key) {
                    continue;
                }
                let answer = if terminal {
                    read_line(&format!("{} ({}): ", field.title, field.placeholder))?
                } else {
                    None
                };
                match answer {
                    Some(a) if !a.is_empty() => {
                        given.insert(field.key.to_string(), a.into());
                    }
                    _ if field.required => {
                        return Err(anyhow!(
                            "{provider} needs {}: pass --field {}=…",
                            field.key,
                            field.key
                        ))
                    }
                    _ => {}
                }
            }
            codesmith_core::providers::save_key(file, provider, key, given)?;
        }
        None if !given.is_empty() => {
            return Err(anyhow!(
                "{provider}'s login asks for nothing beside the key"
            ))
        }
        None => auth::set_in(file, provider, key)?,
    }
    let mut done = format!("Saved a key for {provider} in {}", file.display());
    if loaded.config.provider(provider).is_none() {
        done.push_str(&format!("\nnote: {provider} is not a built-in provider; add it under \"providers\" in codesmith.json to use it"));
    }
    Ok(done)
}

/// A sign-in the picker asks for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoginRequest {
    pub provider: String,
    pub kind: codesmith_core::providers::signin::Kind,
    /// The key, for [`Kind::Key`](codesmith_core::providers::signin::Kind::Key).
    pub key: Option<Secret>,
    /// The method's other answers, by field key.
    pub answers: std::collections::BTreeMap<String, String>,
    /// The client id of the person's own OAuth app, when typed in.
    pub client_id: Option<String>,
}

/// Run a sign-in the picker asked for, through the same logins as
/// `codesmith auth login`; what it says when done.
pub(crate) async fn run_login(
    file: &std::path::Path,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    req: LoginRequest,
    io: LoginIo,
) -> Result<String> {
    use codesmith_core::providers::{self, signin::Kind};
    let LoginRequest {
        provider,
        kind,
        key,
        mut answers,
        client_id,
    } = req;
    let fields = |answers: &std::collections::BTreeMap<String, String>| -> Vec<String> {
        answers
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{k}={v}"))
            .collect()
    };
    let base = PluginLogin {
        cwd: cwd.to_path_buf(),
        home: home.map(std::path::Path::to_path_buf),
        ..Default::default()
    };
    let login = match kind {
        Kind::Key => {
            let key = key.ok_or_else(|| anyhow!("no key entered"))?;
            let loaded = login_config(cwd, home)?;
            let given = answers
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            return save_key_login(file, &loaded, &provider, key.0.trim(), given, io.terminal);
        }
        Kind::ClaudeCode => {
            return plugin_login_with(file, providers::claude_agent::ID, base, io)
                .await?
                .ok_or_else(|| anyhow!("Claude Code could not be checked"));
        }
        // The token was taken from `claude setup-token` or pasted.
        Kind::ClaudeToken { .. } => {
            let key = key.ok_or_else(|| anyhow!("no token entered"))?;
            let done = crate::claude_token::save(file, &key.0)?;
            // Only this process's Claude Code signs in with your own token.
            if home.is_none() {
                crate::claude_token::restart_claude_code().await;
            }
            return Ok(done);
        }
        Kind::Browser { .. } | Kind::Device if provider == providers::copilot::ID => {
            PluginLogin { client_id, ..base }
        }
        Kind::Browser { .. } | Kind::Device => PluginLogin {
            client_id,
            oauth: true,
            fields: fields(&answers),
            ..base
        },
        Kind::AwsProfile => PluginLogin {
            profile: answers.remove("profile").filter(|p| !p.is_empty()),
            ..base
        },
        Kind::VertexProject => PluginLogin {
            project: answers.remove("project").filter(|p| !p.is_empty()),
            location: answers.remove("location").filter(|p| !p.is_empty()),
            ..base
        },
        Kind::AzureCli => PluginLogin {
            cli: true,
            fields: fields(&answers),
            ..base
        },
    };
    plugin_login_with(file, &provider, login, io)
        .await?
        .ok_or_else(|| anyhow!("{provider} has no such login"))
}

/// A Claude Code token for `codesmith auth login`: from `claude
/// setup-token` run here when `create`, else (or when none was found)
/// pasted at a terminal or read from standard input.
fn read_token(create: bool) -> Result<String> {
    let terminal = std::io::stdin().is_terminal();
    if create && terminal {
        if let Some(t) = crate::claude_token::create_on_terminal()? {
            return Ok(t);
        }
        eprintln!("CodeSmith couldn't find the token in Claude Code's output; paste it instead.");
    }
    if terminal {
        return read_secret(
            "Claude Code token (paste the one `claude setup-token` printed; it is not shown): ",
        );
    }
    let mut s = String::new();
    std::io::stdin().take(8192).read_to_string(&mut s)?;
    Ok(s)
}

/// `codesmith auth login claude-agent --token`.
fn token_login(file: &std::path::Path) -> Result<String> {
    let t = read_token(true)?;
    crate::claude_token::save(file, &t)
}

/// Sign out of `provider`: its saved login removed.
pub(crate) fn logout(file: &std::path::Path, provider: &str) -> Result<String> {
    if provider == codesmith_core::providers::claude_agent::ID
        && codesmith_core::providers::claude_agent::remove_token_in(file)?
    {
        return Ok(
            "Removed your saved Claude Code token. Claude Code signs in with its own login again."
                .into(),
        );
    }
    Ok(if codesmith_core::auth::remove_in(file, provider)? {
        format!("Removed the saved key for {provider}")
    } else {
        format!("No saved key for {provider}")
    })
}

/// Text from outside (a provider name, an error) made safe to print:
/// control characters, escape sequences included, are dropped.
fn printable(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}

/// The providers and how to sign in to each, for `codesmith auth login`
/// without a provider: printed when there is no terminal to ask in.
pub(crate) fn login_guidance(choices: &[codesmith_core::providers::signin::Choice]) -> String {
    use codesmith_core::providers::signin::Kind;
    let mut out = String::from("Providers you can sign in to:\n");
    for c in choices {
        let how: Vec<String> = c
            .methods
            .iter()
            .map(|m| match m.kind {
                Kind::Key => format!("codesmith auth login {}", c.id),
                Kind::Browser { .. } | Kind::Device if c.id == "github-copilot" => {
                    format!("codesmith auth login {}", c.id)
                }
                Kind::Browser { .. } | Kind::Device => {
                    format!("codesmith auth login {} --oauth", c.id)
                }
                Kind::ClaudeCode => "codesmith auth login claude-agent".into(),
                Kind::ClaudeToken { .. } => "codesmith auth login claude-agent --token".into(),
                Kind::AwsProfile => format!("codesmith auth login {} --profile <name>", c.id),
                Kind::VertexProject => {
                    format!("codesmith auth login {} --project <id>", c.id)
                }
                Kind::AzureCli => format!("codesmith auth login {} --cli", c.id),
            })
            .collect();
        let how = if how.is_empty() {
            c.note.clone().unwrap_or_default()
        } else {
            how.join("  |  ")
        };
        out.push_str(&format!(
            "  {:<24} {:<34} {}\n",
            printable(&c.id),
            printable(&c.state.describe()),
            printable(&how)
        ));
    }
    out.push_str(
        "Run `codesmith auth login` in a terminal to pick one, or one of the commands above.",
    );
    out
}

/// A number or an id typed at a numbered list.
fn pick_index(answer: &str, ids: &[&str]) -> Option<usize> {
    let a = answer.trim();
    a.parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .filter(|i| *i < ids.len())
        .or_else(|| ids.iter().position(|id| id.eq_ignore_ascii_case(a)))
}

/// Where `codesmith auth login` puts what it signs in to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    /// Your own auth store.
    Own,
    /// Another home's auth store (`--home`), such as a tailbus bridge's.
    Home(PathBuf),
    /// A file of its own (`--to-file`), for a bridge's `secrets`.
    File(PathBuf),
}

impl Target {
    fn from_args(
        cwd: &std::path::Path,
        home: Option<PathBuf>,
        to_file: Option<PathBuf>,
    ) -> Result<Target> {
        Ok(match (home, to_file) {
            (Some(h), _) => Target::Home(codesmith_core::home::resolve(&absolute(cwd, &h), cwd)?),
            (None, Some(f)) => Target::File(absolute(cwd, &f)),
            (None, None) => Target::Own,
        })
    }

    /// The auth store logins go in (none for a file).
    fn auth_file(&self) -> Option<PathBuf> {
        match self {
            Target::Own => Some(paths::auth_file()),
            Target::Home(h) => Some(codesmith_core::home::auth_file(h)),
            Target::File(_) => None,
        }
    }

    fn home(&self) -> Option<&std::path::Path> {
        match self {
            Target::Home(h) => Some(h),
            _ => None,
        }
    }
}

/// `p` against `cwd` when it's relative, with a leading `~/` as your home.
pub(crate) fn absolute(cwd: &std::path::Path, p: &std::path::Path) -> PathBuf {
    let text = p.to_string_lossy();
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            return PathBuf::from(h).join(rest);
        }
    }
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Your own config without any project's, for what decides nothing but
/// what a login is called and which variable a key goes in.
fn own_config(cwd: &std::path::Path) -> Result<config::Loaded> {
    config::load_from(
        cwd,
        &config::Sources {
            project: false,
            ..config::Sources::from_env()
        },
    )
}

/// No environment: what a tailbus bridge's run sees of yours.
pub(crate) fn no_env() -> codesmith_core::providers::Env {
    Arc::new(|_: &str| None)
}

/// Whether a way in makes a login another home can use on its own: a key,
/// a Claude Code token, or a provider's OAuth (into that home's auth
/// store; a file holds only a key or a token, and only a key that needs
/// no other answer). Claude Code's own login, an AWS profile, Google's
/// Application Default Credentials and the Azure CLI rest on files in
/// your own home, so they aren't offered.
pub(crate) fn portable(
    kind: codesmith_core::providers::signin::Kind,
    to_file: bool,
    asks_more: bool,
) -> bool {
    use codesmith_core::providers::signin::Kind;
    match kind {
        Kind::Key => !(to_file && asks_more),
        Kind::ClaudeToken { .. } => true,
        Kind::Browser { .. } | Kind::Device => !to_file,
        Kind::ClaudeCode | Kind::AwsProfile | Kind::VertexProject | Kind::AzureCli => false,
    }
}

/// `choices` with only the [`portable`] ways in, and without the
/// providers that have none left.
pub(crate) fn portable_choices(
    choices: Vec<codesmith_core::providers::signin::Choice>,
    to_file: bool,
) -> Vec<codesmith_core::providers::signin::Choice> {
    choices
        .into_iter()
        .filter_map(|mut c| {
            c.methods
                .retain(|m| portable(m.kind, to_file, m.fields.iter().any(|f| f.required)));
            (!c.methods.is_empty()).then_some(c)
        })
        .collect()
}

/// The environment variable a run reads `provider`'s key from, which a
/// bridge's `secrets` names: Claude Code's token variable for claude-agent.
pub(crate) fn key_var(c: &config::Config, provider: &str) -> Option<String> {
    if provider == codesmith_core::providers::claude_agent::ID {
        return Some(codesmith_ai::claude_agent::token::VAR.to_string());
    }
    c.provider(provider).and_then(|p| p.env.first().cloned())
}

/// Whether `a` is a yes.
pub(crate) fn is_yes(a: &str) -> bool {
    let a = a.trim();
    a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")
}

/// The value of the environment variable `name` (a leading `$` allowed),
/// which is never shown.
pub(crate) fn key_from_env(name: &str) -> Result<String> {
    let name = name.trim().trim_start_matches('$');
    let plain = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !plain {
        return Err(anyhow!(
            "{:?} isn't an environment variable's name",
            printable(name)
        ));
    }
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        _ => Err(anyhow!("${name} isn't set here, or is empty")),
    }
}

/// A key for `name`, at a terminal: taken from an environment variable
/// the person names, or pasted unseen (after offering its key page).
/// Without a terminal, read from stdin.
pub(crate) fn ask_key(name: &str, key_page: Option<&str>, var: Option<&str>) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        return read_secret("");
    }
    let hint = var.map(|v| format!(" (such as {v})")).unwrap_or_default();
    let from = read_line(&format!(
        "Take the key from an environment variable? Type its name{hint}, or press Enter to paste the key: "
    ))?
    .unwrap_or_default();
    if !from.trim().is_empty() {
        let k = key_from_env(&from)?;
        eprintln!(
            "Took the key from ${} (not shown).",
            from.trim().trim_start_matches('$')
        );
        return Ok(k);
    }
    if let Some(url) = key_page {
        eprintln!("Create a key at {url}");
        let open = read_line("Open it in the browser? [y/N] ")?.unwrap_or_default();
        if is_yes(&open) {
            codesmith_core::mcp::oauth::open_browser(url);
        }
    }
    read_secret(&format!("API key for {name} (it is not shown): "))
}

/// Whether `path` may take a credential: `false` when nothing is there,
/// `true` when a file is and you agree at a terminal to replace it. Asked
/// before the key is; without a terminal an existing file is refused.
pub(crate) fn may_write(path: &std::path::Path) -> Result<bool> {
    if !codesmith_core::home::secret_exists(path)? {
        return Ok(false);
    }
    match read_line(&format!(
        "{} already exists. Replace it? [y/N] ",
        path.display()
    ))? {
        Some(a) if is_yes(&a) => Ok(true),
        Some(_) => Err(anyhow!("{} was left as it was", path.display())),
        None => Err(anyhow!(
            "{} already exists: remove it first, or run this at a terminal to replace it",
            path.display()
        )),
    }
}

/// Write `secret` for `provider` to `path`; what to say when done (never
/// the secret).
pub(crate) fn save_to_file(
    path: &std::path::Path,
    provider: &str,
    var: Option<&str>,
    secret: &str,
    replace: bool,
    what: &str,
) -> Result<String> {
    codesmith_core::home::write_secret(path, secret, replace)?;
    let mut done = format!(
        "Wrote the {what} for {provider} to {} (readable only by you).",
        path.display()
    );
    if let Some(v) = var {
        done.push_str(&format!(
            "\nA tailbus bridge reads it with this in its table:\n  [bridges.NAME.secrets]\n  {v} = {}",
            crate::bridge_init::toml_str(&path.display().to_string())
        ));
    }
    Ok(done)
}

/// `codesmith auth login PROVIDER --to-file PATH`: a key (or for
/// claude-agent a Claude Code token) written to `path` alone.
fn file_login(
    cwd: &std::path::Path,
    provider: &str,
    path: &std::path::Path,
    token: bool,
) -> Result<String> {
    use codesmith_core::providers::{self, claude_agent, signin};
    let is_token = token || provider == claude_agent::ID;
    if is_token && provider != claude_agent::ID {
        return Err(anyhow!("--token is for claude-agent (Claude Code)"));
    }
    let loaded = own_config(cwd)?;
    let p = loaded.config.provider(provider).unwrap_or_default();
    if !is_token {
        let form = providers::key_form(provider, &p.settings, &no_env());
        if let Some(f) = form
            .as_ref()
            .and_then(|f| f.fields.iter().find(|f| f.required))
        {
            return Err(anyhow!(
                "{provider}'s login asks for {} beside the key, which a file can't hold: use --home DIR to save it in a bridge's home",
                f.key
            ));
        }
        let methods = signin::methods(provider, &p, &no_env());
        if !methods.is_empty() && !methods.iter().any(|m| m.kind == signin::Kind::Key) {
            return Err(anyhow!(
                "{provider} doesn't sign in with a key, and --to-file takes a key or a Claude Code token: use --home DIR to save its login in a bridge's home"
            ));
        }
    }
    let replace = may_write(path)?;
    if is_token {
        let t = read_token(true)?;
        let t = codesmith_ai::claude_agent::token::validate(&t).map_err(anyhow::Error::msg)?;
        return save_to_file(
            path,
            claude_agent::ID,
            Some(codesmith_ai::claude_agent::token::VAR),
            &t,
            replace,
            "Claude Code token",
        );
    }
    let var = key_var(&loaded.config, provider);
    let key = ask_key(provider, signin::key_page(provider), var.as_deref())?;
    save_to_file(path, provider, var.as_deref(), &key, replace, "key")
}

/// `codesmith auth login` without a provider: the sign-in picker in the
/// terminal (a numbered list), or the list and how to sign in when there
/// is no terminal to ask in. For your own auth store it ends with a way
/// to sign in for a bridge or another home.
async fn login_picker(cwd: &std::path::Path, target: &Target) -> Result<()> {
    use codesmith_core::providers::{self, signin};
    let (loaded, file, env) = match target {
        Target::Own => (
            config::load(cwd)?,
            paths::auth_file(),
            providers::process_env(),
        ),
        Target::Home(h) => (
            config::load_home(h)?,
            codesmith_core::home::auth_file(h),
            no_env(),
        ),
        Target::File(_) => (own_config(cwd)?, PathBuf::new(), no_env()),
    };
    let mut choices = signin::choices(&loaded.config, &file, &env);
    if *target != Target::Own {
        choices = portable_choices(choices, matches!(target, Target::File(_)));
    }
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        println!("{}", login_guidance(&choices));
        match target {
            Target::Own => {}
            Target::Home(h) => println!(
                "Add --home {} to the command.",
                codesmith_core::retry::shell_word(&h.display().to_string())
            ),
            Target::File(p) => println!(
                "Add --to-file {} to the command.",
                codesmith_core::retry::shell_word(&p.display().to_string())
            ),
        }
        return Err(anyhow!(
            "Pass a provider id when running without an interactive terminal"
        ));
    }
    match target {
        Target::Own => {}
        Target::Home(h) => eprintln!(
            "Signing in for the CodeSmith home {}: the login goes in {}.",
            h.display(),
            file.display()
        ),
        Target::File(p) => eprintln!(
            "The key or Claude Code token goes in {}, readable only by you.",
            p.display()
        ),
    }
    for (i, c) in choices.iter().enumerate() {
        let tag = if codesmith_core::providers::verified::verified(&c.id) {
            String::new()
        } else {
            format!(" · {}", codesmith_core::providers::verified::TAG)
        };
        let state = match target {
            Target::File(_) => String::new(),
            _ => printable(&c.state.describe()),
        };
        eprintln!("  {:>2}. {:<30} {state}{tag}", i + 1, printable(&c.name),);
    }
    let elsewhere = (*target == Target::Own).then_some(choices.len() + 1);
    if let Some(n) = elsewhere {
        eprintln!("  {n:>2}. {ELSEWHERE_LABEL}");
    }
    let ids: Vec<&str> = choices.iter().map(|c| c.id.as_str()).collect();
    let answer = read_line("Provider to sign in to (number or id): ")?.unwrap_or_default();
    if elsewhere.is_some_and(|n| answer.trim() == n.to_string()) {
        return Box::pin(elsewhere_picker(cwd)).await;
    }
    let c = pick_index(&answer, &ids)
        .map(|i| &choices[i])
        .ok_or_else(|| anyhow!("no provider {:?}", printable(&answer)))?;
    if let Some(w) = codesmith_core::providers::verified::warning(&c.id, &printable(&c.name)) {
        eprintln!("warning: {w}");
    }
    if c.methods.is_empty() {
        return Err(anyhow!(
            "{}",
            c.note
                .clone()
                .unwrap_or_else(|| "nothing to sign in to".into())
        ));
    }
    // The ways in, then signing out when a login is saved.
    let mut rows: Vec<String> = c.methods.iter().map(|m| m.label.clone()).collect();
    if c.state.saved() && !matches!(target, Target::File(_)) {
        rows.push("Sign out".into());
    }
    let pick = if rows.len() == 1 {
        0
    } else {
        for (i, r) in rows.iter().enumerate() {
            eprintln!("  {}. {r}", i + 1);
        }
        let answer = read_line("How (number): ")?.unwrap_or_default();
        let labels: Vec<&str> = rows.iter().map(String::as_str).collect();
        pick_index(&answer, &labels).ok_or_else(|| anyhow!("no choice {:?}", printable(&answer)))?
    };
    if pick == c.methods.len() {
        println!("{}", logout(&file, &c.id)?);
        return Ok(());
    }
    let m = &c.methods[pick];
    if let Target::File(path) = target {
        let replace = may_write(path)?;
        let done = match m.kind {
            signin::Kind::ClaudeToken { create } => {
                let t = read_token(create)?;
                let t =
                    codesmith_ai::claude_agent::token::validate(&t).map_err(anyhow::Error::msg)?;
                save_to_file(
                    path,
                    codesmith_core::providers::claude_agent::ID,
                    Some(codesmith_ai::claude_agent::token::VAR),
                    &t,
                    replace,
                    "Claude Code token",
                )?
            }
            _ => {
                let var = key_var(&loaded.config, &c.id);
                let key = ask_key(&printable(&c.name), c.key_page, var.as_deref())?;
                save_to_file(path, &c.id, var.as_deref(), &key, replace, "key")?
            }
        };
        println!("{done}");
        return Ok(());
    }
    let p = loaded.config.provider(&c.id).unwrap_or_default();
    let mut client_id = None;
    if matches!(m.kind, signin::Kind::Browser { .. } | signin::Kind::Device) {
        if let Some(why) = signin::client_id_missing(&c.id, &p, &file, &providers::process_env()) {
            eprintln!("{}", printable(&why));
            client_id = read_line("Client id of your OAuth app (empty to cancel): ")?
                .filter(|s| !s.is_empty());
            if client_id.is_none() {
                return Err(anyhow!("no client id entered"));
            }
        }
    }
    let mut answers = std::collections::BTreeMap::new();
    for f in &m.fields {
        let a = read_line(&format!("{} ({}): ", f.title, f.placeholder))?.unwrap_or_default();
        if a.is_empty() && f.required {
            return Err(anyhow!("{} is required", f.title));
        }
        answers.insert(f.key.to_string(), a);
    }
    let key = match m.kind {
        signin::Kind::ClaudeToken { create } => Some(Secret(read_token(create)?)),
        signin::Kind::Key if *target != Target::Own => {
            let var = key_var(&loaded.config, &c.id);
            Some(Secret(ask_key(
                &printable(&c.name),
                c.key_page,
                var.as_deref(),
            )?))
        }
        signin::Kind::Key => {
            if let Some(url) = c.key_page {
                eprintln!("Create a key at {url}");
                let open = read_line("Open it in the browser? [y/N] ")?.unwrap_or_default();
                if is_yes(&open) {
                    codesmith_core::mcp::oauth::open_browser(url);
                }
            }
            Some(Secret(read_secret(&format!("API key for {}: ", c.id))?))
        }
        _ => None,
    };
    // Claude Code's login is Claude Agent's, whichever entry offered it.
    let signed_in = match m.kind {
        signin::Kind::ClaudeCode | signin::Kind::ClaudeToken { .. } => {
            codesmith_core::providers::claude_agent::ID.to_string()
        }
        _ => c.id.clone(),
    };
    let req = LoginRequest {
        provider: c.id.clone(),
        kind: m.kind,
        key,
        answers,
        client_id,
    };
    if let Target::Home(h) = target {
        paths::private_dir(&codesmith_core::home::data_dir(h))?;
    }
    let done = run_login(&file, cwd, target.home(), req, LoginIo::terminal()).await?;
    println!("{done}");
    match target {
        Target::Own => adopt_after_login(cwd, &signed_in),
        _ if !done.contains(&file.display().to_string()) => {
            println!("It is saved in {}.", file.display())
        }
        _ => {}
    }
    Ok(())
}

/// The sign-in picker's row for signing in elsewhere.
pub(crate) const ELSEWHERE_LABEL: &str = "For a bridge or another home…";

/// The sign-in picker's ways to sign in elsewhere: a whole tailbus
/// bridge, a login in another home, or a key in a file.
async fn elsewhere_picker(cwd: &std::path::Path) -> Result<()> {
    eprintln!("  1. Set up a tailbus bridge (codesmith bridge init)");
    eprintln!("  2. Save a login in another CodeSmith home, such as a bridge's (--home)");
    eprintln!(
        "  3. Write a key or Claude Code token to a file, for a bridge's secrets (--to-file)"
    );
    let answer = read_line("Which (number): ")?.unwrap_or_default();
    match answer.trim() {
        "1" => {
            let name = read_line("Bridge name (lowercase letters, digits and hyphens): ")?
                .unwrap_or_default();
            crate::bridge_init::run(cwd, &name, None).await
        }
        "2" => {
            let dir = read_line("The home folder (it must exist): ")?.unwrap_or_default();
            let h = codesmith_core::home::resolve(&absolute(cwd, std::path::Path::new(&dir)), cwd)?;
            Box::pin(login_picker(cwd, &Target::Home(h))).await
        }
        "3" => {
            let file = read_line("The file to write it to: ")?.unwrap_or_default();
            if file.trim().is_empty() {
                return Err(anyhow!("no file entered"));
            }
            let path = absolute(cwd, std::path::Path::new(file.trim()));
            Box::pin(login_picker(cwd, &Target::File(path))).await
        }
        a => Err(anyhow!("no choice {:?}", printable(a))),
    }
}

/// After a login: when no model is chosen in codesmith.json, make the
/// provider's newest model the default (see
/// [`codesmith_core::model_spec::adopt_login`]), and say so.
fn adopt_after_login(cwd: &std::path::Path, provider: &str) {
    let Ok(loaded) = config::load(cwd) else {
        return;
    };
    let access = codesmith_core::model_spec::Access::current(&loaded.config);
    let Some(a) = codesmith_core::model_spec::adopt_login(
        &loaded.config,
        provider,
        &access,
        &paths::config_dir(),
        &paths::data_dir(),
    ) else {
        return;
    };
    match a.saved {
        Ok(path) => println!("Default model: {} (saved in {})", a.spec, path.display()),
        Err(e) => println!(
            "Using {} needs \"model\": {:?} in codesmith.json; it was not saved: {e}",
            a.spec, a.spec
        ),
    }
}

/// Say on stderr that `provider` hasn't been tested with CodeSmith, when
/// it hasn't (see `providers::verified`).
fn warn_untested(provider: &str) {
    if let Some(w) = codesmith_core::providers::verified::warning(provider, provider) {
        eprintln!("warning: {w}");
    }
}

async fn auth_cmd(cwd: &std::path::Path, what: AuthCmd) -> Result<()> {
    use codesmith_core::auth;
    let file = paths::auth_file();
    match what {
        AuthCmd::Login {
            provider: None,
            profile,
            enterprise,
            project,
            location,
            client_id,
            oauth,
            register,
            cli,
            fields,
            token,
            home,
            to_file,
        } => {
            let flagged = token
                || profile.is_some()
                || enterprise.is_some()
                || project.is_some()
                || location.is_some()
                || client_id.is_some()
                || oauth
                || register
                || cli
                || !fields.is_empty();
            if flagged {
                return Err(anyhow!("name the provider these options are for"));
            }
            let target = Target::from_args(cwd, home, to_file)?;
            login_picker(cwd, &target).await?;
        }
        AuthCmd::Login {
            provider: Some(provider),
            profile,
            enterprise,
            project,
            location,
            client_id,
            oauth,
            register,
            cli,
            fields,
            token,
            home,
            to_file,
        } => {
            let target = Target::from_args(cwd, home, to_file)?;
            if let Target::File(path) = &target {
                let other = profile.is_some()
                    || enterprise.is_some()
                    || project.is_some()
                    || location.is_some()
                    || client_id.is_some()
                    || oauth
                    || register
                    || cli
                    || !fields.is_empty();
                if other {
                    return Err(anyhow!(
                        "--to-file takes a key or a Claude Code token; for a login with OAuth or other answers, use --home DIR"
                    ));
                }
                warn_untested(&provider);
                println!("{}", file_login(cwd, &provider, path, token)?);
                return Ok(());
            }
            let elsewhere = target.home().map(std::path::Path::to_path_buf);
            let file = target.auth_file().unwrap_or(file);
            if let Some(h) = &elsewhere {
                paths::private_dir(&codesmith_core::home::data_dir(h))?;
            }
            warn_untested(&provider);
            // Another home can't use your Claude Code login: only a token.
            let token = token
                || (elsewhere.is_some() && provider == codesmith_core::providers::claude_agent::ID);
            if token {
                if provider != codesmith_core::providers::claude_agent::ID {
                    return Err(anyhow!("--token is for claude-agent (Claude Code)"));
                }
                println!("{}", token_login(&file)?);
                match &elsewhere {
                    None => {
                        crate::claude_token::restart_claude_code().await;
                        adopt_after_login(cwd, &provider);
                    }
                    Some(_) => println!("It is saved in {}.", file.display()),
                }
                return Ok(());
            }
            let login = PluginLogin {
                profile,
                enterprise,
                project,
                location,
                client_id,
                oauth,
                register,
                cli,
                fields: fields.clone(),
                cwd: cwd.to_path_buf(),
                home: elsewhere.clone(),
            };
            if let Some(done) = plugin_login(&file, &provider, login).await? {
                println!("{done}");
                if elsewhere.is_none() {
                    adopt_after_login(cwd, &provider);
                }
                return Ok(());
            }
            let loaded = login_config(cwd, elsewhere.as_deref())?;
            let env = match elsewhere {
                Some(_) => no_env(),
                None => codesmith_core::providers::process_env(),
            };
            let form = codesmith_core::providers::key_form(
                &provider,
                &loaded
                    .config
                    .provider(&provider)
                    .unwrap_or_default()
                    .settings,
                &env,
            );
            let given = parse_fields(&fields)?;
            if form.is_none() && !given.is_empty() {
                return Err(anyhow!(
                    "{provider}'s login asks for nothing beside the key"
                ));
            }
            let label = form.as_ref().map_or("API key", |f| f.label);
            let key = match &elsewhere {
                Some(_) => {
                    let var = key_var(&loaded.config, &provider);
                    ask_key(
                        &provider,
                        codesmith_core::providers::signin::key_page(&provider),
                        var.as_deref(),
                    )?
                }
                None => read_secret(&format!("{label} for {provider}: "))?,
            };
            println!(
                "{}",
                save_key_login(&file, &loaded, &provider, &key, given, true)?
            );
            if elsewhere.is_none() {
                adopt_after_login(cwd, &provider);
            }
        }
        AuthCmd::List => {
            let loaded = config::load(cwd)?;
            let c = &loaded.config;
            let saved = auth::providers_in(&file);
            let mut ids: Vec<String> = config::BUILTIN.iter().map(|b| b.0.to_string()).collect();
            ids.extend(
                codesmith_core::providers::PROVIDERS
                    .iter()
                    .map(|p| p.0.to_string()),
            );
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
                } else if let Some(login) = codesmith_core::providers::describe_login(
                    &id,
                    auth::entry_in(&file, &id).as_ref(),
                    &p.settings.plugin,
                ) {
                    login
                } else if p.env.is_empty() {
                    "none needed".into()
                } else {
                    "—".into()
                };
                let tag = if codesmith_core::providers::verified::verified(&id) {
                    String::new()
                } else {
                    format!(" · {}", codesmith_core::providers::verified::TAG)
                };
                println!("{id:<12} {source}{tag}");
            }
        }
        AuthCmd::Logout { provider } => {
            println!("{}", logout(&file, &provider)?);
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
        for (name, s) in &mcp.statuses() {
            match s {
                codesmith_core::mcp::Status::Failed(e) => eprintln!("\x1b[33mwarning:\x1b[0m MCP server {name} failed to start: {}", crate::render::safe(e)),
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
    let mut builtins = command::builtins(&loaded.root);
    builtins.extend(command::plugin_commands(&loaded.config));
    command::discover(
        builtins,
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

/// `$CODESMITH_SERVER_TOKEN`, taken out of the environment at startup.
static ENV_TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The shell daemon a previous server handed over (see
/// `codesmith_core::daemon`), taken out of the environment at startup.
#[cfg(unix)]
static SHELL_HANDOFF: std::sync::OnceLock<codesmith_core::daemon::Handoff> =
    std::sync::OnceLock::new();

/// Take `CODESMITH_SHELLD_HANDOFF` out of the environment before any thread
/// starts, so no command inherits it.
pub(crate) fn take_shell_handoff() {
    #[cfg(unix)]
    match codesmith_core::daemon::take_handoff() {
        Ok(Some(h)) => {
            let _ = SHELL_HANDOFF.set(h);
        }
        Ok(None) => {}
        Err(_) => eprintln!(
            "ignoring an invalid shell daemon handoff; commands and terminals start fresh"
        ),
    }
}

/// Move `$CODESMITH_SERVER_TOKEN` out of this process's environment, where
/// every child would inherit it, to where only `serve` reads it. Call it
/// before any other thread starts: changing the environment races with
/// threads reading it.
pub(crate) fn take_server_token() {
    let var = codesmith_core::SERVER_TOKEN_VAR;
    if let Some(t) = std::env::var_os(var) {
        if let Some(t) = t.to_str() {
            let _ = ENV_TOKEN.set(t.to_string());
        }
        std::env::remove_var(var);
    }
}

async fn generate_cmd(
    cwd: &std::path::Path,
    prompt: String,
    model: Option<String>,
    session_id: Option<String>,
    image: Option<PathBuf>,
    questions: Option<PathBuf>,
) -> Result<()> {
    use codesmith_core::generate;
    if prompt.trim().is_empty() {
        return Err(anyhow!("give a prompt"));
    }
    let loaded = config::load(cwd)?;
    let client = reqwest::Client::new();
    if let Some(file) = questions {
        let model = model.ok_or_else(|| anyhow!("--questions needs --model provider/model"))?;
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let questions = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a list of decision questions", file.display()))?;
        let result = generate::decisions(
            &loaded.config,
            &client,
            &generate::DecisionInput {
                model,
                input: vec![codesmith_ai::decision::Part::Text(prompt)],
                questions,
            },
            Default::default(),
        )
        .await?;
        let mut out = serde_json::json!({ "answers": result.answers });
        if let Some(c) = result.cost {
            out["cost"] = serde_json::json!(c);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if let Some(out) = image {
        let model = model.ok_or_else(|| anyhow!("--image needs --model provider/model"))?;
        let result = generate::image(
            &loaded.config,
            &client,
            &generate::ImageInput {
                model,
                prompt,
                n: Some(1),
                ..Default::default()
            },
            Default::default(),
        )
        .await?;
        let img = result
            .images
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("the model returned no image"))?;
        let ext = codesmith_core::tool::image_generate::extension(&img.mime);
        let file = PathBuf::from(format!("{}{ext}", out.with_extension("").display()));
        std::fs::write(&file, &img.data).with_context(|| format!("writing {}", file.display()))?;
        println!("{}", file.display());
        return Ok(());
    }
    let text = match session_id {
        Some(id) => {
            let mut s = Session::new(
                &loaded,
                cwd.to_path_buf(),
                Options {
                    agent: None,
                    model,
                    resume: Some(id),
                    title: None,
                    parent: None,
                    id: None,
                    questioner: None,
                    mcp: None,
                },
                Arc::new(Fixed(Reply::Reject(None))),
                Some(open_store()?),
            )?;
            s.generate(&prompt).await?
        }
        None => {
            generate::text(
                &loaded.config,
                &client,
                model.as_deref(),
                &prompt,
                Default::default(),
            )
            .await?
            .0
        }
    };
    println!("{text}");
    Ok(())
}

pub(crate) fn open_store() -> Result<Store> {
    let path = store::default_path();
    Store::open(&path).with_context(|| format!("session store {}", path.display()))
}

/// Whether `command` may ask about trusting the project's config: the
/// interactive UI and the commands that run sessions or reach providers
/// and MCP servers. Servers, editors and automations never ask.
fn asks_trust(command: &Option<Command>) -> bool {
    matches!(
        command,
        None | Some(Command::Run(_))
            | Some(Command::Review { .. })
            | Some(Command::Calibrate { .. })
            | Some(Command::Generate { .. })
            | Some(Command::Models { .. })
            | Some(Command::Mcp { .. })
            | Some(Command::Commands)
            | Some(Command::Worktree { .. })
    )
}

/// Warn before a session starts outside any repository (see
/// [`codesmith_core::folder`]), and ask at a terminal. A yes for a loose
/// folder is remembered; the home folder and `/` are asked about every
/// time, and with no terminal to ask they're refused. With no terminal, a
/// loose folder is a warning on stderr.
pub(crate) fn folder_check(cwd: &std::path::Path, any_folder: bool) -> Result<()> {
    use codesmith_core::folder::{self, Place};
    if any_folder {
        return Ok(());
    }
    let place = folder::place(cwd);
    let Some(warning) = folder::warning(&place) else {
        return Ok(());
    };
    let store = folder::Accepted::default_store();
    if let Place::Loose(d) = &place {
        // Your own config only: a folder can't silence the warning about
        // itself.
        let own = config::Sources {
            project: false,
            ..config::Sources::from_env()
        };
        let off = config::load_from(cwd, &own)
            .ok()
            .and_then(|l| l.config.repo_warning)
            == Some(false);
        if off || store.has(d) {
            return Ok(());
        }
    }
    let tty = if std::io::stderr().is_terminal() {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()
    } else {
        None
    };
    let Some(mut tty) = tty else {
        if let Place::Broad(_) = place {
            return Err(anyhow!(
                "{warning}\nNot starting here with no terminal to confirm it. \
                 Start in a project folder, or pass --any-folder."
            ));
        }
        eprintln!("\x1b[33mwarning:\x1b[0m {warning}");
        return Ok(());
    };
    let _ = write!(
        tty,
        "\x1b[33mwarning:\x1b[0m {warning}\nStart here anyway? [y/N] "
    );
    let _ = tty.flush();
    let mut line = Vec::new();
    let mut buf = [0u8; 1];
    while let Ok(1) = tty.read(&mut buf) {
        if buf[0] == b'\n' {
            break;
        }
        line.push(buf[0]);
    }
    let answer = String::from_utf8_lossy(&line).trim().to_ascii_lowercase();
    if answer != "y" && answer != "yes" {
        return Err(anyhow!(
            "not started. Start in a project folder, or pass --any-folder."
        ));
    }
    if let Place::Loose(d) = &place {
        match store.add(d) {
            Ok(()) => {
                let _ = writeln!(tty, "Remembered: this folder won't be asked about again.");
            }
            Err(e) => {
                let _ = writeln!(tty, "could not remember the answer ({e:#})");
            }
        }
    }
    Ok(())
}

/// Ask about trusting `cwd`'s project config when it needs an answer and
/// stdin and stderr are a terminal.
pub(crate) fn trust_prompt(cwd: &std::path::Path) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(());
    }
    codesmith_core::trust::ensure(
        cwd,
        &config::Sources::from_env(),
        &mut std::io::stdin().lock(),
        &mut std::io::stderr(),
    )?;
    Ok(())
}

fn trust_cmd(
    cwd: &std::path::Path,
    target: Option<String>,
    list: bool,
    revoke: bool,
    distrust: bool,
    show: bool,
    json: bool,
) -> Result<()> {
    use codesmith_core::trust::{self, safe, State, Store};
    let store = Store::default_store();
    if list || (target.is_none() && json && !show) {
        let all = store.list();
        if json {
            println!("{}", serde_json::to_string_pretty(&all)?);
            return Ok(());
        }
        if all.is_empty() {
            eprintln!("No trust answers are kept.");
        }
        for r in all {
            println!(
                "{}  {}  {}  ({} capabilities)",
                r.id,
                if r.trusted {
                    "trusted    "
                } else {
                    "not trusted"
                },
                safe(&r.root),
                r.capabilities.len()
            );
        }
        return Ok(());
    }
    if revoke {
        if let Some(t) = target.as_deref().filter(|t| t.starts_with("trust_")) {
            if !store.revoke(t)? {
                return Err(anyhow!("no trust answer {t}"));
            }
            eprintln!("Revoked {t}.");
            return Ok(());
        }
    }
    let dir = match &target {
        Some(t) => std::path::Path::new(t)
            .canonicalize()
            .with_context(|| format!("no folder {t}"))?,
        None => cwd.to_path_buf(),
    };
    let root = config::project_root(&dir);
    let project = trust::project_key(&root);
    if revoke {
        if !store.revoke(&project)? {
            return Err(anyhow!(
                "no trust answer for {}",
                safe(&root.display().to_string())
            ));
        }
        eprintln!(
            "Revoked the trust answer for {}.",
            safe(&root.display().to_string())
        );
        return Ok(());
    }
    let req = config::trust_request(&dir, &config::Sources::from_env())?;
    let state = store.state(&project, &req);
    if show {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "root": req.root,
                    "project": project,
                    "files": req.files,
                    "capabilities": req.capabilities,
                    "state": format!("{state:?}").to_lowercase(),
                }))?
            );
        } else if req.is_empty() {
            println!("This project's config asks for nothing that needs trust.");
        } else {
            print!("{}", req.summary());
            println!(
                "Answer kept: {}",
                match state {
                    State::Trusted => "trusted",
                    State::Distrusted => "not trusted",
                    _ => "none (it is not trusted until you answer)",
                }
            );
        }
        return Ok(());
    }
    if req.is_empty() {
        eprintln!("This project's config asks for nothing that needs trust.");
        return Ok(());
    }
    eprint!("{}", req.summary());
    let r = store.remember(&project, &req, !distrust)?;
    eprintln!(
        "{} ({}). `codesmith trust --revoke` forgets this.",
        if distrust { "Not trusted" } else { "Trusted" },
        r.id
    );
    Ok(())
}

fn permissions_cmd(cwd: &std::path::Path, what: PermissionsCmd) -> Result<()> {
    use codesmith_core::approvals;
    let path = store::default_path();
    // Opening the store creates it owner-only, as sessions do.
    drop(open_store()?);
    let book = approvals::Book::open(&path)?;
    let project = approvals::project_key(&config::project_root(cwd));
    match what {
        PermissionsCmd::List { all, json } => {
            let list = book.list(if all { None } else { Some(&project) })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if list.is_empty() {
                eprintln!(
                    "No saved approvals{}.",
                    if all { "" } else { " for this project" }
                );
            }
            if !json {
                for s in list {
                    let p = if all {
                        format!("  {}", s.project)
                    } else {
                        String::new()
                    };
                    println!("{}  {:<18} {}{p}", s.id, s.action, s.resource);
                }
            }
            Ok(())
        }
        PermissionsCmd::Edit {
            id,
            resource,
            action,
        } => {
            if resource.is_none() && action.is_none() {
                return Err(anyhow!("give --resource or --action"));
            }
            let s = book
                .update(&id, action.as_deref(), resource.as_deref())?
                .map_err(|e| anyhow!("{e}"))?;
            println!("{}  {:<18} {}", s.id, s.action, s.resource);
            Ok(())
        }
        PermissionsCmd::Remove { ids } => {
            for id in ids {
                if !book.remove(&id)? {
                    return Err(anyhow!("no saved approval {id}"));
                }
                eprintln!("Removed {id}.");
            }
            Ok(())
        }
    }
}

fn session_cmd(cwd: PathBuf, what: SessionCmd) -> Result<()> {
    let mut db = open_store()?;
    match what {
        SessionCmd::List {
            all,
            archived,
            search,
            json,
        } => {
            let dir = cwd.display().to_string();
            let mut list =
                db.list_archived(if all { None } else { Some(&dir) }, false, Some(archived))?;
            if let Some(text) = &search {
                list.retain(|i| {
                    i.title
                        .as_deref()
                        .is_some_and(|t| codesmith_core::store::title_matches(t, text))
                });
            }
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
        SessionCmd::Archive { id } => {
            db.archive(&id)?;
            println!("Archived {id}");
        }
        SessionCmd::Unarchive { id } => {
            db.unarchive(&id)?;
            println!("Unarchived {id}");
        }
        SessionCmd::Messages { id } => {
            let info = session_target(&db, &cwd, id)?;
            let entries = db.entries(&info.id)?;
            for (seq, m) in codesmith_core::fork::messages(&entries) {
                if codesmith_core::runner::is_engine(m) {
                    continue;
                }
                let role = match m.role {
                    codesmith_ai::Role::User => "user",
                    codesmith_ai::Role::Assistant => "assistant",
                    codesmith_ai::Role::System => "system",
                };
                let text = message_line(m);
                println!("{seq:>5}  {role:<9}  {text}");
            }
        }
        SessionCmd::Fork { id, before } => {
            let info = session_target(&db, &cwd, id)?;
            let to = session::new_id();
            let forked = db.fork_at(&info.id, &to, before)?;
            println!("Forked {} into {}", info.id, forked.id);
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
        SessionCmd::Diff {
            id,
            from: Some(from),
            to,
        } => {
            let (info, snaps) = snapshot_target(&db, &cwd, id)?;
            match session::diff::unified(&db, &info.id, &snaps, Some(from), to)? {
                Some(d) if d.trim().is_empty() => println!("No file changes in those turns."),
                Some(d) => print!("{d}"),
                None => println!("No snapshots recorded for those turns of {}.", info.id),
            }
        }
        SessionCmd::Diff { id, .. } => {
            let (info, snaps) = snapshot_target(&db, &cwd, id)?;
            match session::turn_diff(&db, &info.id, &snaps)? {
                Some(d) if d.trim().is_empty() => {
                    println!("No file changes since the latest turn began.")
                }
                Some(d) => print!("{d}"),
                None => println!("No snapshot recorded for {}.", info.id),
            }
        }
        SessionCmd::Export { id, sanitize } => {
            let e = db.export(&id)?;
            let v = if sanitize {
                codesmith_core::export::sanitized(&e)
            } else {
                // The credentials CodeSmith holds never leave in an export.
                let secrets = config::load(&cwd)
                    .map(|l| codesmith_core::export::known_secrets(&l.config))
                    .unwrap_or_default();
                codesmith_core::export::raw_json(&e, &secrets)
            };
            println!("{}", serde_json::to_string_pretty(&v)?)
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
        SessionCmd::MigrateMedia { id } => {
            let ids = match id {
                Some(id) => vec![id],
                None => db.list(None, true)?.into_iter().map(|s| s.id).collect(),
            };
            let mut moved = 0;
            for id in &ids {
                moved += db.migrate_media(id)?;
            }
            println!(
                "Moved the media of {moved} record(s) in {} session(s) into the blob store.",
                ids.len()
            );
        }
    }
    Ok(())
}

/// Session `id`, else the latest in `cwd`.
fn session_target(db: &Store, cwd: &std::path::Path, id: Option<String>) -> Result<store::Info> {
    match id {
        Some(id) => db.get(&id)?.ok_or_else(|| anyhow!("no session {id}")),
        None => db
            .latest(&cwd.display().to_string())?
            .ok_or_else(|| anyhow!("no session in {}", cwd.display())),
    }
}

/// A message on one line: its first text, else what its parts are.
fn message_line(m: &codesmith_ai::Message) -> String {
    use codesmith_ai::Part;
    let text = m.parts.iter().find_map(|p| match p {
        Part::Text(t) if !t.text.trim().is_empty() => Some(t.text.trim().to_string()),
        _ => None,
    });
    let line = text.unwrap_or_else(|| {
        let kinds: Vec<String> = m
            .parts
            .iter()
            .map(|p| match p {
                Part::ToolCall { name, .. } => format!("[call {name}]"),
                Part::ToolResult { name, .. } => format!("[result {name}]"),
                Part::Media(_) => "[media]".into(),
                _ => "[...]".into(),
            })
            .collect();
        kinds.join(" ")
    });
    let first = line.lines().next().unwrap_or("");
    let mut out: String = first.chars().take(80).collect();
    if first.chars().count() > 80 || line.lines().count() > 1 {
        out.push_str("...");
    }
    out
}

fn snapshot_target(
    db: &Store,
    cwd: &std::path::Path,
    id: Option<String>,
) -> Result<(store::Info, Snapshots)> {
    let info = session_target(db, cwd, id)?;
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
        .env_remove(codesmith_core::SERVER_TOKEN_VAR)
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
    let mut loaded = config::load(&cwd)?;
    // `--tools` / `--no-tools`: deny everything else, after every rule.
    let narrowed = match (&args.tools, args.no_tools) {
        (Some(list), _) => Some(codesmith_core::tool::narrow::parse(list).map_err(|e| anyhow!(e))?),
        (None, true) => Some(Vec::new()),
        (None, false) => None,
    }
    .map(|allowed| codesmith_core::tool::narrow::narrow(&loaded.config, &allowed));
    if let Some(n) = &narrowed {
        loaded.config.overrides.extend(n.rules.iter().cloned());
    }
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
    let mut custom: Option<(command::Command, String)> = None;
    let mcp = if args.no_mcp {
        codesmith_core::mcp::Mcp::empty()
    } else {
        connect_mcp(&loaded, args.format != Format::Json).await
    };
    let all = if message.trim_start().starts_with('/') {
        all_commands_with(&cwd, &loaded, &mcp)
    } else {
        Default::default()
    };
    if let Some((name, rest)) = command::find_invocation(&message, &all) {
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
            custom = Some((c.clone(), rest.clone()));
        }
        // A subagent command leaves the session's agent and model alone.
        let (agent, model) = c.switches(&loaded.config);
        agent_arg = agent_arg.or(agent);
        model_arg = model_arg.or(model);
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
    let open = |model: Option<String>, db: Option<store::Store>| {
        Session::new(
            &loaded,
            cwd.clone(),
            Options {
                agent: agent_arg.clone(),
                model,
                resume: resume.clone(),
                title: args.title.clone(),
                parent: None,
                id: None,
                questioner: questioner.clone(),
                mcp: Some(mcp.clone()),
            },
            asker.clone(),
            db,
        )
    };
    let mut session = match &args.effort {
        None => open(model_arg, db)?,
        Some(level) => {
            // The model the session would use, at `level`: chosen first
            // without a store, so nothing is opened twice.
            let probe = open(model_arg, None)?;
            let (base, _) = config::split_variant(&probe.model_spec);
            let variants = loaded
                .config
                .resolve_model(base)
                .map(|m| m.variants)
                .unwrap_or_default();
            let spec = crate::tui::effort_spec(base, &variants, level).map_err(|e| anyhow!(e))?;
            drop(probe);
            open(Some(spec), db)?
        }
    };
    // On the bus if a broker is already running (a run never starts one:
    // under a bridge its folders aren't the user's), taking replies to
    // what it sends but no requests, which wait for the session to open.
    if codesmith_core::bus::enabled(&loaded.config) && narrowed.is_none() {
        let member = codesmith_core::bus::client::session_member(
            &session.id,
            session.title.as_deref(),
            &session.cwd.display().to_string(),
            &session.agent.id,
            &session.model_spec,
            false,
        );
        let bus = codesmith_core::bus::Bus::join(
            member,
            codesmith_core::bus::client::Options {
                start: false,
                reconnect: false,
                ..Default::default()
            },
        );
        if matches!(
            bus.ready(std::time::Duration::from_millis(500)).await,
            codesmith_core::bus::Joined::On(_)
        ) {
            session.set_bus(Some(bus));
        }
    }
    if let Some(n) = &narrowed {
        session.tools.retain(|t| n.tools.contains(t));
    }
    // No key at all: say so before anything is sent, instead of after the
    // provider's refusals.
    if let Some(why) = codesmith_core::retry::missing_key(&session.model) {
        return Err(anyhow!(why));
    }
    let json = args.format == Format::Json;
    let mut sink = render::Printer::new(json, args.thinking);
    if let Some(path) = &args.usage_log {
        let log = render::UsageLog::open(path, &session.model_spec, &session.id)
            .with_context(|| format!("opening the usage log {}", path.display()))?;
        sink = sink.with_usage_log(log);
    }
    if !json {
        eprintln!(
            "\x1b[2m> {} · {}/{} · {}\x1b[0m",
            session.agent.name, session.model.provider_id, session.model.model_id, session.id
        );
    }
    // `--archived` archives the session before its prompt runs (as OpenCode
    // does), so a run that fails or is stopped part way stays out of the
    // lists too. The mark is kept with the session's meta, which needs no
    // stored session yet.
    let archive = args.archived.then(open_store).transpose()?;
    if let Some(db) = &archive {
        if db.archived(&session.id)?.is_none() {
            db.set_meta(
                &session.id,
                store::ARCHIVED_META,
                Some(&store::now().to_string()),
            )?;
        }
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
        None => run_turn(&mut session, &message, custom.as_ref(), media, &mut sink).await,
    };
    // A run that never stored its session leaves no mark behind.
    if let Some(db) = &archive {
        if db.get(&session.id)?.is_none() {
            db.set_meta(&session.id, store::ARCHIVED_META, None)?;
        }
    }
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
    command: Option<&(command::Command, String)>,
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
            match command {
                Some((c, rest)) => {
                    session
                        .run_command(message, c, rest, media, &mut *sink)
                        .await?
                }
                None => session.prompt_with(message, media, &mut *sink).await?,
            }
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
/// header names too), the selected one starred, as `resolve_model` built
/// them: the generated reasoning ones (from the catalog, else built in),
/// with the configured ones merged in.
fn variant_lines(m: &config::ModelRef) -> Result<Vec<String>> {
    let variants = &m.variant_list;
    let width = variants.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let mut out = vec![];
    for (name, v) in variants {
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

/// `codesmith models PROVIDER`: the models the provider's API reports.
async fn list_provider_models(loaded: &config::Loaded, pid: &str) -> Result<()> {
    let pid = pid.to_string();
    let m = loaded.config.resolve_model(&format!("{pid}/_"))?;
    let client = reqwest::Client::new();
    // Claude Code reports its own models (no prompt is sent).
    if pid == codesmith_core::providers::claude_agent::ID {
        use codesmith_core::providers::claude_agent;
        let p = loaded.config.provider(&pid).unwrap_or_default();
        let state = claude_agent::check(&claude_agent::connection(&p)).await;
        if let Some(problem) = state.problem() {
            return Err(anyhow!(problem));
        }
        for m in claude_agent::catalog().models {
            println!("{pid}/{}", m.id);
        }
        return Ok(());
    }
    // Providers with a live list keep it for model resolution.
    let settings = loaded.config.provider(&pid).unwrap_or_default().settings;
    if let Some(ids) =
        codesmith_core::providers::live::refresh(&client, &pid, &m.provider, &settings).await
    {
        for id in ids? {
            println!("{pid}/{id}");
        }
        return Ok(());
    }
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

/// The models a provider with a `swap` setting can load on its server.
async fn print_swappable(c: &config::Config, pid: &str) -> Result<()> {
    use codesmith_core::providers::swap;
    let models = swap::swappable(c, pid).await?;
    println!("\n{pid} can load (`codesmith models swap {pid} MODEL`; loading takes a while):");
    for m in models {
        let mut marks = Vec::new();
        if m.live {
            marks.push("live".to_string());
        }
        if m.configured {
            marks.push("configured".to_string());
        }
        if let Some(b) = m.size_bytes {
            marks.push(format!("{:.1} GB", b as f64 / 1e9));
        }
        println!(
            "{pid}/{}{}",
            m.name,
            if marks.is_empty() {
                String::new()
            } else {
                format!("  ({})", marks.join(", "))
            }
        );
    }
    Ok(())
}

/// `codesmith models swap PROVIDER MODEL`: load MODEL on the provider's
/// server with its `swap.use` command. Only from a person's terminal: never
/// from inside a CodeSmith session's shell tool, and not without a terminal.
async fn models_swap(cwd: &std::path::Path, args: &[String]) -> Result<()> {
    use codesmith_core::providers::swap;
    let [pid, model] = args else {
        return Err(anyhow!("usage: codesmith models swap PROVIDER MODEL"));
    };
    swap::guard()?;
    let loaded = config::load(cwd)?;
    let Some(s) = swap::for_provider(&loaded.config, pid) else {
        let ignored: Vec<&String> = loaded
            .config
            .swap_ignored
            .iter()
            .filter(|i| i.starts_with(&format!("{pid} in ")))
            .collect();
        return Err(if ignored.is_empty() {
            anyhow!("provider {pid} has no swap setting (providers.{pid}.swap in the global codesmith.json)")
        } else {
            anyhow!(
                "provider {pid}'s swap setting is only read from the global codesmith.json; ignored: {}",
                ignored.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            )
        });
    };
    swap::validate(s)?;
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "codesmith models swap runs only from a terminal: it changes the model the server serves for everyone"
        ));
    }
    eprintln!("asking {pid} which models it can load...");
    let models = swap::swappable(&loaded.config, pid).await?;
    if models.iter().any(|m| m.name == *model && m.live) {
        println!("{pid}/{model}");
        eprintln!("{model} is already loaded on {pid}'s server");
        return Ok(());
    }
    let known: Vec<String> = models.into_iter().map(|m| m.name).collect();
    eprintln!(
        "loading {model} on {pid}'s server; this takes a while (up to {} s)",
        s.timeout().as_secs()
    );
    let id = swap::swap(s, model, &known, &mut |l| eprintln!("  {l}")).await?;
    println!("{pid}/{id}");
    eprintln!("{pid} now serves {id}: use --model {pid}/{id}, or /model in the UI");
    Ok(())
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
            variant_lines(&m).unwrap(),
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
mod login_tests {
    use super::*;
    use codesmith_core::providers::signin;

    fn env(pairs: &[(&str, &str)]) -> codesmith_core::providers::Env {
        let m: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Arc::new(move |k| m.get(k).cloned())
    }

    #[test]
    fn the_guidance_lists_each_provider_with_how_to_sign_in_and_no_key() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("auth.json");
        codesmith_core::auth::set_in(&file, "groq", "gsk-guidance-secret-1").unwrap();
        let c = signin::choices(&config::Config::default(), &file, &env(&[]));
        let g = login_guidance(&c);
        let line = |id: &str| {
            g.lines()
                .find(|l| l.split_whitespace().next() == Some(id))
                .unwrap_or_else(|| panic!("no {id} in {g}"))
                .to_string()
        };
        assert!(line("groq").contains("signed in · saved key"), "{g}");
        assert!(line("groq").contains("codesmith auth login groq"), "{g}");
        assert!(
            line("openai").contains("codesmith auth login openai --oauth"),
            "{g}"
        );
        assert!(
            line("anthropic").contains("codesmith auth login claude-agent"),
            "{g}"
        );
        assert!(line("amazon-bedrock").contains("--profile <name>"), "{g}");
        assert!(line("github-copilot").contains("codesmith auth login github-copilot"));
        assert!(!line("github-copilot").contains("--oauth"), "{g}");
        assert!(line("sap-ai-core").contains("AICORE_SERVICE_KEY"), "{g}");
        assert!(!g.contains("gsk-guidance-secret"), "{g}");
    }

    #[test]
    fn a_pick_is_a_number_or_an_id() {
        let ids = ["anthropic", "openai"];
        assert_eq!(pick_index("2", &ids), Some(1));
        assert_eq!(pick_index(" OpenAI ", &ids), Some(1));
        assert_eq!(pick_index("0", &ids), None);
        assert_eq!(pick_index("3", &ids), None);
        assert_eq!(pick_index("groq", &ids), None);
    }

    #[test]
    fn signing_out_removes_the_saved_login() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("auth.json");
        codesmith_core::auth::set_in(&file, "groq", "gsk-x-123456789").unwrap();
        assert_eq!(
            logout(&file, "groq").unwrap(),
            "Removed the saved key for groq"
        );
        assert_eq!(codesmith_core::auth::entry_in(&file, "groq"), None);
        assert_eq!(logout(&file, "groq").unwrap(), "No saved key for groq");
    }

    #[test]
    fn a_secret_is_kept_out_of_debug_output() {
        let s = Secret("sk-very-secret".into());
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        let req = LoginRequest {
            provider: "openai".into(),
            kind: signin::Kind::Key,
            key: Some(s),
            answers: Default::default(),
            client_id: None,
        };
        assert!(!format!("{req:?}").contains("sk-very-secret"));
    }

    #[test]
    fn printed_text_drops_control_characters() {
        assert_eq!(printable("a\x1b[2Jb\x07c\nd"), "a[2Jbc\nd");
    }
}

#[cfg(test)]
#[path = "cli_parity.rs"]
mod parity;
