//! The channel between a running session (on its own task) and the UI.
//! The session's sink, permission asker and questioner all turn into
//! messages; prompts carry a reply channel the UI answers on.

use codesmith_ai::{FinishReason, Usage};
use codesmith_core::permission::{Asker, Reply, Request};
use codesmith_core::question::{Prompt, Questioner};
use codesmith_core::session::{Session, Sink};
use codesmith_core::tool::todo::Todo;
use ratatui::crossterm::event::KeyEvent;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

pub type Answers = Option<Vec<Vec<String>>>;

pub enum Ev {
    Key(KeyEvent),
    /// A diff viewer load finished.
    Diff(Box<super::diff_view::Response>),
    /// An answer for the open menu.
    Browse(Box<super::browser::Response>),
    Paste(String),
    Resize,
    /// The terminal gained (`true`) or lost the focus.
    Focus(bool),
    /// The terminal went away (`SIGHUP`): close as on exit.
    Hangup,
    /// The project's files, listed in the background for `@` completion.
    Files(Vec<String>),
    /// The local servers were asked for their models (the model picker
    /// refreshes its list).
    LocalModels,
    /// A model swap's output line or result (see `swap`).
    Swap(super::swap::Progress),
    Text(String),
    Reasoning(String),
    /// A tool call started streaming its arguments.
    ToolInput {
        id: String,
        name: String,
    },
    /// Its arguments finished: the id of the call they formed, or `None`
    /// when the call was dropped.
    ToolInputEnd {
        id: String,
        call: Option<String>,
    },
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
    /// An `execute` call's nested tool calls so far.
    ToolCalls {
        id: String,
        calls: Vec<super::group::Call>,
    },
    /// Output a running command printed since the last report.
    ToolOutput {
        id: String,
        delta: String,
    },
    /// Labels of the images and files a call returned, before its end.
    ToolMedia {
        id: String,
        labels: Vec<String>,
    },
    /// The label of an image (or other file) the model generated.
    Media(String),
    /// Instruction files a step's reads brought in.
    Instructions(Vec<PathBuf>),
    /// A subagent session started (`agent` is the one its call named).
    /// Until its `ChildEnd`, tool calls are its own (or its subagents').
    ChildStart {
        id: String,
        title: String,
        agent: String,
    },
    /// The subagent ended: `completed`, `interrupted` or `failed`.
    ChildEnd {
        id: String,
        status: String,
        error: Option<String>,
    },
    /// Text the running subagent wrote.
    ChildText(String),
    Todos(Vec<Todo>),
    Notice(String),
    Compacted(u64, u64),
    StepEnd(Usage),
    Permission(Request, oneshot::Sender<Reply>),
    Question(Vec<Prompt>, oneshot::Sender<Answers>),
    /// An MCP server's OAuth login ended.
    McpSignedIn {
        server: String,
        result: Result<(), String>,
    },
    /// A sign-in picker login's progress or end, with the number the
    /// runner gave that login (a cancelled one's late word is dropped).
    Login(u64, super::login_view::LoginEv),
    /// A whole form to answer (an MCP server's elicitation).
    Form(
        codesmith_core::form::Info,
        oneshot::Sender<Option<codesmith_core::form::Answer>>,
    ),
    /// The turn ended; the session comes back to the UI.
    Done {
        result: Result<(), String>,
        session: Box<Session>,
    },
    /// An interrupted turn that outlasted the wait for it has now stopped.
    Stopped,
    /// A background command of the idle session finished (or none runs any more).
    Background,
    /// An event of the session in this tab (see `tabs_host`).
    Tab(u64, Box<Ev>),
    /// A message from another session on the local bus.
    Bus(Box<codesmith_core::bus::Message>),
    /// This UI's place on the bus changed.
    BusStatus(codesmith_core::bus::Joined),
}

pub struct ChanSink {
    pub tx: UnboundedSender<Ev>,
    /// The session's directory, for reading files an edit is about to change.
    pub cwd: PathBuf,
    /// The agent the latest `subagent` call named, for its `child_start`.
    agent: String,
}

impl ChanSink {
    pub fn new(tx: UnboundedSender<Ev>, cwd: PathBuf) -> Self {
        ChanSink {
            tx,
            cwd,
            agent: String::new(),
        }
    }
}

impl Sink for ChanSink {
    fn text(&mut self, delta: &str) {
        let _ = self.tx.send(Ev::Text(delta.to_string()));
    }
    fn reasoning(&mut self, delta: &str) {
        let _ = self.tx.send(Ev::Reasoning(delta.to_string()));
    }
    fn tool_input(&mut self, id: &str, name: &str) {
        let _ = self.tx.send(Ev::ToolInput {
            id: id.to_string(),
            name: name.to_string(),
        });
    }
    fn tool_input_end(&mut self, id: &str, call: Option<&str>) {
        let _ = self.tx.send(Ev::ToolInputEnd {
            id: id.to_string(),
            call: call.map(str::to_string),
        });
    }
    fn tool_start(&mut self, id: &str, name: &str, input: &serde_json::Value) {
        if name.rsplit(" › ").next() == Some("subagent") {
            if let Some(a) = input.get("agent").and_then(|a| a.as_str()) {
                self.agent = a.to_string();
            }
        }
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
    fn tool_progress(&mut self, id: &str, metadata: &serde_json::Value) {
        if let Some(calls) = super::group::nested_calls(metadata) {
            let _ = self.tx.send(Ev::ToolCalls {
                id: id.to_string(),
                calls,
            });
        }
    }
    fn tool_output(&mut self, id: &str, delta: &str) {
        let _ = self.tx.send(Ev::ToolOutput {
            id: id.to_string(),
            delta: delta.to_string(),
        });
    }
    fn tool_media(&mut self, id: &str, media: &[codesmith_ai::Media]) {
        let _ = self.tx.send(Ev::ToolMedia {
            id: id.to_string(),
            labels: media.iter().map(super::app::media_label).collect(),
        });
    }
    fn instructions(&mut self, paths: &[PathBuf]) {
        let _ = self.tx.send(Ev::Instructions(paths.to_vec()));
    }
    fn media(&mut self, media: &codesmith_ai::Media) {
        let _ = self.tx.send(Ev::Media(super::app::media_label(media)));
    }
    fn step_end(&mut self, _reason: &FinishReason, usage: &Usage) {
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
    fn child_start(&mut self, child: &codesmith_core::session::ChildSession) {
        let _ = self.tx.send(Ev::ChildStart {
            id: child.id.clone(),
            title: child.title.clone(),
            agent: std::mem::take(&mut self.agent),
        });
    }
    fn child_end(&mut self, id: &str, status: &str, error: Option<&str>) {
        let _ = self.tx.send(Ev::ChildEnd {
            id: id.to_string(),
            status: status.to_string(),
            error: error.map(str::to_string),
        });
    }
    fn child_text(&mut self, delta: &str) {
        let _ = self.tx.send(Ev::ChildText(delta.to_string()));
    }
}

/// A turn's sink: the UI's, and the phones' (remote access) through `tap`.
pub fn turn_sink(
    tx: UnboundedSender<Ev>,
    cwd: PathBuf,
    tap: &codesmith_core::remote::tap::Tap,
) -> impl Sink {
    codesmith_core::events::Tee {
        first: ChanSink::new(tx, cwd),
        second: tap.sink(),
    }
}

/// Answers permission requests and questions through the UI. Called on a
/// blocking section of the session task, so it may wait. While the UI stops
/// a turn (`stopping`), open and new prompts are refused at once, so the
/// task reaches the await where its abort lands.
pub struct ChanPrompter {
    tx: UnboundedSender<Ev>,
    stopping: AtomicBool,
}

/// How often a waiting prompt checks whether its turn is being stopped.
const POLL: Duration = Duration::from_millis(20);

impl ChanPrompter {
    pub fn new(tx: UnboundedSender<Ev>) -> Self {
        ChanPrompter {
            tx,
            stopping: AtomicBool::new(false),
        }
    }

    pub fn set_stopping(&self, on: bool) {
        self.stopping.store(on, Ordering::SeqCst);
    }

    /// The answer on `rx`, or `None` once the turn is being stopped (or
    /// the UI dropped the prompt).
    fn wait<T>(&self, mut rx: oneshot::Receiver<T>) -> Option<T> {
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                return None;
            }
            match rx.try_recv() {
                Ok(v) => return Some(v),
                Err(oneshot::error::TryRecvError::Closed) => return None,
                Err(oneshot::error::TryRecvError::Empty) => std::thread::sleep(POLL),
            }
        }
    }
}

impl Asker for ChanPrompter {
    fn ask(&self, req: &Request) -> Reply {
        let (tx, rx) = oneshot::channel();
        if self.stopping.load(Ordering::SeqCst)
            || self.tx.send(Ev::Permission(req.clone(), tx)).is_err()
        {
            return Reply::Reject(None);
        }
        self.wait(rx).unwrap_or(Reply::Reject(None))
    }
}

impl Questioner for ChanPrompter {
    fn ask(&self, prompts: &[Prompt]) -> Answers {
        let (tx, rx) = oneshot::channel();
        if self.stopping.load(Ordering::SeqCst)
            || self.tx.send(Ev::Question(prompts.to_vec(), tx)).is_err()
        {
            return None;
        }
        self.wait(rx).flatten()
    }

    fn ask_form(
        &self,
        form: &codesmith_core::form::Info,
    ) -> Option<Option<codesmith_core::form::Answer>> {
        let (tx, rx) = oneshot::channel();
        if self.stopping.load(Ordering::SeqCst) || self.tx.send(Ev::Form(form.clone(), tx)).is_err()
        {
            return Some(None);
        }
        Some(self.wait(rx).flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn req() -> Request {
        Request {
            action: "shell".into(),
            resources: vec!["rm x".into()],
            save: vec![],
            title: String::new(),
            access: None,
        }
    }

    #[test]
    fn stopping_refuses_waiting_and_new_prompts() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let p = Arc::new(ChanPrompter::new(tx));
        let asking = {
            let p = p.clone();
            std::thread::spawn(move || Asker::ask(&*p, &req()))
        };
        // The prompt reaches the UI; its reply channel stays open (as when
        // it is still queued behind the key that stops the turn).
        let Some(Ev::Permission(_, reply)) = rx.blocking_recv() else {
            panic!("no permission prompt");
        };
        p.set_stopping(true);
        assert!(matches!(asking.join().unwrap(), Reply::Reject(None)));
        assert!(matches!(Asker::ask(&*p, &req()), Reply::Reject(None)));
        assert!(Questioner::ask(&*p, &[]).is_none());
        assert!(rx.try_recv().is_err(), "nothing more was shown");
        drop(reply);
        // Answered as usual once the stop is over.
        p.set_stopping(false);
        let asking = {
            let p = p.clone();
            std::thread::spawn(move || Asker::ask(&*p, &req()))
        };
        let Some(Ev::Permission(_, reply)) = rx.blocking_recv() else {
            panic!("no permission prompt");
        };
        reply.send(Reply::Once).unwrap();
        assert!(matches!(asking.join().unwrap(), Reply::Once));
    }
}
