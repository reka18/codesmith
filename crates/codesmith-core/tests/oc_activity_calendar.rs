//! The `codesmith stats` activity calendar, ported from OpenCode's
//! `packages/util/src/activity-calendar.test.ts` (MIT). Each test names the
//! OpenCode test it comes from. See `crates/codesmith/PARITY.md`.

use chrono::{Local, NaiveDate, TimeZone};
use codesmith_core::stats::{activity_calendar, Calendar, CalendarDay, CalendarMonth};
use std::collections::{BTreeMap, BTreeSet};

/// Local midnight of a date, in milliseconds, as `new Date(y, m - 1, d)`.
fn ms(y: i32, m: u32, d: u32) -> i64 {
    let day = NaiveDate::from_ymd_opt(y, m, d).unwrap();
    Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap()
        .timestamp_millis()
}

fn activity(days: &[(&str, u64)]) -> BTreeMap<String, u64> {
    days.iter().map(|(d, n)| (d.to_string(), *n)).collect()
}

/// OpenCode's fixture: one week of 2026 with two active days.
fn input() -> BTreeMap<String, u64> {
    activity(&[("2026-01-01", 1), ("2026-01-02", 50)])
}

fn day(date: &str, steps: u64, level: i32) -> CalendarDay {
    CalendarDay {
        date: date.into(),
        steps,
        level,
    }
}

fn month(week: usize, label: &str, span: usize) -> CalendarMonth {
    CalendarMonth {
        week,
        label: label.into(),
        span,
    }
}

fn active_levels(c: &Calendar) -> Vec<i32> {
    c.weeks
        .iter()
        .flatten()
        .filter(|d| d.steps > 0)
        .map(|d| d.level)
        .collect()
}

/// OpenCode: "aligns Monday-first weeks and excludes cells outside the end-exclusive range".
#[test]
fn oc_aligns_monday_first_weeks_and_excludes_cells_outside_the_range() {
    let c = activity_calendar(&input(), ms(2026, 1, 1), ms(2026, 1, 8), 53);
    assert!(!c.clipped);
    assert_eq!(c.weeks.len(), 2);
    assert_eq!(c.weeks[0][0], day("2025-12-29", 0, -1));
    assert_eq!(c.weeks[0][3], day("2026-01-01", 1, 2));
    assert_eq!(c.weeks[0][4], day("2026-01-02", 50, 4));
    assert_eq!(c.weeks[1][2], day("2026-01-07", 0, 0));
    assert_eq!(c.weeks[1][3].level, -1);
    assert_eq!(c.months, vec![month(0, "Jan", 2)]);
}

/// OpenCode: "clips to the latest weeks and keeps empty days distinct from out-of-range cells".
#[test]
fn oc_clips_to_the_latest_weeks_and_keeps_empty_days_distinct() {
    let c = activity_calendar(&BTreeMap::new(), ms(2026, 1, 1), ms(2026, 1, 8), 1);
    assert!(c.clipped);
    assert_eq!(c.weeks.len(), 1);
    assert_eq!(c.weeks[0][0], day("2026-01-05", 0, 0));
    assert_eq!(c.weeks[0][3].level, -1);
}

/// OpenCode: "caps long histories at 53 weeks".
#[test]
fn oc_caps_long_histories_at_53_weeks() {
    let c = activity_calendar(&input(), ms(2020, 1, 1), ms(2026, 1, 8), 200);
    assert!(c.clipped);
    assert_eq!(c.weeks.len(), 53);
    assert_eq!(c.weeks.last().unwrap()[2].date, "2026-01-07");
    assert_eq!(c.months.iter().map(|m| m.span).sum::<usize>(), 53);
}

/// OpenCode: "returns month positions and spans for partial first and last months".
#[test]
fn oc_returns_month_positions_and_spans_for_partial_months() {
    let c = activity_calendar(&input(), ms(2026, 4, 29), ms(2026, 5, 20), 53);
    assert_eq!(c.months, vec![month(0, "Apr", 1), month(1, "May", 3)]);
    let partial = activity_calendar(&input(), ms(2026, 4, 1), ms(2026, 4, 2), 53);
    assert_eq!(partial.months, vec![month(0, "Apr", 1)]);
}

/// OpenCode: "places month labels across a year boundary".
#[test]
fn oc_places_month_labels_across_a_year_boundary() {
    let c = activity_calendar(&input(), ms(2025, 12, 22), ms(2026, 1, 12), 53);
    assert_eq!(c.months, vec![month(0, "Dec", 1), month(1, "Jan", 2)]);
}

/// OpenCode: "keeps exactly seven distinct cells per week across DST and leap days".
#[test]
fn oc_keeps_seven_distinct_cells_per_week_across_dst_and_leap_days() {
    let a = activity(&[("2024-02-29", 1), ("2024-03-10", 1)]);
    let c = activity_calendar(&a, ms(2024, 2, 26), ms(2024, 3, 12), 53);
    assert_eq!(c.weeks.len(), 3);
    let active: Vec<&str> = c
        .weeks
        .iter()
        .flatten()
        .filter(|d| d.steps > 0)
        .map(|d| d.date.as_str())
        .collect();
    assert_eq!(active, ["2024-02-29", "2024-03-10"]);
    let distinct: BTreeSet<&str> = c.weeks.iter().flatten().map(|d| d.date.as_str()).collect();
    assert_eq!(distinct.len(), 21);
}

/// OpenCode: "uses the highest intensity for a single distinct positive count".
#[test]
fn oc_uses_the_highest_intensity_for_a_single_distinct_count() {
    let a = activity(&[("2026-01-01", 10), ("2026-01-02", 10)]);
    let c = activity_calendar(&a, ms(2026, 1, 1), ms(2026, 1, 8), 53);
    assert_eq!(active_levels(&c), [4, 4]);
}

/// OpenCode: "ranks distinct activity counts in four levels".
#[test]
fn oc_ranks_distinct_activity_counts_in_four_levels() {
    let a: BTreeMap<String, u64> = (1..=8).map(|i| (format!("2026-01-0{i}"), i)).collect();
    let c = activity_calendar(&a, ms(2026, 1, 1), ms(2026, 1, 10), 53);
    assert_eq!(active_levels(&c), [1, 1, 2, 2, 3, 3, 4, 4]);
}

/// OpenCode: "keeps intensity relative to the requested range when older weeks are clipped".
#[test]
fn oc_keeps_intensity_relative_to_the_range_when_clipped() {
    let a = activity(&[("2026-01-01", 50), ("2026-01-05", 1)]);
    let c = activity_calendar(&a, ms(2026, 1, 1), ms(2026, 1, 8), 1);
    assert_eq!(c.weeks[0][0].level, 2);
}

/// OpenCode: "returns an empty calendar for an empty range".
#[test]
fn oc_returns_an_empty_calendar_for_an_empty_range() {
    let c = activity_calendar(&BTreeMap::new(), ms(2026, 1, 1), ms(2026, 1, 1), 53);
    assert_eq!(c, Calendar::default());
    assert!(c.weeks.is_empty() && c.months.is_empty() && !c.clipped);
}
