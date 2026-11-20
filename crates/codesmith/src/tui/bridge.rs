//! The channel between a running session (on its own task) and the UI.
//! The session's sink, permission asker and questioner all turn into
//! messages; prompts carry a reply channel the UI answers on.

use codesmith_ai::Usage;
use codesmith_core::permission::{Asker, Reply, Request};
use codesmith_core::question::{Prompt, Questioner};
use codesmith_core::session::{Session, Sink};
use codesmith_core::tool::todo::Todo;
use ratatui::crossterm::event::KeyEvent;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

pub type Answers = Option<Vec<Vec<String>>>;

pub enum Ev {
    Key(KeyEvent),
    Paste(String),
    Resize,
    /// The project's files, listed in the background for `@` completion.
    Files(Vec<String>),
    Text(String),
    Reasoning(String),
    ToolStart {
        id: String,
        name: String,
        detail: String,
        /// The change an edit or write is about to make.
        diff: Option<super::diff::Diff>,
    },
    ToolEnd {
        id: String,
        name: String,
        output: String,
        error: bool,
    },
    Todos(Vec<Todo>),
    Notice(String),
    Compacted(u64, u64),
    StepEnd(Usage),
    Permission(Request, oneshot::Sender<Reply>),
    Question(Vec<Prompt>, oneshot::Sender<Answers>),
    /// The turn ended; the session comes back to the UI.
    Done {
        result: Result<(), String>,
        session: Box<Session>,
    },
}

pub struct ChanSink {
    pub tx: UnboundedSender<Ev>,
    /// The session's directory, for reading files an edit is about to change.
    pub cwd: PathBuf,
}

impl ChanSink {
    pub fn new(tx: UnboundedSender<Ev>, cwd: PathBuf) -> Self {
        ChanSink { tx, cwd }
    }
}

impl Sink for ChanSink {
    fn text(&mut self, delta: &str) {
        let _ = self.tx.send(Ev::Text(delta.to_string()));
    }
    fn reasoning(&mut self, delta: &str) {
        let _ = self.tx.send(Ev::Reasoning(delta.to_string()));
    }
    fn tool_start(&mut self, id: &str, name: &str, input: &serde_json::Value) {
        let _ = self.tx.send(Ev::ToolStart {
            id: id.to_string(),
            name: name.to_string(),
            detail: crate::render::describe(name, input),
            diff: super::diff::for_tool(name, input, &self.cwd),
        });
    }
    fn tool_end(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        let _ = self.tx.send(Ev::ToolEnd {
            id: id.to_string(),
            name: name.to_string(),
            output: output.to_string(),
            error: is_error,
        });
    }
    fn step_end(&mut self, _reason: &str, usage: &Usage) {
        let _ = self.tx.send(Ev::StepEnd(*usage));
    }
    fn retry(&mut self, attempt: u32, error: &str, wait: Duration) {
        let _ = self.tx.send(Ev::Notice(format!(
            "retry {attempt} in {:.1}s: {error}",
            wait.as_secs_f32()
        )));
    }
    fn compacted(&mut self, before: u64, after: u64) {
        let _ = self.tx.send(Ev::Compacted(before, after));
    }
    fn todos(&mut self, todos: &[Todo]) {
        let _ = self.tx.send(Ev::Todos(todos.to_vec()));
    }
    fn notice(&mut self, message: &str) {
        let _ = self.tx.send(Ev::Notice(message.to_string()));
    }
}

/// Answers permission requests and questions through the UI. Called on a
/// blocking section of the session task, so it may wait.
pub struct ChanPrompter(pub UnboundedSender<Ev>);

impl Asker for ChanPrompter {
    fn ask(&self, req: &Request) -> Reply {
        let (tx, rx) = oneshot::channel();
        if self.0.send(Ev::Permission(req.clone(), tx)).is_err() {
            return Reply::Reject(None);
        }
        rx.blocking_recv().unwrap_or(Reply::Reject(None))
    }
}

impl Questioner for ChanPrompter {
    fn ask(&self, prompts: &[Prompt]) -> Answers {
        let (tx, rx) = oneshot::channel();
        if self.0.send(Ev::Question(prompts.to_vec(), tx)).is_err() {
            return None;
        }
        rx.blocking_recv().ok().flatten()
    }
}
