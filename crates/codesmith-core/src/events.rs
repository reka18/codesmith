//! A session's events as JSON: what `codesmith serve` streams for a turn,
//! and what the remote adapters send a phone (wrapped in their envelope,
//! see [`crate::remote::tap`]). One [`JsonEvents`] sink makes both, so the
//! two can't drift apart.
//!
//! Each event is an object with a `type`. A subagent's tool calls name it
//! in `session` (serve's field; the remote envelope moves it to `child`).

use crate::session::{ChildSession, Sink};
use crate::tool::todo::Todo;
use codesmith_ai::{FinishReason, Usage};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The subagents a turn runs, as its events name them.
#[derive(Debug, Default)]
pub struct Children {
    /// Running subagent sessions, outermost first.
    stack: Vec<String>,
    /// The latest `subagent` call and the agent it named, for the next
    /// `child_start`.
    call: Option<(String, Option<String>)>,
}

impl Children {
    /// The running subagent whose events these are, if any.
    pub fn current(&self) -> Option<String> {
        self.stack.last().cloned()
    }
}

/// [`Children`] shared between a sink and the prompts asked meanwhile (a
/// subagent's permission request names it too).
pub type SharedChildren = Arc<Mutex<Children>>;

/// A [`Sink`] that turns each event into JSON and gives it to `out`.
pub struct JsonEvents<F: FnMut(Value) + Send> {
    children: SharedChildren,
    out: F,
}

impl<F: FnMut(Value) + Send> JsonEvents<F> {
    pub fn new(children: SharedChildren, out: F) -> Self {
        JsonEvents { children, out }
    }

    fn child(&self) -> Option<String> {
        self.children.lock().unwrap().current()
    }

    fn send(&mut self, v: Value) {
        (self.out)(v);
    }

    /// Send a tool event, naming the subagent whose call it is.
    fn send_tool(&mut self, mut v: Value) {
        if let Some(child) = self.child() {
            v["session"] = json!(child);
        }
        self.send(v);
    }
}

/// A generated file's metadata (its bytes are in the session's messages).
pub fn media_event(m: &codesmith_ai::Media) -> Value {
    let mut ev = json!({"type": "media", "mime": m.mime, "name": m.filename});
    match &m.source {
        codesmith_ai::Source::Base64(b) => {
            let pad = b.bytes().rev().take_while(|c| *c == b'=').count();
            ev["size"] = json!((b.len() / 4 * 3).saturating_sub(pad));
        }
        codesmith_ai::Source::Url(u) => ev["url"] = json!(u),
    }
    ev
}

impl<F: FnMut(Value) + Send> Sink for JsonEvents<F> {
    fn text(&mut self, delta: &str) {
        self.send(json!({"type": "text", "delta": delta}));
    }
    fn reasoning(&mut self, delta: &str) {
        self.send(json!({"type": "reasoning", "delta": delta}));
    }
    fn media(&mut self, media: &codesmith_ai::Media) {
        self.send(media_event(media));
    }
    fn tool_start(&mut self, id: &str, name: &str, input: &Value) {
        if name.rsplit(" › ").next() == Some("subagent") {
            let agent = input["agent"].as_str().map(str::to_string);
            self.children.lock().unwrap().call = Some((id.to_string(), agent));
        }
        self.send_tool(json!({"type": "tool_start", "id": id, "name": name, "input": input}));
    }
    fn tool_end(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        self.send_tool(
            json!({"type": "tool_end", "id": id, "name": name, "output": output, "error": is_error}),
        );
    }
    fn child_start(&mut self, child: &ChildSession) {
        let (call, agent) = {
            let mut c = self.children.lock().unwrap();
            c.stack.push(child.id.clone());
            c.call.take().unzip()
        };
        self.send(json!({
            "type": "child_start",
            "session": child.id,
            "parent": child.parent_id,
            "depth": child.depth,
            "title": child.title,
            "agent": agent.flatten(),
            "tool": call,
        }));
    }
    fn child_end(&mut self, id: &str, status: &str, error: Option<&str>) {
        {
            let mut c = self.children.lock().unwrap();
            if let Some(at) = c.stack.iter().rposition(|s| s == id) {
                c.stack.truncate(at);
            }
        }
        self.send(json!({"type": "child_end", "session": id, "status": status, "error": error}));
    }
    fn child_text(&mut self, delta: &str) {
        let child = self.child();
        self.send(json!({"type": "child_text", "session": child, "delta": delta}));
    }
    fn child_reasoning(&mut self, delta: &str) {
        let child = self.child();
        self.send(json!({"type": "child_reasoning", "session": child, "delta": delta}));
    }
    fn step_end(&mut self, reason: &FinishReason, usage: &Usage) {
        self.send(json!({"type": "step_end", "reason": reason.normalized, "finish": reason, "usage": usage}));
    }
    fn retry(&mut self, attempt: u32, error: &str, wait: Duration) {
        self.send(json!({"type": "retry", "attempt": attempt, "error": error, "wait_ms": wait.as_millis() as u64}));
    }
    fn compacted(&mut self, before: u64, after: u64) {
        self.send(json!({"type": "compacted", "before": before, "after": after}));
    }
    fn todos(&mut self, todos: &[Todo]) {
        self.send(json!({"type": "todos", "todos": todos}));
    }
    fn notice(&mut self, message: &str) {
        self.send(json!({"type": "notice", "message": message}));
    }
    fn tool_progress(&mut self, id: &str, metadata: &Value) {
        self.send_tool(json!({"type": "tool_progress", "id": id, "metadata": metadata}));
    }
    fn tool_output(&mut self, id: &str, delta: &str) {
        self.send_tool(json!({"type": "tool_output", "id": id, "delta": delta}));
    }
}

/// Two sinks as one: every event goes to `first`, then to `second`. The
/// remote adapters tee an owner's own sink with a [`JsonEvents`] this way.
pub struct Tee<'a, 'b> {
    pub first: &'a mut dyn Sink,
    pub second: &'b mut dyn Sink,
}

impl Sink for Tee<'_, '_> {
    fn text(&mut self, d: &str) {
        self.first.text(d);
        self.second.text(d);
    }
    fn reasoning(&mut self, d: &str) {
        self.first.reasoning(d);
        self.second.reasoning(d);
    }
    fn media(&mut self, m: &codesmith_ai::Media) {
        self.first.media(m);
        self.second.media(m);
    }
    fn tool_input(&mut self, id: &str, name: &str) {
        self.first.tool_input(id, name);
        self.second.tool_input(id, name);
    }
    fn tool_input_end(&mut self, id: &str, call: Option<&str>) {
        self.first.tool_input_end(id, call);
        self.second.tool_input_end(id, call);
    }
    fn tool_start(&mut self, id: &str, name: &str, input: &Value) {
        self.first.tool_start(id, name, input);
        self.second.tool_start(id, name, input);
    }
    fn tool_end(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        self.first.tool_end(id, name, output, is_error);
        self.second.tool_end(id, name, output, is_error);
    }
    fn tool_change(&mut self, id: &str, path: &std::path::Path, before: Option<&str>) {
        self.first.tool_change(id, path, before);
        self.second.tool_change(id, path, before);
    }
    fn tool_media(&mut self, id: &str, media: &[codesmith_ai::Media]) {
        self.first.tool_media(id, media);
        self.second.tool_media(id, media);
    }
    fn tool_progress(&mut self, id: &str, metadata: &Value) {
        self.first.tool_progress(id, metadata);
        self.second.tool_progress(id, metadata);
    }
    fn tool_output(&mut self, id: &str, delta: &str) {
        self.first.tool_output(id, delta);
        self.second.tool_output(id, delta);
    }
    fn served(&mut self, served: &codesmith_ai::Served) {
        self.first.served(served);
        self.second.served(served);
    }
    fn step_billed(&mut self, billed: Option<f64>) {
        self.first.step_billed(billed);
        self.second.step_billed(billed);
    }
    fn step_end(&mut self, reason: &FinishReason, usage: &Usage) {
        self.first.step_end(reason, usage);
        self.second.step_end(reason, usage);
    }
    fn retry(&mut self, attempt: u32, error: &str, wait: Duration) {
        self.first.retry(attempt, error, wait);
        self.second.retry(attempt, error, wait);
    }
    fn compaction_start(&mut self) {
        self.first.compaction_start();
        self.second.compaction_start();
    }
    fn compaction_delta(&mut self, d: &str) {
        self.first.compaction_delta(d);
        self.second.compaction_delta(d);
    }
    fn compaction_end(&mut self, outcome: Result<&str, &str>) {
        self.first.compaction_end(outcome);
        self.second.compaction_end(outcome);
    }
    fn compacted(&mut self, before: u64, after: u64) {
        self.first.compacted(before, after);
        self.second.compacted(before, after);
    }
    fn todos(&mut self, todos: &[Todo]) {
        self.first.todos(todos);
        self.second.todos(todos);
    }
    fn notice(&mut self, m: &str) {
        self.first.notice(m);
        self.second.notice(m);
    }
    fn instructions(&mut self, paths: &[std::path::PathBuf]) {
        self.first.instructions(paths);
        self.second.instructions(paths);
    }
    fn child_start(&mut self, child: &ChildSession) {
        self.first.child_start(child);
        self.second.child_start(child);
    }
    fn child_end(&mut self, id: &str, status: &str, error: Option<&str>) {
        self.first.child_end(id, status, error);
        self.second.child_end(id, status, error);
    }
    fn child_text(&mut self, d: &str) {
        self.first.child_text(d);
        self.second.child_text(d);
    }
    fn child_reasoning(&mut self, d: &str) {
        self.first.child_reasoning(d);
        self.second.child_reasoning(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subagents_tool_calls_name_it_until_it_ends() {
        let out = Arc::new(Mutex::new(Vec::new()));
        let o = out.clone();
        let mut s = JsonEvents::new(SharedChildren::default(), move |v| {
            o.lock().unwrap().push(v)
        });
        s.tool_start("c1", "subagent", &json!({"agent": "explore"}));
        s.child_start(&ChildSession {
            id: "ses_child".into(),
            parent_id: "ses_top".into(),
            depth: 1,
            title: "look".into(),
        });
        s.tool_start("c2", "explore › read", &json!({}));
        s.child_end("ses_child", "completed", None);
        s.tool_end("c1", "subagent", "ok", false);
        let v = out.lock().unwrap();
        assert_eq!(v[1]["agent"], "explore");
        assert_eq!(v[1]["tool"], "c1");
        assert_eq!(v[2]["session"], "ses_child");
        assert!(v[4].get("session").is_none());
        assert_eq!(v[4]["error"], json!(false), "a JSON boolean");
    }

    /// Every method reaches both sides of a tee (each is forwarded, so
    /// none falls back to the trait's default).
    #[test]
    fn a_tee_forwards_every_event_to_both() {
        struct Count(usize);
        impl Sink for Count {
            fn text(&mut self, _: &str) {
                self.0 += 1;
            }
            fn tool_output(&mut self, _: &str, _: &str) {
                self.0 += 1;
            }
            fn compaction_delta(&mut self, _: &str) {
                self.0 += 1;
            }
        }
        let (mut a, mut b) = (Count(0), Count(0));
        let mut t = Tee {
            first: &mut a,
            second: &mut b,
        };
        t.text("x");
        t.tool_output("c", "y");
        t.compaction_delta("z");
        assert_eq!((a.0, b.0), (3, 3));
    }
}
