//! The UI on the local bus (see `codesmith_core::bus`): one member per
//! UI, which follows the session shown. A request or a response from
//! another session starts a turn when the session is idle and waits its
//! turn otherwise; a progress note is shown. `/bus` looks and changes
//! what peers see.

use super::app::Entry;
use super::bridge::{ChanSink, Ev};
use super::Runner;
use codesmith_core::bus::{self, client, Joined, Kind, Message};
use codesmith_core::session::Session;

/// What `/bus share NAME` says once the session is shared.
pub(super) fn share_hint(name: &str) -> String {
    let bridge = if crate::bridge_init::valid_name(name) {
        name
    } else {
        "NAME"
    };
    format!(
        "⇄ Shared as {name}: a tailbus bridge running `codesmith bus relay --to {name}` reaches this session. To set one up, run `codesmith bridge init {bridge}` in a terminal and choose Relay."
    )
}

/// The `/bus` help.
pub const HELP: &str =
    "/bus — this session on the local bus: its name, the other sessions, and messages waiting
/bus name NAME — what other sessions call this one
/bus share NAME — let tailbus reach this session through `codesmith bus relay --to NAME` (`codesmith bridge init NAME` sets up the bridge)
/bus unshare — stop that
/bus pause — take no new requests (they wait) · /bus resume";

impl Runner {
    /// Join the bus as the session now shown, unless it's turned off.
    pub(super) fn join_bus(&mut self) {
        if self.bus.is_some() || !bus::enabled(&self.loaded.config) {
            return;
        }
        let Some(s) = &self.session else { return };
        let member = client::session_member(
            &s.id,
            s.title.as_deref(),
            &s.cwd.display().to_string(),
            &s.agent.id,
            &s.model_spec,
            true,
        );
        let b = bus::Bus::join(member, client::Options::default());
        if let Some(mut rx) = b.incoming() {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                while let Some(m) = rx.recv().await {
                    if tx.send(Ev::Bus(Box::new(m))).is_err() {
                        break;
                    }
                }
            });
        }
        {
            let mut w = b.watch();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                loop {
                    let st = w.borrow_and_update().clone();
                    if tx.send(Ev::BusStatus(st)).is_err() {
                        break;
                    }
                    if w.changed().await.is_err() {
                        break;
                    }
                }
            });
        }
        self.bus = Some(b.clone());
        if let Some(s) = self.session.as_mut() {
            s.set_bus(Some(b));
        }
    }

    /// Give a session being installed the bus, and tell the bus when the
    /// session shown, its title, agent or model changed.
    pub(super) fn bus_follow(&mut self, s: &mut Session) {
        let Some(b) = &self.bus else { return };
        s.set_bus(Some(b.clone()));
        let key = (
            s.id.clone(),
            s.title.clone(),
            s.agent.id.clone(),
            s.model_spec.clone(),
        );
        if self.bus_seen.as_ref() == Some(&key) {
            return;
        }
        let first = self.bus_seen.is_none();
        let switched = self.bus_seen.as_ref().is_some_and(|k| k.0 != key.0);
        self.bus_seen = Some(key);
        if first && b.session() == s.id {
            // It joined as this session, with these.
            return;
        }
        if switched {
            // Requests waiting for the last session stay for it.
            self.bus_queue.clear();
            self.app.bus_waiting = 0;
        }
        let change = client::Change {
            session: switched.then(|| s.id.clone()),
            title: s.title.clone(),
            project: Some(s.cwd.display().to_string()),
            agent: Some(s.agent.id.clone()),
            model: Some(s.model_spec.clone()),
            ..Default::default()
        };
        let b = b.clone();
        tokio::spawn(async move {
            let _ = b.update(change).await;
        });
    }

    pub(super) fn on_bus_status(&mut self, st: Joined) {
        self.app.bus_name = match st {
            Joined::On(n) => Some(n),
            _ => None,
        };
    }

    /// A message from another session.
    pub(super) fn on_bus(&mut self, m: Message) {
        if m.to != self.session_id {
            // For a session no longer shown: it comes back when that one is.
            return;
        }
        match m.kind {
            Kind::Progress => {
                let first = m.body.lines().next().unwrap_or("").to_string();
                self.app
                    .notice(format!("⇄ {} (progress): {first}", m.from_name));
                if let Some(b) = self.bus.clone() {
                    let id = m.id.clone();
                    tokio::spawn(async move {
                        let _ = b.ack(&id).await;
                    });
                }
            }
            Kind::Request | Kind::Response => {
                if self.bus_queue.iter().any(|q| q.id == m.id)
                    || self.bus_running.as_ref().is_some_and(|r| r.id == m.id)
                {
                    return;
                }
                self.bus_queue.push_back(m);
                self.app.bus_waiting = self.bus_queue.len();
                self.bus_next();
            }
        }
    }

    /// Start the next waiting message's turn, if the session is free.
    pub(super) fn bus_next(&mut self) {
        if self.app.busy
            || self.task.is_some()
            || self.stopping
            || self.session.is_none()
            || self.bus_running.is_some()
            || self.bus_paused
        {
            return;
        }
        let Some(m) = self.bus_queue.pop_front() else {
            return;
        };
        self.app.bus_waiting = self.bus_queue.len();
        let Some(mut s) = self.session.take() else {
            self.bus_queue.push_front(m);
            return;
        };
        // `bus_receive`: whether this session takes requests from the sender.
        if m.kind == Kind::Request {
            let gate = s.gate_for_bus();
            let req = codesmith_core::permission::Request {
                action: "bus_receive".into(),
                resources: vec![m.from_name.clone()],
                save: vec![m.from_name.clone()],
                title: format!(
                    "Take a request from {}: {}",
                    m.from_name,
                    m.body.lines().next().unwrap_or("")
                ),
            };
            gate.set_origin(Some(m.from_name.clone()));
            let allowed = tokio::task::block_in_place(|| gate.assert(&req));
            gate.set_origin(None);
            if let Err(e) = allowed {
                self.session = Some(s);
                self.app
                    .notice(format!("⇄ refused a request from {}: {e}", m.from_name));
                if let Some(b) = self.bus.clone() {
                    let id = m.id.clone();
                    let hop = m.hop;
                    tokio::spawn(async move {
                        let _ = b
                            .send(
                                "",
                                Kind::Response,
                                "Refused by the receiving session.",
                                Some(&id),
                                hop,
                            )
                            .await;
                        let _ = b.ack(&id).await;
                    });
                }
                self.bus_next();
                return;
            }
        }
        let shown = match m.kind {
            Kind::Request => format!("⇄ Request from {}\n{}", m.from_name, m.body),
            _ => format!("⇄ Response from {}\n{}", m.from_name, m.body),
        };
        self.app.entries.push(Entry::User(shown));
        self.app.start_turn();
        self.app.bus_origin = Some(m.from_name.clone());
        self.bus_running = Some(m.clone());
        self.bus_busy(true);
        let tx = self.tx.clone();
        let cwd = s.cwd.clone();
        self.task = Some(tokio::spawn(async move {
            let mut sink = ChanSink::new(tx.clone(), cwd);
            let result = s
                .prompt_from_bus(&m, &mut sink)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Ev::Done {
                result,
                session: Box::new(s),
            });
        }));
    }

    /// A turn ended: the message it answered is handled; the next may go.
    pub(super) fn bus_turn_done(&mut self) {
        self.app.bus_origin = None;
        if let Some(m) = self.bus_running.take() {
            if let Some(b) = self.bus.clone() {
                tokio::spawn(async move {
                    let _ = b.ack(&m.id).await;
                });
            }
        }
        self.bus_busy(false);
        self.bus_next();
    }

    /// The user stopped a turn. One answering a request answers it so;
    /// waiting messages hold until `/bus resume`, so the user has the
    /// session back.
    pub(super) fn bus_interrupted(&mut self) {
        self.app.bus_origin = None;
        if let Some(m) = self.bus_running.take() {
            if let Some(b) = self.bus.clone() {
                tokio::spawn(async move {
                    if m.kind == Kind::Request {
                        let _ = b
                            .send(
                                "",
                                Kind::Response,
                                "The session's user stopped this request before it finished.",
                                Some(&m.id),
                                m.hop,
                            )
                            .await;
                    }
                    let _ = b.ack(&m.id).await;
                });
            }
        }
        self.bus_busy(false);
        if !self.bus_queue.is_empty() {
            self.bus_paused = true;
            self.app.notice(format!(
                "⇄ {} bus message(s) wait; /bus resume to take them.",
                self.bus_queue.len()
            ));
        }
    }

    /// Tell peers whether this session is busy.
    pub(super) fn bus_busy(&self, busy: bool) {
        if let Some(b) = self.bus.clone() {
            tokio::spawn(async move {
                let _ = b
                    .update(client::Change {
                        busy: Some(busy),
                        ..Default::default()
                    })
                    .await;
            });
        }
    }

    /// `/bus …`.
    pub(super) fn bus_command(&mut self, args: &str) {
        let Some(b) = self.bus.clone() else {
            self.app.notice(if bus::enabled(&self.loaded.config) {
                "This session isn't on the bus yet."
            } else {
                "The bus is off (\"bus\": false or CODESMITH_BUS=0)."
            });
            return;
        };
        let mut words = args.split_whitespace();
        let verb = words.next().unwrap_or("");
        let arg = words.next().unwrap_or("").to_string();
        let tx = self.tx.clone();
        let say = move |m: String| {
            let _ = tx.send(Ev::Notice(m));
        };
        match verb {
            "" | "status" | "peers" => {
                tokio::spawn(async move {
                    let me = b.session();
                    let name = b.name().unwrap_or_else(|| "(joining)".into());
                    let peers = b.peers().await;
                    let inbox = b.inbox(None).await;
                    let mut out = format!("⇄ On the bus as {name}.");
                    match peers {
                        Ok(p) => {
                            out.push('\n');
                            out.push_str(&codesmith_core::tool::bus::describe_peers(&me, &p));
                            if let Some(s) = p
                                .iter()
                                .find(|p| p.session == me)
                                .and_then(|p| p.shared.clone())
                            {
                                out.push_str(&format!("\nShared to relays as {s}."));
                            }
                        }
                        Err(e) => out.push_str(&format!("\nThe bus didn't answer: {e}")),
                    }
                    if let Ok(i) = inbox {
                        out.push_str(&format!("\n{} message(s) waiting.", i.len()));
                    }
                    say(out);
                });
            }
            "name" if !arg.is_empty() => {
                tokio::spawn(async move {
                    match b
                        .update(client::Change {
                            name: Some(arg),
                            ..Default::default()
                        })
                        .await
                    {
                        Ok(n) => say(format!("⇄ This session is now {n} on the bus.")),
                        Err(e) => say(format!("⇄ {e}")),
                    }
                });
            }
            "share" if !arg.is_empty() => {
                tokio::spawn(async move {
                    match b.share(&arg).await {
                        Ok(()) => say(share_hint(&arg)),
                        Err(e) => say(format!("⇄ {e}")),
                    }
                });
            }
            "unshare" => {
                tokio::spawn(async move {
                    match b.unshare().await {
                        Ok(true) => say("⇄ No longer shared.".into()),
                        Ok(false) => say("⇄ This session wasn't shared.".into()),
                        Err(e) => say(format!("⇄ {e}")),
                    }
                });
            }
            "pause" | "resume" => {
                let on = verb == "pause";
                self.bus_paused = on;
                tokio::spawn(async move {
                    match b.pause(on).await {
                        Ok(()) if on => say(
                            "⇄ Taking no new requests; they wait. /bus resume to take them.".into(),
                        ),
                        Ok(()) => say("⇄ Taking requests again.".into()),
                        Err(e) => say(format!("⇄ {e}")),
                    }
                });
                if !on {
                    self.bus_next();
                }
            }
            _ => self.app.notice(HELP),
        }
    }
}

#[cfg(test)]
mod share_hint_tests {
    use super::share_hint;

    #[test]
    fn sharing_points_at_bridge_init() {
        let h = share_hint("reviewer");
        assert!(h.contains("Shared as reviewer"), "{h}");
        assert!(h.contains("codesmith bus relay --to reviewer"), "{h}");
        assert!(h.contains("codesmith bridge init reviewer"), "{h}");
        // A share name tailbus can't take as an agent's isn't offered as one.
        let h = share_hint("Review Bot");
        assert!(h.contains("codesmith bridge init NAME"), "{h}");
    }
}
