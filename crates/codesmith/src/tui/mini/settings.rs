//! `tui.mini`: what mini mode writes to the scrollback and shows in its
//! footer, as OpenCode's `mini` settings (`thinking`, `tools`,
//! `shell_output`, `turn_summary`, `footer`, `splash`, `work_spinner`,
//! `mono`, `replay`, `replay_limit`), and its three verbosity presets.

use serde_json::Value;

/// The footer's work spinner (`tui.mini.work_spinner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Spinner {
    #[default]
    BlockSoftSlide,
    BlockSoftSweep,
    BlockLowComet,
    BlockLowDuet,
    BlockShuttle,
    BlockBridge,
    BlockSqueeze,
    SmallToggle,
    SquareToggle,
    GrowShrink,
    QuadrantOrbit,
    Crosshatch,
    DensityWave,
    Seed,
}

impl Spinner {
    pub const ALL: [(Spinner, &'static str); 14] = [
        (Spinner::BlockSoftSlide, "block-soft-slide"),
        (Spinner::BlockSoftSweep, "block-soft-sweep"),
        (Spinner::BlockLowComet, "block-low-comet"),
        (Spinner::BlockLowDuet, "block-low-duet"),
        (Spinner::BlockShuttle, "block-shuttle"),
        (Spinner::BlockBridge, "block-bridge"),
        (Spinner::BlockSqueeze, "block-squeeze"),
        (Spinner::SmallToggle, "small-toggle"),
        (Spinner::SquareToggle, "square-toggle"),
        (Spinner::GrowShrink, "grow-shrink"),
        (Spinner::QuadrantOrbit, "quadrant-orbit"),
        (Spinner::Crosshatch, "crosshatch"),
        (Spinner::DensityWave, "density-wave"),
        (Spinner::Seed, "seed"),
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().find(|(_, n)| *n == s).map(|(k, _)| *k)
    }

    /// Its frames and how long each shows, in milliseconds. OpenCode draws
    /// most of these with octant glyphs (Unicode 16), which few terminal
    /// fonts have yet; these use block and geometric characters that
    /// common fonts do.
    pub fn frames(self, mono: bool) -> (&'static [&'static str], u64) {
        if mono {
            return (&["-", "\\", "|", "/"], 120);
        }
        match self {
            Spinner::BlockSoftSlide => (&["▖", "▖", "▌", "▘", "▘", "▝", "▝", "▐", "▗", "▗"], 100),
            Spinner::BlockSoftSweep => (&["▁", "▂", "▃", "▄", "▅", "▆", "▅", "▄", "▃", "▂"], 100),
            Spinner::BlockLowComet => (&["▗", "▄", "▖", "▌", "▘", "▀", "▝", "▐"], 80),
            Spinner::BlockLowDuet => (&["▚", "▞"], 200),
            Spinner::BlockShuttle => (
                &["▏", "▎", "▍", "▌", "▋", "▊", "▉", "▊", "▋", "▌", "▍", "▎"],
                70,
            ),
            Spinner::BlockBridge => (&["▁", "▃", "▅", "▇", "█", "▇", "▅", "▃"], 100),
            Spinner::BlockSqueeze => (&["█", "▉", "▊", "▋", "▌", "▋", "▊", "▉"], 100),
            Spinner::SmallToggle => (&["▪", "▫"], 400),
            Spinner::SquareToggle => (&["■", "□"], 400),
            Spinner::GrowShrink => (&["·", "▪", "■", "▪"], 150),
            Spinner::QuadrantOrbit => (&["▖", "▘", "▝", "▗"], 120),
            Spinner::Crosshatch => (&["░", "▒", "▓", "▒"], 150),
            Spinner::DensityWave => (&["░", "▒", "▓", "█", "▓", "▒"], 120),
            Spinner::Seed => (&["▪"], 1000),
        }
    }

    /// The glyph `elapsed` milliseconds into the animation.
    pub fn frame(self, mono: bool, elapsed: u64) -> &'static str {
        let (frames, every) = self.frames(mono);
        frames[(elapsed / every) as usize % frames.len()]
    }
}

/// Mini mode's settings, OpenCode's defaults where unset.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Reasoning in the scrollback.
    pub thinking: bool,
    /// Tool calls in the scrollback.
    pub tools: bool,
    /// Shell commands' output under their rows (when tools are shown).
    pub shell_output: bool,
    /// The agent, model and duration line after each turn.
    pub turn_summary: bool,
    /// The model, agent, usage and context details in the footer.
    pub footer: bool,
    /// The entry and exit banners.
    pub splash: bool,
    pub work_spinner: Spinner,
    /// Monochrome ASCII output.
    pub mono: bool,
    /// Print the session's history on resume and after a resize.
    pub replay: bool,
    /// The newest messages a replay prints.
    pub replay_limit: usize,
}

/// The newest messages a replay prints unless `tui.mini.replay_limit` says.
pub const REPLAY_LIMIT: usize = 200;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            thinking: true,
            tools: false,
            shell_output: false,
            turn_summary: true,
            footer: true,
            splash: true,
            work_spinner: Spinner::default(),
            mono: false,
            replay: true,
            replay_limit: REPLAY_LIMIT,
        }
    }
}

/// A verbosity preset: the five knobs it sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Quiet,
    Default,
    Everything,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Quiet, Level::Default, Level::Everything];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.key() == s)
    }

    pub fn key(self) -> &'static str {
        match self {
            Level::Quiet => "quiet",
            Level::Default => "default",
            Level::Everything => "everything",
        }
    }

    /// `thinking`, `tools`, `shell_output`, `turn_summary`, `footer`.
    fn knobs(self) -> [bool; 5] {
        match self {
            Level::Quiet => [false; 5],
            Level::Default => [true, false, false, true, true],
            Level::Everything => [true; 5],
        }
    }
}

/// A preset's label, or `Custom` for none.
pub fn label(level: Option<Level>) -> &'static str {
    match level {
        Some(Level::Quiet) => "Quiet",
        Some(Level::Default) => "Default",
        Some(Level::Everything) => "Everything",
        None => "Custom",
    }
}

impl Settings {
    fn knobs(&self) -> [bool; 5] {
        [
            self.thinking,
            self.tools,
            self.shell_output,
            self.turn_summary,
            self.footer,
        ]
    }

    /// The settings with a preset's knobs (splash, spinner, mono and
    /// replay stay).
    pub fn with_level(&self, level: Level) -> Settings {
        let [thinking, tools, shell_output, turn_summary, footer] = level.knobs();
        Settings {
            thinking,
            tools,
            shell_output,
            turn_summary,
            footer,
            ..self.clone()
        }
    }

    /// The preset these settings match, `None` (custom) for none.
    pub fn level(&self) -> Option<Level> {
        Level::ALL.into_iter().find(|l| l.knobs() == self.knobs())
    }

    /// The preset a step up (+1) or down (-1) gives, clamped at the ends
    /// like a slider; custom settings move from the nearest preset.
    pub fn cycle(&self, dir: i32) -> Level {
        let i = match self.level() {
            Some(l) => Level::ALL.iter().position(|x| *x == l).unwrap_or(1),
            None => {
                let distance = |l: Level| {
                    l.knobs()
                        .iter()
                        .zip(self.knobs())
                        .filter(|(a, b)| **a != *b)
                        .count()
                };
                (0..3).fold(0, |best, i| {
                    if distance(Level::ALL[i]) < distance(Level::ALL[best]) {
                        i
                    } else {
                        best
                    }
                })
            }
        };
        Level::ALL[(i as i32 + dir).clamp(0, 2) as usize]
    }

    /// The preset to switch to a step up or down, or `None` when that is
    /// where the settings already are.
    pub fn step(&self, dir: i32) -> Option<Level> {
        let to = self.cycle(dir);
        (self.level() != Some(to)).then_some(to)
    }
}

fn get<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |cur, k| cur.get(*k))
}

/// `tui.mini` (`tui` is the whole `tui` object), with what was wrong in
/// it: a value that does not fit is reported and its default kept.
pub fn load(tui: &Value) -> (Settings, Vec<String>) {
    let mut s = Settings::default();
    let mut errors = Vec::new();
    let Some(m) = get(tui, &["mini"]) else {
        return (s, errors);
    };
    if !m.is_object() {
        errors.push("tui.mini: must be an object".to_string());
        return (s, errors);
    }
    for (key, slot) in [
        ("thinking", &mut s.thinking),
        ("tools", &mut s.tools),
        ("shell_output", &mut s.shell_output),
        ("turn_summary", &mut s.turn_summary),
        ("footer", &mut s.footer),
        ("splash", &mut s.splash),
    ] {
        match m.get(key).map(Value::as_str) {
            None => {}
            Some(Some("show")) => *slot = true,
            Some(Some("hide")) => *slot = false,
            Some(_) => errors.push(format!("tui.mini.{key}: must be \"show\" or \"hide\"")),
        }
    }
    for (key, slot) in [("mono", &mut s.mono), ("replay", &mut s.replay)] {
        match m.get(key) {
            None => {}
            Some(Value::Bool(b)) => *slot = *b,
            Some(_) => errors.push(format!("tui.mini.{key}: must be true or false")),
        }
    }
    match m.get("work_spinner") {
        None => {}
        Some(v) => match v.as_str().and_then(Spinner::parse) {
            Some(k) => s.work_spinner = k,
            None => errors.push(format!(
                "tui.mini.work_spinner: must be one of {}",
                Spinner::ALL.map(|(_, n)| format!("\"{n}\"")).join(", ")
            )),
        },
    }
    match m.get("replay_limit") {
        None => {}
        Some(v) => match v.as_u64().filter(|n| *n > 0) {
            Some(n) => s.replay_limit = n as usize,
            None => errors.push("tui.mini.replay_limit: must be a whole number above 0".into()),
        },
    }
    (s, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // mini/verbosity.test.ts: "default Mini settings match the default
    // verbosity preset".
    #[test]
    fn oc_default_mini_settings_match_the_default_preset() {
        assert_eq!(Settings::default().level(), Some(Level::Default));
    }

    // "verbosity presets leave splash, spinner, and mono alone".
    #[test]
    fn oc_verbosity_presets_leave_splash_spinner_and_mono_alone() {
        let (current, errors) = load(&json!({
            "mini": {"splash": "hide", "work_spinner": "seed", "mono": true}
        }));
        assert!(errors.is_empty());
        let quiet = current.with_level(Level::Quiet);
        assert!(!quiet.splash);
        assert!(!quiet.footer);
        assert_eq!(quiet.work_spinner, Spinner::Seed);
        assert!(quiet.mono);
        assert!(current.with_level(Level::Everything).mono);
        assert_eq!(
            quiet,
            Settings {
                thinking: false,
                tools: false,
                shell_output: false,
                turn_summary: false,
                footer: false,
                ..current.clone()
            }
        );
    }

    // "individual knobs mark verbosity custom until a preset matches
    // again".
    #[test]
    fn oc_individual_knobs_mark_verbosity_custom() {
        let louder = Settings {
            tools: true,
            ..Settings::default()
        };
        assert_eq!(louder.level(), None);
        assert_eq!(label(None), "Custom");
        assert_eq!(louder.with_level(Level::Quiet).level(), Some(Level::Quiet));
    }

    // "verbosity cycles like a clamped slider".
    #[test]
    fn oc_verbosity_cycles_like_a_clamped_slider() {
        let s = Settings::default();
        assert_eq!(s.cycle(1), Level::Everything);
        assert_eq!(s.cycle(-1), Level::Quiet);
        assert_eq!(s.with_level(Level::Quiet).cycle(-1), Level::Quiet);
        assert_eq!(s.with_level(Level::Everything).cycle(1), Level::Everything);
        assert_eq!(s.with_level(Level::Quiet).step(-1), None);
        assert_eq!(s.step(1), Some(Level::Everything));
        assert_eq!(label(Some(Level::Quiet)), "Quiet");
        assert_eq!(label(Some(Level::Everything)), "Everything");
    }

    // "custom verbosity moves toward the nearest preset".
    #[test]
    fn oc_custom_verbosity_moves_toward_the_nearest_preset() {
        let custom = Settings {
            tools: true,
            ..Settings::default()
        };
        assert_eq!(custom.level(), None);
        assert_eq!(custom.cycle(1), Level::Everything);
        assert_eq!(custom.cycle(-1), Level::Quiet);
    }

    // config-v2.test.tsx: "validates mini replay and work spinner settings".
    #[test]
    fn oc_validates_mini_replay_and_work_spinner_settings() {
        let (s, e) = load(&json!({"mini": {"tools": "hide", "replay": false, "replay_limit": 50}}));
        assert!(e.is_empty(), "{e:?}");
        assert!(!s.tools && !s.replay);
        assert_eq!(s.replay_limit, 50);
        assert_eq!(load(&json!({"mini": {"tools": "quiet"}})).1.len(), 1);
        assert_eq!(load(&json!({"mini": {"replay_limit": 0}})).1.len(), 1);
        assert_eq!(load(&json!({"mini": {"replay_limit": 1.5}})).1.len(), 1);
        let (s, e) = load(&json!({"mini": {"work_spinner": "quadrant-orbit"}}));
        assert!(e.is_empty());
        assert_eq!(s.work_spinner, Spinner::QuadrantOrbit);
        let (s, e) = load(&json!({"mini": {"work_spinner": "unknown"}}));
        assert_eq!(e.len(), 1);
        assert_eq!(s.work_spinner, Spinner::BlockSoftSlide);
    }

    // runtime.boot.test.ts: "preserves shared config while resolving
    // independent Mini defaults" (adapted: the rest of `tui` loads as it
    // does for the full UI, and an unset `mini` gives OpenCode's defaults).
    #[test]
    fn oc_mini_defaults_resolve_independently_of_the_shared_config() {
        let tui = json!({"theme": {"name": "nord"}, "session": {"thinking": "hide"}});
        let (s, e) = load(&tui);
        assert!(e.is_empty());
        assert_eq!(s, Settings::default());
        assert!(
            s.thinking,
            "mini shows reasoning unless tui.mini.thinking hides it"
        );
        let (shared, _) = super::super::super::settings::load(&tui);
        assert_eq!(shared.theme.as_deref(), Some("nord"));
        assert_eq!(shared.thinking, Some(false));
    }

    #[test]
    fn every_spinner_has_frames_and_a_name() {
        for (k, name) in Spinner::ALL {
            assert_eq!(Spinner::parse(name), Some(k));
            let (frames, every) = k.frames(false);
            assert!(!frames.is_empty() && every > 0);
            assert!(frames
                .iter()
                .all(|f| unicode_width::UnicodeWidthStr::width(*f) == 1));
            assert!(k.frame(true, 500).is_ascii());
        }
    }
}
