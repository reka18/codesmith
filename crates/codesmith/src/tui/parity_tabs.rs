//! OpenCode's session tab tests, ported: `tui/test/context/session-tabs-model`,
//! the parts of `context/session-tabs` and `component/session-tabs-status`
//! CodeSmith has, and the tab settings of `config-v2`.

use super::app::App;
use super::tabs::{
    self, close, cycle, layout, move_tab, number_label, open, overflow_width, record_closed,
    reopen, Attention, Closed, History, Indicators, Item, Mode, Orientation, Scope, Status, Strip,
    Tab, Unread,
};
use super::{settings, view};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::json;
use std::path::Path;

fn ids(tabs: &[Tab]) -> Vec<&str> {
    tabs.iter().map(|t| t.session.as_str()).collect()
}

fn named(ids: &[&str]) -> Vec<Tab> {
    ids.iter().map(|i| Tab::new(i)).collect()
}

fn history(of: &[&str]) -> History {
    let mut h = History::default();
    for s in of {
        h.record(s);
    }
    h
}

fn active(tabs: &[Tab], id: &str) -> Option<usize> {
    tabs.iter().position(|t| t.session == id)
}

// ----- tui/test/context/session-tabs-model ------------------------------------

#[test]
fn oc_appends_the_branch_to_the_project_detail() {
    use tabs::detail;
    assert_eq!(
        detail("opencode", Some("feature/sidebar"), Some("main"), true),
        "opencode ⎇ feature/sidebar"
    );
    assert_eq!(
        detail("opencode", Some("feature/sidebar"), None, true),
        "opencode ⎇ feature/sidebar"
    );
    assert_eq!(
        detail("opencode", Some("feature/sidebar"), Some("main"), false),
        "opencode"
    );
    assert_eq!(
        detail("opencode", Some("main"), Some("main"), true),
        "opencode"
    );
    assert_eq!(detail("opencode", None, Some("main"), true), "opencode");
}

#[test]
fn oc_labels_tabs_by_ordinal() {
    let labels: Vec<String> = (0..12).map(number_label).collect();
    assert_eq!(
        labels,
        ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12"]
    );
}

#[test]
fn oc_moves_a_tab_to_a_clamped_index_and_returns_the_same_tabs_for_no_ops() {
    let tabs = named(&["a", "b", "c"]);
    let mut t = tabs.clone();
    assert!(move_tab(&mut t, "a", 2));
    assert_eq!(ids(&t), ["b", "c", "a"]);
    let mut t = tabs.clone();
    assert!(move_tab(&mut t, "c", -5));
    assert_eq!(ids(&t), ["c", "a", "b"]);
    let mut t = tabs.clone();
    assert!(move_tab(&mut t, "b", 99));
    assert_eq!(ids(&t), ["a", "c", "b"]);
    let mut t = tabs.clone();
    assert!(!move_tab(&mut t, "b", 1));
    assert!(!move_tab(&mut t, "missing", 0));
    assert_eq!(t, tabs);
}

#[test]
fn oc_overflow_markers_reserve_room_for_a_gap_beside_their_digits() {
    assert_eq!(overflow_width(5), 3);
    assert_eq!(overflow_width(12), 4);
}

#[test]
fn oc_opens_each_session_once_and_refreshes_its_title() {
    let mut tabs = vec![Tab::titled("a", "Old")];
    assert!(open(&mut tabs, Tab::titled("a", "New")));
    assert_eq!(tabs, [Tab::titled("a", "New")]);
    let mut more = tabs.clone();
    assert!(open(&mut more, Tab::new("b")));
    assert_eq!(more, [Tab::titled("a", "New"), Tab::new("b")]);
    assert!(!open(&mut tabs, Tab::titled("a", "New")));
    assert!(
        !open(&mut tabs, Tab::new("a")),
        "no title keeps the old one"
    );
    assert_eq!(tabs, [Tab::titled("a", "New")]);
}

#[test]
fn oc_selects_the_right_tab_then_the_left_tab_after_closing() {
    let mut t = named(&["a", "b", "c"]);
    assert_eq!(close(&mut t, "b", None), (true, Some("c".into())));
    assert_eq!(ids(&t), ["a", "c"]);
    let mut t = named(&["a", "b"]);
    assert_eq!(close(&mut t, "b", None).1.as_deref(), Some("a"));
    let mut t = named(&["a"]);
    assert_eq!(close(&mut t, "a", None).1, None);
}

#[test]
fn oc_closing_an_unknown_session_returns_the_same_tabs_reference() {
    let mut t = named(&["a", "b"]);
    assert!(!close(&mut t, "missing", None).0);
    assert_eq!(t, named(&["a", "b"]));
}

#[test]
fn oc_closing_an_already_removed_tab_picks_the_neighbour_at_its_last_index() {
    let mut t = named(&["a", "c"]);
    assert_eq!(close(&mut t, "b", Some(1)), (false, Some("c".into())));
    assert_eq!(t, named(&["a", "c"]));
    assert_eq!(close(&mut t, "d", Some(2)).1.as_deref(), Some("c"));
    assert_eq!(close(&mut t, "b", None).1, None);
    assert_eq!(close(&mut vec![], "b", Some(0)).1, None);
}

#[test]
fn oc_cycles_through_a_filtered_tab_set_in_either_direction() {
    let t = named(&["a", "c", "e"]);
    let all = |_: &Tab| true;
    assert_eq!(cycle(&t, Some("c"), 1, all).unwrap().session, "e");
    assert_eq!(cycle(&t, Some("c"), -1, all).unwrap().session, "a");
    assert_eq!(cycle(&t, Some("e"), 1, all).unwrap().session, "a");
    assert_eq!(cycle(&t, Some("b"), 1, all).unwrap().session, "a");
}

#[test]
fn oc_cycles_to_the_nearest_matching_tab_from_an_unmatched_active_tab() {
    let t = named(&["a", "b", "c", "d", "e"]);
    let unread = |t: &Tab| t.session == "a" || t.session == "d";
    assert_eq!(cycle(&t, Some("c"), 1, unread).unwrap().session, "d");
    assert_eq!(cycle(&t, Some("c"), -1, unread).unwrap().session, "a");
    assert_eq!(cycle(&t, Some("e"), 1, unread).unwrap().session, "a");
    assert_eq!(cycle(&t, Some("a"), -1, unread).unwrap().session, "d");
}

#[test]
fn oc_moves_backward_and_forward_through_selection_history() {
    let t = named(&["a", "b", "c", "d"]);
    let mut h = history(&["a", "b", "c", "d"]);
    let back_to_c = h.step(&t, Some("d"), -1);
    let back_to_b = h.step(&t, Some("c"), -1);
    let forward_to_c = h.step(&t, Some("b"), 1);
    let forward_to_d = h.step(&t, Some("c"), 1);
    assert_eq!(
        [back_to_c, back_to_b, forward_to_c, forward_to_d].map(|s| s.unwrap()),
        ["c", "b", "c", "d"]
    );
}

#[test]
fn oc_truncates_forward_history_after_a_new_selection() {
    let t = named(&["a", "b", "c", "d"]);
    let mut h = history(&["a", "b", "c"]);
    h.step(&t, Some("c"), -1);
    h.record("d");
    assert_eq!(
        h,
        History {
            entries: vec!["a".into(), "b".into(), "d".into()],
            index: Some(2)
        }
    );
    assert_eq!(h.step(&t, Some("d"), 1), None);
}

#[test]
fn oc_skips_closed_tabs_and_duplicate_entries_for_the_active_tab() {
    let t = named(&["a", "b"]);
    let mut h = history(&["a", "b", "c", "b"]);
    let before = h.clone();
    assert!(!h.record("b"));
    assert_eq!(h, before);
    assert_eq!(h.step(&t, Some("b"), -1).as_deref(), Some("a"));
}

#[test]
fn oc_drops_the_oldest_history_entries_beyond_the_limit() {
    let mut h = History::default();
    for i in 0..150 {
        h.record(&format!("session-{i}"));
    }
    assert_eq!(h.entries.len(), 100);
    assert_eq!(h.entries[0], "session-50");
    assert_eq!(h.entries[h.index.unwrap()], "session-149");
}

#[test]
fn oc_returns_to_the_latest_history_entry_when_no_tab_is_active() {
    let t = named(&["a", "b"]);
    let mut h = history(&["a", "b"]);
    assert_eq!(h.step(&t, None, -1).as_deref(), Some("b"));
}

#[test]
fn oc_returns_to_the_previous_selected_open_tab_after_closing_the_active_tab() {
    let mut t = named(&["a", "b", "c"]);
    let mut h = history(&["a", "c"]);
    close(&mut t, "b", None);
    h.record("b");
    assert_eq!(h.step(&t, Some("b"), -1).as_deref(), Some("c"));
}

#[test]
fn oc_reopens_the_most_recently_closed_tab_at_its_original_position() {
    let mut stack = vec![];
    record_closed(&mut stack, Tab::titled("b", "Middle"), 1);
    let (tabs, id) = reopen(&mut stack, &named(&["a", "c"]), None).unwrap();
    assert_eq!(id, "b");
    assert_eq!(
        tabs,
        [Tab::new("a"), Tab::titled("b", "Middle"), Tab::new("c")]
    );
    assert!(stack.is_empty());
    let mut empty = vec![];
    assert_eq!(reopen(&mut empty, &named(&["a", "b", "c"]), None), None);
    assert!(empty.is_empty());
}

#[test]
fn oc_reopens_a_selected_closed_tab_without_consuming_newer_entries() {
    let mut stack = vec![];
    record_closed(&mut stack, Tab::new("b"), 1);
    record_closed(&mut stack, Tab::new("c"), 2);
    let full = stack.clone();
    let mut s = full.clone();
    let (tabs, id) = reopen(&mut s, &named(&["a"]), Some("b")).unwrap();
    assert_eq!(id, "b");
    assert_eq!(ids(&tabs), ["a", "b"]);
    let c = Closed {
        tab: Tab::new("c"),
        index: 2,
    };
    assert_eq!(s, std::slice::from_ref(&c));
    let mut s = full.clone();
    assert_eq!(reopen(&mut s, &[], Some("missing")), None);
    assert_eq!(s, full);
    let mut s = full.clone();
    assert_eq!(reopen(&mut s, &named(&["b"]), Some("b")), None);
    assert_eq!(s, [c]);
}

#[test]
fn oc_skips_and_consumes_closed_entries_that_are_already_open() {
    let mut stack = vec![
        Closed {
            tab: Tab::new("a"),
            index: 0,
        },
        Closed {
            tab: Tab::new("b"),
            index: 1,
        },
    ];
    let (tabs, id) = reopen(&mut stack, &named(&["b"]), None).unwrap();
    assert_eq!(id, "a");
    assert_eq!(ids(&tabs), ["a", "b"]);
    assert!(stack.is_empty());
}

#[test]
fn oc_clamps_restored_positions_and_keeps_one_entry_per_session() {
    let mut twice = vec![];
    record_closed(&mut twice, Tab::new("a"), 5);
    record_closed(&mut twice, Tab::new("a"), 2);
    assert_eq!(
        twice,
        [Closed {
            tab: Tab::new("a"),
            index: 2
        }]
    );
    let mut s = twice.clone();
    let (tabs, _) = reopen(&mut s, &named(&["b"]), None).unwrap();
    assert_eq!(ids(&tabs), ["b", "a"]);
    let mut overflow = twice;
    for i in 0..27 {
        record_closed(&mut overflow, Tab::new(&i.to_string()), i);
    }
    assert_eq!(overflow.len(), 25);
    assert_eq!(overflow.last().unwrap().tab.session, "26");
    assert_eq!(overflow[0].tab.session, "2");
}

#[test]
fn oc_expands_the_active_tab_and_keeps_inactive_widths_equal() {
    let t = named(&["a", "b", "c", "d", "e", "f", "g"]);
    let l = layout(t.len(), active(&t, "d"), 76, 0);
    assert_eq!((l.before, l.after, l.start, l.total), (0, 0, 0, 76));
    assert_eq!(l.widths, [9, 9, 9, 22, 9, 9, 9]);
    assert_eq!(l.widths.iter().sum::<usize>(), 76);
}

#[test]
fn oc_reserves_an_active_tab_slot_for_the_new_session_page() {
    let t = named(&["a", "b", "c", "d", "new"]);
    let l = layout(t.len(), active(&t, "new"), 54, 0);
    assert_eq!((l.start, l.count), (0, 5));
    assert_eq!(l.widths, [8, 8, 8, 8, 22]);
    assert_eq!(l.widths.iter().sum::<usize>(), l.total);
}

#[test]
fn oc_keeps_the_visible_tab_window_stable_on_the_new_session_page() {
    let t: Vec<Tab> = (0..10).map(|i| Tab::new(&i.to_string())).collect();
    let selected = layout(t.len(), active(&t, "7"), 70, 0);
    let home = layout(t.len(), None, 70, selected.start);
    assert!(selected.start > 0);
    assert_eq!(home.start, selected.start);
}

#[test]
fn oc_only_swaps_old_and_new_active_width_inside_a_sticky_window() {
    let t = named(&["a", "b", "c", "d", "e", "f", "g"]);
    let before = layout(t.len(), active(&t, "c"), 76, 0);
    let after = layout(t.len(), active(&t, "d"), 76, before.start);
    assert_eq!(before.start, after.start);
    assert_eq!(before.widths, [9, 9, 22, 9, 9, 9, 9]);
    assert_eq!(after.widths, [9, 9, 9, 22, 9, 9, 9]);
}

#[test]
fn oc_shares_roomy_width_equally_without_changing_widths_on_selection() {
    let t = named(&["a", "b", "c"]);
    let before = layout(t.len(), active(&t, "a"), 100, 0);
    let after = layout(t.len(), active(&t, "c"), 100, before.start);
    assert_eq!(before.widths, [32, 32, 32]);
    assert_eq!(after.widths, before.widths);
    assert_eq!(before.total, 96);
}

#[test]
fn oc_caps_a_single_tab_instead_of_stretching_it_across_the_terminal() {
    let l = layout(1, Some(0), 100, 0);
    assert_eq!(l.widths, [32]);
    assert_eq!(l.total, 32);
}

#[test]
fn oc_fills_roomy_space_equally_below_the_maximum_width() {
    let t = named(&["a", "b", "c", "d"]);
    assert_eq!(
        layout(t.len(), active(&t, "b"), 100, 0).widths,
        [25, 25, 25, 25]
    );
}

#[test]
fn oc_expands_only_the_active_tab_under_compact_pressure() {
    let t = named(&["a", "b", "c", "d", "e"]);
    let before = layout(t.len(), active(&t, "c"), 100, 0);
    let after = layout(t.len(), active(&t, "d"), 100, before.start);
    assert_eq!(before.widths, [19, 19, 24, 19, 19]);
    assert_eq!(after.widths, [19, 19, 19, 24, 19]);
}

#[test]
fn oc_moves_the_window_only_after_selection_crosses_its_edge() {
    let t: Vec<Tab> = (0..10).map(|i| Tab::new(&i.to_string())).collect();
    let initial = layout(t.len(), active(&t, "3"), 70, 0);
    let inside = layout(t.len(), active(&t, "4"), 70, initial.start);
    let crossed = layout(t.len(), active(&t, "7"), 70, inside.start);
    assert_eq!(initial.start, 0);
    assert_eq!(inside.start, 0);
    assert!(crossed.start > 0);
    assert!((crossed.start..crossed.start + crossed.count).contains(&7));
    assert_eq!(crossed.widths.iter().sum::<usize>(), crossed.total);
}

// ----- tui/test/config-v2 ---------------------------------------------------

fn tabs_of(v: serde_json::Value, env: &[(&str, &str)]) -> (tabs::Settings, Vec<String>) {
    tabs::load(&v, |k| {
        env.iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string())
    })
}

#[test]
fn oc_validates_the_session_tabs_setting() {
    let (s, e) = tabs_of(
        json!({"tabs": {"mode": "on", "layout": "vertical", "indicators": "numbers"}}),
        &[],
    );
    assert!(e.is_empty(), "{e:?}");
    assert_eq!(
        (s.mode, s.layout, s.indicators),
        (Mode::On, Orientation::Vertical, Indicators::Numbers)
    );
    let (_, e) = tabs_of(json!({"tabs": {"indicators": "unknown"}}), &[]);
    assert_eq!(
        e,
        ["tui.tabs.indicators: must be \"status\" or \"numbers\""]
    );
    let (_, e) = tabs_of(json!({"tabs": {"layout": true}}), &[]);
    assert_eq!(
        e,
        ["tui.tabs.layout: must be \"horizontal\" or \"vertical\""]
    );
    let (_, e) = tabs_of(json!({"tabs": {"mode": true}}), &[]);
    assert_eq!(e, ["tui.tabs.mode: must be \"auto\", \"on\" or \"off\""]);
    let (s, e) = tabs_of(json!({"tabs": {"enabled": false}}), &[]);
    assert!(e.is_empty());
    assert!(!s.enabled);
    // Read with the rest of `tui`.
    let (s, e) = settings::load(&json!({"tabs": {"scope": "global"}}));
    assert!(e.is_empty(), "{e:?}");
    assert_eq!(s.tabs.scope, Scope::Global);
}

#[test]
fn oc_resolves_automatic_tabs_from_the_terminal_environment() {
    let herdr = [("HERDR_ENV", "1")];
    assert!(tabs_of(json!({}), &[]).0.enabled);
    assert!(!tabs_of(json!({}), &herdr).0.enabled);
    assert!(tabs_of(json!({"tabs": {"mode": "on"}}), &herdr).0.enabled);
    assert!(!tabs_of(json!({"tabs": {"mode": "off"}}), &[]).0.enabled);
    let (s, _) = tabs_of(json!({"tabs": {"enabled": false}}), &[]);
    assert_eq!((s.mode, s.enabled), (Mode::Off, false));
    let (s, _) = tabs_of(json!({"tabs": {"mode": "on", "enabled": false}}), &[]);
    assert_eq!((s.mode, s.enabled), (Mode::On, true));
}

// (adapted) CodeSmith has no settings dialog: the settings are read from
// `tui.tabs` with OpenCode's values and defaults.
#[test]
fn oc_shows_resolved_tab_defaults_in_settings() {
    let (s, _) = settings::load(&serde_json::Value::Null);
    assert_eq!(s.tabs.mode, Mode::Auto);
    assert_eq!(s.tabs.scope, Scope::Cwd);
    assert_eq!(s.tabs.layout, Orientation::Horizontal);
    assert_eq!(s.tabs.indicators, Indicators::Status);
    for (v, want) in [("off", Mode::Off), ("on", Mode::On), ("auto", Mode::Auto)] {
        assert_eq!(tabs_of(json!({"tabs": {"mode": v}}), &[]).0.mode, want);
    }
    for (v, want) in [
        ("status", Indicators::Status),
        ("numbers", Indicators::Numbers),
    ] {
        assert_eq!(
            tabs_of(json!({"tabs": {"indicators": v}}), &[])
                .0
                .indicators,
            want
        );
    }
}

// ----- tui/test/context/session-tabs: the kept tab list ----------------------

// (adapted) The tab list kept across restarts, in OpenCode's shape: one
// list per working directory, or one shared list with `scope: global`.
#[test]
fn oc_loads_persisted_tab_metadata() {
    let a = Path::new("/work/a");
    let b = Path::new("/work/b");
    let kept = |s: &str| tabs::Saved {
        tab: Tab::titled(s, &s.to_uppercase()),
        viewed: Some(5),
    };
    let text = tabs::with_saved(None, Scope::Cwd, a, &[kept("first"), kept("second")]);
    let text = tabs::with_saved(Some(&text), Scope::Cwd, b, &[kept("other")]);
    let text = tabs::with_saved(Some(&text), Scope::Global, a, &[kept("shared")]);
    assert_eq!(
        tabs::saved(Some(&text), Scope::Cwd, a),
        [kept("first"), kept("second")]
    );
    assert_eq!(tabs::saved(Some(&text), Scope::Cwd, b), [kept("other")]);
    assert_eq!(tabs::saved(Some(&text), Scope::Global, b), [kept("shared")]);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["cwd"]["/work/a"]["tabs"][0],
        json!({"sessionID": "first", "title": "FIRST", "viewed": 5})
    );
    // OpenCode's own file reads too: a tab is its session (and title).
    let oc = json!({"global": {"tabs": [{"sessionID": "s1", "title": "T"}, {"sessionID": "s1"}], "unread": {}}, "cwd": {}});
    assert_eq!(
        tabs::saved(Some(&oc.to_string()), Scope::Global, a),
        [tabs::Saved {
            tab: Tab::titled("s1", "T"),
            viewed: None
        }]
    );
    assert!(tabs::saved(Some("not json"), Scope::Global, a).is_empty());
    assert!(tabs::saved(None, Scope::Cwd, a).is_empty());
}

// ----- tui/test/component/session-tabs-status ---------------------------------

fn strip_frame(items: Vec<Item>, active: usize, numbers: bool, width: u16) -> Vec<String> {
    frame_of(items, active, numbers, false, width)
}

fn frame_of(
    items: Vec<Item>,
    active: usize,
    numbers: bool,
    vertical: bool,
    width: u16,
) -> Vec<String> {
    let mut a = App::new();
    a.tabs = Some(Strip {
        items,
        active: Some(active),
        numbers,
        vertical,
        detail: "project".into(),
        start: 0.into(),
    });
    let mut t = Terminal::new(TestBackend::new(width, 10)).unwrap();
    t.draw(|f| view::draw(f, &a)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect()
        })
        .collect()
}

fn item(title: &str, status: Status) -> Item {
    Item {
        title: title.into(),
        status,
        new: false,
    }
}

// (adapted) Horizontal tabs: the indicator before a title shows the tab's
// status (the spinner frame, `?`, `!`, the unread dot) in the same two
// cells the number takes, so titles never move; `numbers` shows ordinals.
// No mouse, so no context menu.
#[test]
fn oc_horizontal_tabs_replace_ordinals_with_status_without_moving_titles() {
    let idle = Status::default();
    let frame = |s: Status| strip_frame(vec![item("First", s), item("Second", idle)], 1, false, 60);
    let first = &frame(idle)[0];
    assert!(
        first.contains("   First") && first.contains("   Second"),
        "{first}"
    );
    // Columns, not bytes: the spinner is three bytes wide.
    let col = |row: &str| row.find("First").map(|b| row[..b].chars().count());
    let column = col(first).unwrap();
    let states = [
        (Status { busy: true, ..idle }, tabs::SPINNER),
        (
            Status {
                busy: true,
                attention: Some(Attention::Question),
                ..idle
            },
            "?",
        ),
        (
            Status {
                busy: true,
                attention: Some(Attention::Permission),
                ..idle
            },
            "!",
        ),
        (
            Status {
                unread: Some(Unread::Activity),
                ..idle
            },
            tabs::UNREAD,
        ),
        (
            Status {
                unread: Some(Unread::Error),
                ..idle
            },
            tabs::UNREAD,
        ),
        (idle, ""),
    ];
    for (status, label) in states {
        let rows = frame(status);
        let want = format!("{label:>2} First");
        assert!(rows[0].contains(&want), "{:?}: {}", status, rows[0]);
        assert_eq!(col(&rows[0]), Some(column), "{}", rows[0]);
    }
    let rows = strip_frame(
        vec![
            item("First", Status { busy: true, ..idle }),
            item("Second", idle),
        ],
        1,
        true,
        60,
    );
    assert!(
        rows[0].contains(" 1 First") && rows[0].contains(" 2 Second"),
        "{}",
        rows[0]
    );
    let new = Item {
        title: tabs::NEW_SESSION_TITLE.into(),
        status: idle,
        new: true,
    };
    let rows = strip_frame(vec![item("First", idle), new], 1, false, 60);
    assert!(rows[0].contains("+ New session"), "{}", rows[0]);
}

// Many tabs: a window around the active one, with the hidden ones counted.
#[test]
fn a_crowded_strip_counts_the_tabs_it_hides() {
    let items: Vec<Item> = (1..=12)
        .map(|i| item(&format!("Tab {i}"), Status::default()))
        .collect();
    let rows = strip_frame(items, 11, false, 60);
    assert!(rows[0].starts_with("‹"), "{}", rows[0]);
    assert!(rows[0].contains("Tab 12"), "{}", rows[0]);
    assert!(!rows[0].contains('›'), "{}", rows[0]);
}

// (adapted) The vertical half: the column's titles keep their column
// whatever the indicator shows, and the detail line is under each title.
#[test]
fn oc_vertical_tabs_replace_ordinals_with_status_without_moving_titles() {
    let idle = Status::default();
    let frame = |s: Status, numbers: bool| {
        frame_of(
            vec![item("First", s), item("Second", idle)],
            1,
            numbers,
            true,
            120,
        )
    };
    let rows = frame(idle, false);
    assert!(rows[1].starts_with("   First"), "{}", rows[1]);
    assert_eq!(rows[2].trim(), "project");
    assert!(rows[4].starts_with("   Second"), "{}", rows[4]);
    assert_eq!(
        rows[4].chars().nth(42),
        Some(' '),
        "the session starts after the column"
    );
    for (status, label) in [
        (Status { busy: true, ..idle }, tabs::SPINNER),
        (
            Status {
                attention: Some(Attention::Permission),
                ..idle
            },
            "!",
        ),
        (
            Status {
                unread: Some(Unread::Activity),
                ..idle
            },
            tabs::UNREAD,
        ),
    ] {
        let rows = frame(status, false);
        assert!(
            rows[1].starts_with(&format!("{label:>2} First")),
            "{}",
            rows[1]
        );
        assert_eq!(rows[2].trim(), "project");
    }
    let rows = frame(idle, true);
    assert!(rows[1].starts_with(" 1 First") && rows[4].starts_with(" 2 Second"));
}

// (adapted) Vertical tabs need the column's width beside the session's
// preferred width: narrower, they are the strip along the top.
#[test]
fn oc_vertical_session_tabs_switch_to_horizontal_below_readable_content_width() {
    let items = || vec![item("Resize fixture", Status::default())];
    let wide = frame_of(items(), 0, false, true, 120);
    assert!(wide[1][..42].contains("Resize fixture"), "{}", wide[1]);
    let narrow = frame_of(items(), 0, false, true, 100);
    assert!(narrow[0].contains("Resize fixture"), "{}", narrow[0]);
    let wide = frame_of(items(), 0, false, true, 120);
    assert!(wide[1][..42].contains("Resize fixture"));
}

// ----- tui/test/context/session-tabs: each tab's own state --------------------

fn permission() -> (
    super::bridge::Ev,
    tokio::sync::oneshot::Receiver<codesmith_core::permission::Reply>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let req = codesmith_core::permission::Request {
        action: "shell".into(),
        resources: vec!["bun run test".into()],
        save: vec![],
        title: String::new(),
    };
    (super::bridge::Ev::Permission(req, tx), rx)
}

fn question() -> (
    super::bridge::Ev,
    tokio::sync::oneshot::Receiver<super::bridge::Answers>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let p = codesmith_core::question::Prompt {
        question: "Choose an approach".into(),
        header: String::new(),
        options: vec![],
        multiple: false,
    };
    (super::bridge::Ev::Question(vec![p], tx), rx)
}

// (adapted) A tab keeps its own place: each tab's app keeps its scroll
// position, draft and transcript while another is shown, and what all tabs
// share (display settings, the prompt history) moves with the shown one.
// A closed tab's state goes with it.
#[test]
fn oc_keeps_scroll_anchors_for_open_session_tabs() {
    let mut shown = App::new();
    shown.scroll = 12;
    shown.input.set("first draft");
    shown.history = vec!["earlier prompt".into()];
    shown.verbosity = super::group::Verbosity::High;
    let mut other = App::new();
    other.scroll = 3;
    // Switch: the other tab's app comes in with the shared state.
    shown.hand_over(&mut other);
    std::mem::swap(&mut shown, &mut other);
    assert_eq!(shown.scroll, 3);
    assert_eq!(shown.input.text(), "");
    assert_eq!(shown.history, ["earlier prompt"]);
    assert_eq!(shown.verbosity, super::group::Verbosity::High);
    // And back: the first tab's place is where it was left.
    shown.hand_over(&mut other);
    std::mem::swap(&mut shown, &mut other);
    assert_eq!(shown.scroll, 12);
    assert_eq!(shown.input.text(), "first draft");
    assert_eq!(shown.history, ["earlier prompt"]);
}

// (adapted) Kept tabs are unread when their session changed after they
// were last shown (OpenCode compares the server's idle and viewed times).
#[test]
fn oc_derives_unread_state_from_session_times() {
    assert_eq!(tabs::unread_since(2, Some(1)), Some(Unread::Activity));
    assert_eq!(tabs::unread_since(2, Some(2)), None);
    assert_eq!(tabs::unread_since(2, None), None);
}

// (adapted) An unread tab's marker takes the accent color, a failed one's
// the error color.
#[test]
fn oc_marks_unread_failed_sessions_with_error_styling() {
    use super::theme;
    let p = theme::current();
    for (unread, color) in [(Unread::Activity, p.accent), (Unread::Error, p.error)] {
        let mut a = App::new();
        a.tabs = Some(Strip {
            items: vec![
                item("First", Status::default()),
                item(
                    "Second",
                    Status {
                        unread: Some(unread),
                        ..Status::default()
                    },
                ),
            ],
            active: Some(0),
            ..Strip::default()
        });
        let mut t = Terminal::new(TestBackend::new(60, 10)).unwrap();
        t.draw(|f| view::draw(f, &a)).unwrap();
        let buf = t.backend().buffer();
        let x = (0..60)
            .find(|&x| buf[(x, 0)].symbol() == tabs::UNREAD)
            .expect("the unread marker");
        assert_eq!(buf[(x, 0)].fg, color, "{unread:?}");
    }
}

// (adapted) A permission request and a question are told apart, the
// permission first while both wait; showing the tab does not answer them,
// and answering each clears it.
#[test]
fn oc_distinguishes_family_questions_and_permissions_without_clearing_them_on_selection() {
    use super::tabs_host::attention;
    let mut a = App::new();
    assert_eq!(attention(&a), None);
    let (ev, _q) = question();
    a.on_event(ev);
    assert_eq!(attention(&a), Some(Attention::Question));
    // Shown (its app swapped in) and away again: still waiting.
    let mut other = App::new();
    a.hand_over(&mut other);
    other.hand_over(&mut a);
    assert_eq!(attention(&a), Some(Attention::Question));
    a.cancel_modal();
    let (ev, mut reply) = permission();
    a.on_event(ev);
    assert_eq!(attention(&a), Some(Attention::Permission));
    a.on_key(ratatui::crossterm::event::KeyEvent::from(
        ratatui::crossterm::event::KeyCode::Char('y'),
    ));
    assert!(reply.try_recv().is_ok());
    assert_eq!(attention(&a), None);
}
