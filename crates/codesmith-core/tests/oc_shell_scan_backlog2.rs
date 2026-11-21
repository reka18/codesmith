//! Ports of the rest of the shell scanner cases in PARITY.md's porting
//! backlog (`packages/core/test/shell-parse-*.test.ts` and
//! `shell-scan/*.test.ts`, MIT). CodeSmith's scanner reads one grammar for
//! every POSIX shell, so a row OpenCode checks once per dialect is checked
//! once here and counts for both; the real-shell rows run under each of
//! Bash and Zsh that is installed.

use codesmith_core::tool::shell_scan;
use codesmith_core::wildcard;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `(resource, save)` pairs and `cd` directories, as `analyze` reports them.
type Resources = (Vec<(String, String)>, Vec<String>);

/// A command, its `(resource, save)` pairs and directories.
type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a [&'a str]);

fn analyze(source: &str) -> Result<Resources, String> {
    shell_scan::analyze(source, Path::new("/workspace")).map(|a| (a.commands, a.directories))
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(r, s)| (r.to_string(), s.to_string()))
        .collect()
}

fn owned(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// The first word of each command `source` runs, or why it was refused.
fn heads(source: &str) -> Result<Vec<String>, &'static str> {
    shell_scan::scan(source).map(|cs| {
        cs.iter()
            .map(|c| c.words.first().cloned().unwrap_or_default())
            .collect()
    })
}

/// The shell at `name` on a plain PATH, if installed.
fn shell(name: &str) -> Option<String> {
    // The newest first: macOS's /bin/bash is 3.2, which fails an
    // unparenthesized `case` inside `$( )`.
    ["/opt/homebrew/bin", "/usr/local/bin", "/bin", "/usr/bin"]
        .iter()
        .map(|d| format!("{d}/{name}"))
        .find(|p| Path::new(p).is_file())
}

/// Run `prelude; source` in `exe` with no profile and a clean
/// environment: `(exit code, stdout, stderr)`.
fn run(exe: &str, prelude: &str, source: &str) -> (Option<i32>, String, String) {
    let home = tempfile::tempdir().unwrap();
    let flags: &[&str] = if exe.ends_with("/bash") {
        &["--noprofile", "--norc"]
    } else if exe.ends_with("/zsh") {
        &["-f"]
    } else {
        &[]
    };
    let out = Command::new(exe)
        .args(flags)
        .args(["-c", &format!("{prelude}{source}")])
        .current_dir(home.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home.path())
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// --- shell-parse-compatibility.test.ts ---------------------------------

/// "derives the prefix for long argument lists"
#[test]
fn oc_scan_derives_the_prefix_for_long_argument_lists() {
    let command = format!("echo {}", "x ".repeat(16_000));
    let command = command.trim_end();
    assert_eq!(
        analyze(command),
        Ok((pairs(&[(command, "echo *")]), vec![]))
    );
}

/// "scanner behavior on former tree-sitter gaps": the POSIX fixtures.
#[test]
fn oc_scan_handles_the_former_tree_sitter_gaps() {
    let cases: &[Case] = &[
        (
            "FOO=bar > output",
            &[("FOO=bar > output", "FOO=bar > *")],
            &[],
        ),
        (
            "FOO=$(printf value) > output",
            &[
                ("FOO=$(printf value) > output", "FOO=$(printf value) > *"),
                ("printf value", "printf *"),
            ],
            &[],
        ),
        ("git 2 status", &[("git 2 status", "git 2 *")], &[]),
        (
            "cd 123; git == value",
            &[("git == value", "git == *")],
            &["123"],
        ),
        (
            "git $(printf status) diff",
            &[
                ("git $(printf status) diff", "git $(printf status) *"),
                ("printf status", "printf *"),
            ],
            &[],
        ),
        ("cd before\\\nafter", &[], &["beforeafter"]),
    ];
    for (command, commands, directories) in cases {
        assert_eq!(
            analyze(command),
            Ok((pairs(commands), owned(directories))),
            "{command:?}"
        );
    }
}

/// "retains the shared directory command set: %s". Differs on purpose:
/// `chdir` is a directory change, as in OpenCode, but `set-location` and
/// `push-location` are PowerShell's names; under a POSIX shell they are
/// ordinary commands looked up on PATH, so they are checked as commands.
#[test]
fn oc_scan_retains_the_shared_directory_command_set() {
    assert_eq!(
        analyze(
            "chdir /outside; set-location /elsewhere; push-location /stack; sl .; pop-location"
        ),
        Ok((
            pairs(&[
                ("set-location /elsewhere", "set-location *"),
                ("push-location /stack", "push-location *"),
                ("sl .", "sl *"),
                ("pop-location", "pop-location *"),
            ]),
            owned(&["/outside"])
        ))
    );
}

// --- shell-parse-parity.test.ts ----------------------------------------

/// `shell-parse-parity.test.ts`'s generator (its POSIX half): the
/// supported grammar, one source per entry.
fn generated() -> Vec<String> {
    let heads = ["git", "npm", "echo", "printf", "cat", "cd"];
    let args = [
        "",
        " status",
        " plain",
        " 'two words'",
        " \"two words\"",
        " escaped\\ space",
        " hash#word",
        " --flag=value",
        " ./relative",
        " /tmp/absolute",
    ];
    let separators = [" ; ", " && ", " || ", " | ", " |& ", "\n"];
    let redirects = ["", " > output", " 2> error", " < input", " >> output"];
    let mut out = Vec::new();
    for head in heads {
        for arg in args {
            for assignment in ["", "X=value ", "X='two words' ", "X=\"two words\" "] {
                for redirect in redirects {
                    out.push(format!("{assignment}{head}{arg}{redirect}"));
                }
            }
        }
    }
    for left in heads {
        for right in heads {
            for separator in separators {
                out.push(format!("{left} left{separator}{right} right"));
            }
        }
    }
    for outer in ["echo", "printf", "cat"] {
        for inner in heads {
            out.push(format!("{outer} $({inner} nested)"));
            out.push(format!("{outer} \"$({inner} nested)\""));
            out.push(format!("{outer} pre$({inner} nested)post"));
            out.push(format!("{outer} `{inner} nested`"));
        }
    }
    let mut state: u32 = 0x5eed1234;
    let mut random = |length: usize| {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        state as usize % length
    };
    for index in 0..10_000 {
        let left = heads[random(heads.len())];
        let right = heads[random(heads.len())];
        let arg = args[random(args.len())];
        let separator = separators[random(separators.len())];
        let redirect = redirects[random(redirects.len())];
        let forms = [
            format!("{left}{arg}{separator}{right} fuzz{index}"),
            format!("echo $({right} fuzz{index})"),
            format!("{left}{arg} # ignored\n{right} fuzz{index}"),
            format!("X=value {left}{arg}{redirect}"),
            format!("{left} 'two words'{separator}{right} fuzz{index}"),
        ];
        out.push(forms[index % forms.len()].clone());
    }
    out
}

/// "scans generated supported syntax with valid saved prefixes and no
/// fallback": every generated source scans, and each command's saved rule
/// matches its own resource (so "always" allows what was asked).
#[test]
fn oc_scan_scans_generated_supported_syntax_with_valid_saved_prefixes() {
    let sources = generated();
    assert!(sources.len() > 10_000);
    for source in sources {
        let a = shell_scan::analyze(&source, Path::new("/workspace"))
            .unwrap_or_else(|e| panic!("{source:?}: {e}"));
        for (resource, save) in &a.commands {
            let assignment = resource.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !name.starts_with(|c: char| c.is_ascii_digit())
            });
            if !assignment {
                assert!(
                    wildcard::matches(resource, save, false),
                    "{source:?}: {resource:?} vs {save:?}"
                );
            }
        }
    }
}

/// "retains commandless redirects with matching saved prefixes: %s"
#[test]
fn oc_scan_retains_commandless_redirects_with_matching_saved_prefixes() {
    for (command, save) in [("> output", "> *"), ("2>> output", "2>> *")] {
        let (commands, dirs) = analyze(command).unwrap();
        assert_eq!(commands, pairs(&[(command, save)]));
        assert!(dirs.is_empty());
        assert!(wildcard::matches(command, save, false));
    }
}

// --- shell-parse-syntax.test.ts ----------------------------------------

/// "does not invent commands from a quoted second heredoc body". Adapted:
/// the heredoc's owner's resource runs to the end of its bodies.
#[test]
fn oc_scan_does_not_invent_commands_from_a_quoted_second_heredoc_body() {
    let command = "cat <<FIRST <<'SECOND'\n$(printf first)\nFIRST\n$(not_a_command)\nSECOND";
    assert_eq!(
        analyze(command),
        Ok((
            pairs(&[(command, "cat *"), ("printf first", "printf *")]),
            vec![]
        ))
    );
}

/// "keeps heredoc and here-string spans attached to their own commands
/// across lists and pipelines". Adapted: a heredoc's owner's resource runs
/// from its first word to the end of its body (so rules see what is fed to
/// it), which takes in the rest of its line; each other command on the line
/// is still a resource of its own, in order.
#[test]
fn oc_scan_keeps_heredoc_and_here_string_commands_in_order() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "true && cat <<'EOF' && git status\nhello\nEOF",
            &[
                ("true", "true *"),
                ("cat <<'EOF' && git status\nhello\nEOF", "cat *"),
                ("git status", "git status *"),
            ],
        ),
        (
            "cat <<'EOF' && git status\nhello\nEOF",
            &[
                ("cat <<'EOF' && git status\nhello\nEOF", "cat *"),
                ("git status", "git status *"),
            ],
        ),
        (
            "if cat <<'EOF' | grep -q hi; then echo ok; fi\nhi\nEOF",
            &[
                (
                    "cat <<'EOF' | grep -q hi; then echo ok; fi\nhi\nEOF",
                    "cat *",
                ),
                ("grep -q hi", "grep *"),
                ("echo ok", "echo *"),
            ],
        ),
        (
            "true && git add -p file.rs <<< $'n\\ny\\n' >/dev/null",
            &[
                ("true", "true *"),
                ("git add -p file.rs <<< $'n\\ny\\n' >/dev/null", "git add *"),
            ],
        ),
    ];
    for (command, commands) in cases {
        assert_eq!(
            analyze(command),
            Ok((pairs(commands), vec![])),
            "{command:?}"
        );
    }
}

/// "scans Zsh glob qualifiers, indirect parameter flags, and disown
/// operators while rejecting code qualifiers". Adapted: a glob qualifier is
/// Zsh-only syntax (a syntax error in Bash), so the scanner refuses every
/// one, the code-running ones included; Zsh's `${(P)name}` scans but is
/// flagged dynamic when the named value is not static.
#[test]
fn oc_scan_refuses_zsh_glob_qualifiers_including_code_qualifiers() {
    assert!(analyze(
        "for f in locks/waiter-*(N); do echo \"${(P)f}\"; done; ls -d src/**/target(N/) *.ts(.OL[1,3]); nohup sleep 1 >/dev/null 2>&1 &! ; echo started"
    )
    .is_err());
    for unsafe_source in [
        "echo *(e:'rm -rf /':)",
        "echo *(+cmd)",
        "echo *(+1)",
        "echo *(P:-f:)",
        "echo *(u:root:)",
        "echo *(:Q)",
    ] {
        assert!(analyze(unsafe_source).is_err(), "{unsafe_source:?}");
    }
    let a = shell_scan::analyze(
        "a=(1); r='a[$(printf pwn)]'; echo ${(P)r}",
        Path::new("/workspace"),
    )
    .unwrap();
    assert_eq!(a.dynamic.as_deref(), Some("dynamic-execution"));
}

/// "%s rejects variable sinks when a substitution can build the subscript:
/// %s". Adapted: the input is flagged dynamic (asked about as a whole, never
/// allowed by a rule) rather than refused.
#[test]
fn oc_scan_rejects_variable_sinks_when_a_substitution_can_build_the_subscript() {
    for source in [
        "x=\"a$(printf '\\x5b')\\$(touch pwn)]\"; echo $((x))",
        "x=$(printf 'a\\x5b\\x24\\x28touch pwn\\x29]'); echo $((x))",
        "x=$(echo 'a{$(touch pwn)]' | tr '{' '\\133'); (( x ))",
        "F='$HOME/bin'; SIZE=$(stat -f %z file); N=$(( (SIZE + 1023) / 1024 )); echo $N",
        "r=\"a$(printf '\\x5b')\\$(touch pwn)]\"; echo ${(P)r}",
        // Found porting these: Zsh evaluates these builtins' operands and
        // these variables' values as arithmetic too (with `a=(1)`, each runs
        // `touch pwn`; `RANDOM` and `OPTIND` in Bash as well).
        "shift 'a[$(touch pwn)]'",
        "exit 'a[$(touch pwn)]'",
        "f() { return 'a[$(touch pwn)]'; }; f",
        "for i in 1; do break 'a[$(touch pwn)]'; done",
        "v='a[$(touch pwn)]'; noglob shift \"$v\"",
        "repeat 'a[$(touch pwn)]' true",
        "printf '%d' 'a[$(touch pwn)]'",
        "printf '%.*s' 'a[$(touch pwn)]' x",
        "print -f '%d' 'a[$(touch pwn)]'",
        // A format given with other options, attached, or after `-v`.
        "print -rf '%d' 'a[$(touch pwn)]'",
        "print -f%d 'a[$(touch pwn)]'",
        "print -v v -f '%d' 'a[$(touch pwn)]'",
        "printf -v v '%d' 'a[$(touch pwn)]'",
        "printf -vv -- '%d' 'a[$(touch pwn)]'",
        "RANDOM='a[$(touch pwn)]'",
        "OPTIND='a[$(touch pwn)]'",
        "export SECONDS='a[$(touch pwn)]'",
        "typeset -g NULLCMD=touch; >pwn",
    ] {
        let a = shell_scan::analyze(source, Path::new("/workspace")).unwrap();
        assert_eq!(
            a.dynamic.as_deref(),
            Some("dynamic-execution"),
            "{source:?}"
        );
    }
    // A plain number, or a parameter that is always one, is no sink.
    for source in [
        "printf '%s\\n' '$HOME'; exit 1",
        "echo '$x'; exit $?",
        "echo '$x'; shift 2; RANDOM=7",
        "echo '$x'; print -rf '%s' 'a[$(touch pwn)]'",
    ] {
        let a = shell_scan::analyze(source, Path::new("/workspace")).unwrap();
        assert_eq!(a.dynamic, None, "{source:?}");
    }
}

// --- shell-scan/adversarial.test.ts -------------------------------------

/// "scans repeated zsh q flags in bounded time"
#[test]
fn oc_scan_scans_repeated_zsh_q_flags_in_bounded_time() {
    let start = Instant::now();
    let _ = shell_scan::scan(&format!("echo ${{({}!x}}", "q".repeat(40)));
    let _ = shell_scan::scan(&format!("echo ${{({}!x}}", "q-".repeat(40)));
    // OpenCode allows 500ms; a loaded test machine gets more.
    assert!(start.elapsed() < Duration::from_secs(5));
}

// --- shell-scan/arithmetic.test.ts --------------------------------------

/// "${shell} evaluates %s independently": the shell's value and the probes
/// it runs, against the scan, for each expression bare and quoted.
#[test]
fn oc_scan_arithmetic_agrees_with_real_shells() {
    let expressions = [
        ("$((1+1))", "2"),
        ("$(((1 + 2) * (3 + (4))))", "21"),
        ("$((value + $other + ${third}))", "9"),
        ("$((value += 2, value > 1 ? value << 2 : ~value))", "16"),
        ("$(((16#ff & 0xff) | (2 ** 3)))", "255"),
        ("$((1 + $((2 * 3))))", "7"),
        ("$((1 + $(scan_probe)))", "3"),
        ("$((1 + `scan_probe`))", "3"),
        ("$(((1 + $(scan_probe)) * $((2 + $(scan_probe)))))", "12"),
        ("$((1 + $(printf \"%s\" \"$(scan_probe)\")))", "3"),
    ];
    for name in ["bash", "zsh"] {
        let Some(exe) = shell(name) else { continue };
        for (expression, output) in expressions {
            for argument in [expression.to_string(), format!("\"{expression}\"")] {
                let source = format!("value=2; other=3; third=4; printf '%s\\n' {argument}");
                let (code, stdout, stderr) = run(
                    &exe,
                    "scan_probe() { printf 'scan_probe\\n' >&2; printf 2; }; ",
                    &source,
                );
                assert_eq!(code, Some(0), "{name}: {source:?}: {stderr}");
                assert_eq!(stdout, format!("{output}\n"), "{name}: {source:?}");
                let commands =
                    shell_scan::scan(&source).unwrap_or_else(|e| panic!("{source:?}: {e}"));
                let observed = stderr.lines().filter(|l| !l.is_empty()).count();
                assert_eq!(observed, expression.matches("scan_probe").count());
                assert_eq!(
                    commands
                        .iter()
                        .filter(|c| c.words.first().map(String::as_str) == Some("scan_probe"))
                        .count(),
                    observed,
                    "{source:?}"
                );
                assert_eq!(
                    commands[0].words,
                    owned(&["printf", "%s\\n", expression]),
                    "{source:?}"
                );
            }
        }
    }
}

// --- shell-scan/bash-safety.test.ts -------------------------------------

/// "${shell} command syntax is visible: %s": the shell runs the probe, and
/// the scan names the command it ran.
#[test]
fn oc_scan_command_syntax_is_visible() {
    let executions = [
        ("printf safe; \"scan_probe\"", "scan_probe"),
        ("printf safe; 'scan_probe'", "scan_probe"),
        ("printf safe; $(printf scan_probe)", "$(printf scan_probe)"),
        ("X=${unset:-a b} scan_probe", "scan_probe"),
        ("X=value # comment\nscan_probe", "scan_probe"),
        ("printf \"%s\" `\\$(scan_probe)`", "scan_probe"),
        (
            "printf %s `printf \\\\\"; scan_probe; printf \\\\\"`",
            "scan_probe",
        ),
        ("if true; then X=x scan_probe; fi", "scan_probe"),
        ("if true; then >/dev/null X=x scan_probe; fi", "scan_probe"),
        ("printf safe; { scan_probe; }", "scan_probe"),
        ("if true; then { scan_probe; }; fi", "scan_probe"),
        ("s{can_probe,can_probe}", "s{can_probe,can_probe}"),
        ("printf safe; # comment\n\"scan_probe\"", "scan_probe"),
        ("printf safe; \\\n'scan_probe'", "scan_probe"),
    ];
    for name in ["bash", "zsh"] {
        let Some(exe) = shell(name) else { continue };
        for (source, head) in executions {
            let (_, _, stderr) = run(&exe, "scan_probe() { printf 'executed\\n' >&2; }; ", source);
            assert!(
                stderr.contains("executed\n"),
                "{name}: {source:?}: {stderr}"
            );
            let found = heads(source).unwrap_or_else(|e| panic!("{source:?}: {e}"));
            assert!(found.iter().any(|h| h == head), "{source:?}: {found:?}");
        }
    }
}

/// "${shell}: %s": each word form in each context, under Bash, Zsh and
/// dash (or `sh`); every probe the shell runs is a command the scan names.
#[test]
fn oc_scan_names_every_probe_the_shell_runs_in_every_context() {
    let probes = ["scan_first", "scan_second", "scan_third"];
    let words = [
        "scan_first",
        "'scan_first'",
        "s\"can_\"first",
        "scan_first 'literal; $(not_a_command)'",
        "X=value scan_first",
        "X=\"$(scan_second)\" scan_first",
        "scan_first \"$(scan_second)\"",
        "scan_first `scan_second`",
        "scan_first \"$(printf \"'\"; scan_second)\"",
    ];
    let contexts: &[fn(&str) -> String] = &[
        |s| s.to_string(),
        |s| format!("\n\n{s}\n\n"),
        |s| format!("{s}; scan_third"),
        |s| format!("{s} && scan_third"),
        |s| format!("{s} | scan_third"),
        |s| format!("({s})"),
        |s| format!("{{ {s}; }}"),
        |s| format!("scan_third \"$({s})\""),
        |s| format!("if true; then {s}; fi"),
        |s| format!("Y=\"$({s})\" scan_third"),
        |s| format!("{s} >/dev/null"),
    ];
    let prelude: String = probes
        .iter()
        .map(|p| format!("{p}() {{ printf '{p}\\n' >&2; }}; "))
        .collect();
    let posix = shell("dash").or_else(|| shell("sh"));
    for exe in [shell("bash"), shell("zsh"), posix].into_iter().flatten() {
        let mut sources: Vec<&str> = words.to_vec();
        if exe.ends_with("/bash") || exe.ends_with("/zsh") {
            sources.extend([
                "scan_first <(scan_second)",
                "values=(value \"$(scan_second)\"); scan_first",
            ]);
        }
        for word in sources {
            for context in contexts {
                let source = context(word);
                let (code, _, stderr) = run(&exe, &prelude, &source);
                assert_eq!(code, Some(0), "{exe}: {source:?}: {stderr}");
                let observed: Vec<&str> = stderr.lines().filter(|l| !l.is_empty()).collect();
                assert!(!observed.is_empty(), "{exe}: {source:?}");
                assert!(
                    observed.iter().all(|n| probes.contains(n)),
                    "{exe}: {stderr}"
                );
                let found = heads(&source).unwrap_or_else(|e| panic!("{source:?}: {e}"));
                for name in observed {
                    assert!(
                        found.iter().any(|h| h == name),
                        "{exe}: {source:?} ran {name}, scan found {found:?}"
                    );
                }
            }
        }
    }
}

/// "${shell} preserves commands around redirect-only syntax: %s"
#[test]
fn oc_scan_preserves_commands_around_redirect_only_syntax() {
    let prelude = "scan_probe() { printf 'scan_probe\\n' >&2; }; scan_target() { printf 'scan_target\\n' >&2; printf /dev/null; }; ";
    for name in ["bash", "zsh"] {
        let Some(exe) = shell(name) else { continue };
        for source in [
            "VALUE=ok >/dev/null; scan_probe",
            ">/dev/null; scan_probe",
            "(scan_probe) >/dev/null",
            "{ scan_probe; } >/dev/null",
            "{ scan_probe; } >$(scan_target)",
            "VALUE=ok >$(scan_target); scan_probe",
        ] {
            let (code, stdout, stderr) = run(&exe, prelude, source);
            assert_eq!(code, Some(0), "{name}: {source:?}: {stderr}");
            assert_eq!(stdout, "");
            let mut observed: Vec<&str> = stderr.lines().collect();
            observed.sort();
            let expected = if source.contains("scan_target") {
                vec!["scan_probe", "scan_target"]
            } else {
                vec!["scan_probe"]
            };
            assert_eq!(observed, expected, "{name}: {source:?}");
            let commands = shell_scan::scan(source).unwrap_or_else(|e| panic!("{source:?}: {e}"));
            let mut ran: Vec<&str> = commands
                .iter()
                .filter(|c| c.redirect_prefix_end.is_none() && c.redirect_word_count.is_none())
                .filter_map(|c| c.words.first().map(String::as_str))
                .collect();
            ran.sort();
            assert_eq!(ran, expected, "{source:?}");
            assert!(
                commands
                    .iter()
                    .any(|c| c.redirect_prefix_end.is_some() || c.redirect_word_count.is_some()),
                "{source:?}"
            );
        }
    }
}

// --- shell-scan/bash-syntax.test.ts -------------------------------------

/// "${shell} runs only the explicitly found probes: %s"
#[test]
fn oc_scan_finds_only_the_probes_the_shell_runs() {
    let prelude = "scan_probe() { printf 'scan_probe\\n' >&2; printf 1; }; scan_ignored() { printf 'unexpected\\n' >&2; }; ";
    for name in ["bash", "zsh"] {
        let Some(exe) = shell(name) else { continue };
        for source in [
            "if true; then VALUE=$(scan_probe); fi",
            "for value in one; do if true; then scan_probe; fi; done",
            "f() { case value in value) scan_probe;; esac; }; f",
            "printf '%s' \"$(case value in (value) scan_probe;; esac)\"",
            "cat <<EOF\n'$(scan_probe)'\nEOF",
            "cat <<A <<'B'\n$(scan_probe)\nA\n$(scan_ignored)\nB",
            "cat <<< \"$(scan_probe)\"",
            "printf '%s' \"${unset:-'$(scan_probe)'}\"",
            "printf '%s' `printf \\`scan_probe\\``",
            "array=(one \"$(scan_probe)\"); :",
            "(( value = $(scan_probe) + 1 )); :",
        ] {
            let (code, _, stderr) = run(&exe, prelude, source);
            assert_eq!(code, Some(0), "{name}: {source:?}: {stderr}");
            assert_eq!(stderr, "scan_probe\n", "{name}: {source:?}");
            let found = heads(source).unwrap_or_else(|e| panic!("{source:?}: {e}"));
            assert_eq!(
                found.iter().filter(|h| *h == "scan_probe").count(),
                1,
                "{source:?}: {found:?}"
            );
            assert!(!found.iter().any(|h| h == "scan_ignored"), "{source:?}");
        }
    }
}

/// "real Bash agrees with delimiter quoting: %s": what Bash runs and
/// prints, and that the scan finds the probe exactly when Bash runs it.
#[test]
fn oc_scan_real_bash_agrees_with_delimiter_quoting() {
    let Some(bash) = shell("bash") else { return };
    for source in [
        "(cat <<< hello\nprintf done)",
        "printf %s \"$(cat <<< hello\nprintf done)\"",
        "(cat <<\"E\\OF\"\n$(scan_probe)\nE\\OF\nprintf done)",
        "(cat <<\"E\\$OF\"\n$(scan_probe)\nE$OF\nprintf done)",
        "(cat <<''\n$(scan_probe)\n\nprintf done)",
        "(cat <<$'E\\x4fF'\n$(scan_probe)\nEOF\nprintf done)",
        "(cat <<-\"E\\OF\"\n\t$(scan_probe)\n\tE\\OF\nprintf done)",
        "(cat <<EO\\\nF\n$(scan_probe)\nEOF\nprintf done)",
    ] {
        let (code, stdout, stderr) = run(
            &bash,
            "scan_probe() { printf 'executed\\n' >&2; }; ",
            source,
        );
        assert_eq!(code, Some(0), "{source:?}");
        let continued = source.contains("<<EO\\\nF");
        assert_eq!(
            stderr,
            if continued { "executed\n" } else { "" },
            "{source:?}"
        );
        let want = if source.contains("<<<") {
            "hello\ndone"
        } else if continued {
            "\ndone"
        } else {
            "$(scan_probe)\ndone"
        };
        assert_eq!(stdout, want, "{source:?}");
        // The scan never misses the probe Bash runs: a delimiter split by a
        // continuation is refused (see the first backlog pass).
        match heads(source) {
            Ok(found) => assert_eq!(
                found.iter().any(|h| h == "scan_probe"),
                continued,
                "{source:?}: {found:?}"
            ),
            Err(_) => assert!(continued, "{source:?} is refused"),
        }
    }
}

// --- shell-scan/compound-parity.test.ts ---------------------------------

const BODIES: &[&str] = &[
    "for value in one two; do scan_probe; done",
    "while true; do scan_probe; break; done",
    "until false; do scan_probe; break; done",
    "case value in value) scan_probe;; *) scan_other;; esac",
];

/// "keeps compound function bodies inside command substitutions: %s".
/// Differs on purpose for the `case` body: an unparenthesized `case` inside
/// a quoted substitution is refused as unanalyzable (see the first backlog
/// pass), which asks.
#[test]
fn oc_scan_keeps_compound_function_bodies_inside_command_substitutions() {
    for body in BODIES {
        let source = format!("printf '%s' \"$( probe() {body}; probe )\"");
        if body.starts_with("case") {
            assert_eq!(
                shell_scan::scan(&source).err(),
                Some("command-substitution")
            );
            continue;
        }
        let commands = shell_scan::scan(&source).unwrap_or_else(|e| panic!("{source:?}: {e}"));
        assert_eq!(commands[0].resource, source);
        assert!(
            commands.iter().any(|c| c.words[0] == "scan_probe"),
            "{source:?}"
        );
        assert_eq!(commands.last().unwrap().words, owned(&["probe"]));
    }
}

/// Zsh's short `for name (words) body` loops, each with its long form.
fn short_loops() -> Vec<(String, String)> {
    let values = [
        "one two",
        "'two words' one",
        "'cd' '/outside'",
        "'do' 'done'",
        "'(literal)' '$(scan_ignored)'",
        "one\\\ntwo",
        "$(printf one)",
        "\"$(printf one)\"",
        "<(printf one)",
        "",
    ];
    let mut out = Vec::new();
    for value in values {
        let equivalent = format!("for value in {value}; do scan_probe \"$value\"; done");
        let mut forms = Vec::new();
        forms.push(format!("for value ({value}) scan_probe \"$value\""));
        forms.push(format!("for value ({value}) {{ scan_probe \"$value\"; }}"));
        if !value.is_empty() {
            forms.push(format!(
                "for value ({value}) do scan_probe \"$value\"; done"
            ));
            forms.push(format!(
                "for value ({value}); do scan_probe \"$value\"; done"
            ));
            forms.push(format!(
                "for value ({value})\ndo scan_probe \"$value\"; done"
            ));
            forms.push(format!(
                "for value ({value}) # ignored\ndo scan_probe \"$value\"; done"
            ));
            forms.push(format!(
                "for value ({value}) \\\ndo scan_probe \"$value\"; done"
            ));
        }
        out.extend(forms.into_iter().map(|f| (f, equivalent.clone())));
    }
    out
}

const CONTEXTS: &[fn(&str) -> String] = &[
    |s| s.to_string(),
    |s| format!("( {s} )"),
    |s| format!("{{ {s}; }}"),
    |s| format!("if true; then {s}; fi"),
    |s| format!("outer() {{ {s}; }}; outer"),
];

/// "Zsh parenthesized loop acceptance" (`{source}`): each short loop, in
/// each context, scans as the same `for ... in ...; do ...; done` loop.
#[test]
fn oc_scan_reads_zsh_short_for_loops_as_their_long_form() {
    for (source, equivalent) in short_loops() {
        for context in CONTEXTS {
            let source = context(&source);
            let equivalent = context(&equivalent);
            assert_eq!(analyze(&source), analyze(&equivalent), "{source:?}");
            assert!(analyze(&source).is_ok(), "{source:?}");
        }
    }
}

/// "retains commands in nested shorthand loops: %s"
#[test]
fn oc_scan_retains_commands_in_nested_shorthand_loops() {
    for source in [
        "for x (one two) for y (a b) scan_probe",
        "for x (one two) scan_probe && scan_other",
        "for x (one two) scan_probe | scan_other",
        "printf '%s' \"$(for x (one two) scan_probe)\"",
        "for x (one two) { for y (a b); do scan_probe; done; }",
        "for x (one two) [[ $(scan_probe) == ok ]]",
        "for x (one two) (( 1 + $(scan_probe) ))",
    ] {
        let found = heads(source).unwrap_or_else(|e| panic!("{source:?}: {e}"));
        assert!(
            found.iter().any(|h| h == "scan_probe"),
            "{source:?}: {found:?}"
        );
        if source.contains("scan_other") {
            assert!(
                found.iter().any(|h| h == "scan_other"),
                "{source:?}: {found:?}"
            );
        }
    }
}

/// "${shell}: %s" (real-shell compound syntax): the shell runs the probe
/// in a function whose body is a bare compound command, and the scan reads
/// it; Zsh's short loops run in Zsh and are refused by the scan.
#[test]
fn oc_scan_real_shells_run_compound_function_bodies() {
    for name in ["bash", "zsh"] {
        let Some(exe) = shell(name) else { continue };
        let mut sources: Vec<String> = BODIES
            .iter()
            .map(|b| format!("probe() {b}; probe"))
            .chain(
                BODIES
                    .iter()
                    .map(|b| format!("printf '%s' \"$(probe() {b}; probe)\"")),
            )
            .collect();
        let functions = sources.len();
        if name == "zsh" {
            sources.extend(short_loops().into_iter().map(|(s, _)| s));
        }
        for (i, source) in sources.iter().enumerate() {
            let (code, _, stderr) = run(
                &exe,
                "scan_probe() { printf 'executed\\n' >&2; }; ",
                &format!("{source}; wait"),
            );
            assert_eq!(code, Some(0), "{name}: {source:?}: {stderr}");
            if !source.contains("value ()") {
                assert!(
                    stderr.contains("executed\n"),
                    "{name}: {source:?}: {stderr}"
                );
            }
            match heads(source) {
                Ok(found) => assert!(
                    found.iter().any(|h| h == "scan_probe") || source.contains("value ()"),
                    "{source:?}: {found:?}"
                ),
                // An unparenthesized `case` inside a substitution is refused.
                Err(_) => assert!(
                    i < functions && source.starts_with("printf") && source.contains("case"),
                    "{source:?} is refused"
                ),
            }
        }
    }
}

// --- shell-scan/generated.test.ts ---------------------------------------

const STATIC_COMMANDS: &[(&str, &[&str])] = &[
    ("git status", &["git", "status"]),
    ("printf ok", &["printf", "ok"]),
    ("curl example.com", &["curl", "example.com"]),
];

/// "decomposes every combination of static commands and separators"
#[test]
fn oc_scan_decomposes_every_combination_of_static_commands_and_separators() {
    for (left, left_words) in STATIC_COMMANDS {
        for separator in [" ; ", " && ", " || ", " | ", " |& ", "\n"] {
            for (right, right_words) in STATIC_COMMANDS {
                let source = format!("{left}{separator}{right}");
                let commands = shell_scan::scan(&source).unwrap();
                let got: Vec<(String, Vec<String>)> = commands
                    .iter()
                    .map(|c| (c.resource.clone(), c.words.clone()))
                    .collect();
                assert_eq!(
                    got,
                    vec![
                        (left.to_string(), owned(left_words)),
                        (right.to_string(), owned(right_words)),
                    ],
                    "{source:?}"
                );
            }
        }
    }
}

/// "keeps quoted and escaped separators in arguments"
#[test]
fn oc_scan_keeps_quoted_and_escaped_separators_in_arguments() {
    for literal in [";", "|", "&", "#", "<", ">"] {
        for form in [
            format!("'left{literal}right'"),
            format!("\"left{literal}right\""),
            format!("left\\{literal}right"),
        ] {
            let source = format!("printf %s {form}");
            let commands = shell_scan::scan(&source).unwrap();
            assert_eq!(commands.len(), 1, "{source:?}");
            assert_eq!(commands[0].resource, source);
            assert_eq!(
                commands[0].words,
                owned(&["printf", "%s", &format!("left{literal}right")])
            );
        }
    }
}

/// "fails closed when valid commands are mutated with malformed syntax"
#[test]
fn oc_scan_fails_closed_when_valid_commands_are_mutated_with_malformed_syntax() {
    let mutations: &[fn(&str) -> String] = &[
        |c| format!("{c} \"unterminated"),
        |c| format!("{c} 'unterminated"),
        |c| format!("{c} \\"),
        |c| format!("{c} &&"),
        |c| format!("| {c}"),
        |c| format!("{c} || || printf reached"),
        |c| format!("{c} >"),
        |c| format!("{c} > > output"),
    ];
    for (command, _) in STATIC_COMMANDS {
        for mutate in mutations {
            let source = mutate(command);
            assert!(shell_scan::scan(&source).is_err(), "{source:?}");
        }
    }
}

/// "retains generated dynamic command heads without resolving them"
#[test]
fn oc_scan_retains_generated_dynamic_command_heads_without_resolving_them() {
    for head in [
        "$COMMAND",
        "${COMMAND}",
        "pre$COMMAND",
        "\"$COMMAND\"",
        "$(printf git)",
        "`printf git`",
    ] {
        for tail in ["status", "--version", "-rf /"] {
            let source = format!("{head} {tail}");
            let commands = shell_scan::scan(&source).unwrap_or_else(|e| panic!("{source:?}: {e}"));
            assert_eq!(commands[0].raw_words[0], head, "{source:?}");
            assert_eq!(commands[0].words[0], head.replace('"', ""), "{source:?}");
        }
    }
}

/// "keeps wrappers and shell evaluators at their delegated boundary"
#[test]
fn oc_scan_keeps_wrappers_and_shell_evaluators_at_their_delegated_boundary() {
    for prefix in ["", "FOO=bar ", "FOO=bar BAR=baz "] {
        for command in [
            "command git status",
            "builtin printf ok",
            "exec git status",
            "env FOO=bar git status",
            "sudo git status",
            "nice git status",
            "nohup git status",
            "xargs rm",
            "source ./script.sh",
            ". ./script.sh",
            "trap 'git status' EXIT",
            "eval 'git status'",
            "bash -c 'git status'",
            "/bin/sh ./script.sh",
            "time git status",
        ] {
            let source = format!("{prefix}{command}");
            assert!(shell_scan::scan(&source).is_ok(), "{source:?}");
        }
    }
}
