//! Model swaps from the model picker: the models a provider's server can
//! load (its `swap.list`, asked in the background when the picker opens)
//! show as picker items; choosing one runs `swap.use` with a progress view,
//! and the model is selected once the command succeeds. On failure the view
//! shows why and the current model stays. Only a person choosing an item
//! starts one; see [`codesmith_core::providers::swap`].

use super::app::PickItem;
use super::bridge::Ev;
use codesmith_core::config::Config;
use codesmith_core::providers::swap::{self, SwapModel};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

/// Picker ids of swap items: `swap:provider/model`.
pub const PREFIX: &str = "swap:";
/// Output lines a progress view keeps.
const KEEP: usize = 400;

/// What each provider's `list` said when last asked: its models, or why it failed.
static LISTED: Mutex<BTreeMap<String, Result<Vec<SwapModel>, String>>> =
    Mutex::new(BTreeMap::new());
static LISTING: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);

/// A swap's progress, for [`Ev::Swap`].
pub enum Progress {
    Line(String),
    Done {
        provider: String,
        result: Result<String, String>,
    },
}

/// The progress view.
#[derive(Debug, Clone, PartialEq)]
pub struct SwapView {
    pub provider: String,
    pub model: String,
    pub lines: Vec<String>,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Running,
    /// Selected this model.
    Done(String),
    Failed(String),
}

impl SwapView {
    pub fn new(provider: &str, model: &str) -> SwapView {
        SwapView {
            provider: provider.into(),
            model: model.into(),
            lines: vec![],
            status: Status::Running,
        }
    }

    pub fn push(&mut self, line: String) {
        self.lines.push(line);
        if self.lines.len() > KEEP {
            self.lines.remove(0);
        }
    }

    pub fn title(&self) -> String {
        match &self.status {
            Status::Running => format!(
                "Loading {} on {} · esc hides, it keeps going",
                self.model, self.provider
            ),
            Status::Done(m) => format!("Now using {m} · esc closes"),
            Status::Failed(_) => format!("{} was not loaded · esc closes", self.model),
        }
    }

    /// The text rows for `rows` rows: a note, the latest output, the result.
    pub fn rows(&self, rows: usize) -> Vec<String> {
        let mut head = vec![
            "Loading a model on the server takes several minutes; it serves nothing meanwhile."
                .to_string(),
        ];
        let tail = match &self.status {
            Status::Running => vec![],
            Status::Done(m) => vec![
                String::new(),
                format!("Done: the server now serves it, and {m} is selected."),
            ],
            Status::Failed(e) => vec![
                String::new(),
                format!("Failed: {e}"),
                "The current model is unchanged.".into(),
            ],
        };
        let room = rows.saturating_sub(head.len() + tail.len()).max(1);
        let skip = self.lines.len().saturating_sub(room);
        head.extend(self.lines.iter().skip(skip).cloned());
        head.extend(tail);
        head
    }
}

/// `swap:pid/model` as (pid, model).
pub fn parse_id(id: &str) -> Option<(String, String)> {
    let (p, m) = id.strip_prefix(PREFIX)?.split_once('/')?;
    (!p.is_empty()).then(|| (p.to_string(), m.to_string()))
}

/// Providers with a usable swap setting.
fn providers(cfg: &Config) -> Vec<String> {
    cfg.providers
        .iter()
        .filter(|(_, p)| p.swap.as_ref().is_some_and(|s| swap::validate(s).is_ok()))
        .map(|(id, _)| id.clone())
        .collect()
}

/// Ask every provider with a `swap.list` for its models, in the background;
/// the picker refreshes when they answer.
pub fn refresh(cfg: &Config, tx: UnboundedSender<Ev>) {
    let jobs: Vec<(String, swap::SwapConfig)> = providers(cfg)
        .into_iter()
        .filter_map(|id| Some((id.clone(), swap::for_provider(cfg, &id)?.clone())))
        .filter(|(_, s)| !s.list.is_empty())
        .collect();
    if jobs.is_empty() || LISTING.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        for (id, s) in jobs {
            let r = swap::list(&s).await.map_err(|e| format!("{e:#}"));
            LISTED.lock().unwrap().insert(id, r);
        }
        LISTING.store(false, Ordering::SeqCst);
        let _ = tx.send(Ev::LocalModels);
    });
}

/// The swap items for the picker, given the items it has already.
pub fn items(cfg: &Config, existing: &[PickItem]) -> Vec<PickItem> {
    let listed = LISTED.lock().unwrap();
    let mut out = Vec::new();
    for pid in providers(cfg) {
        let p = &cfg.providers[&pid];
        let pname = p.name.clone().unwrap_or_else(|| pid.clone());
        let s = p.swap.as_ref().unwrap();
        let models: Vec<SwapModel> = if s.list.is_empty() {
            p.models
                .keys()
                .filter(|m| swap::valid_name(m))
                .map(|name| SwapModel {
                    name: name.clone(),
                    ..SwapModel::default()
                })
                .collect()
        } else {
            match listed.get(&pid) {
                Some(Ok(m)) => m.clone(),
                Some(Err(e)) => {
                    out.push(PickItem {
                        id: format!("{PREFIX}{pid}/"),
                        label: format!("{pid}/…"),
                        detail: format!("{pname} · could not list the models it can load: {e}"),
                        current: false,
                    });
                    continue;
                }
                None => continue,
            }
        };
        for m in models {
            let spec = format!("{pid}/{}", m.name);
            if m.live {
                // Already on the server: an ordinary choice.
                if !existing.iter().chain(&out).any(|i| i.id == spec) {
                    out.push(PickItem {
                        id: spec.clone(),
                        label: spec,
                        detail: format!("{pname} · loaded on the server"),
                        current: false,
                    });
                }
                continue;
            }
            let size = m
                .size_bytes
                .map(|b| format!(" · {:.0} GB", b as f64 / 1e9))
                .unwrap_or_default();
            out.push(PickItem {
                id: format!("{PREFIX}{spec}"),
                label: spec,
                detail: format!(
                    "{pname} · loads it on the server, which takes several minutes{size}"
                ),
                current: false,
            });
        }
    }
    out
}

/// The names `use` may take for `pid`: what `list` said, else its configured models.
pub fn known(cfg: &Config, pid: &str) -> Vec<String> {
    let Some(p) = cfg.providers.get(pid) else {
        return vec![];
    };
    if p.swap.as_ref().is_some_and(|s| s.list.is_empty()) {
        return p.models.keys().cloned().collect();
    }
    match LISTED.lock().unwrap().get(pid) {
        Some(Ok(m)) => m.iter().map(|m| m.name.clone()).collect(),
        _ => vec![],
    }
}

/// Start loading `model` on `pid`'s server. Progress and the result arrive as [`Ev::Swap`].
pub fn start(cfg: &Config, pid: &str, model: &str, tx: UnboundedSender<Ev>) -> Result<(), String> {
    let s = swap::for_provider(cfg, pid)
        .cloned()
        .ok_or_else(|| format!("{pid} has no swap setting"))?;
    swap::validate(&s).map_err(|e| e.to_string())?;
    let known = known(cfg, pid);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("a model swap is already running".into());
    }
    let (pid, model) = (pid.to_string(), model.to_string());
    tokio::spawn(async move {
        let lines = tx.clone();
        let mut on_line = move |l: &str| {
            let _ = lines.send(Ev::Swap(Progress::Line(l.to_string())));
        };
        let result = swap::swap(&s, &model, &known, &mut on_line)
            .await
            .map_err(|e| format!("{e:#}"));
        RUNNING.store(false, Ordering::SeqCst);
        if result.is_ok() {
            // The server's models changed: ask again next time.
            LISTED.lock().unwrap().remove(&pid);
        }
        let _ = tx.send(Ev::Swap(Progress::Done {
            provider: pid,
            result,
        }));
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> String {
        format!(
            "{}/../codesmith-core/tests/fixtures/swap/fake-swap.sh",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    fn cfg(list: bool) -> Config {
        let f = fixture();
        serde_json::from_value(json!({"providers": {"stable": {
            "name": "Stable",
            "models": {"small": {}},
            "swap": {
                "list": if list { json!(["/bin/sh", f, "list"]) } else { json!([]) },
                "use": ["/bin/sh", f, "use", "{model}"],
                "timeout": 10
            }
        }}}))
        .unwrap()
    }

    #[test]
    fn ids() {
        assert_eq!(
            parse_id("swap:stable/big"),
            Some(("stable".into(), "big".into()))
        );
        assert_eq!(
            parse_id("swap:stable/org/m"),
            Some(("stable".into(), "org/m".into()))
        );
        assert_eq!(parse_id("stable/big"), None);
        assert_eq!(parse_id("swap:/big"), None);
    }

    #[test]
    fn view_rows_keep_the_latest_output_and_the_result() {
        let mut v = SwapView::new("stable", "big");
        for i in 0..50 {
            v.push(format!("line {i}"));
        }
        let rows = v.rows(10);
        assert_eq!(rows.len(), 10);
        assert_eq!(rows.last().unwrap(), "line 49");
        v.status = Status::Failed("big does not fit".into());
        let rows = v.rows(10);
        assert!(rows.iter().any(|r| r == "Failed: big does not fit"));
        assert_eq!(rows.last().unwrap(), "The current model is unchanged.");
        assert!(v.title().contains("was not loaded"));
        for i in 0..1000 {
            v.push(format!("{i}"));
        }
        assert_eq!(v.lines.len(), KEEP);
    }

    // One test drives the whole flow, as the cache is process-wide.
    #[tokio::test]
    async fn picker_items_and_a_swap_through_the_fake_server() {
        // Without `list`, the configured models are offered.
        let c = cfg(false);
        let first = items(&c, &[]);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, "swap:stable/small");
        assert!(first[0].detail.contains("takes several minutes"));
        assert_eq!(known(&c, "stable"), ["small"]);

        // With `list`: nothing until it answers, then its models.
        let c = cfg(true);
        LISTED.lock().unwrap().clear();
        assert!(items(&c, &[]).is_empty());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        refresh(&c, tx.clone());
        assert!(matches!(rx.recv().await, Some(Ev::LocalModels)));
        let got = items(&c, &[]);
        let ids: Vec<&str> = got.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            ["stable/small", "swap:stable/big", "swap:stable/huge"],
            "the live one is an ordinary choice"
        );
        assert!(got[1].detail.contains("Stable · loads it on the server"));

        // A swap that works: lines, then the model to select.
        start(&c, "stable", "big", tx.clone()).unwrap();
        assert!(
            start(&c, "stable", "big", tx.clone()).is_err(),
            "one at a time"
        );
        let mut lines = vec![];
        let result = loop {
            match rx.recv().await.unwrap() {
                Ev::Swap(Progress::Line(l)) => lines.push(l),
                Ev::Swap(Progress::Done { provider, result }) => {
                    assert_eq!(provider, "stable");
                    break result;
                }
                _ => {}
            }
        };
        assert_eq!(result, Ok("big".into()));
        assert!(lines.contains(&"loading big".to_string()), "{lines:?}");
        assert!(lines.iter().all(|l| !l.contains('\x1b')));

        // Not one the server listed: refused before anything runs.
        refresh(&c, tx.clone());
        assert!(matches!(rx.recv().await, Some(Ev::LocalModels)));
        start(&c, "stable", "made-up", tx.clone()).unwrap();
        let Some(Ev::Swap(Progress::Done { result, .. })) = rx.recv().await else {
            panic!()
        };
        assert!(result.unwrap_err().contains("not one of the models"));

        // One that does not fit: the reason comes back.
        start(&c, "stable", "huge", tx.clone()).unwrap();
        let result = loop {
            if let Ev::Swap(Progress::Done { result, .. }) = rx.recv().await.unwrap() {
                break result;
            }
        };
        let e = result.unwrap_err();
        assert!(
            e.contains("does not fit") && e.contains("kept its previous model"),
            "{e}"
        );
    }
}
