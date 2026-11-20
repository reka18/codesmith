//! Jumping between messages (`session.message.next`, `.previous`, their
//! `user` forms, and `session.messages_last_user`), as OpenCode's
//! `message-navigation`: the transcript scrolls so the next or previous
//! prompt (or answer) starts at the top, with blank room added below the
//! last one when there is too little transcript left to scroll that far.

/// Where a prompt or an answer starts in the laid-out transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// Its entry.
    pub entry: usize,
    /// Its row from the top of the whole transcript.
    pub y: usize,
    /// A prompt rather than an answer.
    pub user: bool,
}

/// Where a jump lands: the message, and the row to put at the top (an
/// answer keeps the row above it in view).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub entry: usize,
    pub y: usize,
    pub top: usize,
}

/// The blank rows to keep below the transcript so row `top` can be the
/// first one shown, as OpenCode's `messageNavigationSlack`.
pub fn slack(top: usize, viewport: usize, height: usize, current: usize) -> usize {
    let content = height.saturating_sub(current);
    (top + viewport).saturating_sub(content)
}

/// The next (`forward`) or previous message from `scroll_top` (the first
/// row shown), or from `current` when one was jumped to; `user_only`
/// skips answers. `None` past the first or last.
pub fn find_boundary(
    marks: &[Mark],
    forward: bool,
    scroll_top: usize,
    current: Option<usize>,
    user_only: bool,
) -> Option<Target> {
    let mut visible: Vec<Target> = marks
        .iter()
        .filter(|m| m.user || !user_only)
        .map(|m| Target {
            entry: m.entry,
            y: m.y,
            top: if m.user { m.y } else { m.y.saturating_sub(1) },
        })
        .collect();
    visible.sort_by_key(|t| t.y);
    if let Some(at) = current.and_then(|c| visible.iter().position(|t| t.entry == c)) {
        return if forward {
            visible.get(at + 1).copied()
        } else {
            at.checked_sub(1).map(|i| visible[i])
        };
    }
    if forward {
        visible.iter().find(|t| t.y > scroll_top).copied()
    } else {
        visible.iter().rev().find(|t| t.y < scroll_top).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // OpenCode's fixture: a prompt, an answer and a prompt at rows 0, 20
    // and 40 (entries 0, 1, 2).
    const MARKS: &[Mark] = &[
        Mark {
            entry: 0,
            y: 0,
            user: true,
        },
        Mark {
            entry: 1,
            y: 20,
            user: false,
        },
        Mark {
            entry: 2,
            y: 40,
            user: true,
        },
    ];

    fn t(entry: usize, y: usize, top: usize) -> Option<Target> {
        Some(Target { entry, y, top })
    }

    // message-navigation.test.ts: "adds only enough slack to align the selected message"
    #[test]
    fn oc_adds_only_enough_slack_to_align_the_selected_message() {
        assert_eq!(slack(80, 50, 100, 0), 30);
        assert_eq!(slack(20, 50, 130, 30), 0);
    }

    // message-navigation.test.ts: "finds the next user message without
    // stopping at an assistant message"
    #[test]
    fn oc_finds_the_next_user_message_without_stopping_at_an_answer() {
        assert_eq!(find_boundary(MARKS, true, 0, None, true), t(2, 40, 40));
    }

    // message-navigation.test.ts: "finds the previous user message without
    // stopping at an assistant message"
    #[test]
    fn oc_finds_the_previous_user_message_without_stopping_at_an_answer() {
        assert_eq!(find_boundary(MARKS, false, 35, None, true), t(0, 0, 0));
    }

    // message-navigation.test.ts: "preserves navigation across both user
    // and assistant messages"
    #[test]
    fn oc_preserves_navigation_across_prompts_and_answers() {
        assert_eq!(find_boundary(MARKS, true, 0, None, false), t(1, 20, 19));
        assert_eq!(find_boundary(MARKS, false, 35, None, false), t(1, 20, 19));
    }

    // message-navigation.test.ts: "uses the selected message when the
    // viewport is too tall to scroll"
    #[test]
    fn oc_uses_the_selected_message_when_the_viewport_cannot_scroll() {
        assert_eq!(find_boundary(MARKS, true, 0, Some(0), true), t(2, 40, 40));
        assert_eq!(find_boundary(MARKS, false, 0, Some(2), true), t(0, 0, 0));
    }

    // message-navigation.test.ts: "stops at the first and last selected user message"
    #[test]
    fn oc_stops_at_the_first_and_last_selected_user_message() {
        assert_eq!(find_boundary(MARKS, true, 0, Some(2), true), None);
        assert_eq!(find_boundary(MARKS, false, 0, Some(0), true), None);
    }

    // message-navigation.test.ts: "keeps the logical boundary when layout
    // temporarily moves it outside the viewport"
    #[test]
    fn oc_keeps_the_logical_boundary_when_layout_moves_it() {
        assert_eq!(find_boundary(MARKS, true, 0, Some(2), true), None);
    }

    // message-navigation.test.ts: "stops at the first and last message"
    #[test]
    fn oc_stops_at_the_first_and_last_message() {
        assert_eq!(find_boundary(MARKS, true, 0, Some(2), false), None);
        assert_eq!(find_boundary(MARKS, false, 0, Some(0), false), None);
    }
}
