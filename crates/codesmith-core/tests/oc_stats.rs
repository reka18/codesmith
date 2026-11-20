//! `codesmith stats` rendering, ported from OpenCode's
//! `packages/cli/test/stats.test.ts` (MIT). Each test names the OpenCode test
//! it comes from; "(adapted)" marks one rephrased for CodeSmith's card (no
//! blank rows between the calendar's days, the heading says `codesmith`).
//! See `crates/codesmith/PARITY.md`.

use chrono::{Local, NaiveDate, TimeZone};
use codesmith_ai::Usage;
use codesmith_core::stats::{render, Render, Stats, ToolStat};

fn ms(y: i32, m: u32, d: u32) -> i64 {
    let day = NaiveDate::from_ymd_opt(y, m, d).unwrap();
    Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap()
        .timestamp_millis()
}

fn tool(name: &str, calls: u64, failed: u64, p50: u64) -> ToolStat {
    ToolStat {
        name: name.into(),
        calls,
        succeeded: calls - failed,
        failed,
        p50_ms: Some(p50),
    }
}

/// OpenCode's fixture: two sessions and a subagent over a week in 2026.
fn stats() -> Stats {
    Stats {
        from: ms(2026, 1, 1),
        to: ms(2026, 1, 8),
        sessions: 2,
        subagents: 1,
        prompts: 4,
        steps: 6,
        tokens: Usage {
            input: 10_000,
            output: 2_000,
            reasoning: 1_000,
            cache_read: 5_000,
            cache_write: 500,
            ..Default::default()
        },
        cost: 12.34,
        tools: vec![tool("private_tool", 10, 2, 250)],
        activity: [("2026-01-02".to_string(), 2), ("2026-01-03".to_string(), 4)].into(),
        active_days: 2,
        streak: 2,
        ..Default::default()
    }
}

fn options() -> Render {
    Render {
        label: "2026 so far".into(),
        scope: "all projects".into(),
        models: false,
        tools: false,
        cost: false,
        limit: 10,
        color: false,
        width: 80,
    }
}

// "keeps the default card shareable" (adapted).
#[test]
fn oc_stats_keeps_the_default_card_shareable() {
    let out = render(&stats(), &options());
    assert!(
        out.contains("codesmith stats · 2026 so far · all projects"),
        "{out}"
    );
    assert!(out.contains("activity"));
    let rows: Vec<&str> = out.lines().collect();
    let mo = rows.iter().position(|l| l.starts_with("Mo")).unwrap();
    assert!(
        rows[mo + 1].starts_with("Tu") && rows[mo + 6].starts_with("Su"),
        "{out}"
    );
    assert!(out.contains("less ·░▒▓█ more"), "{out}");
    assert!(out.contains("2 sessions · 1 subagent"), "{out}");
    assert!(
        out.contains("80.0% tool success · 2 active days · best streak 2 days"),
        "{out}"
    );
    assert!(!out.contains("private_tool"));
    assert!(!out.contains("$12.34"));
}

// "renders only requested detail tables".
#[test]
fn oc_stats_renders_only_the_requested_detail_tables() {
    let out = render(
        &stats(),
        &Render {
            tools: true,
            cost: true,
            ..options()
        },
    );
    for want in [
        "COST & TOKENS",
        "TOOL RELIABILITY",
        "private_tool",
        "tool",
        "calls",
        "cached input        32.3%",
    ] {
        assert!(out.contains(want), "{want}: {out}");
    }
    assert!(!out.contains("codesmith stats"));
    assert!(!out.contains("activity"));
}

// "shows when detail tables omit rows" (adapted: the tool table; the model
// table's rows are built from the session store, so its half is covered
// by the e2e stats run).
#[test]
fn oc_stats_shows_when_a_detail_table_omits_rows() {
    let mut s = stats();
    s.tools.push(tool("grep", 4, 0, 20));
    let out = render(
        &s,
        &Render {
            tools: true,
            limit: 1,
            ..options()
        },
    );
    assert!(out.contains("+1 more tool"), "{out}");
    assert!(!out.contains("grep"), "{out}");
}

// "uses the OpenCode palette in color mode".
#[test]
fn oc_stats_uses_the_palette_in_color_mode() {
    let out = render(
        &stats(),
        &Render {
            color: true,
            ..options()
        },
    );
    assert!(out.contains("\x1b[1;36m"), "{out:?}");
    assert!(!out.contains("38;5;45"));
}

// "renders a concise empty state".
#[test]
fn oc_stats_renders_a_concise_empty_state() {
    let s = Stats {
        sessions: 0,
        subagents: 0,
        prompts: 0,
        steps: 0,
        active_days: 0,
        streak: 0,
        activity: Default::default(),
        ..stats()
    };
    let out = render(&s, &options());
    assert!(out.contains("no activity in this range"), "{out}");
    assert!(!out.contains("less ·░▒▓█ more"));
}

// "labels activity when terminal width truncates the requested range".
#[test]
fn oc_stats_labels_activity_when_the_width_truncates_the_range() {
    let s = Stats {
        from: ms(2020, 1, 1),
        ..stats()
    };
    let out = render(
        &s,
        &Render {
            width: 20,
            ..options()
        },
    );
    assert!(out.contains("activity · last 16 weeks"), "{out}");
}

// "renders shared intensity levels and partial month labels" (adapted: no
// blank rows between the days).
#[test]
fn oc_stats_labels_only_the_months_a_partial_range_shows() {
    let s = Stats {
        from: ms(2026, 4, 29),
        to: ms(2026, 5, 20),
        activity: [("2026-04-29".to_string(), 1)].into(),
        ..stats()
    };
    let out = render(&s, &options());
    assert!(out.contains("    May"), "{out}");
    assert!(!out.contains("Apr"), "{out}");
    let we = out.lines().find(|l| l.starts_with("We ")).unwrap();
    assert!(we.contains('\u{2588}'), "{out}");
}
