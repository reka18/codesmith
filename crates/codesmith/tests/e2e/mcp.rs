//! MCP through the binary: a stdio server's tools through Code Mode
//! (`execute`) and as direct calls, and a server prompt as a slash command.

use crate::harness::{python, user_texts, Env};
use crate::model::{self, Model, Reply, Script, M};
use serde_json::{json, Value};

fn fixture(name: &str) -> String {
    format!(
        "{}/../codesmith-core/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn servers(py: &str) -> Value {
    json!({"servers": {
        "fake": {"type": "local", "command": [py, fixture("mcp_stdio.py")]},
        "rich": {"type": "local", "command": [py, fixture("mcp_rich.py")]}
    }})
}

#[tokio::test(flavor = "multi_thread")]
async fn code_mode_runs_mcp_tools_inside_execute() {
    let Some(py) = python() else {
        eprintln!("skipped: python3 is not installed");
        return;
    };
    let e = Env::new();
    let ask = M::tools().and(M::prompt("echo through code"));
    let m = Model::start(
        Script::new()
            .titled("Code")
            .then(
                "program",
                ask.clone().and(M::step(0)),
                Reply::call(
                    "execute",
                    json!({"code": "const a = await tools.fake.echo({text: 'hi'})\nlet b\ntry { await tools.fake['fail.now']({}) } catch (err) { b = 'caught: ' + err.message }\nconsole.log('two calls')\nreturn {a, b}"}),
                ),
            )
            .then("answer", ask.and(M::step(1)), Reply::text("Done.")),
    )
    .await;
    e.config(&m.url, json!({"mcp": servers(py)}));
    let out = e
        .run(&["run", "--format", "json", "echo through code"])
        .await;
    out.ok();
    let first = &m.by("program")[0];
    let tools = model::tools(first);
    assert!(tools.contains(&"execute".to_string()), "{tools:?}");
    assert!(!tools.iter().any(|t| t.starts_with("fake_")), "{tools:?}");
    // The catalog lists the server's tools by their Code Mode paths.
    let system = model::system(first);
    assert!(
        system.contains("tools.fake.echo") && system.contains("Echo text back"),
        "{system}"
    );
    let result = &model::results(&m.by("answer")[0])[0];
    assert_eq!(
        result,
        "Result:\n{\n  \"a\": \"echo: hi\",\n  \"b\": \"caught: fake_fail_now: boom\"\n}\n\nLogs:\ntwo calls\n\n(2 tool calls)"
    );
    let ev = out.events();
    let r = ev.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(
        (&r["name"], &r["is_error"]),
        (&json!("execute"), &json!(false))
    );
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn without_code_mode_mcp_tools_are_direct_calls() {
    let Some(py) = python() else {
        eprintln!("skipped: python3 is not installed");
        return;
    };
    let e = Env::new();
    let ask = M::tools().and(M::prompt("echo directly"));
    let m = Model::start(
        Script::new()
            .titled("Direct")
            .then(
                "echo",
                ask.clone().and(M::step(0)),
                Reply::call("fake_echo", json!({"text": "direct"})),
            )
            .then(
                "fail",
                ask.clone().and(M::step(1)),
                Reply::call("fake_fail_now", json!({})),
            )
            .then(
                "denied",
                ask.clone().and(M::step(2)),
                Reply::call("rich_grow", json!({})),
            )
            .then("answer", ask.and(M::step(3)), Reply::text("Echoed.")),
    )
    .await;
    e.config(
        &m.url,
        json!({"mcp": servers(py), "codemode": false, "permissions": [
            {"action": "rich_grow", "resource": "*", "effect": "deny"}
        ]}),
    );
    let list = e.run(&["mcp", "list"]).await;
    list.ok();
    assert!(
        list.stdout.contains("fake: connected, 2 tool(s)"),
        "{}",
        list.both()
    );
    assert!(
        list.stdout.contains("/rich:review  (prompt)"),
        "{}",
        list.both()
    );

    let out = e.run(&["run", "--format", "json", "echo directly"]).await;
    out.ok();
    let tools = model::tools(&m.by("echo")[0]);
    assert!(
        tools.contains(&"fake_echo".to_string()) && tools.contains(&"fake_fail_now".to_string()),
        "{tools:?}"
    );
    assert!(!tools.contains(&"execute".to_string()));
    assert!(
        !tools.contains(&"rich_grow".to_string()),
        "denied tools are not offered"
    );
    let ev = out.events();
    let results: Vec<&Value> = ev.iter().filter(|e| e["type"] == "tool_result").collect();
    assert_eq!(results[0]["output"], "echo: direct");
    assert_eq!(results[0]["is_error"], false);
    assert_eq!(results[1]["output"], "boom");
    assert_eq!(results[1]["is_error"], true);
    assert_eq!(results[2]["is_error"], true);
    assert_eq!(out.text(), "Echoed.");
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_prompt_runs_as_a_slash_command() {
    let Some(py) = python() else {
        eprintln!("skipped: python3 is not installed");
        return;
    };
    let e = Env::new();
    let m = Model::start(Script::new().titled("Review").then(
        "expanded",
        M::tools().and(M::prompt("Review main.rs focusing on speed.")),
        Reply::text("Looks fast."),
    ))
    .await;
    e.config(&m.url, json!({"mcp": servers(py)}));
    let cmds = e.run(&["commands"]).await;
    cmds.ok();
    assert!(cmds.stdout.contains("/rich:review"), "{}", cmds.both());
    let out = e
        .run(&["run", "--format", "json", "/rich:review main.rs speed"])
        .await;
    out.ok();
    assert_eq!(out.text(), "Looks fast.");
    let sid = out.done()["session"].as_str().unwrap().to_string();
    let users = user_texts(&e.history(&sid));
    assert_eq!(users.len(), 1);
    assert!(
        users[0].starts_with("Review main.rs focusing on speed."),
        "{users:?}"
    );
    // The server's instructions reach the system prompt.
    let sent = &m.by("expanded")[0];
    assert!(model::system(sent).contains("Always cite ticket ids."));
    m.assert_done();
}

/// A local MCP server never inherits the local server's token
/// (OpenCode's `cli/test/env`, "session environment omits server
/// credentials", for MCP servers).
#[tokio::test(flavor = "multi_thread")]
async fn local_mcp_servers_do_not_see_the_server_token() {
    let Some(py) = python() else {
        eprintln!("skipped: python3 is not installed");
        return;
    };
    let e = Env::new();
    let seen = e.root.join("mcp-env.txt");
    let wrapper = format!(
        "import os, runpy, sys\n\
         open({seen:?}, 'w').write('[' + os.environ.get('CODESMITH_SERVER_TOKEN', '') + '][' + os.environ.get('SESSION_ENV_TEST', '') + ']')\n\
         sys.argv = [{fixture:?}]\n\
         runpy.run_path({fixture:?}, run_name='__main__')\n",
        seen = seen.display().to_string(),
        fixture = fixture("mcp_rich.py"),
    );
    let m = Model::start(Script::new()).await;
    e.config(
        &m.url,
        json!({"mcp": {"servers": {"rich": {"type": "local", "command": [py, "-c", wrapper]}}}}),
    );
    let out = e
        .command(&["commands"])
        .env("CODESMITH_SERVER_TOKEN", "secret-token")
        .env("SESSION_ENV_TEST", "included")
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("/rich:review"), "{stdout}");
    assert_eq!(std::fs::read_to_string(&seen).unwrap(), "[][included]");
}
