//! Ports of OpenCode's plugin permission tests
//! (`packages/core/test/plugin/permission-assert.test.ts`, MIT): a plugin
//! asserts a permission for a running session and gets the outcome as an
//! error it hands on as a tool error. Its own test binary, because the
//! plugin registry is process-wide. See `crates/codesmith-core/PARITY.md`.

mod plugin_support;

use codesmith_core::permission::{Asker, Reply, Request};
use codesmith_core::plugin::{
    self, AssertPermission, BoxFuture, Host, PermissionError, Plugin, Tool, ToolContext,
};
use codesmith_core::session::Session;
use codesmith_core::store::Store;
use plugin_support::{loaded, options, serve, text, tool, Collect};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    /// What each assertion came to, in order.
    outcomes: Mutex<Vec<Result<(), PermissionError>>>,
    /// Whether the assertions pass `save`.
    save: Mutex<bool>,
}

/// Asserts each action in `actions` for its session.
struct Probe(Arc<State>);
impl Tool for Probe {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "Assert permissions"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"actions": {"type": "array", "items": {"type": "string"}}}})
    }
    fn execute<'a>(
        &'a self,
        ctx: ToolContext,
        input: Value,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let host = plugin::host().map_err(|e| e.to_string())?;
            let save = *self.0.save.lock().unwrap();
            let mut last = Ok(());
            for a in input["actions"].as_array().unwrap() {
                let r = host
                    .assert_permission(AssertPermission {
                        session_id: ctx.session_id.clone(),
                        action: a.as_str().unwrap().into(),
                        resources: vec!["prod".into()],
                        save: if save { vec!["*".into()] } else { vec![] },
                        title: None,
                    })
                    .await;
                self.0.outcomes.lock().unwrap().push(r.clone());
                last = r;
            }
            last.map(|_| "asserted".to_string())
                .map_err(|e| e.to_string())
        })
    }
}

struct Permissions(Arc<State>);
impl Plugin for Permissions {
    fn id(&self) -> &str {
        "permissions"
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(Probe(self.0.clone()))]
    }
}

/// Answers by action, as the real prompt would for each outcome, and
/// records what it was asked.
#[derive(Default)]
struct ByAction(Mutex<Vec<Request>>);
impl Asker for ByAction {
    fn ask(&self, req: &Request) -> Reply {
        self.0.lock().unwrap().push(req.clone());
        match req.action.as_str() {
            "corrected" => Reply::Reject(Some("Use staging".into())),
            "declined" => Reply::Reject(None),
            _ => Reply::Once,
        }
    }
}

fn state() -> (Arc<State>, tokio::sync::MutexGuard<'static, ()>) {
    static STATE: std::sync::OnceLock<Arc<State>> = std::sync::OnceLock::new();
    let s = STATE
        .get_or_init(|| {
            let s = Arc::new(State::default());
            plugin::install(vec![Arc::new(Permissions(s.clone()))]);
            plugin::set_host(Arc::new(Host::new(
                Store::memory().unwrap(),
                std::env::temp_dir(),
            )));
            s
        })
        .clone();
    let guard = plugin_support::serial();
    s.outcomes.lock().unwrap().clear();
    (s, guard)
}

/// Run a turn whose one tool call asserts `actions`; what the asker was asked.
async fn run(actions: &[&str]) -> (Vec<Request>, Option<&'static str>) {
    let dir = tempfile::tempdir().unwrap();
    let actions = json!(actions);
    let m = serve(move |r| {
        if r.body["messages"].as_array().unwrap().last().unwrap()["role"] == "tool" {
            text("Done")
        } else {
            tool("call-probe", "probe", json!({"actions": actions}))
        }
    })
    .await;
    let l = loaded(
        dir.path(),
        &m.base,
        json!({"permissions": [
            {"action": "probe", "resource": "*", "effect": "allow"},
            {"action": "deploy", "resource": "*", "effect": "ask"},
            {"action": "corrected", "resource": "*", "effect": "ask"},
            {"action": "declined", "resource": "*", "effect": "ask"},
            {"action": "blocked", "resource": "*", "effect": "deny"}
        ]}),
    );
    let asker = Arc::new(ByAction::default());
    let mut s = Session::new(&l, dir.path().to_path_buf(), options(), asker.clone(), None).unwrap();
    s.prompt("Check the permissions", &mut Collect::default())
        .await
        .unwrap();
    let asked = asker.0.lock().unwrap().clone();
    (asked, s.stopped())
}

// "Effect plugins assert permissions and receive outcomes as tool errors"
#[tokio::test(flavor = "multi_thread")]
async fn oc_plugins_assert_permissions_and_receive_outcomes_as_tool_errors() {
    let (state, _lock) = state();
    *state.save.lock().unwrap() = true;
    let (asked, stopped) = run(&["deploy", "blocked", "corrected", "declined"]).await;
    assert_eq!(
        stopped,
        Some("you declined a tool call"),
        "a plain decline stops the turn, as for a built-in tool"
    );
    let outcomes = state.outcomes.lock().unwrap().clone();
    assert_eq!(outcomes[0], Ok(()));
    match &outcomes[1] {
        Err(PermissionError::Blocked(m)) => assert!(m.starts_with("Permission denied"), "{m}"),
        o => panic!("{o:?}"),
    }
    assert_eq!(
        outcomes[2],
        Err(PermissionError::Corrected("Use staging".into()))
    );
    assert_eq!(outcomes[3], Err(PermissionError::Declined));
    // A rule's deny is never asked about.
    assert_eq!(
        asked.iter().map(|r| r.action.as_str()).collect::<Vec<_>>(),
        ["deploy", "corrected", "declined"]
    );
    assert_eq!(asked[0].resources, ["prod"]);
    assert_eq!(asked[0].save, ["*"]);

    // Outside a running turn there is nothing to ask in.
    let host = plugin::host().unwrap();
    let r = host
        .assert_permission(AssertPermission {
            session_id: "ses_idle".into(),
            action: "deploy".into(),
            ..Default::default()
        })
        .await;
    assert_eq!(r, Err(PermissionError::NotRunning("ses_idle".into())));
}

// "Promise plugins assert permissions with plain input"
#[tokio::test(flavor = "multi_thread")]
async fn oc_plugins_assert_permissions_with_plain_input() {
    let (state, _lock) = state();
    *state.save.lock().unwrap() = false;
    let (asked, stopped) = run(&["deploy", "corrected"]).await;
    assert_eq!(stopped, None, "a correction lets the model carry on");
    let outcomes = state.outcomes.lock().unwrap().clone();
    assert_eq!(outcomes[0], Ok(()));
    assert_eq!(asked[0].action, "deploy");
    assert_eq!(asked[0].resources, ["prod"]);
    assert!(asked[0].save.is_empty());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().to_string(),
        "Use staging",
        "a correction reads as the tool error"
    );
}
