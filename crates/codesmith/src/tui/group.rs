//! The grouped transcript, as OpenCode groups it: adjacent thoughts form a
//! reasoning group, adjacent reads and searches an exploration group, and at
//! the `low` verbosity every run of tool calls and thoughts sits in one
//! activity group summed up on one row ("3 commands, 1 edit, 2 reads").
//! `session.verbosity` picks the level and `session.grouping` whether
//! exploration groups are drawn as one row. This is the grouping and the
//! summaries; `view` draws them.

/// How much tool activity the transcript shows (`session.verbosity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    /// Each run of tool calls and thoughts is one summary row.
    #[default]
    Low,
    /// Tools stand alone; reads and searches, and thoughts, are grouped.
    Medium,
    /// As medium, with exploration groups open.
    High,
}

impl Verbosity {
    pub const ALL: [Verbosity; 3] = [Verbosity::Low, Verbosity::Medium, Verbosity::High];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.key() == s)
    }

    pub fn key(self) -> &'static str {
        match self {
            Verbosity::Low => "low",
            Verbosity::Medium => "medium",
            Verbosity::High => "high",
        }
    }

    /// The next level, as `session.verbosity.cycle` steps: low, medium,
    /// high, low.
    pub fn next(self) -> Self {
        match self {
            Verbosity::Low => Verbosity::Medium,
            Verbosity::Medium => Verbosity::High,
            Verbosity::High => Verbosity::Low,
        }
    }
}

/// Whether adjacent reads and searches are drawn as one row
/// (`session.grouping`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    #[default]
    Auto,
    /// Each call on its own row.
    None,
}

impl Grouping {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Grouping::Auto),
            "none" => Some(Grouping::None),
            _ => None,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Grouping::Auto => Grouping::None,
            Grouping::None => Grouping::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    Activity,
    Reasoning,
    Exploration,
}

/// A grouped transcript: entries, and groups of them (which may nest).
#[derive(Debug, Clone, PartialEq)]
pub enum Node<E, K> {
    Entry(E),
    Group {
        kind: K,
        children: Vec<Node<E, K>>,
        /// Entries under it, however deep.
        size: usize,
    },
}

impl<E, K> Node<E, K> {
    pub fn size(&self) -> usize {
        match self {
            Node::Entry(_) => 1,
            Node::Group { size, .. } => *size,
        }
    }

    /// The entries under it, in order.
    pub fn leaves<'a>(&'a self, out: &mut Vec<&'a E>) {
        match self {
            Node::Entry(e) => out.push(e),
            Node::Group { children, .. } => children.iter().for_each(|c| c.leaves(out)),
        }
    }
}

/// Group adjacent entries by their nesting paths: a read whose path is
/// `[exploration]` joins the exploration group before it, one whose path is
/// `[activity, exploration]` the exploration group inside the activity group
/// before it. An empty path is an entry on its own.
pub fn group_entries<E, K, P>(
    entries: impl IntoIterator<Item = E>,
    mut path: impl FnMut(&E) -> P,
) -> Vec<Node<E, K>>
where
    K: Copy + PartialEq,
    P: AsRef<[K]>,
{
    let mut out = Vec::new();
    for e in entries {
        let p = path(&e);
        append(&mut out, e, p.as_ref());
    }
    out
}

fn append<E, K: Copy + PartialEq>(nodes: &mut Vec<Node<E, K>>, entry: E, path: &[K]) {
    let Some((&kind, rest)) = path.split_first() else {
        nodes.push(Node::Entry(entry));
        return;
    };
    if let Some(Node::Group {
        kind: k,
        children,
        size,
    }) = nodes.last_mut()
    {
        if *k == kind {
            *size += 1;
            append(children, entry, rest);
            return;
        }
    }
    let mut children = Vec::new();
    append(&mut children, entry, rest);
    nodes.push(Node::Group {
        kind,
        children,
        size: 1,
    });
}

/// What a transcript entry is, for grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part<'a> {
    Text,
    Reasoning,
    Tool(&'a str),
}

/// A tool's name as OpenCode shows it: `bash` is `shell`, `task`
/// `subagent`, `apply_patch` `patch`.
pub fn canonical(name: &str) -> &str {
    match name {
        "bash" => "shell",
        "task" => "subagent",
        "apply_patch" => "patch",
        n => n,
    }
}

fn exploring(name: &str) -> bool {
    matches!(name, "read" | "glob" | "grep" | "webfetch" | "websearch")
}

/// The grouping path of an assistant part: adjacent thoughts group, as do
/// reads, searches and web fetches; at `low`, every run of tools and
/// thoughts is wrapped in one activity group. Questions always stand alone.
pub fn part_path(part: Part, v: Verbosity) -> &'static [GroupKind] {
    use GroupKind::*;
    let low = v == Verbosity::Low;
    match part {
        Part::Text => &[],
        Part::Tool(n) if n.eq_ignore_ascii_case("question") => &[],
        Part::Reasoning if low => &[Activity, Reasoning],
        Part::Reasoning => &[Reasoning],
        Part::Tool(n) if exploring(&n.to_ascii_lowercase()) => {
            if low {
                &[Activity, Exploration]
            } else {
                &[Exploration]
            }
        }
        Part::Tool(_) if low => &[Activity],
        Part::Tool(_) => &[],
    }
}

/// How a call stands, for its group's summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The model is still writing its arguments.
    Streaming,
    Running,
    /// Finished, or failed.
    Done,
}

/// An activity group's thought or call, for its summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item<'a> {
    /// A thought: whether it is still streaming, and whether it has
    /// anything to show (a redacted one has not).
    Thought {
        streaming: bool,
        visible: bool,
    },
    Tool {
        name: &'a str,
        state: State,
    },
}

impl Item<'_> {
    /// Still going: a thought streaming in an open group, a call not done.
    pub fn active(&self, closed: bool) -> bool {
        match *self {
            Item::Thought { streaming, .. } => !closed && streaming,
            Item::Tool { state, .. } => state != State::Done,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub label: String,
    /// Something in the group is still going.
    pub active: bool,
}

/// Low verbosity's summary of an activity group, such as "3 commands, 1
/// edit, 2 thoughts, 4 reads". It counts only finished work (failed calls
/// included); running work makes it `active`. Once a later row closes the
/// group (`closed`), its thoughts count as finished.
pub fn activity_summary(items: &[Item], closed: bool) -> Summary {
    // command, edit, thought, read, tool
    let mut counts = [0usize; 5];
    for item in items {
        if item.active(closed) {
            continue;
        }
        let i = match *item {
            // A redacted thought shows nothing, so it is not a visible one.
            Item::Thought { visible, .. } if !visible => continue,
            Item::Thought { .. } => 2,
            Item::Tool { name, .. } => match canonical(name) {
                "shell" => 0,
                "read" => 3,
                "edit" | "write" | "patch" => 1,
                _ => 4,
            },
        };
        counts[i] += 1;
    }
    let names = ["command", "edit", "thought", "read", "tool"];
    let label = counts
        .iter()
        .zip(names)
        .filter(|(n, _)| **n > 0)
        .map(|(n, name)| format!("{n} {name}{}", if *n == 1 { "" } else { "s" }))
        .collect::<Vec<_>>()
        .join(", ");
    Summary {
        label,
        active: items.iter().any(|i| i.active(closed)),
    }
}

/// The summary row's text: the counts, or until something finished, the
/// status of the first item still going ("Running command…").
pub fn summarize(items: &[Item], closed: bool) -> Summary {
    let mut s = activity_summary(items, closed);
    if s.label.is_empty() {
        if let Some(i) = items.iter().find(|i| i.active(closed)) {
            s.label = busy_label(i);
        }
    }
    s
}

/// The status of an item still going, from its tool alone (so it does not
/// flicker as the details arrive).
pub fn busy_label(item: &Item) -> String {
    match *item {
        Item::Thought { .. } => "Thinking…".into(),
        Item::Tool { name, state } => {
            let noun = match canonical(name) {
                "shell" => "command",
                "execute" => "code",
                n => n,
            };
            let verb = if state == State::Streaming {
                "Preparing"
            } else {
                "Running"
            };
            format!("{verb} {noun}…")
        }
    }
}

/// An exploration group's row: "Explored: 2 reads, 1 search" (or
/// "Exploring" while it runs), counting its calls by kind in order of
/// first appearance.
pub fn exploration_label<'a>(names: impl IntoIterator<Item = &'a str>, done: bool) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for n in names {
        let n = match canonical(n) {
            "grep" | "glob" | "websearch" => "search",
            "webfetch" => "fetch",
            n => n,
        };
        match counts.iter_mut().find(|(k, _)| *k == n) {
            Some((_, c)) => *c += 1,
            None => counts.push((n, 1)),
        }
    }
    let names = counts
        .iter()
        .map(|(n, c)| match (*c, *n) {
            (1, n) => format!("1 {n}"),
            (c, n @ ("search" | "fetch")) => format!("{c} {n}es"),
            (c, n) => format!("{c} {n}s"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}: {names}", if done { "Explored" } else { "Exploring" })
}

/// A thought's text, without the placeholder of an encrypted one.
pub fn thought_text(t: &str) -> String {
    t.replace("[REDACTED]", "").trim().to_string()
}

/// A thought's title: the bold first line it starts with
/// (`**Planning the fix**`), as OpenCode's reasoning summary takes it.
pub fn thought_title(text: &str) -> Option<&str> {
    reasoning_summary(text).0
}

/// A reasoning summary split as OpenCode's `reasoningSummary` splits it:
/// a leading bold title block (`**Inspecting the workflow**` followed by a
/// blank line, or alone while its body still streams) and the Markdown
/// body after it, indentation kept. Bold text that is part of a sentence
/// (`**Important:** keep this`) is not a title.
pub fn reasoning_summary(text: &str) -> (Option<&str>, &str) {
    let content = text.trim();
    let whole = (None, content);
    let Some(rest) = content.strip_prefix("**") else {
        return whole;
    };
    let Some(end) = rest.find("**") else {
        return whole;
    };
    let title = &rest[..end];
    if title.is_empty() || title.contains(['*', '\n']) {
        return whole;
    }
    let after = &rest[end + 2..];
    let body = if after.is_empty() {
        after
    } else if let Some(b) = after
        .strip_prefix("\n\n")
        .or_else(|| after.strip_prefix("\r\n\r\n"))
        .or_else(|| after.strip_prefix("\n\r\n"))
        .or_else(|| after.strip_prefix("\r\n\n"))
    {
        b
    } else {
        return whole;
    };
    (Some(title.trim()), body.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use GroupKind::*;
    use Verbosity::*;

    #[derive(Debug, Clone, PartialEq)]
    struct E {
        id: &'static str,
        path: Vec<GroupKind>,
    }
    fn e(id: &'static str, path: &[GroupKind]) -> E {
        E {
            id,
            path: path.to_vec(),
        }
    }
    fn leaf(x: &E) -> Node<E, GroupKind> {
        Node::Entry(x.clone())
    }
    fn group(kind: GroupKind, children: Vec<Node<E, GroupKind>>) -> Node<E, GroupKind> {
        Node::Group {
            kind,
            size: children.iter().map(Node::size).sum(),
            children,
        }
    }

    #[test]
    fn oc_groups_adjacent_entries_by_path_and_counts_leaves_not_wrappers() {
        let read = e("read", &[Activity, Exploration]);
        let search = e("search", &[Activity, Exploration]);
        let thought = e("thought", &[Activity, Reasoning]);
        let text = e("text", &[]);
        let got = group_entries(
            [&read, &search, &thought, &text, &read].map(Clone::clone),
            |x: &E| x.path.clone(),
        );
        assert_eq!(
            got,
            vec![
                group(
                    Activity,
                    vec![
                        group(Exploration, vec![leaf(&read), leaf(&search)]),
                        group(Reasoning, vec![leaf(&thought)]),
                    ]
                ),
                leaf(&text),
                group(Activity, vec![group(Exploration, vec![leaf(&read)])]),
            ]
        );
        assert_eq!(got[0].size(), 3);
    }

    #[test]
    fn oc_a_direct_child_breaks_a_subgroup_without_ending_the_outer_group() {
        let read = e("read", &[Activity, Exploration]);
        let shell = e("shell", &[Activity]);
        let search = e("search", &[Activity, Exploration]);
        let got = group_entries([read.clone(), shell.clone(), search.clone()], |x: &E| {
            x.path.clone()
        });
        assert_eq!(
            got,
            vec![group(
                Activity,
                vec![
                    group(Exploration, vec![leaf(&read)]),
                    leaf(&shell),
                    group(Exploration, vec![leaf(&search)]),
                ]
            )]
        );
        assert_eq!(got[0].size(), 3);
        let empty: Vec<Node<E, GroupKind>> = group_entries(Vec::<E>::new(), |x: &E| x.path.clone());
        assert!(empty.is_empty());
    }

    #[test]
    fn oc_reasoning_and_exploration_group_at_every_level_other_tools_stand_alone() {
        for v in [Medium, High] {
            assert_eq!(part_path(Part::Reasoning, v), &[Reasoning]);
            assert_eq!(part_path(Part::Tool("read"), v), &[Exploration]);
        }
    }

    #[test]
    fn oc_medium_and_high_add_web_tools_to_exploration() {
        // Adapted: CodeSmith's transcript does not show instruction loads,
        // so their half is dropped.
        for v in [Medium, High] {
            assert_eq!(part_path(Part::Tool("webfetch"), v), &[Exploration]);
            assert_eq!(part_path(Part::Tool("websearch"), v), &[Exploration]);
            assert_eq!(part_path(Part::Tool("shell"), v), &[] as &[GroupKind]);
        }
    }

    #[test]
    fn oc_low_wraps_tools_and_thoughts_in_activity_text_breaks_it() {
        assert_eq!(part_path(Part::Reasoning, Low), &[Activity, Reasoning]);
        assert_eq!(part_path(Part::Tool("read"), Low), &[Activity, Exploration]);
        assert_eq!(part_path(Part::Tool("shell"), Low), &[Activity]);
        assert_eq!(part_path(Part::Text, Low), &[] as &[GroupKind]);
    }

    #[test]
    fn oc_questions_stand_alone_at_every_level() {
        for v in Verbosity::ALL {
            assert_eq!(part_path(Part::Tool("question"), v), &[] as &[GroupKind]);
        }
    }

    fn tool(name: &str, state: State) -> Item<'_> {
        Item::Tool { name, state }
    }
    const DONE_THOUGHT: Item<'static> = Item::Thought {
        streaming: false,
        visible: true,
    };

    #[test]
    fn oc_activity_summary_counts_finished_work_by_category() {
        // Adapted: no instruction loads to count.
        let items = [
            DONE_THOUGHT,
            tool("shell", State::Done),
            tool("bash", State::Done),
            tool("edit", State::Done),
            tool("apply_patch", State::Done),
            tool("read", State::Done),
            tool("grep", State::Done),
        ];
        assert_eq!(
            activity_summary(&items, false),
            Summary {
                label: "2 commands, 2 edits, 1 thought, 1 read, 1 tool".into(),
                active: false
            }
        );
    }

    #[test]
    fn oc_running_work_is_active_but_not_counted_and_failed_tools_are_counted() {
        let items = [
            Item::Thought {
                streaming: true,
                visible: true,
            },
            tool("read", State::Running),
            tool("read", State::Done),
            // Failed: done.
            tool("custom.check", State::Done),
        ];
        assert_eq!(
            activity_summary(&items, false),
            Summary {
                label: "1 read, 1 tool".into(),
                active: true
            }
        );
    }

    #[test]
    fn oc_activity_summary_skips_redacted_thoughts() {
        // Adapted: the instruction-file half is dropped.
        let redacted = thought_text("[REDACTED]");
        let items = [
            Item::Thought {
                streaming: false,
                visible: !redacted.is_empty(),
            },
            Item::Thought {
                streaming: false,
                visible: !thought_text("Plan").is_empty(),
            },
            tool("read", State::Done),
        ];
        assert_eq!(activity_summary(&items, true).label, "1 thought, 1 read");
    }

    #[test]
    fn oc_a_thought_still_streaming_counts_as_finished_once_a_later_row_closes_its_group() {
        let items = [
            Item::Thought {
                streaming: true,
                visible: true,
            },
            tool("read", State::Done),
        ];
        assert_eq!(
            activity_summary(&items, false),
            Summary {
                label: "1 read".into(),
                active: true
            }
        );
        assert_eq!(
            activity_summary(&items, true),
            Summary {
                label: "1 thought, 1 read".into(),
                active: false
            }
        );
    }

    #[test]
    fn oc_until_something_finishes_the_label_is_the_first_running_items_status() {
        let items = [tool("edit", State::Running), tool("shell", State::Running)];
        assert_eq!(
            summarize(&items, false),
            Summary {
                label: "Running edit…".into(),
                active: true
            }
        );
    }

    #[test]
    fn oc_busy_labels_depend_only_on_the_tool_and_whether_it_is_still_being_prepared() {
        assert_eq!(
            busy_label(&tool("bash", State::Streaming)),
            "Preparing command…"
        );
        assert_eq!(
            busy_label(&tool("shell", State::Running)),
            "Running command…"
        );
        assert_eq!(busy_label(&tool("read", State::Running)), "Running read…");
        assert_eq!(
            busy_label(&tool("execute", State::Running)),
            "Running code…"
        );
        assert_eq!(
            busy_label(&Item::Thought {
                streaming: true,
                visible: false
            }),
            "Thinking…"
        );
    }

    #[test]
    fn oc_a_finished_execute_with_no_nested_calls_counts_as_one_tool() {
        assert_eq!(
            activity_summary(&[tool("execute", State::Done)], false).label,
            "1 tool"
        );
    }

    #[test]
    fn exploration_rows_count_reads_searches_and_fetches() {
        assert_eq!(
            exploration_label(["read", "grep", "read", "glob", "webfetch"], true),
            "Explored: 2 reads, 2 searches, 1 fetch"
        );
        assert_eq!(exploration_label(["read"], false), "Exploring: 1 read");
    }

    #[test]
    fn thought_titles_are_a_leading_bold_line() {
        assert_eq!(
            thought_title("**Reset title**\n\nReset thought body"),
            Some("Reset title")
        );
        assert_eq!(thought_title("**Only a title**"), Some("Only a title"));
        assert_eq!(thought_title("**Bold** inline"), None);
        assert_eq!(thought_title("plain"), None);
    }

    #[test]
    fn verbosity_cycles_low_medium_high() {
        assert_eq!(Low.next(), Medium);
        assert_eq!(Medium.next(), High);
        assert_eq!(High.next(), Low);
        assert_eq!(Verbosity::parse("medium"), Some(Medium));
        assert_eq!(Verbosity::parse("loud"), None);
        assert_eq!(Verbosity::default(), Low);
    }
}
