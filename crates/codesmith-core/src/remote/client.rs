//! How a client follows a session's events, as the app team's
//! `remote_events.json` (S24) pins it: the cursor's decision for each
//! event, the plan a snapshot gives, and the transcript the events build.
//! The tests use it as the phone does, and it shows what a client does
//! with what the machine sends.

use serde_json::{json, Value};
use std::collections::HashMap;

/// A position in a session's stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

impl Cursor {
    pub fn json(&self) -> Value {
        json!({"epoch": self.epoch, "seq": self.seq})
    }

    /// `Last-Event-ID`'s value.
    pub fn id(&self) -> String {
        format!("{}:{}", self.epoch, self.seq)
    }

    /// `<epoch>:<seq>`, or `None` when it isn't one.
    pub fn parse(id: &str) -> Option<Cursor> {
        let (e, s) = id.rsplit_once(':')?;
        if e.is_empty() || s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Cursor {
            epoch: e.to_string(),
            seq: s.parse().ok()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Apply it; the cursor moves here (`None`: an unnumbered event,
    /// applied without moving it).
    Apply(Option<Cursor>),
    Skip,
    /// Reload the session.
    Reset(String),
}

/// What to do with one event (`id` is its SSE id).
pub fn observe(cursor: Option<&Cursor>, id: Option<&str>, data: &Value) -> Verdict {
    if data["type"] == "reset" {
        return Verdict::Reset(data["reason"].as_str().unwrap_or("unknown").to_string());
    }
    let pos = id.and_then(Cursor::parse).or_else(|| {
        let e = data["epoch"].as_str().filter(|e| !e.is_empty())?;
        Some(Cursor {
            epoch: e.to_string(),
            seq: data["seq"].as_u64()?,
        })
    });
    let Some(pos) = pos else {
        return Verdict::Apply(None);
    };
    if let Some(c) = cursor {
        if pos.epoch != c.epoch {
            return Verdict::Reset("epoch".into());
        }
        if pos.seq <= c.seq {
            return Verdict::Skip;
        }
        if data["merged_from"].as_u64().is_some_and(|m| m <= c.seq) {
            return Verdict::Reset("merged".into());
        }
    }
    Verdict::Apply(Some(pos))
}

/// What a `GET /v1/sessions/{id}` snapshot tells a client to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Where to subscribe from.
    pub cursor: Option<Cursor>,
    /// The running turn the subscription replays from its start
    /// (`epoch:turn.id`).
    pub replaying_turn: Option<String>,
    /// Whether to append `turn.partial` to the messages.
    pub partial: bool,
}

pub fn plan(snapshot: &Value) -> Plan {
    let none = Plan {
        cursor: None,
        replaying_turn: None,
        partial: false,
    };
    let Some(epoch) = snapshot["epoch"].as_str().filter(|e| !e.is_empty()) else {
        return none;
    };
    let Some(events_seq) = snapshot["events_seq"].as_u64() else {
        return none;
    };
    let turn = &snapshot["turn"];
    if let (true, Some(p)) = (turn["partial"].is_array(), turn["partial_seq"].as_u64()) {
        return Plan {
            cursor: Some(Cursor {
                epoch: epoch.into(),
                seq: p,
            }),
            replaying_turn: None,
            partial: true,
        };
    }
    Plan {
        cursor: Some(Cursor {
            epoch: epoch.into(),
            seq: events_seq,
        }),
        replaying_turn: turn["id"].as_str().map(|t| turn_key(Some(epoch), t)),
        partial: false,
    }
}

/// The key a card's turn is filed under.
pub fn turn_key(epoch: Option<&str>, turn: &str) -> String {
    match epoch.filter(|e| !e.is_empty()) {
        Some(e) => format!("{e}:{turn}"),
        None => turn.to_string(),
    }
}

/// How long to wait before reloading after `in_a_row` resets.
pub fn reload_delay(in_a_row: u64, backoff_ms: u64) -> u64 {
    if in_a_row == 1 {
        0
    } else {
        backoff_ms
    }
}

/// A transcript as the app shows it: items of kind `user`, `assistant`,
/// `tool`, `permission` and `question`.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub items: Vec<Value>,
    pub busy: bool,
    pub cursor: Option<Cursor>,
    /// Answers given on this client, by turn key and prompt id; a card
    /// that comes back with a replay gets its answer again.
    pub local: HashMap<(String, String), Value>,
    /// The turn the last event was in.
    turn: Option<String>,
    /// Resets seen, for the reload back-off.
    pub resets: u64,
}

impl Transcript {
    /// Load a snapshot (messages in serve's format): items from them, and
    /// the plan's cursor.
    pub fn load(&mut self, snapshot: &Value) -> Plan {
        let p = plan(snapshot);
        self.items.clear();
        self.load_messages(&snapshot["messages"]);
        if p.partial {
            self.load_messages(&snapshot["turn"]["partial"]);
        }
        self.busy = snapshot["busy"].as_bool().unwrap_or(false);
        self.cursor = p.cursor.clone();
        p
    }

    fn load_messages(&mut self, messages: &Value) {
        for m in messages.as_array().into_iter().flatten() {
            let role = m["role"].as_str().unwrap_or("");
            for part in m["parts"].as_array().into_iter().flatten() {
                let data = &part["data"];
                match (role, part["type"].as_str().unwrap_or("")) {
                    ("user", "text") => self
                        .items
                        .push(json!({"kind": "user", "text": text_of(data)})),
                    ("assistant", "text") => self.assistant(&text_of(data)),
                    (_, "tool_call") => self.items.push(tool_item(
                        data["id"].as_str().unwrap_or(""),
                        data["name"].as_str().unwrap_or(""),
                        &data["input"],
                    )),
                    (_, "tool_result") => {
                        if let Some(t) = self.tool(data["id"].as_str().unwrap_or("")) {
                            t["output"] = data["content"].clone();
                            t["status"] = json!(if data["is_error"] == json!(true) {
                                "error"
                            } else {
                                "done"
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn assistant(&mut self, text: &str) {
        match self.items.last_mut() {
            Some(i) if i["kind"] == "assistant" => {
                let t = format!("{}{text}", i["text"].as_str().unwrap_or(""));
                i["text"] = json!(t);
            }
            _ => self.items.push(json!({"kind": "assistant", "text": text})),
        }
    }

    fn tool(&mut self, id: &str) -> Option<&mut Value> {
        self.items
            .iter_mut()
            .rev()
            .find(|i| i["kind"] == "tool" && i["callId"] == id)
    }

    fn card(&mut self, id: &str) -> Option<&mut Value> {
        self.items.iter_mut().rev().find(|i| {
            (i["kind"] == "permission" && i["requestId"] == id)
                || (i["kind"] == "question" && i["questionId"] == id)
        })
    }

    /// Answer a prompt on this client.
    pub fn answer(&mut self, id: &str, reply: Value) {
        if let Some(turn) = self.turn.clone() {
            self.local.insert((turn, id.to_string()), reply.clone());
        }
        self.resolve(id, &reply);
    }

    fn resolve(&mut self, id: &str, reply: &Value) {
        if let Some(c) = self.card(id) {
            if c["kind"] == "permission" {
                c["answered"] = reply.clone();
            } else {
                c["status"] = json!("answered");
                c["answers"] = reply.clone();
            }
        }
    }

    /// Deliver one event through the cursor: the verdict, and with
    /// `apply`, its effect on the transcript.
    pub fn deliver(&mut self, id: Option<&str>, data: &Value) -> Verdict {
        let v = observe(self.cursor.as_ref(), id, data);
        if let Verdict::Apply(at) = &v {
            if let Some(at) = at {
                self.cursor = Some(at.clone());
            }
            self.apply(data);
        }
        if matches!(v, Verdict::Reset(_)) {
            self.resets += 1;
        }
        v
    }

    fn apply(&mut self, e: &Value) {
        if e.get("child").is_some_and(|c| !c.is_null()) {
            return;
        }
        if let Some(t) = e["turn"].as_str() {
            self.turn = Some(turn_key(e["epoch"].as_str(), t));
        }
        let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
        match e["type"].as_str().unwrap_or("") {
            "turn_start" => self.busy = true,
            "user_message" => self.items.push(json!({"kind": "user", "text": s("text")})),
            "text" => self.assistant(&s("delta")),
            "tool_start" => self
                .items
                .push(tool_item(&s("id"), &s("name"), &e["input"])),
            "tool_output" => {
                if let Some(t) = self.tool(&s("id")) {
                    let o = format!("{}{}", t["output"].as_str().unwrap_or(""), s("delta"));
                    t["output"] = json!(o);
                }
            }
            "tool_end" => {
                let (out, err) = (e["output"].clone(), e["error"] == json!(true));
                if let Some(t) = self.tool(&s("id")) {
                    t["output"] = out;
                    t["status"] = json!(if err { "error" } else { "done" });
                }
            }
            "permission" => {
                self.items
                    .push(json!({"kind": "permission", "requestId": s("id"),
                    "title": e["title"], "action": e["action"], "resources": e["resources"],
                    "answered": null}));
                self.restore(&s("id"));
            }
            "question" => {
                self.items
                    .push(json!({"kind": "question", "questionId": s("id"),
                    "prompts": e["prompts"], "status": "pending"}));
                self.restore(&s("id"));
            }
            "prompt_resolved" => self.resolve(&s("id"), &e["reply"]),
            "done" => self.busy = false,
            "session_status" => self.busy = e["busy"].as_bool().unwrap_or(self.busy),
            _ => {}
        }
    }

    fn restore(&mut self, id: &str) {
        if let Some(turn) = self.turn.clone() {
            if let Some(r) = self.local.get(&(turn, id.to_string())).cloned() {
                self.resolve(id, &r);
            }
        }
    }
}

fn text_of(data: &Value) -> String {
    data.as_str()
        .or_else(|| data["text"].as_str())
        .unwrap_or("")
        .to_string()
}

fn tool_item(id: &str, name: &str, input: &Value) -> Value {
    let input = input["command"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| input.to_string());
    json!({"kind": "tool", "callId": id, "name": name, "input": input, "output": "", "status": "running"})
}
