//! Folders and files a command names in its arguments: options that run a
//! program in another folder or point it at a repository elsewhere (`git
//! -C`, `make -C`, `env -C`, `tar -C`, `npm --prefix`...), and the operands
//! of programs that write, move or delete files by name (`rm`, `cp`, `mv`,
//! `perl -i`, `curl -o`...). Folders are checked like `cd` targets
//! ([`Analysis::directories`]), files like redirect targets
//! ([`Analysis::writes`]). A target only known at run time is checked the
//! way a computed `cd` or redirect target is.
//!
//! Each command is also classified as reading or not (see
//! [`Analysis::mutating`]), so a request for a folder outside the project
//! can say whether the command only reads there.

use super::{expand_known_directory, is_static_word, push_write, tilde_prefix, Analysis, Write};
use std::path::Path;

/// What an option's value or an operand is to the program.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Role {
    /// A folder the program changes to; later folders of the same kind are
    /// relative to it (`git -C a -C b`, `make -C a -C b`).
    Cd,
    /// A folder the program works in, relative to the last [`Role::Cd`]
    /// (`git --git-dir`, `cmake -S`).
    Dir,
    /// A file whose folder the program works in (`cargo --manifest-path`).
    Parent,
    /// A file or folder the program writes, creates, moves or deletes.
    Write,
    /// A Maven `-f`: a `pom.xml` (its folder) or a folder.
    Pom,
}

/// A value as written (`raw`, for the static check) and with quotes removed.
#[derive(Debug, Clone)]
pub(super) struct Val {
    raw: String,
    value: String,
    /// Whether a leading `~` is the shell's (the value is a word of its own).
    tilde: bool,
}

/// How a program reads its arguments: short letters and long options that
/// take a value, so values are not taken for operands.
struct Grammar {
    short_args: &'static str,
    long_args: &'static [&'static str],
    /// Options whose value is a target: `-C` (short) or `--directory`.
    targets: &'static [(&'static str, Role)],
}

const NONE: Grammar = Grammar {
    short_args: "",
    long_args: &[],
    targets: &[],
};

/// Options (`(name, value)`, the name as `-x` or `--long`) and operands of
/// `cooked[from..end]`, read like getopt with GNU's permutation: options
/// after operands still count, `--` ends them.
fn getopt(
    raw: &[String],
    cooked: &[String],
    from: usize,
    end: usize,
    g: &Grammar,
) -> (Vec<(String, Val)>, Vec<Val>) {
    let end = end.min(cooked.len());
    let mut opts = Vec::new();
    let mut operands = Vec::new();
    let whole = |k: usize| Val {
        raw: raw[k].clone(),
        value: cooked[k].clone(),
        tilde: tilde_prefix(&raw[k], &cooked[k]),
    };
    let attached = |k: usize, value: &str| Val {
        raw: raw[k].clone(),
        value: value.to_string(),
        tilde: false,
    };
    let mut i = from;
    let mut options_done = false;
    while i < end {
        let w = cooked[i].as_str();
        if options_done || w == "-" || !w.starts_with('-') {
            operands.push(whole(i));
            i += 1;
            continue;
        }
        if w == "--" {
            options_done = true;
            i += 1;
            continue;
        }
        if let Some(long) = w.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (format!("--{n}"), Some(v)),
                None => (w.to_string(), None),
            };
            let takes =
                g.long_args.contains(&name.as_str()) || g.targets.iter().any(|(t, _)| *t == name);
            match (takes, value) {
                (_, Some(v)) => opts.push((name, attached(i, v))),
                (true, None) if i + 1 < end => {
                    opts.push((name, whole(i + 1)));
                    i += 1;
                }
                (true, None) => {}
                (false, None) => opts.push((name, attached(i, ""))),
            }
            i += 1;
            continue;
        }
        let letters: Vec<char> = w[1..].chars().collect();
        let mut next = i + 1;
        for (j, &c) in letters.iter().enumerate() {
            let name = format!("-{c}");
            let takes = g.short_args.contains(c) || g.targets.iter().any(|(t, _)| *t == name);
            if !takes {
                opts.push((name, attached(i, "")));
                continue;
            }
            let rest: String = letters[j + 1..].iter().collect();
            if !rest.is_empty() {
                opts.push((name, attached(i, &rest)));
            } else if i + 1 < end {
                opts.push((name, whole(i + 1)));
                next = i + 2;
            }
            break;
        }
        i = next;
    }
    (opts, operands)
}

fn has(opts: &[(String, Val)], names: &[&str]) -> bool {
    opts.iter().any(|(n, _)| names.contains(&n.as_str()))
}

/// The folder later relative targets resolve against: `Some("")` for the
/// working directory, `None` once a folder is only known at run time.
pub(super) struct Chain {
    base: Option<String>,
}

impl Chain {
    pub(super) fn new() -> Self {
        Chain {
            base: Some(String::new()),
        }
    }

    /// `v` as the shell and the program resolve it, relative to the
    /// working directory: `None` when only known at run time.
    fn resolve(&self, v: &Val, cwd: &Path) -> Option<String> {
        // `src/{a,b}` is in `src`; `{a,../b}` could be anywhere.
        let (raw, value) = (tame_braces(&v.raw)?, tame_braces(&v.value)?);
        if !is_static_word(&raw) {
            return None;
        }
        let value = expand_known_directory(&value, cwd, v.tilde)?;
        // A glob names files in its fixed folder (`../x/*.o` is in `../x`).
        let value = match value.find(['*', '?', '[']) {
            Some(i) => match value[..i].rfind('/') {
                Some(0) => "/".to_string(),
                Some(j) => value[..j].to_string(),
                None => ".".to_string(),
            },
            None => value,
        };
        if Path::new(&value).is_absolute() {
            return Some(value);
        }
        let base = self.base.as_ref()?;
        Some(if base.is_empty() {
            value
        } else {
            Path::new(base).join(&value).display().to_string()
        })
    }

    fn push(&mut self, role: Role, v: &Val, cwd: &Path, out: &mut Analysis) {
        if role == Role::Write {
            if v.value == "-" || v.value == "{}" {
                // Standard output; `find -exec`'s file, its start folder
                // checked instead.
                return;
            }
            match self.resolve(v, cwd) {
                Some(path) if !path.is_empty() => {
                    out.writes.push(Write {
                        raw: v.raw.clone(),
                        path: Some(path),
                    });
                    if v.value.starts_with('~') && v.tilde {
                        // Also as written, as `push_write` does.
                        push_write(out, &v.raw, &v.value, cwd);
                    }
                }
                _ => unknown_write(out, &v.raw),
            }
            return;
        }
        if v.value.is_empty() && role == Role::Cd {
            return; // `git -C ""` stays where it is
        }
        let resolved = self.resolve(v, cwd);
        let dir = match (role, &resolved) {
            (Role::Parent, Some(p)) => Some(parent(p)),
            (Role::Pom, Some(p)) if p.ends_with(".xml") => Some(parent(p)),
            _ => resolved.clone(),
        };
        match dir {
            Some(d) => {
                let d = if d.is_empty() { ".".to_string() } else { d };
                out.directories.push(d.clone());
                if role == Role::Cd {
                    self.base = Some(d);
                }
            }
            None => {
                out.unknown_cwd = true;
                if role == Role::Cd {
                    self.base = None;
                }
            }
        }
    }
}

/// `s` with each brace group that only picks among plain names (`{a,b}`,
/// `{1..3}`) replaced by its first choice, which names the same folder;
/// `None` when a group could name another folder (`{a,../b}`).
fn tame_braces(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find('{') {
        let Some(len) = rest[open..].find('}') else {
            break;
        };
        let body = &rest[open + 1..open + len];
        let expands = body.contains(',') || body.contains("..");
        if !expands {
            out.push_str(&rest[..open + len + 1]);
        } else {
            let choices: Vec<&str> = if body.contains(',') {
                body.split(',').collect()
            } else {
                body.split("..").collect()
            };
            let plain = body.contains(',')
                && choices.iter().all(|c| {
                    !c.starts_with("..")
                        && c.chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || "_.-".contains(ch))
                })
                || !body.contains(',')
                    && choices
                        .iter()
                        .all(|c| c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-'));
            if !plain {
                return None;
            }
            out.push_str(&rest[..open]);
            let first = choices.first().copied().unwrap_or("");
            out.push_str(if first.is_empty() { "x" } else { first });
        }
        rest = &rest[open + len + 1..];
    }
    out.push_str(rest);
    Some(out)
}

fn parent(p: &str) -> String {
    Path::new(p)
        .parent()
        .map(|d| d.display().to_string())
        .unwrap_or_default()
}

/// A write whose path the scan cannot know (`patch < x.diff`, `xargs rm`).
fn unknown_write(out: &mut Analysis, raw: &str) {
    out.unknown_target.get_or_insert_with(|| raw.to_string());
}

/// Which operands a program writes.
#[derive(Clone, Copy, PartialEq)]
enum Operands {
    No,
    All,
    /// The last of two or more (a destination), or `-t DIR`.
    Last,
    /// All but the first of two or more (`chmod MODE file...`).
    AfterFirst,
    First,
    /// The second (`uniq in out`).
    Second,
}

/// The grammar of a program that names targets, by its base name, and
/// which operands it writes.
fn program(base: &str) -> Option<(Grammar, Operands)> {
    use Operands::*;
    use Role::*;
    Some(match base {
        "rm" | "unlink" | "rmdir" | "srm" => (NONE, All),
        "shred" => (
            Grammar {
                short_args: "ns",
                long_args: &["--iterations", "--size", "--random-source"],
                ..NONE
            },
            All,
        ),
        "touch" => (
            Grammar {
                short_args: "rtdA",
                long_args: &["--reference", "--date", "--time"],
                ..NONE
            },
            All,
        ),
        "mkdir" | "mkfifo" => (
            Grammar {
                short_args: "m",
                long_args: &["--mode", "--context"],
                ..NONE
            },
            All,
        ),
        "mknod" => (
            Grammar {
                short_args: "m",
                long_args: &["--mode", "--context"],
                ..NONE
            },
            First,
        ),
        "truncate" => (
            Grammar {
                short_args: "sr",
                long_args: &["--size", "--reference"],
                ..NONE
            },
            All,
        ),
        "chmod" | "chown" | "chgrp" | "chflags" | "lchown" | "lchmod" => (NONE, AfterFirst),
        "cp" | "ln" => (
            Grammar {
                short_args: "S",
                long_args: &["--suffix"],
                targets: &[("-t", Write), ("--target-directory", Write)],
            },
            Last,
        ),
        "mv" => (
            Grammar {
                short_args: "S",
                long_args: &["--suffix"],
                targets: &[("-t", Write), ("--target-directory", Write)],
            },
            All,
        ),
        "install" => (
            Grammar {
                short_args: "mogSf",
                long_args: &[
                    "--mode",
                    "--owner",
                    "--group",
                    "--suffix",
                    "--strip-program",
                ],
                targets: &[("-t", Write), ("--target-directory", Write)],
            },
            Last,
        ),
        "ditto" | "link" => (NONE, Last),
        "tee" => (NONE, All),
        "sort" => (
            Grammar {
                short_args: "kStT",
                long_args: &[
                    "--key",
                    "--buffer-size",
                    "--field-separator",
                    "--temporary-directory",
                    "--parallel",
                    "--files0-from",
                    "--batch-size",
                    "--compress-program",
                    "--sort",
                    "--random-source",
                ],
                targets: &[("-o", Write), ("--output", Write)],
            },
            No,
        ),
        "uniq" => (
            Grammar {
                short_args: "fsw",
                long_args: &["--skip-fields", "--skip-chars", "--check-chars"],
                ..NONE
            },
            Second,
        ),
        "gzip" | "gunzip" | "bzip2" | "bunzip2" | "xz" | "unxz" | "lzma" | "unlzma" | "pigz"
        | "compress" | "uncompress" | "zstd" | "unzstd" | "lz4" | "brotli" => (
            Grammar {
                short_args: "S",
                long_args: &["--suffix", "--output-dir-flat"],
                targets: &[("-o", Write), ("--output", Write)],
            },
            All,
        ),
        "zip" => (
            Grammar {
                short_args: "bnt",
                long_args: &["--temp-path", "--suffixes"],
                ..NONE
            },
            First,
        ),
        "unzip" => (
            Grammar {
                short_args: "P",
                targets: &[("-d", Write)],
                ..NONE
            },
            No,
        ),
        "curl" => (
            Grammar {
                short_args: "AbCdeEFHKmPQrtTuUwxXyYz",
                long_args: &[
                    "--data",
                    "--data-raw",
                    "--data-binary",
                    "--data-urlencode",
                    "--data-ascii",
                    "--form",
                    "--form-string",
                    "--header",
                    "--user",
                    "--user-agent",
                    "--referer",
                    "--request",
                    "--proxy",
                    "--max-time",
                    "--connect-timeout",
                    "--retry",
                    "--url",
                    "--cacert",
                    "--cert",
                    "--key",
                    "--upload-file",
                    "--write-out",
                    "--range",
                    "--resolve",
                    "--unix-socket",
                    "--limit-rate",
                    "--cookie",
                    "--etag-compare",
                    "--config",
                ],
                targets: &[
                    ("-o", Write),
                    ("--output", Write),
                    ("--output-dir", Write),
                    ("-D", Write),
                    ("--dump-header", Write),
                    ("-c", Write),
                    ("--cookie-jar", Write),
                    ("--trace", Write),
                    ("--trace-ascii", Write),
                    ("--stderr", Write),
                    ("--libcurl", Write),
                    ("--etag-save", Write),
                    ("--hsts", Write),
                    ("--alt-svc", Write),
                ],
            },
            No,
        ),
        "wget" => (
            Grammar {
                short_args: "iBetTwQlUDARIX",
                long_args: &[
                    "--input-file",
                    "--base",
                    "--execute",
                    "--tries",
                    "--timeout",
                    "--wait",
                    "--quota",
                    "--level",
                    "--user-agent",
                    "--domains",
                    "--accept",
                    "--reject",
                    "--header",
                    "--user",
                    "--password",
                ],
                targets: &[
                    ("-O", Write),
                    ("--output-document", Write),
                    ("-o", Write),
                    ("--output-file", Write),
                    ("-a", Write),
                    ("--append-output", Write),
                    ("-P", Write),
                    ("--directory-prefix", Write),
                    ("--save-cookies", Write),
                    ("--warc-file", Write),
                ],
            },
            No,
        ),
        "make" | "bmake" => (
            Grammar {
                short_args: "fIjlLoOWE",
                long_args: &[
                    "--file",
                    "--makefile",
                    "--include-dir",
                    "--jobs",
                    "--load-average",
                    "--old-file",
                    "--what-if",
                    "--new-file",
                    "--assume-new",
                    "--assume-old",
                    "--eval",
                ],
                targets: &[("-C", Cd), ("--directory", Cd)],
            },
            No,
        ),
        "npm" | "npx" => (
            Grammar {
                short_args: "w",
                long_args: &["--workspace", "--registry", "--loglevel", "--tag"],
                targets: &[("-C", Cd), ("--prefix", Cd)],
            },
            No,
        ),
        "pnpm" => (
            Grammar {
                short_args: "Fw",
                long_args: &["--filter", "--workspace"],
                targets: &[("-C", Cd), ("--dir", Cd), ("--prefix", Cd)],
            },
            No,
        ),
        "yarn" | "bun" => (
            Grammar {
                targets: &[("--cwd", Cd)],
                ..NONE
            },
            No,
        ),
        "cargo" => (
            Grammar {
                short_args: "pjZF",
                long_args: &[
                    "--package",
                    "--features",
                    "--jobs",
                    "--bin",
                    "--example",
                    "--test",
                    "--bench",
                    "--target",
                    "--profile",
                    "--color",
                    "--message-format",
                    "--config",
                    "--exclude",
                ],
                targets: &[
                    ("-C", Cd),
                    ("--manifest-path", Parent),
                    ("--target-dir", Write),
                    ("--artifact-dir", Write),
                    ("--out-dir", Write),
                    ("--root", Write),
                    ("--lockfile-path", Write),
                ],
            },
            No,
        ),
        "mvn" | "mvnw" => (
            Grammar {
                short_args: "DPslbTe",
                long_args: &[
                    "--projects",
                    "--settings",
                    "--threads",
                    "--activate-profiles",
                ],
                targets: &[("-f", Pom), ("--file", Pom)],
            },
            No,
        ),
        "gradle" | "gradlew" => (
            Grammar {
                short_args: "DPIx",
                long_args: &["--init-script", "--exclude-task", "--max-workers"],
                targets: &[
                    ("-p", Cd),
                    ("--project-dir", Cd),
                    ("-b", Parent),
                    ("--build-file", Parent),
                    ("-c", Parent),
                    ("--settings-file", Parent),
                    ("-g", Write),
                    ("--gradle-user-home", Write),
                    ("--project-cache-dir", Write),
                ],
            },
            No,
        ),
        "cmake" => (
            Grammar {
                short_args: "DCGTAUW",
                long_args: &[
                    "--preset",
                    "--target",
                    "--config",
                    "--parallel",
                    "--component",
                ],
                targets: &[
                    ("-S", Dir),
                    ("-B", Write),
                    ("--build", Cd),
                    ("--install", Cd),
                    ("--install-prefix", Write),
                    ("--prefix", Write),
                ],
            },
            No,
        ),
        "just" => (
            Grammar {
                targets: &[
                    ("-d", Cd),
                    ("--working-directory", Cd),
                    ("-f", Parent),
                    ("--justfile", Parent),
                ],
                ..NONE
            },
            No,
        ),
        "uv" => (
            Grammar {
                short_args: "p",
                long_args: &["--python", "--with", "--index-url"],
                targets: &[("--directory", Cd), ("--project", Dir), ("--target", Write)],
            },
            No,
        ),
        "poetry" => (
            Grammar {
                targets: &[
                    ("-C", Cd),
                    ("--directory", Cd),
                    ("-P", Dir),
                    ("--project", Dir),
                ],
                ..NONE
            },
            No,
        ),
        "pip" | "pip3" => (
            Grammar {
                short_args: "rceif",
                long_args: &[
                    "--requirement",
                    "--constraint",
                    "--editable",
                    "--index-url",
                    "--find-links",
                    "--extra-index-url",
                ],
                targets: &[
                    ("-t", Write),
                    ("--target", Write),
                    ("--prefix", Write),
                    ("--root", Write),
                    ("-d", Write),
                    ("--dest", Write),
                    ("--src", Write),
                    ("-w", Write),
                    ("--wheel-dir", Write),
                ],
            },
            No,
        ),
        "docker" | "docker-compose" | "podman" => (
            Grammar {
                targets: &[("--project-directory", Cd)],
                ..NONE
            },
            No,
        ),
        _ => return None,
    })
}

/// GNU builds Homebrew installs under a `g` prefix (`gcp`, `gmake`).
fn gnu_name(base: &str) -> &str {
    const GNU: &[&str] = &[
        "rm", "cp", "mv", "ln", "touch", "mkdir", "rmdir", "install", "tee", "truncate", "shred",
        "chmod", "chown", "chgrp", "sort", "uniq", "tar", "patch", "make", "unlink", "dd",
        "mkfifo", "mknod", "link",
    ];
    match base.strip_prefix('g') {
        Some(rest) if GNU.contains(&rest) => rest,
        _ => base,
    }
}

/// The targets a command names in its own arguments, `raw`/`cooked` from
/// the command name on.
pub(super) fn command_targets(
    base: &str,
    raw: &[String],
    cooked: &[String],
    cwd: &Path,
    out: &mut Analysis,
) {
    let base = gnu_name(base);
    match base {
        "tar" | "bsdtar" => return tar(raw, cooked, cwd, out),
        "patch" => return patch(raw, cooked, cwd, out),
        "dd" => {
            for k in 1..cooked.len() {
                if let Some(v) = cooked[k].strip_prefix("of=") {
                    let val = Val {
                        raw: raw[k].clone(),
                        value: v.to_string(),
                        tilde: false,
                    };
                    Chain::new().push(Role::Write, &val, cwd, out);
                }
            }
            return;
        }
        "terraform" | "tofu" => {
            for k in 1..cooked.len() {
                if let Some(v) = cooked[k].strip_prefix("-chdir=") {
                    let val = Val {
                        raw: raw[k].clone(),
                        value: v.to_string(),
                        tilde: false,
                    };
                    Chain::new().push(Role::Cd, &val, cwd, out);
                }
            }
            return;
        }
        "go" => return go(raw, cooked, cwd, out),
        _ if matches!(
            base.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.'),
            "perl" | "ruby"
        ) =>
        {
            return in_place(base, raw, cooked, cwd, out);
        }
        "rsync" | "scp" => return copy_dest(base, raw, cooked, cwd, out),
        "cmake" if cooked.get(1).is_some_and(|w| w == "-E") => {
            return cmake_tool(raw, cooked, cwd, out);
        }
        _ => {}
    }
    let Some((g, operands)) = program(base) else {
        return;
    };
    let (opts, ops) = getopt(raw, cooked, 1, cooked.len(), &g);
    let mut chain = Chain::new();
    for (name, v) in &opts {
        if let Some((_, role)) = g.targets.iter().find(|(t, _)| t == name) {
            chain.push(*role, v, cwd, out);
        }
    }
    if base == "curl" && has(&opts, &["-K", "--config"]) {
        // A config file may name an output anywhere.
        unknown_write(out, &cooked.join(" "));
    }
    let read_only_compress = matches!(
        base,
        "gzip"
            | "gunzip"
            | "bzip2"
            | "bunzip2"
            | "xz"
            | "unxz"
            | "lzma"
            | "unlzma"
            | "pigz"
            | "compress"
            | "uncompress"
            | "zstd"
            | "unzstd"
            | "lz4"
            | "brotli"
    ) && (has(
        &opts,
        &[
            "-c",
            "-l",
            "-t",
            "--stdout",
            "--to-stdout",
            "--list",
            "--test",
        ],
    ));
    let has_target = has(&opts, &["-t", "--target-directory"]);
    let writes: Vec<&Val> = match operands {
        _ if read_only_compress => Vec::new(),
        Operands::No => Vec::new(),
        Operands::All => ops.iter().collect(),
        Operands::Last if base == "install" && has(&opts, &["-d", "--directory"]) => {
            ops.iter().collect()
        }
        Operands::Last if has_target => Vec::new(),
        Operands::Last if ops.len() >= 2 => ops.last().into_iter().collect(),
        // `ln -s target` makes a link here; `cp x` fails.
        Operands::Last => Vec::new(),
        Operands::AfterFirst if has(&opts, &["--reference"]) => ops.iter().collect(),
        Operands::AfterFirst if ops.len() >= 2 => ops.iter().skip(1).collect(),
        // `chmod -x file`: the mode looked like an option.
        Operands::AfterFirst => ops.iter().collect(),
        Operands::First => ops.iter().take(1).collect(),
        Operands::Second => ops.iter().skip(1).take(1).collect(),
    };
    for v in writes {
        chain.push(Role::Write, v, cwd, out);
    }
}

fn tar(raw: &[String], cooked: &[String], cwd: &Path, out: &mut Analysis) {
    let g = Grammar {
        short_args: "bfFgHIKLNsTVX",
        long_args: &[
            "--file",
            "--files-from",
            "--exclude",
            "--exclude-from",
            "--format",
            "--use-compress-program",
            "--transform",
            "--strip-components",
            "--owner",
            "--group",
            "--mode",
            "--newer",
            "--blocking-factor",
        ],
        targets: &[
            ("-C", Role::Cd),
            ("--directory", Role::Cd),
            ("--cd", Role::Cd),
        ],
    };
    // Old style: `tar xzf archive.tgz`, the letters' values following.
    let mut words_raw = raw.to_vec();
    let mut words = cooked.to_vec();
    if let Some(first) = cooked
        .get(1)
        .filter(|w| !w.starts_with('-') && !w.is_empty())
    {
        if first.chars().all(|c| c.is_ascii_alphabetic()) {
            words[1] = format!("-{first}");
            words_raw[1] = format!("-{}", raw[1]);
        }
    }
    let (opts, _) = getopt(&words_raw, &words, 1, words.len(), &g);
    let extract = has(&opts, &["-x", "--extract", "--get"]);
    let create = has(
        &opts,
        &[
            "-c",
            "-r",
            "-u",
            "-A",
            "--create",
            "--append",
            "--update",
            "--catenate",
            "--concatenate",
            "--delete",
        ],
    );
    let mut chain = Chain::new();
    for (name, v) in &opts {
        match name.as_str() {
            "-C" | "--directory" | "--cd" => {
                chain.push(Role::Cd, v, cwd, out);
                if extract {
                    // Files are extracted there.
                    Chain::new().push(Role::Write, v, cwd, out);
                }
            }
            "-f" | "--file" if create => Chain::new().push(Role::Write, v, cwd, out),
            _ => {}
        }
    }
    if extract && has(&opts, &["-P", "--absolute-names"]) {
        // Members may name any path.
        unknown_write(out, &cooked.join(" "));
    }
}

fn patch(raw: &[String], cooked: &[String], cwd: &Path, out: &mut Analysis) {
    let g = Grammar {
        short_args: "DFgipVYz",
        long_args: &[
            "--ifdef",
            "--fuzz",
            "--get",
            "--input",
            "--strip",
            "--version-control",
            "--basename-prefix",
            "--suffix",
            "--quoting-style",
        ],
        targets: &[
            ("-d", Role::Cd),
            ("--directory", Role::Cd),
            ("-o", Role::Write),
            ("--output", Role::Write),
            ("-r", Role::Write),
            ("--reject-file", Role::Write),
            ("-B", Role::Write),
            ("--prefix", Role::Write),
        ],
    };
    let (opts, ops) = getopt(raw, cooked, 1, cooked.len(), &g);
    let mut chain = Chain::new();
    for (name, v) in &opts {
        if let Some((_, role)) = g.targets.iter().find(|(t, _)| t == name) {
            chain.push(*role, v, cwd, out);
        }
    }
    let dry = has(&opts, &["--dry-run", "-C", "--check"]);
    match ops.first() {
        Some(v) if !dry => chain.push(Role::Write, v, cwd, out),
        Some(_) => {}
        // The files a patch names: `patch` writes `../x` and absolute names
        // too (macOS's does), and only the patch says which.
        None if !dry => unknown_write(out, &cooked.join(" ")),
        None => {}
    }
}

fn go(raw: &[String], cooked: &[String], cwd: &Path, out: &mut Analysis) {
    // Go's flags are single-dash words: `-C dir`, `-o=out`.
    let mut chain = Chain::new();
    let mut k = 1;
    while k < cooked.len() {
        let w = cooked[k].as_str();
        let (name, attached) = match w.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (w, None),
        };
        let name = name.strip_prefix('-').unwrap_or(name);
        let name = name.strip_prefix('-').unwrap_or(name);
        let role = match name {
            "C" => Some(Role::Cd),
            "o" | "coverprofile" | "pkgdir" | "cpuprofile" | "memprofile" | "blockprofile"
            | "mutexprofile" | "trace" | "outputdir"
                if w.starts_with('-') =>
            {
                Some(Role::Write)
            }
            _ => None,
        };
        if let Some(role) = role.filter(|_| w.starts_with('-')) {
            let val = match attached {
                Some(v) => Some(Val {
                    raw: raw[k].clone(),
                    value: v.to_string(),
                    tilde: false,
                }),
                None if k + 1 < cooked.len() => {
                    k += 1;
                    Some(Val {
                        raw: raw[k].clone(),
                        value: cooked[k].clone(),
                        tilde: tilde_prefix(&raw[k], &cooked[k]),
                    })
                }
                None => None,
            };
            if let Some(v) = val {
                chain.push(role, &v, cwd, out);
            }
        }
        k += 1;
    }
}

/// `cmake -E <tool> args`: CMake's portable file commands.
fn cmake_tool(raw: &[String], cooked: &[String], cwd: &Path, out: &mut Analysis) {
    let Some(tool) = cooked.get(2) else {
        return;
    };
    let args = 3..cooked.len();
    let whole = |k: usize| Val {
        raw: raw[k].clone(),
        value: cooked[k].clone(),
        tilde: tilde_prefix(&raw[k], &cooked[k]),
    };
    let mut chain = Chain::new();
    match tool.as_str() {
        "chdir" => {
            if cooked.len() > 3 {
                chain.push(Role::Cd, &whole(3), cwd, out);
            }
        }
        "rm"
        | "remove"
        | "remove_directory"
        | "make_directory"
        | "touch"
        | "touch_nocreate"
        | "rename"
        | "copy"
        | "copy_directory"
        | "copy_directory_if_different"
        | "copy_if_different"
        | "create_symlink"
        | "create_hardlink"
        | "tar"
        | "true" => {
            if tool == "true" {
                return;
            }
            for k in args.filter(|&k| !cooked[k].starts_with('-')) {
                chain.push(Role::Write, &whole(k), cwd, out);
            }
        }
        _ => {}
    }
}

/// `git`'s own options: `-C` (each relative to the one before, as git
/// reads them), `--git-dir` and `--work-tree` (relative to the last `-C`).
/// Returns the folder later relative paths resolve against.
pub(super) fn git_globals(
    raw: &[String],
    cooked: &[String],
    cwd: &Path,
    out: &mut Analysis,
) -> Chain {
    let mut chain = Chain::new();
    let n = cooked.len();
    let mut i = 1;
    let whole = |k: usize| Val {
        raw: raw[k].clone(),
        value: cooked[k].clone(),
        tilde: tilde_prefix(&raw[k], &cooked[k]),
    };
    while i < n {
        let w = cooked[i].as_str();
        let attached = |prefix: &str| {
            w.strip_prefix(prefix).map(|v| Val {
                raw: raw[i].clone(),
                value: v.to_string(),
                tilde: false,
            })
        };
        match w {
            "-C" if i + 1 < n => {
                chain.push(Role::Cd, &whole(i + 1), cwd, out);
                i += 2;
            }
            "--git-dir" | "--work-tree" if i + 1 < n => {
                chain.push(Role::Dir, &whole(i + 1), cwd, out);
                i += 2;
            }
            "-c" | "--namespace" | "--super-prefix" | "--list-cmds" | "--attr-source"
            | "--config-env" => i += 2,
            _ if w.starts_with("--git-dir=") || w.starts_with("--work-tree=") => {
                let v = attached("--git-dir=")
                    .or_else(|| attached("--work-tree="))
                    .unwrap();
                chain.push(Role::Dir, &v, cwd, out);
                i += 1;
            }
            _ if w.starts_with('-') => i += 1,
            _ => break,
        }
    }
    chain
}

/// Where git's subcommand is in `cooked`, past its own options.
pub(super) fn git_subcommand(cooked: &[String]) -> Option<usize> {
    let mut i = 1;
    while i < cooked.len() {
        match cooked[i].as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix"
            | "--list-cmds" | "--attr-source" | "--config-env" => i += 2,
            w if w.starts_with('-') => i += 1,
            _ => return Some(i),
        }
    }
    None
}

/// Files and folders a git subcommand writes by name (relative to `-C`).
pub(super) fn git_targets(
    raw: &[String],
    cooked: &[String],
    chain: &mut Chain,
    cwd: &Path,
    out: &mut Analysis,
) {
    let Some(at) = git_subcommand(cooked) else {
        return;
    };
    let sub = cooked[at].as_str();
    let rest = at + 1;
    let n = cooked.len();
    let whole = |k: usize| Val {
        raw: raw[k].clone(),
        value: cooked[k].clone(),
        tilde: tilde_prefix(&raw[k], &cooked[k]),
    };
    // `--output=f`/`--output f` (diff, log, show, format-patch, archive).
    let mut k = rest;
    while k < n {
        let w = cooked[k].as_str();
        let named = |long: &str, short: Option<&str>| w == long || short == Some(w);
        let out_opt = match sub {
            "archive" => named("--output", Some("-o")),
            "format-patch" => named("--output-directory", Some("-o")) || named("--output", None),
            "clone" | "init" => named("--separate-git-dir", None),
            "bundle" => false,
            _ => named("--output", None),
        };
        if out_opt && k + 1 < n {
            chain.push(Role::Write, &whole(k + 1), cwd, out);
            k += 2;
            continue;
        }
        for long in ["--output=", "--output-directory=", "--separate-git-dir="] {
            if let Some(v) = w.strip_prefix(long) {
                let val = Val {
                    raw: raw[k].clone(),
                    value: v.to_string(),
                    tilde: false,
                };
                chain.push(Role::Write, &val, cwd, out);
            }
        }
        k += 1;
    }
    // Operands, past options and the values of those that take one.
    let with_value: &[&str] = match sub {
        "clone" => &[
            "-b",
            "--branch",
            "-o",
            "--origin",
            "-c",
            "--config",
            "--depth",
            "--reference",
            "--reference-if-able",
            "-u",
            "--upload-pack",
            "--template",
            "-j",
            "--jobs",
            "--filter",
            "--shallow-since",
            "--shallow-exclude",
            "--bundle-uri",
            "--separate-git-dir",
            "--server-option",
        ],
        "worktree" => &["-b", "-B", "--reason", "--orphan"],
        "init" => &[
            "--template",
            "--separate-git-dir",
            "-b",
            "--initial-branch",
            "--object-format",
            "--shared",
        ],
        "submodule" => &["-b", "--branch", "--name", "--reference", "--depth"],
        "bundle" => &[],
        _ => &[],
    };
    let operands = |from: usize| -> Vec<usize> {
        let mut v = Vec::new();
        let mut k = from;
        let mut done = false;
        while k < n {
            let w = cooked[k].as_str();
            if done || !w.starts_with('-') || w == "-" {
                v.push(k);
            } else if w == "--" {
                done = true;
            } else if with_value.contains(&w) {
                k += 1;
            }
            k += 1;
        }
        v
    };
    match sub {
        "clone" => {
            let ops = operands(rest);
            if let Some(&d) = ops.get(1) {
                chain.push(Role::Write, &whole(d), cwd, out);
            }
        }
        "init" => {
            if let Some(&d) = operands(rest).first() {
                chain.push(Role::Write, &whole(d), cwd, out);
            }
        }
        "worktree" if cooked.get(rest).is_some_and(|w| w == "add" || w == "move") => {
            let ops = operands(rest + 1);
            let picks: Vec<usize> = if cooked[rest] == "add" {
                ops.first().copied().into_iter().collect()
            } else {
                ops.get(1).copied().into_iter().collect()
            };
            for d in picks {
                chain.push(Role::Write, &whole(d), cwd, out);
            }
        }
        "submodule" if cooked.get(rest).is_some_and(|w| w == "add") => {
            if let Some(&d) = operands(rest + 1).get(1) {
                chain.push(Role::Write, &whole(d), cwd, out);
            }
        }
        "bundle" if cooked.get(rest).is_some_and(|w| w == "create") => {
            if let Some(&d) = operands(rest + 1).first() {
                chain.push(Role::Write, &whole(d), cwd, out);
            }
        }
        "apply" => {
            if cooked[rest..].iter().any(|w| w == "--unsafe-paths") {
                // Git then writes outside the work tree, where the patch says.
                unknown_write(out, &cooked.join(" "));
            }
            for k in rest..n {
                if let Some(v) = cooked[k].strip_prefix("--directory=") {
                    if Path::new(v).is_absolute() || v.split('/').any(|c| c == "..") {
                        let val = Val {
                            raw: raw[k].clone(),
                            value: v.to_string(),
                            tilde: false,
                        };
                        chain.push(Role::Dir, &val, cwd, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// `GIT_DIR=...` and the other variables that point git at a repository,
/// work tree or index elsewhere: their folder is checked like `-C`'s.
pub(super) const GIT_DIR_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

pub(super) fn git_dir_var(
    name: &str,
    raw_value: &str,
    value: &str,
    cwd: &Path,
    out: &mut Analysis,
) {
    let role = if name == "GIT_INDEX_FILE" {
        Role::Write
    } else {
        Role::Dir
    };
    let values: Vec<&str> = if name == "GIT_ALTERNATE_OBJECT_DIRECTORIES" {
        value.split(':').collect()
    } else {
        vec![value]
    };
    for v in values {
        if out.directories.iter().any(|d| d == v) {
            // `env GIT_DIR=x git ...` names it twice.
            continue;
        }
        let val = Val {
            raw: raw_value.to_string(),
            value: v.to_string(),
            tilde: false,
        };
        Chain::new().push(role, &val, cwd, out);
    }
}

/// Wrapper options that run the command in another folder: `env -C`,
/// `sudo -D`, in `cooked[1..at]`.
pub(super) fn wrapper_dirs(
    kind: &str,
    raw: &[String],
    cooked: &[String],
    at: usize,
    cwd: &Path,
    out: &mut Analysis,
) {
    let g = match kind {
        "env" => Grammar {
            short_args: "uSP",
            long_args: &["--unset", "--split-string"],
            targets: &[("-C", Role::Cd), ("--chdir", Role::Cd)],
        },
        "sudo" => Grammar {
            short_args: "acCghprRtTUu",
            long_args: &[
                "--auth-type",
                "--login-class",
                "--close-from",
                "--group",
                "--prompt",
                "--role",
                "--type",
                "--command-timeout",
                "--other-user",
                "--user",
            ],
            targets: &[
                ("-D", Role::Cd),
                ("--chdir", Role::Cd),
                ("-R", Role::Cd),
                ("--chroot", Role::Cd),
            ],
        },
        _ => return,
    };
    let (opts, _) = getopt(raw, cooked, 1, at, &g);
    let mut chain = Chain::new();
    for (name, v) in &opts {
        if let Some((_, role)) = g.targets.iter().find(|(t, _)| t == name) {
            chain.push(*role, v, cwd, out);
        }
    }
}

/// `perl -i` and `ruby -i`: the files after the code are rewritten.
pub(super) fn in_place(
    kind: &str,
    raw: &[String],
    cooked: &[String],
    cwd: &Path,
    out: &mut Analysis,
) {
    let family = kind.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    let (code, args, attached, digits): (&str, &str, &str, &str) = match family {
        "perl" => ("eE", "I", "ixdDMmFV", "l0C"),
        "ruby" => ("e", "ICrE", "iFxWK", "0T"),
        _ => return,
    };
    let mut inplace = false;
    let mut has_code = false;
    let mut files = Vec::new();
    let mut i = 1;
    let mut options_done = false;
    while i < cooked.len() {
        let w = cooked[i].as_str();
        if options_done || !w.starts_with('-') || w == "-" {
            files.push(i);
            i += 1;
            continue;
        }
        if w == "--" {
            options_done = true;
            i += 1;
            continue;
        }
        if w.starts_with("--") {
            i += 1;
            continue;
        }
        let letters: Vec<char> = w[1..].chars().collect();
        let mut skip = 0;
        let mut k = 0;
        while k < letters.len() {
            let c = letters[k];
            if c == 'i' {
                inplace = true;
                break;
            }
            if code.contains(c) {
                has_code = true;
                if k + 1 == letters.len() {
                    skip = 1;
                }
                break;
            }
            if args.contains(c) {
                if k + 1 == letters.len() {
                    skip = 1;
                }
                break;
            }
            if attached.contains(c) {
                break;
            }
            k += 1;
            if digits.contains(c) {
                while k < letters.len() && letters[k].is_ascii_digit() {
                    k += 1;
                }
            }
        }
        i += 1 + skip;
    }
    if !inplace {
        return;
    }
    // Without `-e`, the first operand is the script.
    let files = if has_code {
        &files[..]
    } else {
        files.get(1..).unwrap_or_default()
    };
    if files.is_empty() {
        return;
    }
    let mut chain = Chain::new();
    for &k in files {
        let v = Val {
            raw: raw[k].clone(),
            value: cooked[k].clone(),
            tilde: tilde_prefix(&raw[k], &cooked[k]),
        };
        chain.push(Role::Write, &v, cwd, out);
    }
}

/// Whether a command (cooked words, the program first) runs only to
/// change files by name: such a program fed names by `xargs` or `find
/// -exec` writes where the names point.
pub(super) fn writes_operands(base: &str) -> bool {
    let base = gnu_name(base);
    matches!(
        program(base),
        Some((_, op)) if op != Operands::No
    ) || matches!(base, "perl" | "ruby" | "sed" | "patch" | "dd")
}

/// Paths a static string of code in another language names (`open(
/// '/x/y', 'w')`, `os.remove("../z")`): quoted text starting with `/`,
/// `~/` or `../`. The code may read or write them, so they are checked
/// as places the command reaches.
pub(super) fn code_paths(code: &str, out: &mut Analysis) {
    for (n, piece) in code.split(['\'', '"']).enumerate() {
        if n % 2 == 0 {
            continue;
        }
        let p = piece.trim_start_matches(['+', '<', '>', '|', ' ']);
        let looks = p.len() > 1
            && (p.starts_with('/') || p.starts_with("~/") || p.starts_with("../"))
            && !p.contains(char::is_whitespace)
            && !p.starts_with("/dev/");
        if !looks {
            continue;
        }
        let path = match p.strip_prefix("~/") {
            Some(rest) => super::home().join(rest).display().to_string(),
            None => p.to_string(),
        };
        if !out.reached.iter().any(|w| w.path.as_deref() == Some(&path)) {
            out.reached.push(Write {
                raw: p.to_string(),
                path: Some(path),
            });
        }
    }
}

const READ_ONLY: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "ag",
    "ack",
    "wc",
    "stat",
    "file",
    "du",
    "df",
    "pwd",
    "echo",
    "printf",
    "true",
    "false",
    "test",
    "[",
    "diff",
    "cmp",
    "cut",
    "tr",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "which",
    "type",
    "jq",
    "md5",
    "md5sum",
    "shasum",
    "sha1sum",
    "sha256sum",
    "sha512sum",
    "cksum",
    "od",
    "hexdump",
    "strings",
    "nl",
    "column",
    "fold",
    "cd",
    "pushd",
    "popd",
    "chdir",
    "id",
    "whoami",
    "date",
    "uname",
    "hostname",
    "sleep",
    "exit",
    "return",
    ":",
    "read",
    "printenv",
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "comm",
    "join",
    "paste",
    "expand",
    "unexpand",
    "rev",
    "tac",
    "seq",
    "yes",
    "zcat",
    "bat",
    "fd",
    "lsof",
    "ps",
];

/// Whether a command only reads: a program from a known list, `find`,
/// `sort`, `sed` or `tree` without their writing options, or a git
/// subcommand that changes no file, ref or index (`log`, `status`...).
/// Anything else, a wrapper included, may write.
pub(super) fn read_only(cooked: &[String]) -> bool {
    let Some(name) = cooked.first() else {
        return true;
    };
    let base = name.rsplit('/').next().unwrap_or(name);
    let base = gnu_name(base);
    let args = &cooked[1..];
    let any = |set: &[&str]| args.iter().any(|w| set.contains(&w.as_str()));
    match base {
        "find" => !any(&[
            "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf",
            "-fls",
        ]),
        "sort" => !args
            .iter()
            .any(|w| w.starts_with("-o") || w.starts_with("--output")),
        "uniq" => args.iter().filter(|w| !w.starts_with('-')).count() <= 1,
        "tree" => !args.iter().any(|w| w.starts_with("-o")),
        "sed" => !args.iter().any(|w| {
            w.starts_with("--in-place")
                || (w.starts_with('-') && !w.starts_with("--") && w.contains('i'))
        }),
        "git" => git_read_only(cooked),
        _ => READ_ONLY.contains(&base),
    }
}

fn git_read_only(cooked: &[String]) -> bool {
    let Some(at) = git_subcommand(cooked) else {
        return true; // `git --version`
    };
    let rest: Vec<&str> = cooked[at + 1..].iter().map(String::as_str).collect();
    if rest.iter().any(|w| w.starts_with("--output")) {
        return false;
    }
    let operands = rest.iter().filter(|w| !w.starts_with('-')).count();
    let flag = |set: &[&str]| rest.iter().any(|w| set.contains(w));
    match cooked[at].as_str() {
        "log" | "show" | "diff" | "status" | "blame" | "annotate" | "grep" | "ls-files"
        | "ls-tree" | "ls-remote" | "rev-parse" | "rev-list" | "cat-file" | "describe"
        | "shortlog" | "show-ref" | "for-each-ref" | "name-rev" | "merge-base" | "whatchanged"
        | "range-diff" | "cherry" | "count-objects" | "check-ignore" | "check-attr" | "var"
        | "version" | "help" | "diff-tree" | "diff-files" | "diff-index" | "show-branch"
        | "verify-commit" | "verify-tag" | "check-ref-format" => true,
        "branch" => {
            flag(&["-l", "--list", "--show-current"])
                || rest.iter().all(|w| {
                    w.starts_with('-')
                        && ![
                            "-d",
                            "-D",
                            "-m",
                            "-M",
                            "-c",
                            "-C",
                            "-f",
                            "--delete",
                            "--move",
                            "--copy",
                            "--force",
                            "-u",
                            "--set-upstream-to",
                            "--unset-upstream",
                            "--edit-description",
                            "-t",
                            "--track",
                        ]
                        .iter()
                        .any(|x| w == x || w.starts_with(&format!("{x}=")))
                })
        }
        "tag" => {
            flag(&["-l", "--list"])
                || rest.is_empty()
                || rest.iter().all(|w| {
                    w.starts_with("-n") || w.starts_with("--sort") || w.starts_with("--format")
                })
        }
        "remote" => rest.is_empty() || matches!(rest[0], "-v" | "--verbose" | "show" | "get-url"),
        "config" => {
            let writes = flag(&[
                "--unset",
                "--unset-all",
                "--add",
                "--replace-all",
                "--rename-section",
                "--remove-section",
                "-e",
                "--edit",
                "set",
                "unset",
                "rename-section",
                "remove-section",
                "edit",
            ]);
            !writes
                && (flag(&[
                    "--get",
                    "--get-all",
                    "--get-regexp",
                    "--get-urlmatch",
                    "--list",
                    "-l",
                    "list",
                    "get",
                ]) || operands <= 1)
        }
        "stash" => matches!(rest.first(), Some(&"list") | Some(&"show")),
        "worktree" => matches!(rest.first(), Some(&"list")),
        "reflog" => rest.is_empty() || matches!(rest[0], "show") || (rest[0].starts_with('-')),
        "symbolic-ref" => operands <= 1 && !flag(&["-d", "--delete"]),
        "notes" => matches!(rest.first(), Some(&"list") | Some(&"show")) || rest.is_empty(),
        _ => false,
    }
}

/// `find`: with `-delete`, or `-exec` of a program that writes the files
/// it is given, the start folders are written.
pub(super) fn find_targets(raw: &[String], cooked: &[String], cwd: &Path, out: &mut Analysis) {
    let mut writes = cooked.iter().any(|w| w == "-delete");
    for (k, w) in cooked.iter().enumerate() {
        if ["-exec", "-execdir", "-ok", "-okdir"].contains(&w.as_str()) {
            if let Some(p) = cooked.get(k + 1) {
                let b = p.rsplit('/').next().unwrap_or(p);
                writes |= writes_operands(super::canonical_name(b));
            }
        }
    }
    if !writes {
        return;
    }
    // Start folders: after `-H`/`-L`/`-P` (BSD's `-E`, `-X`, `-f dir`
    // too), before the first expression.
    let mut starts = Vec::new();
    let mut k = 1;
    while k < cooked.len() {
        let w = cooked[k].as_str();
        if ["-H", "-L", "-P", "-E", "-X", "-d", "-s", "-x"].contains(&w) {
            k += 1;
            continue;
        }
        if w == "-f" && k + 1 < cooked.len() {
            starts.push(k + 1);
            k += 2;
            continue;
        }
        if w.starts_with('-') || w == "(" || w == "!" {
            break;
        }
        starts.push(k);
        k += 1;
    }
    if starts.is_empty() {
        out.writes.push(Write {
            raw: ".".into(),
            path: Some(".".into()),
        });
    }
    let mut chain = Chain::new();
    for k in starts {
        let v = Val {
            raw: raw[k].clone(),
            value: cooked[k].clone(),
            tilde: tilde_prefix(&raw[k], &cooked[k]),
        };
        chain.push(Role::Write, &v, cwd, out);
    }
}

/// The local destination of `rsync` or `scp` (the last operand, when it
/// names no host), and rsync's own files.
fn copy_dest(base: &str, raw: &[String], cooked: &[String], cwd: &Path, out: &mut Analysis) {
    let g = if base == "rsync" {
        Grammar {
            short_args: "efMB@",
            long_args: &[
                "--rsh",
                "--rsync-path",
                "--filter",
                "--exclude",
                "--include",
                "--exclude-from",
                "--include-from",
                "--files-from",
                "--compare-dest",
                "--copy-dest",
                "--link-dest",
                "--chmod",
                "--chown",
                "--port",
                "--password-file",
                "--suffix",
                "--max-size",
                "--min-size",
                "--timeout",
                "--block-size",
                "--bwlimit",
            ],
            targets: &[
                ("-T", Role::Write),
                ("--temp-dir", Role::Write),
                ("--backup-dir", Role::Write),
                ("--partial-dir", Role::Write),
                ("--log-file", Role::Write),
                ("--write-batch", Role::Write),
                ("--only-write-batch", Role::Write),
            ],
        }
    } else {
        Grammar {
            short_args: "cFiJlLoPSDX",
            ..NONE
        }
    };
    let (opts, ops) = getopt(raw, cooked, 1, cooked.len(), &g);
    let mut chain = Chain::new();
    for (name, v) in &opts {
        if let Some((_, role)) = g.targets.iter().find(|(t, _)| t == name) {
            chain.push(*role, v, cwd, out);
        }
    }
    if ops.len() < 2 {
        return;
    }
    let dest = ops.last().unwrap();
    let remote = dest.value.starts_with("rsync://")
        || dest
            .value
            .split_once(':')
            .is_some_and(|(host, _)| !host.contains('/'));
    if !remote {
        chain.push(Role::Write, dest, cwd, out);
    }
}

#[cfg(test)]
#[path = "targets_tests.rs"]
mod tests;
