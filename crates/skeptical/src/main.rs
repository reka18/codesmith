//! `skeptical`: a terminal coding agent (a Rust port of OpenCode).

mod render;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use skeptical_core::permission::{Asker, Fixed, Reply, Request};
use skeptical_core::session::{Options, Session};
use skeptical_core::{agent, config, instructions, paths, prompt};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

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
    /// Continue the last session (arrives with persistent sessions).
    #[arg(short, long)]
    r#continue: bool,
    /// Continue a session by id (arrives with persistent sessions).
    #[arg(short, long)]
    session: Option<String>,
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

async fn run_prompt(cwd: PathBuf, args: RunArgs) -> Result<()> {
    if args.r#continue || args.session.is_some() {
        return Err(anyhow!(
            "--continue and --session need persistent sessions, which are not built yet"
        ));
    }
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
    let asker: Box<dyn Asker> = if args.auto {
        Box::new(Fixed(Reply::Once))
    } else {
        Box::new(Terminal)
    };
    let mut session = Session::new(
        &loaded,
        cwd,
        Options {
            agent: args.agent,
            model: args.model,
        },
        asker,
    )?;
    let json = args.format == Format::Json;
    let mut sink = render::Printer::new(json, args.thinking);
    if !json {
        eprintln!(
            "\x1b[2m> {} · {}/{}\x1b[0m",
            session.agent.name, session.model.provider_id, session.model.model_id
        );
    }
    let result = session.prompt(&message, &mut sink).await;
    sink.done(&session.usage, result.as_ref().err().map(|e| e.to_string()));
    result
}
