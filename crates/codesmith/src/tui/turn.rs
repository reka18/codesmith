//! A turn's duration and throughput, as OpenCode's `util/session-turn`
//! measures them: the duration runs from the prompt that started the turn
//! to the end of its last step; the throughput is the output and
//! reasoning tokens over the time the provider spent streaming them (from
//! each step's first token, or from its request when that is unknown, to
//! the end of its stream), so tool time and the wait for the first token
//! are left out. The UI records the steps of the running turn as their
//! events come in.

use std::time::Instant;

/// What a turn measurement reads of a message.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Message {
    pub id: String,
    /// `user`, `assistant`, `synthetic`, `idle`, `system`.
    pub kind: &'static str,
    pub created: u64,
    pub first_token: Option<u64>,
    pub streamed: Option<u64>,
    pub completed: Option<u64>,
    /// Output and reasoning tokens, when known.
    pub tokens: Option<(u64, u64)>,
}

/// No idle markers: a turn ends at the next prompt.
pub fn legacy(messages: &[Message]) -> bool {
    !messages.iter().any(|m| m.kind == "idle")
}

fn position(messages: &[Message], message: &Message, index: Option<usize>) -> Option<usize> {
    index.or_else(|| messages.iter().position(|m| m.id == message.id))
}

fn input_index(messages: &[Message], end: usize, legacy: bool) -> Option<usize> {
    let mut input = None;
    for i in (0..end).rev() {
        let m = &messages[i];
        if m.kind == "idle" {
            return input;
        }
        if m.kind != "user" && m.kind != "synthetic" {
            continue;
        }
        if legacy {
            return Some(i);
        }
        input = Some(i);
    }
    input
}

/// Milliseconds from the turn's prompt until `message` completed (0 while
/// it runs).
pub fn duration(
    messages: &[Message],
    message: &Message,
    index: Option<usize>,
    legacy_turns: Option<bool>,
) -> u64 {
    let Some(done) = message.completed else {
        return 0;
    };
    let at = position(messages, message, index).unwrap_or(messages.len());
    let legacy_turns = legacy_turns.unwrap_or_else(|| legacy(messages));
    let start = input_index(messages, at, legacy_turns)
        .map(|i| messages[i].created)
        .unwrap_or(message.created);
    done.saturating_sub(start)
}

/// Output tokens per second while the provider streamed them, or `None`
/// until every step of the turn has streamed.
pub fn tokens_per_second(
    messages: &[Message],
    message: &Message,
    index: Option<usize>,
    legacy_turns: Option<bool>,
) -> Option<f64> {
    let legacy_turns = legacy_turns.unwrap_or_else(|| legacy(messages));
    let end = position(messages, message, index).map_or(messages.len(), |i| i + 1);
    let start = input_index(messages, end, legacy_turns).map_or(0, |i| i + 1);
    let steps: Vec<&Message> = messages[start.min(end)..end]
        .iter()
        .filter(|m| m.kind == "assistant")
        .collect();
    if steps.is_empty() {
        return None;
    }
    let mut total = 0u64;
    for s in &steps {
        let streamed = s.streamed?;
        total += streamed.saturating_sub(s.first_token.unwrap_or(s.created));
    }
    let output: u64 = steps
        .iter()
        .map(|s| s.tokens.map_or(0, |(o, r)| o + r))
        .sum();
    if output == 0 || total == 0 {
        return None;
    }
    Some(output as f64 / (total as f64 / 1000.0))
}

/// The running turn as the UI sees it: when it started, and its steps.
#[derive(Debug, Clone)]
pub struct Recorder {
    origin: Instant,
    messages: Vec<Message>,
    /// The step under way: when its request went out, and its first token.
    step: Option<(u64, Option<u64>)>,
}

impl Recorder {
    pub fn start() -> Recorder {
        Recorder {
            origin: Instant::now(),
            messages: vec![Message {
                id: "prompt".into(),
                kind: "user",
                ..Default::default()
            }],
            step: Some((0, None)),
        }
    }

    fn now(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    /// Something streamed: the step's first token, if it is.
    pub fn token(&mut self) {
        let now = self.now();
        match &mut self.step {
            Some((_, first @ None)) => *first = Some(now),
            Some(_) => {}
            None => self.step = Some((now, Some(now))),
        }
    }

    /// A tool call ended: the next step's request goes out after it.
    pub fn tool_ended(&mut self) {
        let now = self.now();
        self.step = Some((now, None));
    }

    /// A step ended with these output and reasoning tokens.
    pub fn step_ended(&mut self, output: u64, reasoning: u64) {
        let now = self.now();
        let (created, first) = self.step.take().unwrap_or((now, None));
        let n = self.messages.len();
        self.messages.push(Message {
            id: format!("step-{n}"),
            kind: "assistant",
            created,
            first_token: first,
            streamed: Some(now),
            completed: Some(now),
            tokens: Some((output, reasoning)),
        });
    }

    /// A model step ended in it.
    pub fn stepped(&self) -> bool {
        self.messages.iter().any(|m| m.kind == "assistant")
    }

    /// The finished turn: its duration (ms) and throughput.
    pub fn finish(&self) -> (u64, Option<f64>) {
        let last = self.messages.last().filter(|m| m.kind == "assistant");
        let ms = last.map_or_else(
            || self.now(),
            |m| duration(&self.messages, m, None, Some(true)),
        );
        let tps = last.and_then(|m| tokens_per_second(&self.messages, m, None, Some(true)));
        (ms, tps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(id: &str, created: u64) -> Message {
        Message {
            id: id.into(),
            kind: "user",
            created,
            ..Default::default()
        }
    }

    fn other(id: &str, kind: &'static str, created: u64) -> Message {
        Message {
            id: id.into(),
            kind,
            created,
            ..Default::default()
        }
    }

    /// A step: created, first token, streamed, completed, tokens.
    fn step(
        id: &str,
        (created, first_token, streamed, completed): (u64, Option<u64>, Option<u64>, Option<u64>),
        output: Option<u64>,
        reasoning: u64,
    ) -> Message {
        Message {
            id: id.into(),
            kind: "assistant",
            created,
            first_token,
            streamed,
            completed,
            tokens: output.map(|o| (o, reasoning)),
        }
    }

    fn timed(
        id: &str,
        created: u64,
        streamed: u64,
        completed: u64,
        output: u64,
        r: u64,
    ) -> Message {
        step(
            id,
            (created, None, Some(streamed), Some(completed)),
            Some(output),
            r,
        )
    }

    // session-turn.test.ts: "measures turn duration from the user prompt across assistant steps"
    #[test]
    fn oc_measures_turn_duration_from_the_prompt_across_steps() {
        let fin = step("assistant-2", (27_000, None, None, Some(30_000)), None, 0);
        let messages = [
            input("user-1", 1_000),
            step("assistant-1", (8_000, None, None, Some(11_000)), None, 0),
            fin.clone(),
        ];
        assert_eq!(duration(&messages, &fin, None, None), 29_000);
    }

    // session-turn.test.ts: "measures decode throughput from the first streamed token"
    #[test]
    fn oc_measures_decode_throughput_from_the_first_streamed_token() {
        let first = step(
            "a1",
            (6_000, Some(8_000), Some(10_000), Some(20_000)),
            Some(20),
            5,
        );
        let fin = step(
            "a2",
            (24_000, Some(25_000), Some(28_000), Some(31_000)),
            Some(30),
            10,
        );
        let messages = [input("user-1", 1_000), first, fin.clone()];
        assert_eq!(tokens_per_second(&messages, &fin, None, None), Some(13.0));
    }

    // session-turn.test.ts: "falls back to the request window for steps recorded without a first token"
    #[test]
    fn oc_falls_back_to_the_request_window_without_a_first_token() {
        let first = step("a1", (6_000, None, Some(10_000), Some(20_000)), Some(20), 5);
        let fin = step(
            "a2",
            (24_000, Some(25_000), Some(28_000), Some(31_000)),
            Some(30),
            10,
        );
        let messages = [input("user-1", 1_000), first, fin.clone()];
        assert_eq!(
            tokens_per_second(&messages, &fin, None, None),
            Some(65.0 / 7.0)
        );
    }

    // session-turn.test.ts: "measures request throughput including reasoning without tool time"
    #[test]
    fn oc_measures_request_throughput_including_reasoning_without_tool_time() {
        let first = step("a1", (6_000, None, Some(10_000), Some(20_000)), Some(20), 5);
        let fin = step(
            "a2",
            (24_000, None, Some(30_000), Some(31_000)),
            Some(30),
            10,
        );
        let mut messages = vec![input("user-1", 1_000), first, fin.clone()];
        assert_eq!(tokens_per_second(&messages, &fin, None, None), Some(6.5));
        messages[1].streamed = None;
        assert_eq!(tokens_per_second(&messages, &fin, None, None), None);
    }

    // session-turn.test.ts: "omits turn throughput when a stream boundary is unavailable"
    #[test]
    fn oc_omits_turn_throughput_without_a_stream_boundary() {
        let fin = step("a1", (1, None, None, None), Some(20), 0);
        assert_eq!(
            tokens_per_second(std::slice::from_ref(&fin), &fin, None, None),
            None
        );
    }

    // session-turn.test.ts: "measures historical footers without later inputs
    // or incomplete steps (indexed: %s)" (2 cases)
    #[test]
    fn oc_measures_historical_footers_without_later_inputs_or_incomplete_steps() {
        let messages = vec![
            timed("before-input", 0, 1_000, 2_000, 5, 2),
            input("input", 3_000),
            timed("first-step", 4_000, 5_000, 6_000, 10, 2),
            other("system", "system", 6_500),
            timed("second-step", 7_000, 8_000, 9_000, 20, 2),
            other("synthetic", "synthetic", 10_000),
            timed("after-synthetic", 11_000, 13_000, 14_000, 12, 2),
            input("later-input", 15_000),
            step("incomplete", (1, None, None, None), None, 0),
        ];
        for indexed in [false, true] {
            let got: Vec<(u64, Option<f64>)> = messages
                .iter()
                .enumerate()
                .filter(|(_, m)| m.kind == "assistant")
                .map(|(i, m)| {
                    let index = indexed.then_some(i);
                    (
                        duration(&messages, m, index, None),
                        tokens_per_second(&messages, m, index, None),
                    )
                })
                .collect();
            assert_eq!(
                got,
                vec![
                    (2_000, Some(7.0)),
                    (3_000, Some(12.0)),
                    (6_000, Some(17.0)),
                    (4_000, Some(7.0)),
                    (0, None),
                ],
                "indexed: {indexed}"
            );
        }
    }

    // session-turn.test.ts: "preserves missing-anchor fallbacks without
    // including the absent assistant's tokens"
    #[test]
    fn oc_preserves_missing_anchor_fallbacks() {
        let absent = step(
            "absent",
            (8_000, None, Some(9_000), Some(10_000)),
            Some(900),
            0,
        );
        let stored = step(
            "stored",
            (6_000, None, Some(8_000), Some(9_000)),
            Some(20),
            0,
        );
        let prompt = input("input", 5_000);
        let both = [prompt, stored.clone()];
        assert_eq!(duration(&both, &absent, None, None), 5_000);
        assert_eq!(tokens_per_second(&both, &absent, None, None), Some(10.0));
        let one = [stored];
        assert_eq!(duration(&one, &absent, None, None), 2_000);
        assert_eq!(tokens_per_second(&one, &absent, None, None), Some(10.0));
        assert_eq!(duration(&[], &absent, None, None), 2_000);
        assert_eq!(tokens_per_second(&[], &absent, None, None), None);
    }

    // session-turn.test.ts: "measures a marker-era turn from its first prompt across steers"
    #[test]
    fn oc_measures_a_marker_era_turn_from_its_first_prompt_across_steers() {
        let fin = timed("second-step", 8_000, 9_000, 10_000, 20, 0);
        let messages = vec![
            input("old-input", 0),
            timed("old-step", 1_000, 2_000, 3_000, 5, 0),
            other("idle-1", "idle", 3_500),
            input("input", 4_000),
            timed("first-step", 5_000, 6_000, 7_000, 10, 0),
            input("steer", 7_500),
            fin.clone(),
        ];
        assert!(!legacy(&messages));
        assert_eq!(duration(&messages, &fin, None, None), 6_000);
        assert_eq!(tokens_per_second(&messages, &fin, None, None), Some(15.0));
        assert_eq!(duration(&messages, &fin, Some(6), Some(true)), 2_500);
        assert_eq!(
            tokens_per_second(&messages, &fin, Some(6), Some(true)),
            Some(20.0)
        );
    }
}
