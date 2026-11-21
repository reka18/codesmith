//! Ports of the Bash and Zsh scanner tables in PARITY.md's porting backlog
//! (`packages/core/test/shell-scan/bash-syntax.test.ts` and
//! `compound-parity.test.ts`, MIT). CodeSmith's scanner reads one grammar
//! for every POSIX shell, so a row OpenCode checks once per dialect is
//! checked once here and counts for both.

use codesmith_core::tool::shell_scan;
use std::path::Path;
use std::process::Command;

/// `bash-syntax.test.ts`'s fixtures: a source and the first word of each
/// command it runs, in order.
const FIXTURES: &[(&str, &[&str])] = &[
    (
        "if true; then VALUE=$(scan_probe); fi",
        &["true", "scan_probe"],
    ),
    ("if VALUE=$(scan_probe); then :; fi", &["scan_probe", ":"]),
    (
        "if true; then if false; then :; else scan_probe; fi; fi",
        &["true", "false", ":", "scan_probe"],
    ),
    (
        "for value in one two; do if true; then scan_probe; fi; done",
        &["true", "scan_probe"],
    ),
    (
        "for value in $(scan_probe); do :; done",
        &["scan_probe", ":"],
    ),
    (
        "for ((i=0; i<1; i++)); do scan_probe; done",
        &["scan_probe"],
    ),
    (
        "while false; do scan_probe; done; scan_after",
        &["false", "scan_probe", "scan_after"],
    ),
    (
        "until true; do scan_probe; done; scan_after",
        &["true", "scan_probe", "scan_after"],
    ),
    (
        "case value in value|other) scan_probe;; *) scan_after;; esac",
        &["scan_probe", "scan_after"],
    ),
    (
        "case value in (value) case x in x) scan_probe;; esac;; esac",
        &["scan_probe"],
    ),
    ("case if in if) scan_probe;; esac", &["scan_probe"]),
    (
        "case $(scan_probe) in value) :;; esac",
        &["scan_probe", ":"],
    ),
    (
        "f() { case value in value) scan_probe;; esac; }; f",
        &["scan_probe", "f"],
    ),
    ("function f { scan_probe; }; f", &["scan_probe", "f"]),
    ("f() (scan_probe); f", &["scan_probe", "f"]),
    (
        "printf ok; { scan_probe; } | scan_after",
        &["printf", "scan_probe", "scan_after"],
    ),
    (
        "(scan_probe # ) ignored\nscan_after)",
        &["scan_probe", "scan_after"],
    ),
    (
        "{ scan_probe; # } ignored\nscan_after; }",
        &["scan_probe", "scan_after"],
    ),
    (
        "printf '%s' \"$(case value in value) scan_probe;; esac)\"",
        &["printf", "scan_probe"],
    ),
    (
        "printf '%s' \"$(for value in one; do scan_probe; done)\"",
        &["printf", "scan_probe"],
    ),
    (
        "printf '%s' \"$(printf %s case in; scan_probe)\"",
        &["printf", "printf", "scan_probe"],
    ),
    (
        "cat <<EOF\nscan_ignored; $(scan_probe)\nEOF",
        &["cat", "scan_probe"],
    ),
    ("cat <<'EOF'\nscan_ignored; $(scan_ignored)\nEOF", &["cat"]),
    ("cat <<\\EOF\n$(scan_ignored)\nEOF", &["cat"]),
    ("cat <<E'O'F\n$(scan_ignored)\nEOF", &["cat"]),
    ("cat <<-EOF\n\t$(scan_probe)\n\tEOF", &["cat", "scan_probe"]),
    ("cat <<EOF\n'$(scan_probe)'\nEOF", &["cat", "scan_probe"]),
    ("cat <<EOF\n\\$(scan_ignored)\nEOF", &["cat"]),
    (
        "cat <<EOF\n$(scan_probe)\nE\\\nOF\nscan_after",
        &["cat", "scan_probe", "scan_after"],
    ),
    (
        "cat <<A <<'B'\n$(scan_probe)\nA\n$(scan_ignored)\nB\nscan_after",
        &["cat", "scan_probe", "scan_after"],
    ),
    (
        "if cat <<EOF\n$(scan_probe)\nEOF\nthen scan_after; fi",
        &["cat", "scan_probe", "scan_after"],
    ),
    (
        "printf '%s' \"$(cat <<EOF\n) $(scan_probe)\nEOF\n)\"",
        &["printf", "cat", "scan_probe"],
    ),
    (
        "{ cat <<'EOF'\n} ignored\nEOF\nscan_probe; }",
        &["cat", "scan_probe"],
    ),
    ("cat <<< \"$(scan_probe)\"", &["cat", "scan_probe"]),
    (
        "printf '%s' ${unset:-$(scan_probe)}",
        &["printf", "scan_probe"],
    ),
    (
        "printf '%s' \"${unset:-'$(scan_probe)'}\"",
        &["printf", "scan_probe"],
    ),
    ("printf '%s' ${unset:-'$(scan_ignored)'}", &["printf"]),
    (
        "printf '%s' \"${unset:-${other:-$(scan_probe)}}\"",
        &["printf", "scan_probe"],
    ),
    (
        "printf '%s' \"${value%)}\"; scan_probe",
        &["printf", "scan_probe"],
    ),
    (
        "printf '%s' \"${value//x/$(scan_probe)}\"",
        &["printf", "scan_probe"],
    ),
    (
        "printf '%s' \"${array[$(scan_probe)]}\"",
        &["printf", "scan_probe"],
    ),
    (
        "array[$(scan_probe)]=value; scan_after",
        &["scan_probe", "scan_after"],
    ),
    (
        "array=(one \"$(scan_probe)\"); scan_after",
        &["scan_probe", "scan_after"],
    ),
    (
        "array=(<(scan_probe)); scan_after",
        &["scan_probe", "scan_after"],
    ),
    (
        "(( value = $(scan_probe) + 1 )); scan_after",
        &["scan_probe", "scan_after"],
    ),
    (
        "printf '%s' $((array[$(scan_probe)] + 1))",
        &["printf", "scan_probe"],
    ),
    (
        "printf '%s' $[1 + $(scan_probe)]",
        &["printf", "scan_probe"],
    ),
    (
        "[[ $(scan_probe) = value ]] && scan_after",
        &["scan_probe", "scan_after"],
    ),
    (
        "printf '%s' `printf \\2`; scan_probe",
        &["printf", "printf", "scan_probe"],
    ),
    (
        "printf '%s' `printf \\`scan_probe\\``",
        &["printf", "printf", "scan_probe"],
    ),
    (
        "printf '%s' $'literal\\'$(scan_ignored)'; scan_probe",
        &["printf", "scan_probe"],
    ),
    ("printf %s $\"$(scan_probe)\"", &["printf", "scan_probe"]),
    (
        "if true; then \\\nVALUE=$(scan_probe); fi",
        &["true", "scan_probe"],
    ),
    ("! scan_probe", &["scan_probe"]),
    ("time scan_probe", &["time"]),
    ("{fd}>/dev/null scan_probe", &["scan_probe"]),
    ("case $r in a) ls | head;; esac", &["ls", "head"]),
    ("case $r in a) ls && echo;; esac", &["ls", "echo"]),
    ("{ find . -exec echo {} \\; ; }", &["find"]),
    ("{ echo {a,{b,c}}; }", &["echo"]),
    ("if true; then\\\n echo hi; fi", &["true", "echo"]),
    ("for ((i=0; i<2; i++)) do echo hi; done", &["echo"]),
    (
        "echo \"$(case x in @(a)) echo hi;; esac)\"",
        &["echo", "echo"],
    ),
    (
        "set -- 1; for x do scan_probe; done",
        &["set", "scan_probe"],
    ),
    ("'q'; x=1 a", &["q", "a"]),
    ("cat <<E; ( true\nscan_ignored\nE\n)", &["cat", "true"]),
    (
        "cat <<E; f() { true\nscan_ignored\nE\n}; f",
        &["cat", "true", "f"],
    ),
    (
        "cat <<E; case x in\nscan_ignored\nE\nx) scan_probe;; esac",
        &["cat", "scan_probe"],
    ),
    ("time [[ ( -f foo ) ]]", &[]),
    ("time ! { echo a; echo b; }", &["echo", "echo"]),
];

/// Rows CodeSmith reads differently, on purpose: valid syntax its scanner
/// refuses as unanalyzable (so the command is asked about rather than
/// guessed at), and an arithmetic command or a subscript assignment, which
/// it keeps as a command of its own (so a rule can still match its text).
const DIFFERENT: &[(&str, Option<&[&str]>)] = &[
    (
        "printf '%s' \"$(case value in value) scan_probe;; esac)\"",
        None,
    ),
    ("cat <<EOF\n$(scan_probe)\nE\\\nOF\nscan_after", None),
    ("printf '%s' \"$(cat <<EOF\n) $(scan_probe)\nEOF\n)\"", None),
    ("echo \"$(case x in @(a)) echo hi;; esac)\"", None),
    ("time [[ ( -f foo ) ]]", None),
    ("time ! { echo a; echo b; }", None),
    (
        "array[$(scan_probe)]=value; scan_after",
        Some(&["array[$(scan_probe)]=value", "scan_probe", "scan_after"]),
    ),
    (
        "(( value = $(scan_probe) + 1 )); scan_after",
        Some(&[
            "(( value = $(scan_probe) + 1 ))",
            "scan_probe",
            "scan_after",
        ]),
    ),
];

/// "extracts actual command nodes: %s" (62 of the 70 rows; the others are
/// [`DIFFERENT`]).
#[test]
fn oc_scan_extracts_actual_command_nodes() {
    let mut failed = Vec::new();
    for (source, names) in FIXTURES {
        let got = shell_scan::scan(source).map(|c| {
            c.into_iter()
                .map(|c| c.words.first().cloned().unwrap_or_default())
                .collect::<Vec<_>>()
        });
        let want: Result<Vec<String>, ()> = match DIFFERENT.iter().find(|(s, _)| s == source) {
            Some((_, None)) => Err(()),
            Some((_, Some(heads))) => Ok(heads.iter().map(|s| s.to_string()).collect()),
            None => Ok(names.iter().map(|s| s.to_string()).collect()),
        };
        if got.as_ref().map_err(|_| ()) != want.as_ref().map_err(|_| ()) {
            failed.push(format!("{source:?}: {got:?}, expected {want:?}"));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
    assert_eq!(
        DIFFERENT
            .iter()
            .filter(|(s, _)| FIXTURES.iter().any(|(f, _)| f == s))
            .count(),
        DIFFERENT.len()
    );
}

/// The shell at `name` on a plain PATH, if installed.
fn shell(name: &str) -> Option<String> {
    ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"]
        .iter()
        .map(|d| format!("{d}/{name}"))
        .find(|p| Path::new(p).is_file())
}

/// "${shell} accepts the source grammar: ${source}": the fixtures are
/// valid Bash and Zsh (`-n` parses without running anything).
#[test]
fn oc_scan_fixtures_are_accepted_by_bash_and_zsh() {
    let home = tempfile::tempdir().unwrap();
    for name in ["bash", "zsh"] {
        let Some(exe) = shell(name) else { continue };
        for (source, _) in FIXTURES {
            // Bash spellings Zsh lacks, and extglob, which Bash parses only when enabled.
            if name == "zsh"
                && (source.contains("$\"") || source.starts_with("{fd}") || source.contains("$["))
                || name == "bash" && source.contains("@(")
            {
                continue;
            }
            let flags: &[&str] = if name == "bash" {
                &["--noprofile", "--norc"]
            } else {
                &["-f"]
            };
            let out = Command::new(&exe)
                .args(flags)
                .args(["-n", "-c", &format!("{source}\n:")])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", home.path())
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stderr),
                "",
                "{name}: {source:?}"
            );
            // Zsh negates the skipped command's status even under NOEXEC.
            let code = if name == "zsh" && source.starts_with("! ") {
                1
            } else {
                0
            };
            assert_eq!(out.status.code(), Some(code), "{name}: {source:?}");
        }
    }
}

const CONTEXTS: &[fn(&str) -> String] = &[
    |s| s.to_string(),
    |s| format!("( {s} )"),
    |s| format!("{{ {s}; }}"),
    |s| format!("if true; then {s}; fi"),
    |s| format!("outer() {{ {s}; }}; outer"),
];

const BODIES: &[&str] = &[
    "for value in one two; do scan_probe; done",
    "while true; do scan_probe; break; done",
    "until false; do scan_probe; break; done",
    "case value in value) scan_probe;; *) scan_other;; esac",
];

/// "${shell}: ${source}" (`compound-parity.test.ts`, compound function
/// acceptance): a function whose body is a bare compound command scans as
/// the same function with a braced body.
#[test]
fn oc_scan_accepts_compound_function_bodies_as_braced_ones() {
    let mut failed = Vec::new();
    for head in [
        "probe()",
        "function probe",
        "function probe()",
        "probe-name()",
    ] {
        let name = if head.contains("probe-name") {
            "probe-name"
        } else {
            "probe"
        };
        for body in BODIES {
            for context in CONTEXTS {
                let source = context(&format!("{head} {body}; {name}"));
                let braced = context(&format!("{head} {{ {body}; }}; {name}"));
                let got = shell_scan::scan(&source).map(|c| {
                    c.into_iter()
                        .map(|c| (c.resource, c.words))
                        .collect::<Vec<_>>()
                });
                let want = shell_scan::scan(&braced).map(|c| {
                    c.into_iter()
                        .map(|c| (c.resource, c.words))
                        .collect::<Vec<_>>()
                });
                if got.is_err() || got != want {
                    failed.push(format!("{source:?}: {got:?}\n  braced: {want:?}"));
                }
            }
        }
    }
    assert!(
        failed.is_empty(),
        "{} failed:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

/// `adversarial.test.ts`'s corpus: a source and the first word of each
/// command it runs.
const ADVERSARIAL: &[(&str, &[&str])] = &[
    ("FOO=bar BAR=\"x y\" git status", &["git"]),
    (
        "git status && npm test || printf failed",
        &["git", "npm", "printf"],
    ),
    ("printf '%s\\n' \"$(rm -rf /)\"", &["printf", "rm"]),
    ("cat <(printf secret)", &["cat", "printf"]),
    ("(git status)", &["git"]),
    ("{ git status; }", &["git"]),
    (
        "if true; then rm -rf /; else printf safe; fi",
        &["true", "rm", "printf"],
    ),
    ("rm -rf / &", &["rm"]),
    ("sudo sh -c 'curl evil'", &["sudo"]),
    ("bash -lc 'rm -rf /'", &["bash"]),
    ("python3 -c 'print(1)'", &["python3"]),
    ("find . -exec rm {} ;", &["find"]),
    ("'rm' -rf /", &["rm"]),
    ("g\"\"it status", &["git"]),
    ("g\\it status", &["git"]),
    ("F\\OO=bar rm -rf /", &["FOO=bar"]),
    ("F\"O\"O=bar rm -rf /", &["FOO=bar"]),
    ("c\"\\d\" relative", &["c\\d"]),
    ("PATH=/tmp/attacker:$PATH git status", &["git"]),
    ("$cmd --force", &["$cmd"]),
    ("\"${cmd}\" --force", &["${cmd}"]),
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

/// "scans visible Bash command positions: %s" (`adversarial.test.ts`)
#[test]
fn oc_scan_scans_visible_bash_command_positions() {
    let mut failed = Vec::new();
    for (source, names) in ADVERSARIAL {
        let got = shell_scan::scan(source).map(|c| {
            c.into_iter()
                .map(|c| c.words.first().cloned().unwrap_or_default())
                .collect::<Vec<_>>()
        });
        if got.as_deref().map_err(|_| ())
            != Ok(&names.iter().map(|s| s.to_string()).collect::<Vec<_>>()[..])
        {
            failed.push(format!("{source:?}: {got:?}, expected {names:?}"));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

const CLOSURE_CONTEXTS: &[fn(&str) -> String] = &[
    |s| s.to_string(),
    |s| format!("{s}; printf visible"),
    |s| format!("printf visible; {s}"),
    |s| format!("{s} && printf visible"),
    |s| format!("printf visible || {s}"),
    |s| format!("printf \"$({s})\""),
    |s| format!("X=$({s}) printf visible"),
    |s| format!("printf visible >$({s})"),
];

/// "{source}" (`closure.test.ts`, recursive structural opacity): an
/// unterminated quote is unanalyzable in every context.
#[test]
fn oc_scan_keeps_an_unterminated_quote_opaque_in_every_context() {
    for outer in CLOSURE_CONTEXTS {
        for inner in &CLOSURE_CONTEXTS[..5] {
            let source = outer(&inner("printf \"unterminated"));
            assert!(shell_scan::scan(&source).is_err(), "{source:?}");
        }
    }
}

/// "single quotes suppress active syntax: %s" (`closure.test.ts`)
#[test]
fn oc_scan_single_quotes_suppress_active_syntax() {
    assert!(shell_scan::scan("printf '%s' 'printf \"unterminated'").is_ok());
}

/// "{source}" (`closure.test.ts`, recursive parameter expansion coverage):
/// a parameter expansion in command position stays a command of its own.
#[test]
fn oc_scan_keeps_parameter_expansions_in_command_position_in_every_context() {
    let mut failed = Vec::new();
    for seed in ["${COMMAND:-hidden}", "$(printf ${value:-command}) hidden"] {
        for outer in CLOSURE_CONTEXTS {
            for inner in &CLOSURE_CONTEXTS[..5] {
                let source = outer(&inner(seed));
                match shell_scan::scan(&source) {
                    Ok(c) if c.iter().any(|c| c.resource == seed) => {}
                    other => failed.push(format!("{source:?}: {other:?}")),
                }
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// `shell-parse-compatibility.test.ts`'s POSIX cases: shell, command, the
/// environment OpenCode ran it under (JSON, empty for none), the
/// `(resource, save)` pairs and the `cd` targets.
type Compat = (
    &'static str,
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
);
const COMPATIBILITY: &[Compat] = &[
    (
        "bash",
        "echo $((1+1))",
        "",
        &[("echo $((1+1))", "echo *")],
        &[],
    ),
    (
        "bash",
        "echo $((1 + $(printf hidden)))",
        "",
        &[
            ("echo $((1 + $(printf hidden)))", "echo *"),
            ("printf hidden", "printf *"),
        ],
        &[],
    ),
    ("bash", "cd ~/project", "", &[], &["/home/project"]),
    ("bash", "cd src&&cd..", "", &[("cd..", "cd.. *")], &["src"]),
    (
        "bash",
        "cd src && cd .. && git status",
        "",
        &[("git status", "git status *")],
        &["src", ".."],
    ),
    (
        "zsh",
        "git status",
        "",
        &[("git status", "git status *")],
        &[],
    ),
    (
        "fish",
        "git status",
        "",
        &[("git status", "git status *")],
        &[],
    ),
    (
        "bash",
        "git status",
        "{\"BASH_ENV\":\"/startup\"}",
        &[("git status", "git status *")],
        &[],
    ),
    (
        "bash",
        "git status",
        "{\"BASH_FUNC_cd%%\":\"() { :; }\"}",
        &[("git status", "git status *")],
        &[],
    ),
    (
        "bash",
        "cd $HOME; pwd",
        "{\"HOME\":\"/session-home\"}",
        &[("cd $HOME", "cd *"), ("pwd", "pwd *")],
        &[],
    ),
    (
        "bash",
        "cd; pwd",
        "{\"HOME\":\"/session-home\"}",
        &[("cd", "cd *"), ("pwd", "pwd *")],
        &[],
    ),
    (
        "bash",
        "target=/outside; cd \"$target\"; pwd",
        "",
        &[("cd \"$target\"", "cd *"), ("pwd", "pwd *")],
        &[],
    ),
    (
        "bash",
        "cd 'src dir' && git status",
        "",
        &[("git status", "git status *")],
        &["src dir"],
    ),
    (
        "bash",
        "cd \"src dir\"; cd escaped\\ space",
        "",
        &[],
        &["src dir", "escaped space"],
    ),
    (
        "bash",
        "cd '$HOME'; cd '~/outside'",
        "",
        &[],
        &["$HOME", "/home/outside"],
    ),
    (
        "bash",
        "g\"\"it status",
        "",
        &[("g\"\"it status", "g\"\"it *")],
        &[],
    ),
    (
        "bash",
        "npm \"run\" test",
        "",
        &[("npm \"run\" test", "npm \"run\" *")],
        &[],
    ),
    ("bash", "git '*'", "", &[("git '*'", "git '*' *")], &[]),
    (
        "bash",
        "FOO=bar git status",
        "",
        &[("FOO=bar git status", "git status *")],
        &[],
    ),
    (
        "bash",
        "HOME=/outside; cd; pwd",
        "",
        &[("cd", "cd *"), ("pwd", "pwd *")],
        &[],
    ),
    ("bash", "CDPATH=/outside cd child", "", &[], &["child"]),
    (
        "bash",
        "cd child; pwd",
        "{\"CDPATH\":\"/outside\"}",
        &[("pwd", "pwd *")],
        &["child"],
    ),
    ("bash", "cd /workspace > output", "", &[], &["/workspace"]),
    (
        "bash",
        "export X=value; unset X; git status",
        "",
        &[("git status", "git status *")],
        &[],
    ),
    (
        "bash",
        "printf ok && git status > output",
        "",
        &[
            ("printf ok", "printf *"),
            ("git status > output", "git status *"),
        ],
        &[],
    ),
    (
        "bash",
        "printf ok | cat < input > output",
        "",
        &[("printf ok", "printf *"), ("cat < input > output", "cat *")],
        &[],
    ),
    (
        "bash",
        "cd -- -/../../../etc; pwd",
        "",
        &[("pwd", "pwd *")],
        &["-/../../../etc"],
    ),
    (
        "bash",
        "cd -; pushd; popd; pwd",
        "",
        &[
            ("cd -", "cd *"),
            ("pushd", "pushd *"),
            ("popd", "popd *"),
            ("pwd", "pwd *"),
        ],
        &[],
    ),
    (
        "bash",
        "command cd /outside; builtin cd /elsewhere",
        "",
        &[
            ("command cd /outside", "command *"),
            ("builtin cd /elsewhere", "builtin *"),
        ],
        &["/outside", "/elsewhere"],
    ),
];

/// Cases CodeSmith answers differently, on purpose, with its answer: a
/// bare `cd` is a directory (the home directory; a script that changes
/// `HOME` makes the directory unknown, which asks), not a `cd *` command,
/// and a quoted `~` is checked both as written and expanded.
/// `(resource, save)` pairs and `cd` targets.
type Resources = (Vec<(String, String)>, Vec<String>);

fn compatibility_differs(command: &str, home: &str) -> Option<Resources> {
    let pwd = ("pwd".to_string(), "pwd *".to_string());
    match command {
        "cd; pwd" | "HOME=/outside; cd; pwd" => Some((vec![pwd], vec![home.to_string()])),
        "cd '$HOME'; cd '~/outside'" => Some((
            vec![],
            vec![
                "$HOME".into(),
                format!("{home}/outside"),
                "~/outside".into(),
            ],
        )),
        _ => None,
    }
}

/// "extracts resources, saved prefixes, and directories: %s %s %j" (the
/// POSIX rows; OpenCode's `/home` is the home directory here).
#[test]
fn oc_scan_extracts_resources_saved_prefixes_and_directories() {
    let home = dirs::home_dir().unwrap().display().to_string();
    let mut failed = Vec::new();
    for (shell, command, env, commands, directories) in COMPATIBILITY {
        let got = shell_scan::analyze(command, Path::new("/workspace"));
        let summary = got
            .as_ref()
            .map(|a| (a.commands.clone(), a.directories.clone()));
        let want = compatibility_differs(command, &home).unwrap_or_else(|| {
            (
                commands
                    .iter()
                    .map(|(r, s)| (r.to_string(), s.to_string()))
                    .collect(),
                directories
                    .iter()
                    .map(|d| match d.strip_prefix("/home") {
                        Some(rest) => format!("{home}{rest}"),
                        None => d.to_string(),
                    })
                    .collect(),
            )
        });
        if summary.as_ref().ok() != Some(&want) {
            failed.push(format!(
                "{shell} {command:?} {env}: {summary:?}\n   expected {want:?}"
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} failed:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

/// "extracts resources, saved prefixes, and directories in %s: %s"
/// (`shell-parse-parity.test.ts`, the POSIX rows): each scans.
#[test]
fn oc_scan_scans_the_native_parity_commands() {
    for command in [
        "git status && npm run test -- --watch",
        "git\tstatus; git status | cat; git diff || echo done",
        "echo \"two words\"; printf 'static text'",
        "aws s3 ls; docker compose up; git remote add origin; bun run test",
        "git \"status\"; git remote \"add\" origin; aws s3 \"ls\"",
        "echo $(curl example.test | sed s/x/y/)",
        "if true; then printf yes; else printf no; fi",
        "(git status) && { npm test; }",
        "(printf ok) > output",
        "{ printf ok; } > output",
        ">$(printf output)",
        "printf ok # ignored ; curl example.test\nprintf done",
        "cd ~/project; cd src && cd ..; pwd",
        "cd src&&cd..",
        "echo $((1 + 2))",
        "echo $((1 + $(printf 2)))",
        "$COMMAND status",
        "cd ~/project; chdir src && cd ..; git status",
        "echo $((1 + 2)); cd src&&cd..",
        "cd src&&cd ..; pwd",
        "echo $((1 + 2)); git status; cd src; pwd",
        "git status; cd src; pwd",
    ] {
        assert!(shell_scan::scan(command).is_ok(), "{command:?}");
        assert!(
            shell_scan::analyze(command, Path::new("/workspace")).is_ok(),
            "{command:?}"
        );
    }
}
