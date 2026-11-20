//! Themes, as OpenCode has them: the built-in themes (OpenCode's own V2
//! `opencode` theme and its V1 themes, migrated as they load), theme files
//! in `themes/` folders, and `system`, built from the terminal's colors.
//! Each theme has a light and/or a dark mode; the mode follows the
//! terminal's background unless one is pinned.
//!
//! [`Themes`] is the selection (OpenCode's theme context); [`Palette`] is
//! the resolved theme as the colors the UI draws with, made once per
//! change and read by every frame through [`current`].

pub mod color;
pub mod doc;
pub mod migrate;
pub mod system;
pub mod v1;

#[cfg(test)]
mod tests;

use color::Rgba;
use doc::{Mode, Resolved};
use ratatui::style::Color;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use system::TerminalColors;

macro_rules! builtin {
    ($($name:literal => $file:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("assets/", $file)))),*]
    };
}

/// OpenCode's V1 themes (`DEFAULT_THEMES`), by name.
const V1_THEMES: &[(&str, &str)] = builtin! {
    "aura" => "aura.json",
    "ayu" => "ayu.json",
    "catppuccin" => "catppuccin.json",
    "catppuccin-frappe" => "catppuccin-frappe.json",
    "catppuccin-macchiato" => "catppuccin-macchiato.json",
    "cobalt2" => "cobalt2.json",
    "cursor" => "cursor.json",
    "dracula" => "dracula.json",
    "everforest" => "everforest.json",
    "flexoki" => "flexoki.json",
    "github" => "github.json",
    "gruvbox" => "gruvbox.json",
    "kanagawa" => "kanagawa.json",
    "material" => "material.json",
    "matrix" => "matrix.json",
    "mercury" => "mercury.json",
    "monokai" => "monokai.json",
    "nightowl" => "nightowl.json",
    "nord" => "nord.json",
    "one-dark" => "one-dark.json",
    "osaka-jade" => "osaka-jade.json",
    "orng" => "orng.json",
    "lucent-orng" => "lucent-orng.json",
    "palenight" => "palenight.json",
    "rosepine" => "rosepine.json",
    "solarized" => "solarized.json",
    "synthwave84" => "synthwave84.json",
    "tokyonight" => "tokyonight.json",
    "vesper" => "vesper.json",
    "vercel" => "vercel.json",
    "zenburn" => "zenburn.json",
    "carbonfox" => "carbonfox.json",
};

/// OpenCode's own theme, natively V2.
const OPENCODE: &str = include_str!("assets/opencode-v2.json");
/// The V1 version of it, which OpenCode keeps for its migration tests.
#[cfg(test)]
pub const OPENCODE_V1: &str = include_str!("assets/opencode.json");

/// The default theme's name.
pub const DEFAULT: &str = "opencode";

fn parse_static(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::Null)
}

/// The built-in V1 themes, parsed once.
pub fn default_themes() -> &'static BTreeMap<String, Value> {
    static T: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    T.get_or_init(|| {
        V1_THEMES
            .iter()
            .map(|(n, s)| (n.to_string(), parse_static(s)))
            .collect()
    })
}

/// OpenCode's V2 theme document.
pub fn opencode() -> &'static Value {
    static T: OnceLock<Value> = OnceLock::new();
    T.get_or_init(|| parse_static(OPENCODE))
}

/// Whether a value looks like a theme at all: a V1 `theme` or a V2 `base`.
pub fn is_theme_source(v: &Value) -> bool {
    v.is_object() && (v.get("theme").is_some() || v.get("base").is_some())
}

/// A theme file as a V2 document: V1 themes are migrated, V2 ones
/// validated (OpenCode's `parseTheme`).
pub fn parse(source: &Value, name: &str) -> Result<Value, String> {
    if v1::is_v1(source) {
        migrate::migrate(source)
    } else {
        doc::parse_document(source, name)
    }
}

/// The folders whose `themes/*.json` are read, lowest precedence first: the
/// global config folder, then each `.codesmith` folder from the project
/// root down to `cwd` (OpenCode's `configDirectories`, which walks from the
/// filesystem root; CodeSmith's config stops at the project root).
pub fn theme_dirs(global: &Path, cwd: &Path, root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![global.to_path_buf()];
    let mut project: Vec<PathBuf> = cwd
        .ancestors()
        .take_while(|d| d.starts_with(root))
        .map(|d| d.join(".codesmith"))
        .collect();
    project.reverse();
    for d in project {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

/// Read every `<dir>/themes/*.json`, later folders overriding earlier
/// ones, files in name order. A file that is not JSON is reported.
pub fn discover(dirs: &[PathBuf]) -> (BTreeMap<String, Value>, Vec<String>) {
    let mut out = BTreeMap::new();
    let mut errors = Vec::new();
    for d in dirs {
        let Ok(entries) = std::fs::read_dir(d.join("themes")) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json") && p.is_file())
            .collect();
        files.sort();
        for f in files {
            let name = f
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            match std::fs::read_to_string(&f)
                .map_err(|e| e.to_string())
                .and_then(|t| {
                    serde_json::from_str::<Value>(&codesmith_core::config::strip_jsonc(&t))
                        .map_err(|e| e.to_string())
                }) {
                Ok(v) => {
                    out.insert(name, v);
                }
                Err(e) => errors.push(format!("theme {}: {e}", f.display())),
            }
        }
    }
    (out, errors)
}

/// The mode setting: follow the terminal, or pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeSetting {
    System,
    Dark,
    Light,
}

impl ModeSetting {
    pub fn parse(s: &str) -> Option<ModeSetting> {
        match s {
            "system" => Some(ModeSetting::System),
            "dark" => Some(ModeSetting::Dark),
            "light" => Some(ModeSetting::Light),
            _ => None,
        }
    }

    pub fn pinned(self) -> Option<Mode> {
        match self {
            ModeSetting::System => None,
            ModeSetting::Dark => Some(Mode::Dark),
            ModeSetting::Light => Some(Mode::Light),
        }
    }
}

/// A theme loaded in a mode.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub name: String,
    pub modes: Vec<Mode>,
    pub mode: Mode,
    pub theme: Resolved,
}

fn load(source: &Value, name: &str, requested: Mode) -> Result<Loaded, String> {
    let document = parse(source, name)?;
    let modes = doc::modes(&document);
    let mode = if modes.contains(&requested) {
        requested
    } else {
        *modes
            .first()
            .ok_or("Theme must provide at least one mode")?
    };
    let theme = doc::resolve(&document, Some(mode))?;
    Ok(Loaded {
        name: name.to_string(),
        modes,
        mode,
        theme,
    })
}

/// The theme selection: the themes there are, the one chosen, and the mode
/// (OpenCode's theme context). The requested mode and a pinned one are
/// kept even while a theme without that mode shows its other one.
#[derive(Debug, Clone)]
pub struct Themes {
    custom: BTreeMap<String, Value>,
    system: Option<Value>,
    palette: Option<TerminalColors>,
    active: String,
    /// The mode asked for (pinned, or the terminal's).
    mode: Mode,
    lock: Option<Mode>,
    current: Loaded,
    /// Themes that failed to load, with why (shown once each).
    pub errors: Vec<(String, String)>,
}

impl Themes {
    /// `name` (the default when unset), `setting` for the mode, `detected`
    /// the terminal's mode when known; `custom` from [`discover`].
    pub fn new(
        name: Option<&str>,
        setting: Option<ModeSetting>,
        detected: Option<Mode>,
        custom: BTreeMap<String, Value>,
    ) -> Themes {
        let lock = setting.and_then(ModeSetting::pinned);
        let mode = lock.or(detected).unwrap_or(Mode::Dark);
        let custom = custom
            .into_iter()
            .filter(|(_, v)| is_theme_source(v))
            .collect();
        let mut t = Themes {
            custom,
            system: None,
            palette: None,
            active: name.unwrap_or(DEFAULT).to_string(),
            mode,
            lock,
            current: load(opencode(), DEFAULT, mode).expect("the built-in theme loads"),
            errors: Vec::new(),
        };
        t.reload();
        t
    }

    /// Every theme by name: the built-ins, then custom files (which may
    /// replace them), then `system`.
    pub fn all(&self) -> BTreeMap<String, Value> {
        let mut out = default_themes().clone();
        out.insert(DEFAULT.into(), opencode().clone());
        for (k, v) in &self.custom {
            out.insert(k.clone(), v.clone());
        }
        let system = self
            .system
            .clone()
            .or_else(|| out.get("system").cloned())
            .unwrap_or_else(|| opencode().clone());
        out.insert("system".into(), system);
        out
    }

    /// The theme names, sorted as the picker lists them.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = default_themes()
            .keys()
            .chain(self.custom.keys())
            .cloned()
            .chain([DEFAULT.to_string(), "system".to_string()])
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        names
    }

    pub fn has(&self, name: &str) -> bool {
        matches!(name, DEFAULT | "system")
            || self.custom.contains_key(name)
            || default_themes().contains_key(name)
    }

    fn source(&self, name: &str) -> Option<Value> {
        if name == "system" {
            if let Some(s) = &self.system {
                return Some(s.clone());
            }
        }
        if let Some(v) = self.custom.get(name) {
            return Some(v.clone());
        }
        match name {
            DEFAULT | "system" => Some(opencode().clone()),
            n => default_themes().get(n).cloned(),
        }
    }

    /// Load the active theme in the requested mode, falling back to the
    /// default (and noting why) when it is missing or does not load.
    fn reload(&mut self) {
        let name = if self.has(&self.active) {
            self.active.clone()
        } else {
            DEFAULT.to_string()
        };
        let loaded = self
            .source(&name)
            .ok_or_else(|| "missing".to_string())
            .and_then(|s| load(&s, &name, self.mode));
        self.current = match loaded {
            Ok(l) => l,
            Err(e) => {
                if !self.errors.iter().any(|(n, _)| *n == name) {
                    self.errors.push((name.clone(), e));
                }
                self.active = DEFAULT.into();
                load(opencode(), DEFAULT, self.mode).expect("the built-in theme loads")
            }
        };
    }

    /// The selected theme's name (what was chosen, even when a fallback shows).
    pub fn selected(&self) -> &str {
        &self.active
    }

    pub fn current(&self) -> &Loaded {
        &self.current
    }

    /// The mode the theme shows in.
    pub fn mode(&self) -> Mode {
        self.current.mode
    }

    /// The modes the theme has.
    pub fn modes(&self) -> &[Mode] {
        &self.current.modes
    }

    pub fn supports(&self, mode: Mode) -> bool {
        self.current.modes.contains(&mode)
    }

    pub fn locked(&self) -> bool {
        self.lock.is_some()
    }

    /// Choose a theme; false when there is none by that name.
    pub fn set(&mut self, name: &str) -> bool {
        if !self.has(name) {
            return false;
        }
        self.active = name.to_string();
        self.reload();
        true
    }

    /// Pin a mode the theme has; false when it has not.
    pub fn set_mode(&mut self, mode: Mode) -> bool {
        if !self.supports(mode) {
            return false;
        }
        self.lock = Some(mode);
        self.mode = mode;
        self.reload();
        true
    }

    /// Pin the mode showing now.
    pub fn lock(&mut self) {
        let m = self.mode();
        self.lock = Some(m);
        self.mode = m;
    }

    /// Follow the terminal again (`detected`, when it is known).
    pub fn unlock(&mut self, detected: Option<Mode>) {
        self.lock = None;
        if let Some(m) = detected {
            self.mode = m;
        }
        self.reload();
    }

    /// The terminal reported its colors: they become the `system` theme,
    /// and its background sets the mode unless one is pinned.
    pub fn set_palette(&mut self, colors: TerminalColors) {
        if !colors.usable() {
            return;
        }
        if self.lock.is_none() {
            if let Some(m) = system::terminal_mode(&colors) {
                self.mode = m;
            }
        }
        self.system = Some(system::generate(&colors, self.mode));
        self.palette = Some(colors);
        self.reload();
    }

    /// Replace the custom themes (their files were read again).
    pub fn set_custom(&mut self, custom: BTreeMap<String, Value>) {
        self.custom = custom
            .into_iter()
            .filter(|(_, v)| is_theme_source(v))
            .collect();
        self.reload();
    }

    /// The UI colors of the current theme.
    pub fn palette(&self) -> Palette {
        Palette::new(&self.current.theme, self.mode())
    }
}

// ----- the UI's colors ------------------------------------------------------

static EPOCH: AtomicU64 = AtomicU64::new(1);

/// The colors the UI draws with, from a resolved theme.
#[derive(Debug, Clone)]
pub struct Palette {
    /// Different for every palette made: caches of drawn text key on it.
    pub epoch: u64,
    pub mode: Mode,
    /// The page; `Reset` (the terminal's own) when the theme's is clear.
    pub bg: Color,
    pub panel: Color,
    pub element: Color,
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub primary: Color,
    pub accent: Color,
    /// The first categorical color: agent names.
    pub agent: Color,
    pub error: Color,
    pub warning: Color,
    pub success: Color,
    pub info: Color,
    pub border: Color,
    /// A focused list row.
    pub selected_bg: Color,
    pub selected_fg: Color,
    /// Dialogs.
    pub dialog_bg: Color,
    pub dialog_text: Color,
    pub dialog_muted: Color,
    pub diff_added: Color,
    pub diff_removed: Color,
    pub diff_context: Color,
    pub diff_hunk: Color,
    pub diff_added_bg: Color,
    pub diff_removed_bg: Color,
    pub diff_line_number: Color,
    /// In [`doc::MARKDOWN`] order.
    pub markdown: [Color; 14],
    /// In [`doc::SYNTAX`] order.
    pub syntax: [Color; 9],
    syntect: Arc<OnceLock<syntect::highlighting::Theme>>,
}

/// A theme color as a terminal color: clear is the terminal's own, and a
/// translucent one is mixed over `under`.
fn term(c: Rgba, under: Option<Rgba>) -> Color {
    match c.a {
        0 => Color::Reset,
        255 => Color::Rgb(c.r, c.g, c.b),
        a => match under {
            Some(u) => {
                let m = color::tint(u, Rgba::rgb(c.r, c.g, c.b), a as f64 / 255.0);
                Color::Rgb(m.r, m.g, m.b)
            }
            None if a < 128 => Color::Reset,
            None => Color::Rgb(c.r, c.g, c.b),
        },
    }
}

impl Palette {
    pub fn new(t: &Resolved, mode: Mode) -> Palette {
        let page = t.rgba(t.background.base);
        let under = (page.a == 255).then_some(page);
        let c = |id: doc::C| term(t.rgba(id), under);
        let d = t.surface("dialog");
        let dc = |id: doc::C| term(d.rgba(id), under);
        let cat = t.categorical();
        Palette {
            epoch: EPOCH.fetch_add(1, Ordering::Relaxed),
            mode,
            bg: c(t.background.base),
            panel: c(t.background.raised.base),
            element: c(t.background.raised.high),
            text: c(t.text.base),
            muted: c(t.text.muted),
            faint: c(t.text.faint),
            primary: c(t.step("interactive", 200)),
            accent: c(t.step("accent", 200)),
            agent: cat.first().map(|h| c(h[1])).unwrap_or(Color::Reset),
            error: c(t.text.feedback[0].base),
            warning: c(t.text.feedback[1].base),
            success: c(t.text.feedback[2].base),
            info: c(t.text.feedback[3].base),
            border: c(t.border),
            selected_bg: dc(d.background.action[0].focused),
            selected_fg: dc(d.text.action[0].focused),
            dialog_bg: dc(d.background.base),
            dialog_text: dc(d.text.base),
            dialog_muted: dc(d.text.muted),
            diff_added: c(t.diff.added),
            diff_removed: c(t.diff.removed),
            diff_context: c(t.diff.context),
            diff_hunk: c(t.diff.hunk_header),
            diff_added_bg: c(t.diff.added_bg),
            diff_removed_bg: c(t.diff.removed_bg),
            diff_line_number: c(t.diff.line_number),
            markdown: t.markdown.map(c),
            syntax: t.syntax.map(c),
            syntect: Arc::new(OnceLock::new()),
        }
    }

    pub fn md(&self, token: &str) -> Color {
        self.markdown[doc::MARKDOWN.iter().position(|x| *x == token).unwrap_or(0)]
    }

    pub fn syn(&self, token: &str) -> Color {
        self.syntax[doc::SYNTAX.iter().position(|x| *x == token).unwrap_or(0)]
    }

    /// The syntect theme code blocks are highlighted with: the theme's
    /// syntax colors on TextMate scopes, as OpenCode maps them onto
    /// tree-sitter captures (`generateSyntax`).
    pub fn syntect(&self) -> &syntect::highlighting::Theme {
        self.syntect.get_or_init(|| syntect_theme(self))
    }
}

fn scolor(c: Color) -> Option<syntect::highlighting::Color> {
    match c {
        Color::Rgb(r, g, b) => Some(syntect::highlighting::Color { r, g, b, a: 255 }),
        _ => None,
    }
}

fn syntect_theme(p: &Palette) -> syntect::highlighting::Theme {
    use std::str::FromStr;
    use syntect::highlighting::{
        FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
    };
    let italic = Some(FontStyle::ITALIC);
    let bold = Some(FontStyle::BOLD);
    let rules: Vec<(&str, Color, Option<FontStyle>)> = vec![
        ("comment, punctuation.definition.comment", p.syn("comment"), italic),
        ("string, constant.character, string.quoted", p.syn("string"), None),
        ("constant.numeric, constant.language, constant.other, constant", p.syn("number"), None),
        ("keyword.control, keyword.other, keyword.declaration", p.syn("keyword"), italic),
        ("keyword, storage.type, storage.modifier", p.syn("keyword"), italic),
        ("keyword.operator, punctuation.accessor, punctuation.separator.key-value", p.syn("operator"), None),
        ("entity.name.function, support.function, meta.function-call entity.name, variable.function", p.syn("function"), None),
        ("variable, variable.parameter, variable.other, meta.property-name, support.variable", p.syn("variable"), None),
        ("entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, entity.name.namespace, support.type, support.class, storage.type.primitive", p.syn("type"), None),
        ("punctuation, meta.brace, meta.delimiter", p.syn("punctuation"), None),
        ("variable.language, support.constant, constant.language.boolean", p.error, None),
        ("entity.name.tag", p.error, None),
        ("entity.other.attribute-name, string.regexp, constant.character.escape", p.syn("keyword"), None),
        ("markup.heading, entity.name.section", p.md("heading"), bold),
        ("markup.bold", p.md("strong"), bold),
        ("markup.italic", p.md("emphasis"), italic),
        ("markup.list", p.md("listItem"), None),
        ("markup.quote", p.md("blockQuote"), italic),
        ("markup.raw", p.md("code"), None),
        ("markup.underline.link, string.other.link", p.md("link"), Some(FontStyle::UNDERLINE)),
        ("markup.inserted", p.diff_added, None),
        ("markup.deleted", p.diff_removed, None),
        ("meta.diff.range, meta.diff.header", p.diff_hunk, None),
        ("invalid", p.error, bold),
    ];
    Theme {
        name: Some("codesmith".into()),
        author: None,
        settings: ThemeSettings {
            foreground: scolor(p.md("codeBlock")).or(scolor(p.text)),
            ..Default::default()
        },
        scopes: rules
            .into_iter()
            .filter_map(|(sel, c, font)| {
                Some(ThemeItem {
                    scope: ScopeSelectors::from_str(sel).ok()?,
                    style: StyleModifier {
                        foreground: scolor(c),
                        background: None,
                        font_style: font,
                    },
                })
            })
            .collect(),
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Palette>>> = const { RefCell::new(None) };
}

/// The palette the UI draws with (the default theme's dark mode until one
/// is installed).
pub fn current() -> Rc<Palette> {
    CURRENT.with(|c| {
        c.borrow_mut()
            .get_or_insert_with(|| {
                Rc::new(Themes::new(None, Some(ModeSetting::Dark), None, BTreeMap::new()).palette())
            })
            .clone()
    })
}

/// Draw with `p` from now on (on this thread).
pub fn install(p: Palette) {
    CURRENT.with(|c| *c.borrow_mut() = Some(Rc::new(p)));
}

/// Where the theme chosen in the UI is remembered.
pub fn state_file() -> PathBuf {
    codesmith_core::paths::data_dir().join("tui-state.json")
}

/// The theme and mode chosen in the UI last, unless the configured theme
/// changed since (then the configuration wins again).
pub fn remembered(configured: Option<&str>) -> (Option<String>, Option<ModeSetting>) {
    let Ok(text) = std::fs::read_to_string(state_file()) else {
        return (None, None);
    };
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if v.get("configured").and_then(Value::as_str) != configured {
        return (None, None);
    }
    (
        v.get("theme").and_then(Value::as_str).map(String::from),
        v.get("mode")
            .and_then(Value::as_str)
            .and_then(ModeSetting::parse),
    )
}

/// Remember the theme and mode chosen in the UI.
pub fn remember(configured: Option<&str>, theme: &str, mode: Option<Mode>) {
    let v = serde_json::json!({
        "configured": configured,
        "theme": theme,
        "mode": mode.map(Mode::key).unwrap_or("system"),
    });
    let _ = codesmith_core::paths::write_private(&state_file(), v.to_string().as_bytes());
}
