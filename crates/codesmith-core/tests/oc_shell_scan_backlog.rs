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

// --- arithmetic.test.ts ---

/// A command's source, unquoted words and words as written.
fn parts(c: &shell_scan::Command) -> (String, Vec<String>, Vec<String>) {
    (c.resource.clone(), c.words.clone(), c.raw_words.clone())
}

fn owned(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// "preserves arithmetic without evaluating it: %s"
#[test]
fn oc_scan_preserves_arithmetic_without_evaluating_it() {
    let mut failed = Vec::new();
    for expression in [
        "$((1+1))",
        "$((1 + 1))",
        "$(((1 + 2) * (3 + (4))))",
        "$((value + $other + ${third}))",
        "$((value += 2, value > 1 ? value << 2 : ~value))",
        "$((16#ff & 0xff | 2 ** 3))",
        "$((1 + $((2 * 3))))",
        "$((1 + \\\n2))",
        "$((1 +\n2))",
        "$((1 + ${value:-2}))",
        "$((array[index]))",
        "$((1 + $[2]))",
        "$((1 + \"2\"))",
        "$((1 + '2'))",
    ] {
        for argument in [expression.to_string(), format!("\"{expression}\"")] {
            let source = format!("echo {argument}");
            let want = vec![(
                source.clone(),
                owned(&["echo", expression]),
                vec!["echo".to_string(), argument.clone()],
            )];
            match shell_scan::scan(&source) {
                Ok(c) if c.iter().map(parts).collect::<Vec<_>>() == want => {}
                other => failed.push(format!("{source:?}: {other:?}")),
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// "reports explicit commands inside arithmetic: %s"
#[test]
fn oc_scan_reports_explicit_commands_inside_arithmetic() {
    let mut failed = Vec::new();
    for expression in [
        "$((1 + $(printf 2)))",
        "$((1 + `printf 2`))",
        "$((1 + $((2 * $(printf 2)))))",
        "$((1 + $(printf \"%s\" \"$(printf 2)\")))",
        "$((array[$(printf 2)]))",
        "$((1 + `printf \\2`))",
    ] {
        for argument in [expression.to_string(), format!("\"{expression}\"")] {
            let source = format!("echo {argument}; pwd");
            let inner: &[&str] = if expression.contains("\"$(printf") {
                &["printf", "printf"]
            } else {
                &["printf"]
            };
            let ok = shell_scan::scan(&source).is_ok_and(|c| {
                c.len() == inner.len() + 2
                    && parts(&c[0])
                        == (
                            format!("echo {argument}"),
                            owned(&["echo", expression]),
                            vec!["echo".to_string(), argument.clone()],
                        )
                    && c[1..c.len() - 1]
                        .iter()
                        .map(|c| c.words[0].as_str())
                        .collect::<Vec<_>>()
                        == inner
                    && parts(&c[c.len() - 1]) == ("pwd".into(), owned(&["pwd"]), owned(&["pwd"]))
            });
            if !ok {
                failed.push(format!("{source:?}: {:?}", shell_scan::scan(&source)));
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// "recognizes arithmetic in existing shell contexts: %s"
#[test]
fn oc_scan_recognizes_arithmetic_in_existing_shell_contexts() {
    for source in [
        "VALUE=$((1 + $(printf 2))) echo ok >\"$((3 + $(printf 4)))\"",
        "echo \"$(echo $((1 + $(printf 2))))\"",
        "{ echo \"$((1 + $(printf 2)))\"; }",
        "(echo \"$((1 + $(printf 2)))\")",
        "if true; then echo \"$((1 + $(printf 2)))\"; fi",
    ] {
        let heads: Vec<String> = shell_scan::scan(source)
            .unwrap_or_else(|e| panic!("{source:?}: {e}"))
            .into_iter()
            .map(|c| c.words[0].clone())
            .collect();
        assert!(
            heads.contains(&"printf".into()) && heads.contains(&"echo".into()),
            "{source:?}: {heads:?}"
        );
    }
}

/// "does not expand arithmetic inside single quotes"
#[test]
fn oc_scan_does_not_expand_arithmetic_inside_single_quotes() {
    let c = shell_scan::scan("echo '$((1 + $(ignored)))'").unwrap();
    assert_eq!(
        c.iter().map(parts).collect::<Vec<_>>(),
        [(
            "echo '$((1 + $(ignored)))'".to_string(),
            owned(&["echo", "$((1 + $(ignored)))"]),
            owned(&["echo", "'$((1 + $(ignored)))'"])
        )]
    );
}

/// "rejects malformed arithmetic syntax: %s"
#[test]
fn oc_scan_rejects_malformed_arithmetic_syntax() {
    for source in [
        "echo $((1 + 2)",
        "echo $(((1 + 2))",
        "echo $((1 + $(printf 2))) &&",
        "echo $((1 + $(printf 2 &&)))",
    ] {
        assert!(shell_scan::scan(source).is_err(), "{source:?}");
    }
    // Differs on purpose: read as an arithmetic expansion that names no
    // command (every shell fails it as arithmetic when it runs).
    let c = shell_scan::scan("echo $((1; printf 2))").unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].words, ["echo", "$((1; printf 2))"]);
}

/// "bounds arithmetic nesting, input size, and repeated conditional work"
#[test]
fn oc_scan_bounds_arithmetic_nesting_input_size_and_repeated_work() {
    // Adapted: deep parentheses inside one expansion name no command, so
    // they scan; nested expansions and oversized input are refused.
    assert!(shell_scan::scan(&format!("echo $(({}1{}))", "(".repeat(33), ")".repeat(33))).is_ok());
    assert!(shell_scan::scan(&format!("echo {}1{}", "$((".repeat(33), "))".repeat(33))).is_err());
    assert!(shell_scan::scan(&format!("echo $(({}1))", "1+".repeat(32 * 1024))).is_err());
    // Repeated conditional work stays bounded. Adapted: CodeSmith scans
    // eleven levels of conditionals around substitutions and refuses deeper
    // ones (OpenCode scans sixteen), which asks rather than guesses.
    let start = std::time::Instant::now();
    let mut source = format!("printf {}", "1".repeat(1024));
    for level in 1..=16 {
        source = format!("if true; then echo $((1 + $({source}))); fi");
        let scanned = shell_scan::scan(&source);
        if level <= 11 {
            assert_eq!(scanned.map(|c| c.len()), Ok(2 * level + 1), "{level}");
        } else {
            assert!(scanned.is_err(), "{level}");
        }
    }
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    assert!(shell_scan::scan("echo $((1+1))").is_ok());
}

// --- bash-safety.test.ts ---

fn scanned(source: &str) -> Vec<(String, Vec<String>, Vec<String>)> {
    shell_scan::scan(source)
        .unwrap_or_else(|e| panic!("{source:?}: {e}"))
        .iter()
        .map(parts)
        .collect()
}

fn triple(resource: &str, words: &[&str], raw: &[&str]) -> (String, Vec<String>, Vec<String>) {
    (resource.to_string(), owned(words), owned(raw))
}

/// "recognizes shell whitespace %j"
#[test]
fn oc_scan_recognizes_shell_whitespace() {
    for space in [" ", "\t"] {
        let c = shell_scan::scan(&format!("printf{space}ok")).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].resource, format!("printf{space}ok"));
        assert_eq!(c[0].words, ["printf", "ok"]);
    }
}

/// "does not normalize non-shell whitespace %j". Adapted: the scan
/// either refuses it or keeps it in the word, as the shell reads it, so
/// `printf\rok` is checked as that command name, never as `printf`.
#[test]
fn oc_scan_does_not_normalize_non_shell_whitespace() {
    for space in ["\r", "\u{b}", "\u{c}", "\u{a0}", "\u{feff}"] {
        for source in [format!("{space}printf ok"), format!("printf{space}ok")] {
            if let Ok(c) = shell_scan::scan(&source) {
                assert!(c[0].words[0].contains(space), "{source:?}: {c:?}");
            }
        }
    }
}

/// "does not consume quoted command names as fd prefixes: %s"
#[test]
fn oc_scan_does_not_consume_quoted_command_names_as_fd_prefixes() {
    for head in ["'123'", "\"123\"", "1\\23"] {
        let source = format!("{head}>/dev/null argument");
        let c = shell_scan::scan(&source).unwrap();
        assert_eq!(c.len(), 1, "{source:?}");
        assert_eq!(c[0].resource, source);
        assert_eq!(c[0].words, ["123", "argument"], "{source:?}");
    }
}

/// "retains commandless redirects as explicit permission resources: %s".
/// Adapted: a commandless command has no words; its resource is the
/// source, and "always" saves its operator prefix.
#[test]
fn oc_scan_retains_commandless_redirects_as_permission_resources() {
    for (source, save) in [
        ("X=value > /dev/null", "X=value > *"),
        ("X=x 2> output", "X=x 2> *"),
        ("X=x < input", "X=x < *"),
        ("> /dev/null", "> *"),
        ("2>> output", "2>> *"),
        (">output 2>&1", "> *"),
    ] {
        assert_eq!(scanned(source), [triple(source, &[], &[])], "{source:?}");
        let a = shell_scan::analyze(source, Path::new("/w")).unwrap();
        assert_eq!(
            a.commands,
            [(source.to_string(), save.to_string())],
            "{source:?}"
        );
    }
}

/// "still requires redirect targets: %s". Fixed: `> # comment` read `#`
/// as the target and `comment` as a command.
#[test]
fn oc_scan_still_requires_redirect_targets() {
    for source in [
        ">",
        "X=x >",
        "X=x > # comment\nprintf ok",
        ">; printf ok",
        "(printf ok) >",
        "{ printf ok; } >",
    ] {
        assert!(shell_scan::scan(source).is_err(), "{source:?}");
    }
}

/// "retains file redirects on compound groups and trailing assignments:
/// %s". Adapted: the redirect's resource has no words (see above).
#[test]
fn oc_scan_retains_file_redirects_on_groups_and_trailing_assignments() {
    for (source, resource) in [
        ("{ printf ok; } > output", "> output"),
        ("{ printf ok; } 2> output", "2> output"),
        ("{ printf ok; } &> output", "&> output"),
        ("(printf ok) > output", "> output"),
        ("{ printf ok; }; X=x > output", "X=x > output"),
    ] {
        assert_eq!(
            scanned(source),
            [
                triple("printf ok", &["printf", "ok"], &["printf", "ok"]),
                triple(resource, &[], &[])
            ],
            "{source:?}"
        );
    }
}

/// "does not invent redirect commands for pure group descriptor
/// duplications: %s". Fixed: `(make) 2>&1` also asked about `2>&1`.
#[test]
fn oc_scan_invents_no_redirect_commands_for_group_descriptor_duplications() {
    for source in [
        "{ printf ok; } 2>&1",
        "(printf ok) 2>&1",
        "(printf ok) 2>&-",
    ] {
        assert_eq!(
            scanned(source),
            [triple("printf ok", &["printf", "ok"], &["printf", "ok"])],
            "{source:?}"
        );
    }
}

/// "retains both redirect resources and explicit substitutions in
/// commandless redirects: %s". Adapted: resources, since the redirect's
/// has no words.
#[test]
fn oc_scan_retains_redirect_resources_and_substitutions_in_commandless_redirects() {
    for source in [
        "X=x >$(printf output)",
        ">$(printf output)",
        "{ :; } >$(printf output)",
        "(:) >$(printf output)",
    ] {
        let c = shell_scan::scan(source).unwrap();
        let head = if source.starts_with("X=x") {
            "X=x >$(printf output)"
        } else {
            ">$(printf output)"
        };
        let resources: Vec<&str> = c.iter().map(|c| c.resource.as_str()).collect();
        let want: Vec<&str> = if source.contains(':') {
            vec![":", head, "printf output"]
        } else {
            vec![head, "printf output"]
        };
        assert_eq!(resources, want, "{source:?}");
        assert_eq!(
            parts(c.last().unwrap()),
            triple(
                "printf output",
                &["printf", "output"],
                &["printf", "output"]
            )
        );
    }
}

/// "scans a list following a group redirect suffix" (adapted: the
/// redirect's resource has no words)
#[test]
fn oc_scan_scans_a_list_following_a_group_redirect_suffix() {
    assert_eq!(
        scanned("(printf ok) >output && pwd"),
        [
            triple("printf ok", &["printf", "ok"], &["printf", "ok"]),
            triple(">output", &[], &[]),
            triple("pwd", &["pwd"], &["pwd"]),
        ]
    );
}

/// "retains explicit colon commands and their redirects after a group"
#[test]
fn oc_scan_retains_colon_commands_and_their_redirects_after_a_group() {
    let c = scanned("(printf ok); : > output");
    assert_eq!(c.len(), 2);
    assert_eq!(
        (c[0].0.as_str(), c[0].1.clone()),
        ("printf ok", owned(&["printf", "ok"]))
    );
    assert_eq!(
        (c[1].0.as_str(), c[1].1.clone()),
        (": > output", owned(&[":"]))
    );
}

/// "does not repeatedly rescan nested conditionals". Adapted: eleven
/// levels scan in bounded time; deeper ones are refused (see the
/// arithmetic bound above).
#[test]
fn oc_scan_does_not_repeatedly_rescan_nested_conditionals() {
    let start = std::time::Instant::now();
    let mut source = format!("printf {}", "x".repeat(1024));
    for _ in 0..11 {
        source = format!("if true; then echo $({source}); fi");
    }
    assert!(shell_scan::scan(&source).is_ok());
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    let c = scanned("if true; then echo $(printf safe); fi");
    let summary: Vec<(String, Vec<String>)> = c.into_iter().map(|(r, w, _)| (r, w)).collect();
    assert_eq!(
        summary,
        [
            ("true".to_string(), owned(&["true"])),
            (
                "echo $(printf safe)".to_string(),
                owned(&["echo", "$(printf safe)"])
            ),
            ("printf safe".to_string(), owned(&["printf", "safe"])),
        ]
    );
}

/// "does not lose commands through delimiter or quote confusion: %s"
#[test]
fn oc_scan_does_not_lose_commands_through_delimiter_or_quote_confusion() {
    for source in [
        "(printf safe # ) ignored\nscan_probe)",
        "{ printf safe; # } ignored\nscan_probe; }",
        "echo \"$(printf \"'\"; scan_probe)\"",
    ] {
        let heads: Vec<String> = scanned(source)
            .into_iter()
            .map(|c| c.1[0].clone())
            .collect();
        assert!(
            heads.contains(&"scan_probe".to_string()),
            "{source:?}: {heads:?}"
        );
    }
}
