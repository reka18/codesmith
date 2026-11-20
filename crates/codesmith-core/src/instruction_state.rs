//! Instruction state: what the system prompt says, tracked through a
//! session as values, so a change mid-session reaches the model as a short
//! update in its place in the conversation instead of a rewritten system
//! prompt. Ported from OpenCode's `instructions/index.ts` and
//! `session/instruction-state.ts` (MIT), pinned by `tests/oc_instruction_state.rs`.
//!
//! Each [`Source`] has a key and reads one JSON value (or reports that it is
//! [`Observed::Unavailable`], a read that failed for now, or
//! [`Observed::Removed`], an observed absence). The first observation of a
//! session is its *baseline*: the values the system prompt is rendered from.
//! Later observations compare against the newest values and, when something
//! changed, record a delta whose rendered text the model sees as a
//! [`Role::System`](codesmith_ai::Role::System) message at that point. The
//! baseline, and with it the cached prompt prefix, stays the same until the
//! next *epoch*: a compaction moves the baseline to the newest values, and
//! an undo that removes an update (or the baseline) drops the state, so the
//! next observation starts a fresh baseline that supersedes every earlier
//! update.
//!
//! Where OpenCode keeps the state in a projected table (values by hash plus
//! a blob table), CodeSmith keeps the deltas in the session's record log
//! ([`Record::Instructions`]) with the values inline, and [`fold`]s the log
//! into the state, so export, import and forks carry it along.

use crate::store::{Entry, Record};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Values by source key.
pub type Values = BTreeMap<String, Value>;

/// What changed by source key: the new value, or `None` for a removal.
pub type Delta = BTreeMap<String, Option<Value>>;

/// What one read of a source found.
#[derive(Debug, Clone, PartialEq)]
pub enum Observed<A = Value> {
    Value(A),
    /// The read failed for now; the stored value stands.
    Unavailable,
    /// An observed absence: the source exists but its value is gone.
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The first observation found sources it could not read.
    Blocked(Vec<String>),
    DuplicateKey(String),
    /// A source rendered empty text the model would see.
    Empty {
        key: String,
        kind: &'static str,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Blocked(keys) => write!(
                f,
                "Instruction initialization blocked by unavailable sources: {}",
                keys.join(", ")
            ),
            Error::DuplicateKey(k) => write!(f, "Duplicate instruction key: {k}"),
            Error::Empty { key, kind } => {
                write!(f, "Instruction source {key} rendered an empty {kind}")
            }
        }
    }
}

impl std::error::Error for Error {}

type Read = Box<dyn Fn() -> Observed + Send + Sync>;
type Render1 = Box<dyn Fn(&Value) -> Result<Option<String>, Error> + Send + Sync>;
type Render2 = Box<dyn Fn(&Value, &Value) -> Result<Option<String>, Error> + Send + Sync>;

/// One instruction source over JSON, the form that is compared, stored and
/// replayed. [`make`] builds one from a typed [`Definition`]; a renderer
/// that returns `None` stays silent (a value it cannot decode).
pub struct Source {
    pub key: String,
    read: Read,
    initial: Render1,
    changed: Render2,
    removed: Render1,
}

/// Ordered sources; identical values render identical text.
pub type List = Vec<Source>;

type Initial<A> = Box<dyn Fn(&A) -> String + Send + Sync>;
type Changed<A> = Box<dyn Fn(&A, &A) -> Option<String> + Send + Sync>;

/// A typed source definition.
pub struct Definition<A> {
    pub key: String,
    pub read: Box<dyn Fn() -> Observed<A> + Send + Sync>,
    pub initial: Initial<A>,
    /// `None` is a silent update: the value is kept, nothing is said.
    pub changed: Changed<A>,
    pub removed: Option<Initial<A>>,
}

fn require(key: &str, kind: &'static str, text: String) -> Result<String, Error> {
    if text.is_empty() {
        return Err(Error::Empty {
            key: key.to_string(),
            kind,
        });
    }
    Ok(text)
}

/// Close a typed definition into one [`Source`], so sources of different
/// types compose into one [`List`].
pub fn make<A>(def: Definition<A>) -> List
where
    A: Serialize + DeserializeOwned + 'static,
{
    let Definition {
        key,
        read,
        initial,
        changed,
        removed,
    } = def;
    let initial = std::sync::Arc::new(initial);
    let decode = |v: &Value| serde_json::from_value::<A>(v.clone()).ok();
    let (k1, k2, k3) = (key.clone(), key.clone(), key.clone());
    let init = initial.clone();
    vec![Source {
        key,
        read: Box::new(move || match read() {
            Observed::Value(a) => {
                Observed::Value(serde_json::to_value(&a).expect("instruction value"))
            }
            Observed::Unavailable => Observed::Unavailable,
            Observed::Removed => Observed::Removed,
        }),
        initial: Box::new(move |v| match decode(v) {
            None => Ok(None),
            Some(a) => require(&k1, "initial", (init.as_ref())(&a)).map(Some),
        }),
        changed: Box::new(move |before, after| {
            let Some(after) = decode(after) else {
                return Ok(None);
            };
            let Some(before) = decode(before) else {
                return require(&k2, "initial", (initial.as_ref())(&after)).map(Some);
            };
            match changed(&before, &after) {
                None => Ok(None),
                Some(t) => require(&k2, "changed", t).map(Some),
            }
        }),
        removed: Box::new(move |v| match (decode(v), &removed) {
            (Some(a), Some(r)) => require(&k3, "removed", r(&a)).map(Some),
            _ => Ok(None),
        }),
    }]
}

/// Join lists, rejecting a key that appears twice.
pub fn combine(lists: Vec<List>) -> Result<List, Error> {
    let sources: List = lists.into_iter().flatten().collect();
    let mut keys = std::collections::BTreeSet::new();
    for s in &sources {
        if !keys.insert(s.key.clone()) {
            return Err(Error::DuplicateKey(s.key.clone()));
        }
    }
    Ok(sources)
}

/// Read every source once.
pub fn read(list: &[Source]) -> Vec<(String, Observed)> {
    list.iter().map(|s| (s.key.clone(), (s.read)())).collect()
}

/// The canonical JSON of a value: object keys sorted, no whitespace.
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical_json).collect::<Vec<_>>().join(",")
        ),
        Value::Object(o) => {
            let mut entries: Vec<_> = o.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap(),
                        canonical_json(v)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        v => serde_json::to_string(v).unwrap(),
    }
}

/// SHA-256 of the canonical JSON, in hex: equal for values that differ
/// only in key order.
pub fn hash(v: &Value) -> String {
    Sha256::digest(canonical_json(v).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What an observation admits: the changed values (`None`: removed).
/// Without `previous` (no state yet) it is the baseline, and an unavailable
/// source blocks it.
pub fn diff(observed: &[(String, Observed)], previous: Option<&Values>) -> Result<Delta, Error> {
    if previous.is_none() {
        let blocked: Vec<String> = observed
            .iter()
            .filter(|(_, o)| matches!(o, Observed::Unavailable))
            .map(|(k, _)| k.clone())
            .collect();
        if !blocked.is_empty() {
            return Err(Error::Blocked(blocked));
        }
    }
    let mut delta = Delta::new();
    for (key, o) in observed {
        match o {
            Observed::Unavailable => {}
            Observed::Removed => {
                if previous.is_some_and(|p| p.contains_key(key)) {
                    delta.insert(key.clone(), None);
                }
            }
            Observed::Value(v) => {
                if previous
                    .and_then(|p| p.get(key))
                    .is_some_and(|old| hash(old) == hash(v))
                {
                    continue;
                }
                delta.insert(key.clone(), Some(v.clone()));
            }
        }
    }
    Ok(delta)
}

/// `values` with `delta` applied.
pub fn apply(values: &Values, delta: &Delta) -> Values {
    let mut out = values.clone();
    for (k, v) in delta {
        match v {
            Some(v) => out.insert(k.clone(), v.clone()),
            None => out.remove(k),
        };
    }
    out
}

/// One source's rendered text, keyed so the sources stay apart.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub key: String,
    pub text: String,
}

/// The baseline text of each source that has a value, in list order.
pub fn render_initial(list: &[Source], values: &Values) -> Result<Vec<Rendered>, Error> {
    let mut out = Vec::new();
    for s in list {
        if let Some(v) = values.get(&s.key) {
            if let Some(text) = (s.initial)(v)? {
                out.push(Rendered {
                    key: s.key.clone(),
                    text,
                });
            }
        }
    }
    Ok(out)
}

/// The text models see for rendered sources.
pub fn join(rendered: &[Rendered]) -> String {
    rendered
        .iter()
        .map(|r| r.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The update text for `delta` against the `previous` values: a source new
/// to the session renders its baseline text, a changed one its change, a
/// removed one its removal. Empty when every update is silent.
pub fn render_update(list: &[Source], previous: &Values, delta: &Delta) -> Result<String, Error> {
    let mut parts = Vec::new();
    for s in list {
        let Some(current) = delta.get(&s.key) else {
            continue;
        };
        let text = match (current, previous.get(&s.key)) {
            (None, None) => None,
            (None, Some(before)) => (s.removed)(before)?,
            (Some(next), Some(before)) => (s.changed)(before, next)?,
            (Some(next), None) => (s.initial)(next)?,
        };
        parts.extend(text);
    }
    Ok(parts.join("\n\n"))
}

/// A keyed comparison of two lists.
#[derive(Debug, Clone, PartialEq)]
pub struct ByKey<A> {
    pub added: Vec<A>,
    pub removed: Vec<A>,
    /// (previous, current)
    pub changed: Vec<(A, A)>,
}

pub fn diff_by_key<A: Clone>(
    previous: &[A],
    current: &[A],
    key: impl Fn(&A) -> String,
    changed: impl Fn(&A, &A) -> bool,
) -> ByKey<A> {
    let current_keys: std::collections::BTreeSet<String> = current.iter().map(&key).collect();
    let by_key: BTreeMap<String, &A> = previous.iter().map(|v| (key(v), v)).collect();
    ByKey {
        added: current
            .iter()
            .filter(|v| !by_key.contains_key(&key(v)))
            .cloned()
            .collect(),
        removed: previous
            .iter()
            .filter(|v| !current_keys.contains(&key(v)))
            .cloned()
            .collect(),
        changed: current
            .iter()
            .filter_map(|v| {
                let before = by_key.get(&key(v))?;
                changed(before, v).then(|| ((*before).clone(), v.clone()))
            })
            .collect(),
    }
}

/// How a keyed list (skills, references, MCP servers) narrates an update:
/// added, changed and removed entries as deltas, or the whole list
/// restated when no entry differs.
pub struct ListUpdate<A> {
    pub key: fn(&A) -> String,
    pub changed: fn(&A, &A) -> bool,
    pub entries: fn(&[A]) -> Vec<String>,
    pub render: fn(&[A]) -> String,
    pub replaced: &'static str,
    pub added: &'static str,
    pub changed_text: &'static str,
    pub removed: fn(&str) -> String,
}

impl<A: Clone> ListUpdate<A> {
    pub fn render(&self, previous: &[A], current: &[A]) -> String {
        let d = diff_by_key(previous, current, self.key, self.changed);
        if d.added.is_empty() && d.changed.is_empty() && d.removed.is_empty() {
            return [self.replaced.to_string(), (self.render)(current)].join("\n");
        }
        let mut lines = Vec::new();
        if !d.added.is_empty() {
            lines.push(self.added.to_string());
            lines.extend((self.entries)(&d.added));
        }
        if !d.changed.is_empty() {
            lines.push(self.changed_text.to_string());
            let now: Vec<A> = d.changed.into_iter().map(|(_, c)| c).collect();
            lines.extend((self.entries)(&now));
        }
        if !d.removed.is_empty() {
            let keys: Vec<String> = d.removed.iter().map(self.key).collect();
            lines.push((self.removed)(&keys.join(", ")));
        }
        lines.join("\n")
    }
}

/// A session's instruction state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// Where the baseline starts: updates recorded before it are folded
    /// into it, and the model no longer sees them.
    pub epoch: i64,
    /// The newest record the state reflects.
    pub through: i64,
    /// The baseline values the system prompt is rendered from.
    pub initial: Values,
    /// The newest values.
    pub current: Values,
}

impl State {
    /// A new baseline at `seq`.
    pub fn baseline(seq: i64, values: Values) -> State {
        State {
            epoch: seq,
            through: seq,
            initial: values.clone(),
            current: values,
        }
    }

    /// Move the baseline to the newest values (a compaction at `seq`).
    pub fn advance(&mut self, seq: i64) {
        self.epoch = seq;
        self.through = seq;
        self.initial = self.current.clone();
    }
}

/// The record a delta is kept as.
pub fn record(delta: &Delta, text: Option<String>, baseline: bool) -> Record {
    let mut changed = BTreeMap::new();
    let mut removed = Vec::new();
    for (k, v) in delta {
        match v {
            Some(v) => {
                changed.insert(k.clone(), v.clone());
            }
            None => removed.push(k.clone()),
        }
    }
    Record::Instructions {
        changed,
        removed,
        text: text.filter(|t| !t.is_empty()),
        baseline,
        fork: false,
    }
}

fn delta_of(changed: &BTreeMap<String, Value>, removed: &[String]) -> Delta {
    let mut d: Delta = changed
        .iter()
        .map(|(k, v)| (k.clone(), Some(v.clone())))
        .collect();
    for k in removed {
        d.insert(k.clone(), None);
    }
    d
}

/// The instruction state a log leaves, as OpenCode's projector builds it:
/// every record applies in order, hidden or not, and an undo whose range
/// holds an instruction record (or the compaction the baseline came from)
/// drops the state. A redone undo counts as never made. `None`: no state,
/// so the next observation starts a baseline.
pub fn fold(entries: &[Entry]) -> Option<State> {
    let redone: Vec<i64> = entries
        .iter()
        .filter_map(|e| match e.record {
            Record::Redo { revert_seq } => Some(revert_seq),
            _ => None,
        })
        .collect();
    let mut state: Option<State> = None;
    for e in entries {
        match &e.record {
            Record::Instructions {
                changed,
                removed,
                baseline,
                fork,
                ..
            } => {
                let delta = delta_of(changed, removed);
                match (&mut state, baseline) {
                    (Some(s), false) => {
                        s.current = apply(&s.current, &delta);
                        s.through = e.seq;
                    }
                    _ => {
                        let mut s = State::baseline(e.seq, apply(&Values::new(), &delta));
                        // A fork's baseline keeps the updates it copied in view.
                        if *fork {
                            s.epoch = 0;
                        }
                        state = Some(s);
                    }
                }
            }
            Record::Checkpoint { .. } | Record::Native { .. } => {
                if let Some(s) = &mut state {
                    s.advance(e.seq);
                }
            }
            Record::Revert { from_seq, .. }
                if !redone.contains(&e.seq)
                    && state.as_ref().is_some_and(|s| s.through >= *from_seq) =>
            {
                state = None;
            }
            _ => {}
        }
    }
    state
}

/// One observation against the state.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// No state yet: this is the baseline.
    pub initial: bool,
    pub previous: Values,
    pub delta: Delta,
    pub current: Values,
}

impl Observation {
    /// Whether there is anything to record.
    pub fn changed(&self) -> bool {
        self.initial || !self.delta.is_empty()
    }
}

/// Read `list` and compare it with `state`.
pub fn observe(list: &[Source], state: Option<&State>) -> Result<Observation, Error> {
    let observed = read(list);
    let previous = state.map(|s| s.current.clone());
    let delta = diff(&observed, previous.as_ref())?;
    let previous = previous.unwrap_or_default();
    Ok(Observation {
        initial: state.is_none(),
        current: apply(&previous, &delta),
        previous,
        delta,
    })
}

/// The update text an observation records (empty for a baseline).
pub fn update_text(list: &[Source], o: &Observation) -> Result<String, Error> {
    if o.initial {
        return Ok(String::new());
    }
    render_update(list, &o.previous, &o.delta)
}

// --- API context entries (OpenCode's `InstructionEntry`) ---

/// The largest value, as JSON, a context entry may hold.
pub const MAX_ENTRY_BYTES: usize = 256 * 1024;

/// One context entry a host attached to a session.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextEntry {
    pub key: String,
    pub value: Value,
    /// Removed entries stay listed so their removal is announced.
    pub removed: bool,
}

fn render_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => serde_json::to_string_pretty(v).unwrap(),
    }
}

fn render_block(key: &str, v: &Value) -> String {
    format!("<context key=\"{key}\">\n{}\n</context>", render_value(v))
}

/// One source per context entry, keyed `api/<key>`. Rendering names the
/// context, not how it was attached; only updates and removals narrate.
pub fn context_sources(entries: &[ContextEntry]) -> List {
    entries
        .iter()
        .flat_map(|e| {
            let (k1, k2, k3) = (e.key.clone(), e.key.clone(), e.key.clone());
            let value = (!e.removed).then(|| e.value.clone());
            make::<Value>(Definition {
                key: format!("api/{}", e.key),
                read: Box::new(move || match &value {
                    Some(v) => Observed::Value(v.clone()),
                    None => Observed::Removed,
                }),
                initial: Box::new(move |v| render_block(&k1, v)),
                changed: Box::new(move |_, v| {
                    Some(format!(
                        "The context under \"{k2}\" changed and supersedes the previous value:\n{}",
                        render_block(&k2, v)
                    ))
                }),
                removed: Some(Box::new(move |_| {
                    format!("The context under \"{k3}\" no longer applies. Disregard it.")
                })),
            })
        })
        .collect()
}
