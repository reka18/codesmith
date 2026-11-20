//! The footer's status line at a given width, as OpenCode's
//! `footerStatuslinePolicy` (`mini/footer.width`): the required controls
//! (spinner, status, escape) always show, wrapping if they must; then
//! groups are admitted stage by stage while the line fits, stopping at the
//! first that does not (backfilling shorter, later facts would make the
//! line jump about as the terminal is resized). Admission order is not
//! placement order: the agent is admitted after the model but drawn
//! before it.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Id {
    Spinner,
    Status,
    Escape,
    Queued,
    Background,
    Subagents,
    Agent,
    Model,
    Context,
    Cost,
    Provider,
    Menu,
}

/// Placement order.
const ORDER: [Id; 12] = [
    Id::Spinner,
    Id::Status,
    Id::Escape,
    Id::Queued,
    Id::Background,
    Id::Subagents,
    Id::Agent,
    Id::Model,
    Id::Context,
    Id::Provider,
    Id::Cost,
    Id::Menu,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Text,
    Muted,
    Agent,
    Status,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub id: Id,
    pub parts: Vec<(String, Tone)>,
}

impl Group {
    fn text(&self) -> String {
        self.parts.iter().map(|(t, _)| t.as_str()).collect()
    }
}

/// A key and what it does: `ctrl+p menu`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Action {
    pub key: String,
    pub label: String,
    /// The longer label shown when there is room.
    pub expanded: Option<String>,
}

pub fn action(key: &str, label: &str) -> Action {
    Action {
        key: key.into(),
        label: label.into(),
        expanded: None,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Input {
    pub width: usize,
    pub mono: bool,
    /// The status and its longer form.
    pub status: Option<(String, Option<String>)>,
    pub escape: Option<Action>,
    /// `Queued` or `Subagents` work, in admission order.
    pub work: Vec<(Id, Action)>,
    /// The model's name and selected variant.
    pub model: Option<(String, Option<String>)>,
    pub agent: Option<String>,
    /// The context in use: compact (`1% ctx`) and full (`14.1K (1%)`).
    pub context: Option<(String, String)>,
    pub cost: Option<String>,
    pub provider: Option<String>,
    pub menu: Option<Action>,
    pub spinner: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub groups: Vec<Group>,
    pub text: String,
}

impl Layout {
    #[cfg(test)]
    pub fn ids(&self) -> Vec<Id> {
        self.groups.iter().map(|g| g.id).collect()
    }
}

/// The first `cells` columns of `s`, never splitting a character.
pub fn take_width(s: &str, cells: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > cells {
            return &s[..i];
        }
        used += w;
    }
    s
}

fn group(id: Id, text: &str, tone: Tone) -> Group {
    Group {
        id,
        parts: vec![(text.to_string(), tone)],
    }
}

fn act(id: Id, a: &Action, expanded: bool) -> Group {
    let label = if expanded {
        a.expanded.as_deref().unwrap_or(&a.label)
    } else {
        &a.label
    };
    let mut parts = Vec::new();
    if !a.key.is_empty() {
        parts.push((a.key.clone(), Tone::Text));
        parts.push((format!(" {label}"), Tone::Muted));
    } else {
        parts.push((label.to_string(), Tone::Muted));
    }
    Group { id, parts }
}

pub fn policy(input: &Input) -> Layout {
    let identity = |cells: usize| {
        let (name, variant) = input.model.as_ref().unwrap();
        let ellipsis = if input.mono { "..." } else { "\u{2026}" };
        let text = if name.width() > cells.saturating_add(ellipsis.width()) {
            format!("{}{ellipsis}", take_width(name, cells).trim_end())
        } else {
            name.clone()
        };
        let text = match variant {
            Some(v) => format!("{text} [{v}]"),
            None => text,
        };
        group(Id::Model, &text, Tone::Text)
    };
    let mut selected: Vec<Group> = Vec::new();
    let set = |selected: &mut Vec<Group>, g: Group| match selected.iter_mut().find(|x| x.id == g.id)
    {
        Some(x) => *x = g,
        None => selected.push(g),
    };
    if let Some(s) = &input.spinner {
        set(&mut selected, group(Id::Spinner, s, Tone::Text));
    }
    if let Some((s, _)) = input.status.as_ref().filter(|(s, _)| !s.is_empty()) {
        set(&mut selected, group(Id::Status, s, Tone::Status));
    }
    if let Some(e) = &input.escape {
        set(&mut selected, act(Id::Escape, e, false));
    }
    let separator = if input.mono { " - " } else { " \u{b7} " };
    let place = |selected: &[Group]| {
        let groups: Vec<Group> = ORDER
            .iter()
            .filter_map(|id| selected.iter().find(|g| g.id == *id).cloned())
            .collect();
        let mut text = String::new();
        for (i, g) in groups.iter().enumerate() {
            if i > 0 {
                text.push_str(if groups[i - 1].id == Id::Spinner {
                    " "
                } else {
                    separator
                });
            }
            text.push_str(&g.text());
        }
        Layout { groups, text }
    };
    let fits = |l: &Layout| l.text.width() <= input.width && !l.text.contains('\n');
    let mut layout = place(&selected);
    // Required controls may wrap; optional facts never crowd that fallback.
    if !fits(&layout) {
        return layout;
    }
    let mut stages: Vec<Group> = Vec::new();
    stages.extend(input.work.iter().map(|(id, a)| act(*id, a, false)));
    if input.model.is_some() {
        stages.push(identity(8));
    }
    if let Some(a) = &input.agent {
        stages.push(group(Id::Agent, a, Tone::Agent));
    }
    if let Some((compact, _)) = &input.context {
        stages.push(group(Id::Context, compact, Tone::Muted));
    }
    if input.model.is_some() {
        stages.push(identity(24));
    }
    if let Some((_, full)) = &input.context {
        stages.push(group(Id::Context, full, Tone::Muted));
    }
    if let Some(c) = &input.cost {
        stages.push(group(Id::Cost, c, Tone::Muted));
    }
    if let Some(p) = &input.provider {
        stages.push(group(Id::Provider, p, Tone::Muted));
    }
    if let Some(m) = &input.menu {
        stages.push(act(Id::Menu, m, false));
    }
    if input.model.is_some() {
        stages.push(identity(usize::MAX / 2));
    }
    stages.extend(
        input
            .work
            .iter()
            .filter(|(_, a)| a.expanded.is_some())
            .map(|(id, a)| act(*id, a, true)),
    );
    if let Some((_, Some(expanded))) = &input.status {
        stages.push(group(Id::Status, expanded, Tone::Status));
    }
    for stage in stages {
        set(&mut selected, stage);
        let next = place(&selected);
        if !fits(&next) {
            break;
        }
        layout = next;
    }
    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screenshot() -> Input {
        Input {
            model: Some(("GPT-5.6 Sol (50% Off)".into(), Some("max".into()))),
            agent: Some("Build".into()),
            context: Some(("1% ctx".into(), "14.1K (1%)".into())),
            cost: Some("$0.04".into()),
            provider: Some("Anomaly / OpenCode".into()),
            menu: Some(action("ctrl+p", "menu")),
            ..Default::default()
        }
    }

    fn at(input: &Input, width: usize) -> Layout {
        policy(&Input {
            width,
            ..input.clone()
        })
    }

    // mini/footer.width.test.ts: "progressive layouts fit and preserve
    // information at every width (mono=false|true)".
    #[test]
    fn oc_progressive_layouts_fit_and_preserve_information_at_every_width() {
        for mono in [false, true] {
            let s = screenshot();
            let spin = if mono { "*" } else { "\u{25aa}" };
            let fixtures = vec![
                Input::default(),
                s.clone(),
                Input {
                    model: Some(("GPT-5".into(), Some("xhigh".into()))),
                    ..s.clone()
                },
                Input {
                    model: Some((
                        "a model name with a very long and informative suffix".repeat(3),
                        None,
                    )),
                    ..s.clone()
                },
                Input {
                    model: Some((
                        "\u{6a21}\u{578b}\u{1f680}e\u{301}\u{5f00}\u{53d1} Model".repeat(3),
                        Some("\u{9ad8}\u{7ea7}".into()),
                    )),
                    ..s.clone()
                },
                Input {
                    model: Some((
                        "GPT-5".into(),
                        Some("an-unusually-long-but-complete-variant".into()),
                    )),
                    ..s.clone()
                },
                Input {
                    status: Some(("esc stop".into(), Some("esc interrupt".into()))),
                    spinner: Some(spin.into()),
                    ..s.clone()
                },
                Input {
                    status: Some((
                        "ctrl+shift+alt+i stop".into(),
                        Some("ctrl+shift+alt+i interrupt".into()),
                    )),
                    work: vec![
                        (Id::Queued, action("ctrl+x q", "12 queued")),
                        (
                            Id::Subagents,
                            Action {
                                key: if mono { "down" } else { "\u{2193}" }.into(),
                                label: "2 sub".into(),
                                expanded: Some("2 subagents".into()),
                            },
                        ),
                    ],
                    ..s.clone()
                },
                Input {
                    status: Some(("Shell".into(), None)),
                    escape: Some(action("esc", "normal")),
                    context: s.context.clone(),
                    ..Default::default()
                },
            ];
            for f in fixtures {
                let f = Input { mono, ..f };
                let mut previous: Vec<Id> = vec![];
                for width in 0..=280 {
                    let l = at(&f, width);
                    let ids = l.ids();
                    if let Some(sp) = &f.spinner {
                        assert_eq!(ids[0], Id::Spinner);
                        assert!(l.text.starts_with(&format!("{sp} ")));
                        let status = &f.status.as_ref().unwrap().0;
                        if width >= format!("{sp} {status}").width() {
                            assert!(l.text.width() <= width);
                        }
                    }
                    if l.text.width() > width {
                        assert!(ids
                            .iter()
                            .all(|i| matches!(i, Id::Spinner | Id::Status | Id::Escape)));
                    }
                    for id in &previous {
                        assert!(ids.contains(id), "{id:?} dropped at {width}");
                    }
                    let mut uniq = ids.clone();
                    uniq.dedup();
                    assert_eq!(uniq.len(), ids.len());
                    if ids.contains(&Id::Menu) {
                        assert_eq!(ids.last(), Some(&Id::Menu));
                    }
                    if let (Some(g), Some((_, Some(v)))) =
                        (l.groups.iter().find(|g| g.id == Id::Model), &f.model)
                    {
                        assert!(g.text().ends_with(&format!(" [{v}]")));
                    }
                    assert!(!l.text.contains('\u{fffd}'));
                    previous = ids;
                }
            }
        }
    }

    // "identity admission cannot separate the selected variant from its model".
    #[test]
    fn oc_identity_admission_keeps_the_variant_with_its_model() {
        let input = Input {
            model: Some(("GPT-5".into(), Some("max".into()))),
            ..Default::default()
        };
        assert!(at(&input, 10).groups.is_empty());
        assert_eq!(at(&input, 11).text, "GPT-5 [max]");
    }

    // "allocation priority is separate from placement".
    #[test]
    fn oc_allocation_priority_is_separate_from_placement() {
        let s = screenshot();
        assert_eq!(at(&s, 15).ids(), [Id::Model]);
        assert_eq!(
            at(&s, 60).ids(),
            [Id::Agent, Id::Model, Id::Context, Id::Cost]
        );
        assert_eq!(
            at(&s, 112).text,
            "Build \u{b7} GPT-5.6 Sol (50% Off) [max] \u{b7} 14.1K (1%) \u{b7} Anomaly / OpenCode \u{b7} $0.04 \u{b7} ctrl+p menu"
        );
    }

    // "a non-fitting high-priority stage does not backfill with shorter facts".
    #[test]
    fn oc_a_non_fitting_stage_does_not_backfill() {
        let input = Input {
            work: vec![(Id::Queued, action("ctrl+shift+alt+q", "12 queued"))],
            ..screenshot()
        };
        assert!(at(&input, 24).groups.is_empty());
        assert_eq!(at(&input, 26).ids(), [Id::Queued]);
    }

    // "context and cost are distinct additions".
    #[test]
    fn oc_context_and_cost_are_distinct_additions() {
        let s = screenshot();
        let input = Input {
            context: s.context,
            cost: s.cost,
            ..Default::default()
        };
        assert_eq!(at(&input, 6).text, "1% ctx");
        assert_eq!(at(&input, 10).text, "14.1K (1%)");
        assert_eq!(at(&input, 18).text, "14.1K (1%) \u{b7} $0.04");
    }

    // "stop-label enhancement cannot remove identity at the former
    // 56-column breakpoint".
    #[test]
    fn oc_the_stop_label_never_removes_the_identity() {
        let input = Input {
            status: Some(("esc stop".into(), Some("esc interrupt".into()))),
            ..screenshot()
        };
        for width in [55, 56, 57, 79, 80, 81] {
            let l = at(&input, width);
            assert!(l.text.contains("GPT-5.6 Sol (50% Off) [max]"), "{width}");
            assert!(l.text.starts_with("esc stop"));
        }
        assert!(at(&input, 160).text.starts_with("esc interrupt"));
    }

    // "required controls use an explicit wrapping fallback without
    // optional groups".
    #[test]
    fn oc_required_controls_wrap_without_optional_groups() {
        let s = screenshot();
        let input = Input {
            status: Some(("ctrl+shift+alt+x exit".into(), None)),
            ..s.clone()
        };
        assert_eq!(at(&input, 16).text, "ctrl+shift+alt+x exit");
        let input = Input {
            status: Some(("Please confirm\nExit now?".into(), None)),
            ..s
        };
        assert_eq!(at(&input, 80).text, "Please confirm\nExit now?");
    }
}
