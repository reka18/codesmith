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
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// An interrupted turn that outlasted the wait for it has now stopped.
    Stopped,
    /// A background command of the idle session finished (or none runs any more).
    Background,
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
