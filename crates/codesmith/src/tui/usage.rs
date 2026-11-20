//! Turn usage footers (`tui.debug.turn_tokens`), as OpenCode's: after a
//! turn, a `Tokens` row sums up its steps' tokens (new, cached, total) and
//! flags likely cache busts, a step that read fewer cached tokens than the
//! one before it on the same model; the tool output key (ctrl+o) opens the
//! per-step table. The baseline carries across turns, and a compaction
//! resets it.

/// The model a cache read was on: provider, model and variant.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Model {
    pub provider: String,
    pub id: String,
    pub variant: Option<String>,
}

impl Model {
    /// From a `provider/model#variant` reference.
    pub fn parse(spec: &str) -> Model {
        let (base, variant) = match spec.split_once('#') {
            Some((b, v)) => (b, Some(v.to_string())),
            None => (spec, None),
        };
        let (provider, id) = base.split_once('/').unwrap_or(("", base));
        Model {
            provider: provider.into(),
            id: id.into(),
            variant,
        }
    }
}

/// A step's cache read and the model it was on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cache {
    pub read: u64,
    pub model: Model,
}

/// How many fewer cached tokens `current` read than `previous`, when that
/// is likely a cache bust: on the same model, and (for OpenAI, whose
/// reads move between one and two 1,024-token buckets) more than that.
pub fn cache_reuse_drop(previous: Option<&Cache>, current: &Cache) -> Option<u64> {
    let previous = previous?;
    if previous.model != current.model {
        return None;
    }
    let drop = previous.read.checked_sub(current.read)?;
    if current.model.provider == "openai" && (1_024..=2_048).contains(&drop) {
        return None;
    }
    (drop > 0).then_some(drop)
}

/// A message as the usage rows read it.
#[derive(Debug, Clone, PartialEq)]
pub enum Item<'a> {
    #[allow(dead_code)]
    User(&'a str),
    /// A step: its ID, whether it ended the turn (a finish other than
    /// tool calls), its tokens (zero when it reported none) and cache.
    Step {
        id: &'a str,
        terminal: bool,
        total: u64,
        cache: Cache,
    },
    Compaction,
    /// The turn ended (OpenCode's idle marker).
    Idle,
}

/// One turn's usage row: its steps, and the cache read before them.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnUsage<'a> {
    pub steps: Vec<&'a str>,
    pub previous: Option<Cache>,
}

/// The usage rows of `items`, as OpenCode's `reduceSessionRows` closes
/// them: at each idle marker, or, in history without markers, at each
/// turn's terminal step.
pub fn turn_usage<'a>(items: &[Item<'a>]) -> Vec<TurnUsage<'a>> {
    let legacy = !items.iter().any(|i| matches!(i, Item::Idle));
    let mut out = Vec::new();
    let mut steps: Vec<(&'a str, u64, &Cache)> = Vec::new();
    let mut previous: Option<Cache> = None;
    let mut flush = |steps: &mut Vec<(&'a str, u64, &Cache)>, previous: &mut Option<Cache>| {
        let used: Vec<_> = steps.drain(..).filter(|s| s.1 > 0).collect();
        let Some(last) = used.last() else {
            return;
        };
        out.push(TurnUsage {
            steps: used.iter().map(|s| s.0).collect(),
            previous: previous.clone(),
        });
        *previous = Some(last.2.clone());
    };
    for item in items {
        match item {
            Item::Idle => flush(&mut steps, &mut previous),
            Item::Compaction => previous = None,
            Item::User(_) => {}
            Item::Step {
                id,
                terminal,
                total,
                cache,
            } => {
                steps.push((id, *total, cache));
                if *terminal && legacy {
                    flush(&mut steps, &mut previous);
                }
            }
        }
    }
    out
}

/// One step of a turn's usage table.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// `tool-call` or `stop`.
    pub finish: &'static str,
    pub new: u64,
    pub cached: u64,
    pub total: u64,
    /// A likely cache bust: how many fewer tokens it read from the cache.
    pub drop: Option<u64>,
}

/// The session's steps, compactions and turn ends so far, which give a
/// turn's cache baseline.
#[derive(Debug, Clone, Default)]
pub struct Log {
    items: Vec<Logged>,
}

#[derive(Debug, Clone)]
enum Logged {
    Step {
        id: String,
        total: u64,
        cache: Cache,
    },
    Compaction,
    Idle,
}

impl Log {
    pub fn compacted(&mut self) {
        self.items.push(Logged::Compaction);
    }

    /// A turn's steps (their usage and model) ended: their table rows,
    /// each against the cache read before it (the previous turn's last
    /// step for the first).
    pub fn turn(&mut self, steps: &[(codesmith_ai::Usage, bool)], model: &Model) -> Vec<Step> {
        let first = self.items.len();
        for (i, (u, _)) in steps.iter().enumerate() {
            self.items.push(Logged::Step {
                id: format!("{first}-{i}"),
                total: u.input + u.output + u.cache_read + u.cache_write,
                cache: Cache {
                    read: u.cache_read,
                    model: model.clone(),
                },
            });
        }
        self.items.push(Logged::Idle);
        let items: Vec<Item> = self
            .items
            .iter()
            .map(|l| match l {
                Logged::Step { id, total, cache } => Item::Step {
                    id,
                    terminal: false,
                    total: *total,
                    cache: cache.clone(),
                },
                Logged::Compaction => Item::Compaction,
                Logged::Idle => Item::Idle,
            })
            .collect();
        let mut previous = match turn_usage(&items).pop() {
            Some(t)
                if t.steps
                    .first()
                    .is_some_and(|s| s.starts_with(&format!("{first}-"))) =>
            {
                t.previous
            }
            _ => return Vec::new(),
        };
        let mut out = Vec::new();
        for (u, tools) in steps {
            let cache = Cache {
                read: u.cache_read,
                model: model.clone(),
            };
            let finish = if *tools { "tool-call" } else { "stop" };
            if let Some(s) = step(u, finish, cache_reuse_drop(previous.as_ref(), &cache)) {
                out.push(s);
                previous = Some(cache);
            }
        }
        out
    }
}

/// A step's tokens, from its usage.
pub fn step(u: &codesmith_ai::Usage, finish: &'static str, drop: Option<u64>) -> Option<Step> {
    // Output includes reasoning.
    let total = u.input + u.output + u.cache_read + u.cache_write;
    (total > 0).then(|| Step {
        finish,
        new: total - u.cache_read,
        cached: u.cache_read,
        total,
        drop,
    })
}

/// `12,345`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The `Tokens` row: `+ Tokens: 2 steps · latest: 1,234 new · 5,000
/// cached · 6,234 total`, and the likely cache busts.
pub fn summary(steps: &[Step], open: bool) -> (String, Option<String>) {
    let latest = steps.last();
    let n = steps.len();
    let head = format!(
        "{} Tokens: {n} {} · latest: {} new · {} cached · {} total",
        if open { "-" } else { "+" },
        if n == 1 { "step" } else { "steps" },
        thousands(latest.map_or(0, |s| s.new)),
        thousands(latest.map_or(0, |s| s.cached)),
        thousands(latest.map_or(0, |s| s.total)),
    );
    let busts = steps.iter().filter(|s| s.drop.is_some()).count();
    let warn = (busts > 0).then(|| {
        format!(
            " · ! {busts} likely cache {}",
            if busts == 1 { "bust" } else { "busts" }
        )
    });
    (head, warn)
}

/// The per-step table's rows, with the bust warnings under their steps.
pub fn table(steps: &[Step]) -> Vec<(String, bool)> {
    let col = |f: &dyn Fn(&Step) -> usize, title: &str| {
        steps.iter().map(f).chain([title.len()]).max().unwrap_or(0)
    };
    let w_step = col(&|s| s.finish.len(), "Step");
    let w_new = col(&|s| thousands(s.new).len(), "New");
    let w_cached = col(&|s| thousands(s.cached).len(), "Cached");
    let w_total = col(&|s| thousands(s.total).len(), "Total");
    let mut out = vec![(
        format!(
            "{:<a$}{:>b$}  {:>c$}  {:>d$}",
            "Step",
            "New",
            "Cached",
            "Total",
            a = w_step + 2,
            b = w_new,
            c = w_cached,
            d = w_total
        ),
        false,
    )];
    for s in steps {
        out.push((
            format!(
                "{:<a$}{:>b$}  {:>c$}  {:>d$}",
                s.finish,
                thousands(s.new),
                thousands(s.cached),
                thousands(s.total),
                a = w_step + 2,
                b = w_new,
                c = w_cached,
                d = w_total
            ),
            false,
        ));
        if let Some(d) = s.drop {
            out.push((
                format!(
                    "! Likely cache bust: {} fewer cached tokens than the previous step",
                    thousands(d)
                ),
                true,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str, variant: Option<&str>) -> Model {
        Model {
            provider: provider.into(),
            id: id.into(),
            variant: variant.map(str::to_string),
        }
    }

    fn cache(read: u64, m: &Model) -> Cache {
        Cache {
            read,
            model: m.clone(),
        }
    }

    // session-rows.test.ts: "filters OpenAI cache quantization from cache reuse drops"
    #[test]
    fn oc_filters_openai_cache_quantization_from_cache_reuse_drops() {
        let openai = model("openai", "gpt", None);
        let c = |r| cache(r, &openai);
        assert_eq!(cache_reuse_drop(None, &c(10_000)), None);
        assert_eq!(cache_reuse_drop(Some(&c(10_000)), &c(11_000)), None);
        assert_eq!(cache_reuse_drop(Some(&c(10_000)), &c(8_977)), Some(1_023));
        assert_eq!(cache_reuse_drop(Some(&c(10_000)), &c(8_976)), None);
        assert_eq!(cache_reuse_drop(Some(&c(10_000)), &c(8_500)), None);
        assert_eq!(cache_reuse_drop(Some(&c(10_000)), &c(7_952)), None);
        assert_eq!(cache_reuse_drop(Some(&c(10_000)), &c(7_951)), Some(2_049));
    }

    // session-rows.test.ts: "compares cache reuse only for the same model"
    #[test]
    fn oc_compares_cache_reuse_only_for_the_same_model() {
        let claude = model("anthropic", "claude", None);
        let previous = cache(10_000, &claude);
        assert_eq!(
            cache_reuse_drop(
                Some(&previous),
                &cache(8_976, &model("openai", "gpt", None))
            ),
            None
        );
        assert_eq!(
            cache_reuse_drop(Some(&previous), &cache(8_976, &claude)),
            Some(1_024)
        );
        assert_eq!(
            cache_reuse_drop(
                Some(&cache(10_000, &model("openai", "gpt", Some("low")))),
                &cache(8_976, &model("openai", "gpt", Some("high")))
            ),
            None
        );
        assert_eq!(
            Model::parse("openai/gpt#high"),
            model("openai", "gpt", Some("high"))
        );
    }

    fn step<'a>(id: &'a str, read: u64, m: &Model) -> Item<'a> {
        Item::Step {
            id,
            terminal: true,
            total: read + 1,
            cache: cache(read, m),
        }
    }

    // session-rows.test.ts: "carries model identity with the cross-turn cache baseline"
    #[test]
    fn oc_carries_model_identity_with_the_cross_turn_cache_baseline() {
        let claude = model("anthropic", "claude", None);
        let gpt = model("openai", "gpt", None);
        let rows = turn_usage(&[
            Item::User("user-1"),
            step("assistant-1", 10_000, &claude),
            Item::User("user-2"),
            step("assistant-2", 8_976, &gpt),
        ]);
        assert_eq!(
            rows,
            vec![
                TurnUsage {
                    steps: vec!["assistant-1"],
                    previous: None
                },
                TurnUsage {
                    steps: vec!["assistant-2"],
                    previous: Some(cache(10_000, &claude))
                },
            ]
        );
    }

    // session-rows.test.ts: "resets the cross-turn cache baseline after compaction"
    #[test]
    fn oc_resets_the_cross_turn_cache_baseline_after_compaction() {
        let m = model("provider", "model", None);
        let rows = turn_usage(&[
            step("assistant-1", 370_176, &m),
            Item::Compaction,
            step("assistant-2", 13_824, &m),
        ]);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.previous.is_none()), "{rows:?}");
    }

    // session-rows.test.ts: "closes turn usage on the idle marker so steered
    // steps share one footer" (adapted: the usage rows; CodeSmith's
    // transcript rows are its own)
    #[test]
    fn oc_closes_turn_usage_on_the_idle_marker() {
        let m = model("provider", "model", None);
        let items = vec![
            Item::User("user-1"),
            step("assistant-1", 1_000, &m),
            Item::User("steer"),
            step("assistant-2", 2_000, &m),
            Item::Idle,
            Item::User("user-2"),
            step("assistant-3", 3_000, &m),
            Item::User("steer-2"),
            step("assistant-4", 4_000, &m),
        ];
        assert_eq!(
            turn_usage(&items),
            vec![TurnUsage {
                steps: vec!["assistant-1", "assistant-2"],
                previous: None
            }]
        );
        let mut closed = items.clone();
        closed.push(Item::Idle);
        assert_eq!(
            turn_usage(&closed),
            vec![
                TurnUsage {
                    steps: vec!["assistant-1", "assistant-2"],
                    previous: None
                },
                TurnUsage {
                    steps: vec!["assistant-3", "assistant-4"],
                    previous: Some(cache(2_000, &m))
                },
            ]
        );
    }

    #[test]
    fn the_tokens_row_sums_up_the_steps() {
        let steps = vec![
            Step {
                finish: "tool-call",
                new: 1_200,
                cached: 10_000,
                total: 11_200,
                drop: None,
            },
            Step {
                finish: "stop",
                new: 3_000,
                cached: 8_000,
                total: 11_000,
                drop: Some(2_000),
            },
        ];
        assert_eq!(
            summary(&steps, false),
            (
                "+ Tokens: 2 steps · latest: 3,000 new · 8,000 cached · 11,000 total".to_string(),
                Some(" · ! 1 likely cache bust".to_string())
            )
        );
        let t = table(&steps);
        assert_eq!(t[0].0, "Step         New  Cached   Total");
        assert_eq!(t[1].0, "tool-call  1,200  10,000  11,200");
        assert!(t[3].1 && t[3].0.contains("2,000 fewer"));
    }
}
