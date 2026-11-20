//! Terminals that live in the server's shell daemon (OpenCode's
//! persistent terminals): the same [`super::Ptys`] API, each call a
//! request to the daemon, which runs a `Ptys` of its own. They survive a
//! server restart that hands the daemon over; the daemon's terminal events
//! are republished on the server's own event channel.

use super::{Attachment, CreateInput, Event, Info, Output, PtyError, ReadResult, Size};
use crate::daemon::protocol::{self, PtyCreate, Request, Response};
use crate::daemon::{Client, DaemonError, Kind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{broadcast, mpsc};

pub(super) struct Remote {
    pub client: Arc<Client>,
    pub directory: PathBuf,
    pub shell: String,
    /// Whether the event forwarder runs.
    following: Arc<AtomicBool>,
}

fn error(e: DaemonError, id: &str) -> PtyError {
    match e.code.as_deref() {
        Some("not_found") => PtyError::NotFound(id.into()),
        Some("exited") => PtyError::Exited(id.into()),
        Some("limit") => PtyError::Limit(super::RUNNING),
        Some("invalid_cwd") => PtyError::InvalidCwd(e.message),
        Some("invalid") => PtyError::Invalid(e.message),
        Some(_) => PtyError::Spawn(e.message),
        None if e.kind == Kind::Connect => PtyError::NotFound(id.into()),
        None => PtyError::Spawn(format!("shell daemon: {}", e.message)),
    }
}

fn unexpected(r: Response) -> PtyError {
    PtyError::Spawn(format!("unexpected shell daemon response: {}", r.kind()))
}

impl Remote {
    pub fn new(client: Arc<Client>, directory: PathBuf, shell: String) -> Remote {
        Remote {
            client,
            directory,
            shell,
            following: Arc::default(),
        }
    }

    fn info(&self, req: Request, id: &str, start: bool) -> Result<Info, PtyError> {
        match self.client.request(&req, start).map_err(|e| error(e, id))? {
            Response::Pty { info } => Ok(info),
            r => Err(unexpected(r)),
        }
    }

    fn ok(&self, req: Request, id: &str) -> Result<(), PtyError> {
        match self.client.request(&req, false).map_err(|e| error(e, id))? {
            Response::Ok => Ok(()),
            r => Err(unexpected(r)),
        }
    }

    /// Republish the daemon's terminal events on `events` while the
    /// daemon runs and `alive` does.
    pub fn follow(&self, events: broadcast::Sender<Event>, alive: Weak<()>) {
        if self.following.swap(true, Ordering::SeqCst) {
            return;
        }
        let (client, following) = (self.client.clone(), self.following.clone());
        let started = std::thread::Builder::new()
            .name("codesmith-pty-events".into())
            .spawn(move || {
                if let Ok((_, mut s)) = client.stream(&Request::Events) {
                    while alive.upgrade().is_some() {
                        let Ok(Some(body)) = protocol::read_frame(&mut s) else {
                            break;
                        };
                        if let Ok(Response::Event { event }) = protocol::decode(&body) {
                            let _ = events.send(event);
                        }
                    }
                }
                following.store(false, Ordering::SeqCst);
            });
        if started.is_err() {
            self.following.store(false, Ordering::SeqCst);
        }
    }

    pub fn list(&self, session: Option<&str>) -> Vec<Info> {
        let req = Request::ListPty {
            session: session.map(str::to_string),
        };
        match self.client.request_if_running(&req) {
            Ok(Some(Response::Ptys { terminals })) => terminals,
            _ => vec![],
        }
    }

    pub fn get(&self, id: &str) -> Result<Info, PtyError> {
        self.info(Request::GetPty { id: id.into() }, id, false)
    }

    /// Start a terminal, starting the daemon if none runs yet.
    pub fn create(&self, input: CreateInput) -> Result<Info, PtyError> {
        let command = input
            .command
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| self.shell.clone());
        let cwd = match input.cwd.filter(|c| !c.is_empty()) {
            Some(c) => self.directory.join(c),
            None => self.directory.clone(),
        };
        if !cwd.is_dir() {
            return Err(PtyError::InvalidCwd(cwd.display().to_string()));
        }
        let size = input.size.unwrap_or_default();
        if size.cols == 0 || size.rows == 0 {
            return Err(PtyError::Invalid("size must be positive".into()));
        }
        let req = Request::CreatePty {
            input: PtyCreate {
                command,
                args: input.args.unwrap_or_default(),
                cwd: cwd.display().to_string(),
                title: input.title,
                env: input.env.unwrap_or_default(),
                size: Some(size),
                session_id: input.session_id,
            },
        };
        self.info(req, "", true)
    }

    pub fn update(
        &self,
        id: &str,
        title: Option<Option<String>>,
        size: Option<Size>,
    ) -> Result<Info, PtyError> {
        self.info(
            Request::UpdatePty {
                id: id.into(),
                title,
                size,
            },
            id,
            false,
        )
    }

    pub fn write(&self, id: &str, data: &str) -> Result<(), PtyError> {
        self.ok(
            Request::WritePty {
                id: id.into(),
                data: data.into(),
            },
            id,
        )
    }

    pub fn remove(&self, id: &str) -> Result<(), PtyError> {
        self.ok(Request::RemovePty { id: id.into() }, id)
    }

    pub fn remove_session(&self, session: &str) {
        let _ = self.client.request_if_running(&Request::RemovePtySession {
            session: session.into(),
        });
    }

    pub fn read(&self, session: &str, lines: Option<u32>) -> Result<Option<ReadResult>, PtyError> {
        let req = Request::ReadPty {
            session: session.into(),
            lines,
        };
        match self.client.request_if_running(&req) {
            Ok(Some(Response::Read { result })) => Ok(result),
            Ok(None) => Ok(None),
            Ok(Some(r)) => Err(unexpected(r)),
            Err(e) => Err(error(e, session)),
        }
    }

    pub fn attach(&self, id: &str, cursor: Option<i64>) -> Result<Attachment, PtyError> {
        let (first, mut stream) = self
            .client
            .stream(&Request::Attach {
                id: id.into(),
                cursor,
            })
            .map_err(|e| error(e, id))?;
        let Response::Attached { replay, cursor } = first else {
            return Err(unexpected(first));
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let closer = stream
            .try_clone()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        {
            let pending = pending.clone();
            std::thread::Builder::new()
                .name("codesmith-pty-attach".into())
                .spawn(move || loop {
                    let Ok(Some(body)) = protocol::read_frame(&mut stream) else {
                        let _ = tx.send(Output::End { exit_code: None });
                        return;
                    };
                    match protocol::decode(&body) {
                        Ok(Response::Output { data }) => {
                            pending.fetch_add(data.len(), Ordering::SeqCst);
                            if tx.send(Output::Data(data)).is_err() {
                                return;
                            }
                        }
                        Ok(Response::End { exit_code }) => {
                            let _ = tx.send(Output::End { exit_code });
                            return;
                        }
                        _ => {}
                    }
                })
                .map_err(|e| PtyError::Spawn(e.to_string()))?;
        }
        Ok(Attachment {
            replay,
            cursor,
            sub: 0,
            rx,
            pending,
            term: Weak::new(),
            remote: Some(super::RemoteAttachment {
                stream: closer,
                client: self.client.clone(),
                id: id.into(),
            }),
        })
    }
}
