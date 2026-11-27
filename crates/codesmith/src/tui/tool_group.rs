//! Tool groups: a run of tool calls with no model text between them is
//! drawn as one line, worded by what the calls did ("Ran 2 commands, read a
//! file, edited 3 files"), with a one-column glyph that animates in the
//! style of the call running now. This is the wording and the glyphs;
//! `view` finds the groups and draws them (see `docs/design/tool-groups.md`).
//!
//! Every glyph is a single terminal column from braille, block and
//! geometric characters, never an emoji, so a line never shifts as it
//! animates. They all step on the forge's 120 ms clock.

use super::group::canonical;
use super::view::ForgeLook;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// What a call does, for its glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `read`, `grep`, `glob`, `list`.
    Looking,
    /// `edit`, `write`, `patch`.
    Shaping,
    /// `shell`.
    Commands,
    /// `webfetch`, `websearch`.
    Web,
    /// `subagent`.
    Subagent,
    /// Any other tool (MCP, plugin, the rest of the built-ins): a sprite
    /// generated from its name.
    Other,
}

/// A call's kind from its tool's name.
pub fn kind(name: &str) -> Kind {
    match canonical(&name.to_ascii_lowercase()) {
        "read" | "grep" | "glob" | "list" => Kind::Looking,
        "edit" | "write" | "patch" => Kind::Shaping,
        "shell" => Kind::Commands,
        "webfetch" | "websearch" => Kind::Web,
        "subagent" => Kind::Subagent,
        _ => Kind::Other,
    }
}

/// A dot sweeping around the cell.
pub const LOOKING: &[char] = &['⡀', '⠄', '⠂', '⠁', '⠈', '⠐', '⠠', '⢀'];
/// A hammer rising, striking and sparking.
pub const SHAPING: &[char] = &['▁', '▂', '▃', '▅', '▇', '█', '✶', '▂'];
/// The shaping frame that is the spark, drawn in the hottest colour.
pub const SPARK: usize = 6;
/// A turning block.
pub const COMMANDS: &[char] = &['▖', '▘', '▝', '▗'];
/// An orbit.
pub const WEB: &[char] = &['◜', '◝', '◞', '◟'];
/// A braille spinner.
pub const SUBAGENT: &[char] = &['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];

/// A call's glyph: its frames while it runs and its settled form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glyph {
    pub kind: Kind,
    pub frames: Vec<char>,
    pub rest: char,
}

impl Glyph {
    /// The glyph of a call to `name`.
    pub fn of(name: &str) -> Glyph {
        let k = kind(name);
        let (frames, rest) = match k {
            Kind::Looking => (LOOKING.to_vec(), '⠶'),
            Kind::Shaping => (SHAPING.to_vec(), '▆'),
            Kind::Commands => (COMMANDS.to_vec(), '▞'),
            Kind::Web => (WEB.to_vec(), '◯'),
            Kind::Subagent => (SUBAGENT.to_vec(), '⣿'),
            Kind::Other => {
                let s = sprite(name);
                (s.frames.to_vec(), s.rest)
            }
        };
        Glyph {
            kind: k,
            frames,
            rest,
        }
    }

    /// The frame shown at `tick`.
    pub fn frame(&self, tick: u64) -> char {
        self.frames[(tick % self.frames.len() as u64) as usize]
    }

    /// Whether the frame at `tick` is the shaping spark.
    pub fn sparks(&self, tick: u64) -> bool {
        self.kind == Kind::Shaping && (tick % self.frames.len() as u64) as usize == SPARK
    }
}

/// The cell's eight braille dots clockwise around its ring, from the top
/// left: dots 1, 4, 5, 6, 8, 7, 3 and 2.
pub const RING: [u8; 8] = [0x01, 0x08, 0x10, 0x20, 0x80, 0x40, 0x04, 0x02];
/// The two centre dots (3 and 6) a sprite's resting form adds.
pub const CENTRE: u8 = 0x24;
/// The first braille character, the empty cell.
const BRAILLE: u32 = 0x2800;

/// A braille sprite generated from a tool's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sprite {
    /// Ring positions of the starting dots, in the order they were picked.
    pub picks: [Option<u8>; 4],
    /// The way the dots step around the ring: 1 clockwise, -1 the other.
    pub dir: i8,
    pub frames: [char; 8],
    pub rest: char,
}

/// FNV-1a over the name's code points.
fn hash(s: &str) -> u32 {
    let mut h: u32 = 2_166_136_261;
    for c in s.chars() {
        h ^= c as u32;
        h = h.wrapping_mul(16_777_619);
    }
    h
}

fn braille(bits: u8) -> char {
    char::from_u32(BRAILLE + bits as u32).unwrap_or('⠀')
}

/// The sprite of `name`: its hash picks 2 to 4 of the ring's dots and a
/// direction; each frame steps those dots one place around the ring, and
/// the resting form is the starting dots plus the two centre ones. The
/// same name always gives the same sprite.
pub fn sprite(name: &str) -> Sprite {
    let h = hash(if name.is_empty() { " " } else { name });
    let count = 2 + (h % 3) as usize;
    let mut picks: Vec<u8> = Vec::with_capacity(4);
    let mut x = h;
    // The mix reaches every residue quickly; the bound only guards the
    // loop, filling from the lowest free place if it ever ran out.
    for _ in 0..256 {
        if picks.len() == count {
            break;
        }
        x = (x ^ (x >> 15)).wrapping_mul(2_246_822_507);
        let p = (x % 8) as u8;
        if !picks.contains(&p) {
            picks.push(p);
        }
    }
    let mut free = 0u8;
    while picks.len() < count {
        if !picks.contains(&free) {
            picks.push(free);
        }
        free += 1;
    }
    let dir: i8 = if (h >> 8) & 1 == 1 { 1 } else { -1 };
    let mut frames = ['⠀'; 8];
    for (f, slot) in frames.iter_mut().enumerate() {
        let bits = picks.iter().fold(0u8, |b, &p| {
            b | RING[(p as i32 + dir as i32 * f as i32).rem_euclid(8) as usize]
        });
        *slot = braille(bits);
    }
    let rest_bits = picks.iter().fold(0u8, |b, &p| b | RING[p as usize]);
    let mut out = [None; 4];
    for (i, p) in picks.iter().enumerate() {
        out[i] = Some(*p);
    }
    Sprite {
        picks: out,
        dir,
        frames,
        rest: braille(rest_bits | CENTRE),
    }
}

/// The heat colours a running glyph glows through, by step: cherry red,
/// orange, amber, orange.
const HEAT_STEPS: [usize; 4] = [0, 1, 2, 1];

/// How hot each heat colour is on the forge's ramp: cherry red, orange and
/// amber; a light background's ramp stops at deep amber.
fn heat_level(step: usize, light: bool) -> f32 {
    let levels: [f32; 3] = if light {
        [0.0, 0.5, 1.0]
    } else {
        [0.0, 0.35, 0.7]
    };
    levels[step.min(2)]
}

/// The colours glyphs use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    pub forge: ForgeLook,
    pub agent: Color,
    pub success: Color,
    pub error: Color,
}

impl Look {
    pub fn from_palette(p: &super::theme::Palette) -> Look {
        Look {
            forge: ForgeLook::from_palette(p),
            agent: p.agent,
            success: p.success,
            error: p.error,
        }
    }

    /// `c`, or the muted colour under `NO_COLOR`.
    fn plain(&self, c: Color) -> Style {
        Style::default().fg(if self.forge.color {
            c
        } else {
            self.forge.muted
        })
    }

    /// The forge's heat colour `step` (0 cherry red, 1 orange, 2 amber).
    pub fn heat(&self, step: usize) -> Style {
        self.forge.heat(heat_level(step, self.forge.light))
    }
}

/// A running call's glyph at `tick`: its frame, glowing through the
/// forge's heat colours; the spark in the hottest, a subagent's in the
/// agent colour.
pub fn running(g: &Glyph, tick: u64, look: &Look) -> Span<'static> {
    let style = if g.kind == Kind::Subagent {
        look.plain(look.agent)
    } else if g.sparks(tick) {
        look.heat(2)
    } else {
        look.heat(HEAT_STEPS[(tick % 4) as usize])
    };
    Span::styled(g.frame(tick).to_string(), style)
}

/// A finished call's settled glyph: in the success colour, or the error
/// colour when `failed`.
pub fn settled(g: &Glyph, failed: bool, look: &Look) -> Span<'static> {
    let c = if failed { look.error } else { look.success };
    Span::styled(g.rest.to_string(), look.plain(c))
}

/// A finished call's mark in an open group: ✓, or ✗ when it failed.
pub fn mark(failed: bool, look: &Look) -> Span<'static> {
    if failed {
        Span::styled("✗", look.plain(look.error))
    } else {
        Span::styled("✓", look.plain(look.success))
    }
}

/// What the group line counts a call as.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Count {
    Command,
    Read,
    Search,
    Edit,
    Page,
    Subagent,
    /// Any other tool, by name.
    Named(String),
}

fn count_of(name: &str) -> Count {
    match canonical(&name.to_ascii_lowercase()) {
        "shell" => Count::Command,
        "read" => Count::Read,
        "grep" | "glob" | "list" => Count::Search,
        "edit" | "write" | "patch" => Count::Edit,
        "webfetch" | "websearch" => Count::Page,
        "subagent" => Count::Subagent,
        _ => Count::Named(name.to_string()),
    }
}

/// A group's line: what its calls did, counted by kind in the order the
/// kinds first appeared, in the past tense once `done` and the present
/// while it runs ("Running a command, reading a file…"). A tool of no kind
/// (an MCP or plugin tool) is named: "called github_create_issue".
pub fn label<S: AsRef<str>>(names: &[S], done: bool) -> String {
    let mut counts: Vec<(Count, usize)> = Vec::new();
    for n in names {
        let c = count_of(n.as_ref());
        match counts.iter_mut().find(|(k, _)| *k == c) {
            Some((_, n)) => *n += 1,
            None => counts.push((c, 1)),
        }
    }
    let part = |n: usize, one: &str, many: &str, past: &str, now: &str| {
        let verb = if done { past } else { now };
        if n == 1 {
            format!("{verb} {one}")
        } else {
            format!("{verb} {n} {many}")
        }
    };
    let parts: Vec<String> = counts
        .iter()
        .map(|(c, n)| match c {
            Count::Command => part(*n, "a command", "commands", "ran", "running"),
            Count::Read => part(*n, "a file", "files", "read", "reading"),
            Count::Search => part(*n, "a search", "searches", "ran", "running"),
            Count::Edit => part(*n, "a file", "files", "edited", "editing"),
            Count::Page => part(*n, "a page", "pages", "fetched", "fetching"),
            Count::Subagent => part(*n, "a subagent", "subagents", "ran", "running"),
            Count::Named(name) => {
                let verb = if done { "called" } else { "calling" };
                match n {
                    1 => format!("{verb} {name}"),
                    2 => format!("{verb} {name} twice"),
                    n => format!("{verb} {name} {n} times"),
                }
            }
        })
        .collect();
    let mut s = parts.join(", ");
    if let Some(first) = s.chars().next() {
        s.replace_range(..first.len_utf8(), &first.to_uppercase().to_string());
    }
    if !done && !s.is_empty() {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthChar;

    const DARK: Look = Look {
        forge: ForgeLook {
            muted: Color::Rgb(0x80, 0x80, 0x80),
            color: true,
            light: false,
        },
        agent: Color::Rgb(0xb4, 0x9c, 0xff),
        success: Color::Rgb(0x5f, 0xc4, 0x8a),
        error: Color::Rgb(0xf2, 0x76, 0x6b),
    };

    fn rgb(c: (u8, u8, u8)) -> Option<Color> {
        Some(Color::Rgb(c.0, c.1, c.2))
    }

    // ----- wording -----

    #[test]
    fn every_kind_reads_in_both_tenses_singular_and_plural() {
        let cases: &[(&str, &str, &str, &str, &str)] = &[
            (
                "shell",
                "Ran a command",
                "Ran 2 commands",
                "Running a command…",
                "Running 2 commands…",
            ),
            (
                "read",
                "Read a file",
                "Read 2 files",
                "Reading a file…",
                "Reading 2 files…",
            ),
            (
                "grep",
                "Ran a search",
                "Ran 2 searches",
                "Running a search…",
                "Running 2 searches…",
            ),
            (
                "edit",
                "Edited a file",
                "Edited 2 files",
                "Editing a file…",
                "Editing 2 files…",
            ),
            (
                "webfetch",
                "Fetched a page",
                "Fetched 2 pages",
                "Fetching a page…",
                "Fetching 2 pages…",
            ),
            (
                "subagent",
                "Ran a subagent",
                "Ran 2 subagents",
                "Running a subagent…",
                "Running 2 subagents…",
            ),
            (
                "github_create_issue",
                "Called github_create_issue",
                "Called github_create_issue twice",
                "Calling github_create_issue…",
                "Calling github_create_issue twice…",
            ),
        ];
        for (name, one, two, now_one, now_two) in cases {
            assert_eq!(label(&[*name], true), *one);
            assert_eq!(label(&[*name, *name], true), *two);
            assert_eq!(label(&[*name], false), *now_one);
            assert_eq!(label(&[*name, *name], false), *now_two);
        }
        assert_eq!(label(&["x_tool"; 3], true), "Called x_tool 3 times");
    }

    #[test]
    fn aliases_and_kinds_share_a_count() {
        assert_eq!(label(&["bash", "shell"], true), "Ran 2 commands");
        assert_eq!(label(&["grep", "glob", "list"], true), "Ran 3 searches");
        assert_eq!(
            label(&["edit", "write", "patch", "apply_patch"], true),
            "Edited 4 files"
        );
        assert_eq!(label(&["webfetch", "websearch"], true), "Fetched 2 pages");
        assert_eq!(label(&["task", "subagent"], true), "Ran 2 subagents");
        assert_eq!(label(&["Read"], true), "Read a file");
    }

    #[test]
    fn counts_follow_the_order_kinds_first_appeared() {
        assert_eq!(
            label(&["shell", "read", "shell", "edit", "edit", "edit"], true),
            "Ran 2 commands, read a file, edited 3 files"
        );
        assert_eq!(
            label(&["edit", "shell", "read"], true),
            "Edited a file, ran a command, read a file"
        );
        assert_eq!(
            label(&["read", "grep", "read"], false),
            "Reading 2 files, running a search…"
        );
        assert_eq!(
            label(
                &[
                    "linear_search",
                    "shell",
                    "github_create_issue",
                    "linear_search"
                ],
                true
            ),
            "Called linear_search twice, ran a command, called github_create_issue"
        );
    }

    #[test]
    fn the_preview_turns_wording() {
        // The scripted turn of the approved preview.
        assert_eq!(label(&["shell", "shell"], true), "Ran 2 commands");
        assert_eq!(
            label(&["shell", "read", "grep"], true),
            "Ran a command, read a file, ran a search"
        );
        assert_eq!(
            label(
                &[
                    "edit",
                    "edit",
                    "webfetch",
                    "subagent",
                    "github_create_issue"
                ],
                true
            ),
            "Edited 2 files, fetched a page, ran a subagent, called github_create_issue"
        );
    }

    #[test]
    fn no_calls_is_no_label() {
        assert_eq!(label::<&str>(&[], true), "");
        assert_eq!(label::<&str>(&[], false), "");
    }

    #[test]
    fn a_name_starting_with_a_non_ascii_letter_is_capitalised_whole() {
        assert_eq!(label(&["ünicode_tool"], true), "Called ünicode_tool");
        // Only the first word's first letter: "Called", not the name.
        assert!(label(&["éclair"], true).starts_with("Called éclair"));
    }

    #[test]
    fn wording_is_bounded_by_its_parts() {
        // Bounded property: any mix of up to 8 calls of 9 names gives one
        // part per distinct count kind, in first-seen order, ending in "…"
        // only while running.
        let names = [
            "shell", "read", "grep", "edit", "webfetch", "subagent", "mcp_a", "mcp_b", "glob",
        ];
        let mut seed: u32 = 7;
        for _ in 0..500 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let n = (seed >> 16) as usize % 8 + 1;
            let mut calls = Vec::new();
            for _ in 0..n {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                calls.push(names[(seed >> 16) as usize % names.len()]);
            }
            let mut kinds: Vec<Count> = Vec::new();
            for c in &calls {
                let k = count_of(c);
                if !kinds.contains(&k) {
                    kinds.push(k);
                }
            }
            for done in [true, false] {
                let l = label(&calls, done);
                assert_eq!(l.split(", ").count(), kinds.len(), "{calls:?} {l}");
                assert_eq!(l.ends_with('…'), !done);
                assert!(l.chars().next().unwrap().is_uppercase(), "{l}");
            }
        }
    }

    // ----- glyphs -----

    fn all_glyphs() -> Vec<Glyph> {
        let mut out: Vec<Glyph> = [
            "read",
            "grep",
            "glob",
            "list",
            "edit",
            "write",
            "patch",
            "shell",
            "webfetch",
            "websearch",
            "subagent",
        ]
        .iter()
        .map(|n| Glyph::of(n))
        .collect();
        for i in 0..200 {
            out.push(Glyph::of(&format!("server_{i}_tool")));
        }
        out
    }

    #[test]
    fn every_frame_and_settled_glyph_is_one_column() {
        for g in all_glyphs() {
            for c in g.frames.iter().chain([&g.rest]) {
                assert_eq!(c.width(), Some(1), "{c:?} of {:?}", g.kind);
            }
        }
        for c in ['✓', '✗'] {
            assert_eq!(c.width(), Some(1));
        }
    }

    #[test]
    fn each_kind_has_its_sequence() {
        let seq = |n: &str| Glyph::of(n).frames.iter().collect::<String>();
        let rest = |n: &str| Glyph::of(n).rest;
        for n in ["read", "grep", "glob", "list"] {
            assert_eq!(seq(n), "⡀⠄⠂⠁⠈⠐⠠⢀");
            assert_eq!(rest(n), '⠶');
            assert_eq!(kind(n), Kind::Looking);
        }
        for n in ["edit", "write", "patch", "apply_patch"] {
            assert_eq!(seq(n), "▁▂▃▅▇█✶▂");
            assert_eq!(rest(n), '▆');
        }
        for n in ["shell", "bash"] {
            assert_eq!(seq(n), "▖▘▝▗");
            assert_eq!(rest(n), '▞');
        }
        for n in ["webfetch", "websearch"] {
            assert_eq!(seq(n), "◜◝◞◟");
            assert_eq!(rest(n), '◯');
        }
        for n in ["subagent", "task"] {
            assert_eq!(seq(n), "⣾⣽⣻⢿⡿⣟⣯⣷");
            assert_eq!(rest(n), '⣿');
        }
        assert_eq!(kind("todowrite"), Kind::Other);
        assert_eq!(kind("github_create_issue"), Kind::Other);
    }

    #[test]
    fn frames_step_with_the_tick_and_wrap() {
        let g = Glyph::of("shell");
        let got: String = (0..9).map(|t| g.frame(t)).collect();
        assert_eq!(got, "▖▘▝▗▖▘▝▗▖");
        assert_eq!(g.frame(u64::MAX), g.frames[(u64::MAX % 4) as usize]);
    }

    #[test]
    fn a_running_glyph_glows_through_the_heat_colours() {
        use super::super::view::heat_color;
        let g = Glyph::of("read");
        let colors: Vec<_> = (0..4).map(|t| running(&g, t, &DARK).style.fg).collect();
        assert_eq!(
            colors,
            [
                rgb(heat_color(0.0, false)),
                rgb(heat_color(0.35, false)),
                rgb(heat_color(0.7, false)),
                rgb(heat_color(0.35, false)),
            ]
        );
        // Cherry red, orange, amber.
        assert_eq!(colors[0], Some(Color::Rgb(0x8b, 0x1a, 0x1a)));
        assert_eq!(colors[1], Some(Color::Rgb(0xd9, 0x48, 0x0f)));
        assert_eq!(colors[2], Some(Color::Rgb(0xff, 0xb0, 0x2e)));
    }

    #[test]
    fn a_light_theme_stops_at_deep_amber() {
        let light = Look {
            forge: ForgeLook {
                light: true,
                ..DARK.forge
            },
            ..DARK
        };
        let g = Glyph::of("read");
        let colors: Vec<_> = (0..4).map(|t| running(&g, t, &light).style.fg).collect();
        assert_eq!(colors[0], Some(Color::Rgb(0x7a, 0x12, 0x12)));
        assert_eq!(colors[1], Some(Color::Rgb(0xb8, 0x36, 0x08)));
        assert_eq!(colors[2], Some(Color::Rgb(0xc7, 0x6a, 0x00)));
    }

    #[test]
    fn the_spark_flashes_in_the_hottest_colour() {
        let g = Glyph::of("edit");
        let spark = running(&g, SPARK as u64, &DARK);
        assert_eq!(spark.content, "✶");
        assert_eq!(spark.style.fg, DARK.heat(2).fg);
        assert_ne!(running(&g, 2, &DARK).content, "✶");
        assert!(!Glyph::of("shell").sparks(SPARK as u64));
    }

    #[test]
    fn a_subagent_glows_in_the_agent_colour() {
        let g = Glyph::of("subagent");
        for t in 0..8 {
            assert_eq!(running(&g, t, &DARK).style.fg, Some(DARK.agent));
        }
    }

    #[test]
    fn settled_glyphs_are_green_or_red() {
        for n in ["read", "edit", "shell", "webfetch", "subagent", "mcp_tool"] {
            let g = Glyph::of(n);
            let ok = settled(&g, false, &DARK);
            let bad = settled(&g, true, &DARK);
            assert_eq!(ok.content, g.rest.to_string());
            assert_eq!(ok.style.fg, Some(DARK.success));
            assert_eq!(bad.style.fg, Some(DARK.error));
        }
        assert_eq!(mark(false, &DARK).content, "✓");
        assert_eq!(mark(true, &DARK).content, "✗");
        assert_eq!(mark(true, &DARK).style.fg, Some(DARK.error));
    }

    #[test]
    fn no_color_keeps_the_motion_in_the_muted_colour() {
        let plain = Look {
            forge: ForgeLook {
                color: false,
                ..DARK.forge
            },
            ..DARK
        };
        for n in ["read", "edit", "subagent", "mcp_tool"] {
            let g = Glyph::of(n);
            let frames: Vec<String> = (0..8)
                .map(|t| running(&g, t, &plain).content.to_string())
                .collect();
            // Still animating.
            assert!(
                frames
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    > 1
            );
            for t in 0..8 {
                assert_eq!(running(&g, t, &plain).style.fg, Some(plain.forge.muted));
            }
            assert_eq!(settled(&g, true, &plain).style.fg, Some(plain.forge.muted));
            assert_eq!(settled(&g, false, &plain).style.fg, Some(plain.forge.muted));
        }
    }

    // ----- generated sprites -----

    #[test]
    fn a_sprite_is_stable_for_a_name() {
        for n in ["github_create_issue", "slack_send_message", "", "x"] {
            assert_eq!(sprite(n), sprite(n));
        }
        // An empty name is drawn as a space's.
        assert_eq!(sprite(""), sprite(" "));
    }

    #[test]
    fn sprites_match_the_preview() {
        // From the approved preview's generator, run on the same names.
        assert_eq!(hash(""), 2_166_136_261);
        assert_eq!(hash("a"), 0xe40c_292c);
        for (name, frames, rest) in [
            ("github_create_issue", "⡴⣘⢩⠳⠞⡍⣃⢦", '⡴'),
            ("linear_search_issues", "⡲⢥⣊⡕⠮⢓⡩⢜", '⡶'),
            ("slack_send_message", "⢠⣀⡄⠆⠃⠉⠘⠰", '⢤'),
            ("postgres_query", "⢘⡰⢤⣂⡅⠎⠓⠩", '⢼'),
            ("figma_get_frame", "⣒⡥⢎⡓⠭⢚⡱⢬", '⣶'),
            ("my_plugin_tool", "⠃⠆⡄⣀⢠⠰⠘⠉", '⠧'),
            ("", "⡰⢤⣂⡅⠎⠓⠩⢘", '⡴'),
            ("ünicode", "⣂⢤⡰⢘⠩⠓⠎⡅", '⣦'),
        ] {
            let s = sprite(name);
            assert_eq!(s.frames.iter().collect::<String>(), frames, "{name:?}");
            assert_eq!(s.rest, rest, "{name:?}");
            let g = Glyph::of(name);
            assert_eq!(g.frames.iter().collect::<String>(), frames);
        }
    }

    fn bits(c: char) -> u8 {
        (c as u32 - BRAILLE) as u8
    }

    #[test]
    fn sprites_are_braille_and_each_frame_rotates_the_resting_dots() {
        // Bounded property over many names.
        for i in 0..2000 {
            let name = format!("tool_{i}_{}", i * 7919 % 1013);
            let s = sprite(&name);
            let picks: Vec<u8> = s.picks.iter().flatten().copied().collect();
            assert!((2..=4).contains(&picks.len()), "{name}");
            let mut uniq = picks.clone();
            uniq.sort();
            uniq.dedup();
            assert_eq!(uniq.len(), picks.len(), "{name}: repeated dots");
            assert!(s.dir == 1 || s.dir == -1);
            for c in s.frames.iter().chain([&s.rest]) {
                assert!(('\u{2800}'..='\u{28ff}').contains(c), "{name}: {c:?}");
                assert_eq!(c.width(), Some(1));
            }
            let rest = picks.iter().fold(0, |b, &p| b | RING[p as usize]);
            assert_eq!(bits(s.rest), rest | CENTRE, "{name}");
            assert_eq!(bits(s.frames[0]), rest, "{name}");
            for (f, c) in s.frames.iter().enumerate() {
                let want = picks.iter().fold(0, |b, &p| {
                    b | RING[(p as i32 + s.dir as i32 * f as i32).rem_euclid(8) as usize]
                });
                assert_eq!(bits(*c), want, "{name} frame {f}");
                assert_eq!(bits(*c).count_ones() as usize, picks.len());
            }
        }
    }

    #[test]
    fn different_names_look_different() {
        // Bounded property: 154 dot sets times 2 directions give 308
        // sprites, so many sampled names spread widely over them, and the
        // preview's sample names are told apart.
        let samples = [
            "github_create_issue",
            "linear_search_issues",
            "slack_send_message",
            "postgres_query",
            "figma_get_frame",
            "my_plugin_tool",
        ];
        let got: std::collections::HashSet<_> = samples.iter().map(|n| sprite(n).frames).collect();
        assert_eq!(got.len(), samples.len());
        let names: Vec<String> = (0..300).map(|i| format!("mcp_server_{i}_action")).collect();
        let distinct: std::collections::HashSet<_> =
            names.iter().map(|n| sprite(n).frames).collect();
        assert!(distinct.len() >= 150, "{} distinct of 300", distinct.len());
        // Names one letter apart differ too.
        let near: std::collections::HashSet<_> = ('a'..='z')
            .map(|c| sprite(&format!("tool_{c}")).frames)
            .collect();
        assert!(near.len() >= 18, "{}", near.len());
    }
}
