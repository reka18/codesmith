//! `codesmith mini` in a pseudo-terminal: the transcript goes into the
//! terminal's own scrollback (checked on a `vt100` screen with a
//! scrollback, and in the byte stream), the alternate screen is never
//! used, and permission prompts wait in the footer.

use crate::harness::{user_texts, Env};
use crate::model::{Model, Reply, Script, M};
use crate::tui::Ui;
use serde_json::json;

/// The terminal's rows: the newest of its scrollback (`vt100` shows at
/// most a screenful of it), then its screen.
fn history(term: &mut vt100::Parser) -> Vec<String> {
    let (rows, cols) = term.screen().size();
    term.set_scrollback(rows as usize);
    let back = term.screen().scrollback();
    let mut out = Vec::new();
    for off in (1..=back).rev() {
        term.set_scrollback(off);
        out.push(term.screen().rows(0, cols).next().unwrap_or_default());
    }
    term.set_scrollback(0);
    out.extend(term.screen().rows(0, cols));
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn mini_writes_the_transcript_into_the_scrollback() {
    let e = Env::new();
    // Longer than the screen, so its start scrolls into the history.
    let answer: String = (1..=26).map(|i| format!("- Answer line {i}.\n")).collect();
    let m = Model::start(Script::new().titled("Greeting").then(
        "hello",
        M::tools().and(M::prompt("say hello")),
        Reply::text(&answer),
    ))
    .await;
    e.config(&m.url, json!({}));
    let Some(mut ui) = Ui::start(&e, "mini --prompt 'say hello'") else {
        return;
    };
    ui.wait_for("Answer line 26.").await;
    // The turn's usage is in the footer once it is over.
    ui.wait_for("110 (1%)").await;
    ui.quit_reading().await;
    let bytes = ui.screen.clone();
    assert!(
        !bytes.contains("\x1b[?1049h"),
        "mini mode used the alternate screen"
    );
    assert!(bytes.contains("\x1b[?2026h"), "frames are synchronized");
    let rows = history(&mut ui.term);
    let at = |s: &str| {
        rows.iter()
            .position(|r| r.contains(s))
            .unwrap_or_else(|| panic!("{s:?} is not on the terminal:\n{}", rows.join("\n")))
    };
    // The banner, the prompt and every line of the answer, once and in order.
    assert!(at("cs mini") < at("› say hello"));
    assert_eq!(rows[at("cs mini") + 1], "", "a blank row after the banner");
    assert!(at("› say hello") < at("Answer line 1."));
    assert!(at("Answer line 1.") < at("Answer line 26."));
    assert_eq!(
        rows.iter().filter(|r| r.contains("Answer line 1.")).count(),
        1
    );
    // The turn summary follows the answer.
    let summary = rows
        .iter()
        .position(|r| r.starts_with("Build · fake/m · ") && r.ends_with('s'))
        .expect("a turn summary");
    assert!(summary > at("Answer line 26."));
    // Its start is in the scrollback, not on the screen.
    let screen: Vec<String> = ui.term.screen().rows(0, 100).collect();
    assert!(!screen.iter().any(|r| r.contains("Answer line 1.")));
    // The exit banner, where the shell's prompt will follow.
    let list = e.store().list(None, false).unwrap();
    assert_eq!(list.len(), 1);
    assert!(rows
        .iter()
        .any(|r| r.contains(&format!("Continue codesmith mini -s {}", list[0].id))));
    assert!(rows.iter().any(|r| r.contains("Session  Greeting")));
    assert_eq!(user_texts(&e.history(&list[0].id)), ["say hello"]);
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn mini_asks_for_permission_in_the_footer() {
    let e = Env::new();
    e.write("victim.txt", "bye\n");
    let m = Model::start(
        Script::new()
            .titled("T")
            .then(
                "delete",
                M::tools()
                    .and(M::prompt("delete the victim"))
                    .and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "rm victim.txt", "description": "Delete"}),
                ),
            )
            .then(
                "done",
                M::tools().and(M::step(1)),
                Reply::text("Deleted it."),
            ),
    )
    .await;
    e.config(
        &m.url,
        json!({"permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}),
    );
    let Some(mut ui) = Ui::start(&e, "mini --prompt 'delete the victim'") else {
        return;
    };
    ui.wait_for("Permission required").await;
    ui.send(b"y").await;
    ui.wait_for("Deleted it.").await;
    ui.quit_reading().await;
    assert!(!std::path::Path::new(&e.project.join("victim.txt")).exists());
    let rows = history(&mut ui.term);
    // The dialog was the footer's: it is not left in the transcript.
    assert!(
        !rows.iter().any(|r| r.contains("Permission required")),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|r| r.contains("Deleted it.")));
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn mini_takes_piped_input_as_its_first_prompt() {
    if !std::path::Path::new("/usr/bin/script").exists() {
        eprintln!("skipped: script(1) is not installed");
        return;
    }
    let e = Env::new();
    let m = Model::start(Script::new().titled("Piped").then(
        "piped",
        M::tools().and(M::prompt("from the pipe\nand the flag")),
        Reply::text("Got both."),
    ))
    .await;
    e.config(&m.url, json!({}));
    e.write("in.txt", "from the pipe");
    let Some(mut ui) = Ui::start(&e, "mini --prompt 'and the flag' < in.txt") else {
        return;
    };
    ui.wait_for("Got both.").await;
    ui.quit_reading().await;
    m.assert_done();
}
