//! Ports of OpenCode's callback command tests (`packages/core/test/`
//! `command.test.ts`, MIT): commands a plugin runs as code. Its own test
//! binary, because the plugin registry is process-wide. See
//! `crates/codesmith-core/PARITY.md`.

use codesmith_ai::Media;
use codesmith_core::command;
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::plugin::{self, BoxFuture, Command, CommandContext, Plugin};
use codesmith_core::session::{Options, Session, Sink};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Calls(Mutex<Vec<(String, String, usize)>>);

struct Goal(Arc<Calls>);
impl Command for Goal {
    fn name(&self) -> &str {
        "goal"
    }
    fn description(&self) -> Option<&str> {
        Some("Manage the session goal")
    }
    fn execute<'a>(&'a self, ctx: CommandContext) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            self.0
                 .0
                .lock()
                .unwrap()
                .push((ctx.session_id, ctx.args, ctx.media.len()));
            Ok(None)
        })
    }
}

struct Fail;
impl Command for Fail {
    fn name(&self) -> &str {
        "fail"
    }
    fn execute<'a>(&'a self, _: CommandContext) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async { Err("command failed".to_string()) })
    }
}

struct Commands(Arc<Calls>);
impl Plugin for Commands {
    fn id(&self) -> &str {
        "commands"
    }
    fn commands(&self) -> Vec<Arc<dyn Command>> {
        vec![Arc::new(Goal(self.0.clone())), Arc::new(Fail)]
    }
}

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

fn session(config: &Config) -> Session {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let loaded = Loaded {
        config: config.clone(),
        files: vec![],
        root: root.clone(),
    };
    let opts = Options {
        agent: None,
        model: None,
        resume: None,
        title: Some("t".into()),
        parent: None,
        questioner: None,
        mcp: None,
    };
    Session::new(&loaded, root, opts, Arc::new(Fixed(Reply::Once)), None).unwrap()
}

fn config() -> Config {
    serde_json::from_value(json!({
        "model": "test/m",
        "snapshots": false,
        "providers": {"test": {"protocol": "openai-chat", "settings": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "x"}}},
    }))
    .unwrap()
}

/// Both ports share the process-wide registry, so they run in one test.
#[tokio::test(flavor = "multi_thread")]
async fn oc_command_registers_and_executes_callback_commands() {
    let calls = Arc::new(Calls::default());
    plugin::install(vec![Arc::new(Commands(calls.clone()))]);
    let c = config();

    // "registers and executes callback commands"
    let all = command::plugin_commands(&c);
    let goal = &all["goal"];
    assert_eq!(goal.description.as_deref(), Some("Manage the session goal"));
    assert_eq!(goal.plugin.as_deref(), Some("goal"));
    assert!(!goal.runs_as_subagent(&c));
    let mut s = session(&c);
    let media = vec![Media::base64("text/markdown", "IyBQbGFu")];
    s.run_command("/goal ship it", goal, "ship it", media, &mut Quiet)
        .await
        .unwrap();
    assert_eq!(
        *calls.0.lock().unwrap(),
        [(s.id.clone(), "ship it".to_string(), 1)]
    );
    assert!(
        s.messages.is_empty(),
        "a command that returns nothing sends nothing"
    );

    // "returns callback error messages without stack traces"
    let err = s
        .run_command("/fail", &all["fail"], "", vec![], &mut Quiet)
        .await
        .unwrap_err();
    assert_eq!(format!("{err:#}"), "command failed");

    // A config command of the same name replaces the plugin's.
    let mut configured = std::collections::BTreeMap::new();
    configured.insert(
        "goal".to_string(),
        command::Command {
            template: "Set the goal".into(),
            ..Default::default()
        },
    );
    let mut builtins = command::builtins(std::path::Path::new("/p"));
    builtins.extend(all);
    let merged = command::discover(builtins, &[], &configured);
    assert_eq!(merged["goal"].plugin, None);
    assert!(merged["fail"].plugin.is_some());

    // A plugin turned off in config offers no commands.
    let mut off = c.clone();
    off.plugins.insert("commands".into(), json!(false));
    assert!(command::plugin_commands(&off).is_empty());
}
