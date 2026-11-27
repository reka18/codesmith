//! What the scan finds in a command's arguments: folders it runs in
//! (`git -C`, `make -C`...), files it writes by name (`rm`, `cp`, `perl
//! -i`...), and whether it only reads. `shell_outside_tests.rs` checks what
//! the shell tool then asks.

use super::super::{analyze, Analysis};
use super::read_only;
use std::path::Path;

fn full(src: &str) -> Analysis {
    analyze(src, Path::new("/workspace")).unwrap_or_else(|e| panic!("{src:?}: {e}"))
}

fn dirs(src: &str) -> Vec<String> {
    full(src).directories
}

fn writes(src: &str) -> Vec<Option<String>> {
    full(src).writes.into_iter().map(|w| w.path).collect()
}

fn some(v: &[&str]) -> Vec<Option<String>> {
    v.iter().map(|s| Some(s.to_string())).collect()
}

fn words(src: &str) -> Vec<String> {
    src.split_whitespace().map(str::to_string).collect()
}

#[test]
fn directory_options_are_checked_like_cd() {
    for (src, want) in [
        // git: `-C`, cumulative and relative, `--git-dir`, `--work-tree`.
        ("git -C /x/sib commit -am x", &["/x/sib"][..]),
        ("git -C ../sib checkout -b y", &["../sib"]),
        ("git -C a -C b status", &["a", "a/b"]),
        ("git -C a -C /abs log", &["a", "/abs"]),
        (
            "git -C ../sib -C ../other log",
            &["../sib", "../sib/../other"],
        ),
        ("git --git-dir=/x/.git status", &["/x/.git"]),
        (
            "git --git-dir /x/.git --work-tree /x add .",
            &["/x/.git", "/x"],
        ),
        (
            "git -C ../sib --work-tree=wt status",
            &["../sib", "../sib/wt"],
        ),
        ("git -c user.name=a -C ../sib commit -m x", &["../sib"]),
        ("git --no-pager -C /x diff", &["/x"]),
        // Assignments pointing git elsewhere.
        ("GIT_DIR=/x/sib/.git git commit -m x", &["/x/sib/.git"]),
        ("GIT_WORK_TREE=/x/sib git add -A", &["/x/sib"]),
        ("export GIT_DIR=../sib/.git; git log", &["../sib/.git"]),
        ("env GIT_DIR=/x/.git git reset --hard", &["/x/.git"]),
        ("GIT_COMMON_DIR=/x/c git status", &["/x/c"]),
        // Wrappers that change folder.
        ("env -C /x/sib rm f", &["/x/sib"]),
        ("env --chdir=/x/sib touch f", &["/x/sib"]),
        ("sudo -D /x rm f", &["/x"]),
        ("sudo --chdir /x rm f", &["/x"]),
        // Build tools and package managers.
        ("make -C ../sib", &["../sib"]),
        ("make --directory=/x install", &["/x"]),
        ("make -C a -C b", &["a", "a/b"]),
        ("gmake -C /x", &["/x"]),
        ("tar -xzf a.tgz -C /x/out", &["/x/out"]),
        ("tar --directory /x -cf - .", &["/x"]),
        ("npm --prefix ../sib install", &["../sib"]),
        ("npm -C /x run build", &["/x"]),
        ("pnpm -C /x install", &["/x"]),
        ("pnpm --dir=/x test", &["/x"]),
        ("yarn --cwd /x build", &["/x"]),
        ("bun --cwd=/x install", &["/x"]),
        ("cargo --manifest-path ../sib/Cargo.toml build", &["../sib"]),
        ("cargo -C /x build", &["/x"]),
        ("go -C ../sib build ./...", &["../sib"]),
        ("mvn -f /x/pom.xml package", &["/x"]),
        ("mvn -f /x package", &["/x"]),
        ("gradle -p /x build", &["/x"]),
        ("./gradlew --project-dir=/x build", &["/x"]),
        ("cmake -S /x -B build", &["/x"]),
        ("cmake --build /x/build", &["/x/build"]),
        ("just -d /x test", &["/x"]),
        ("uv --directory /x run pytest", &["/x"]),
        ("poetry -C /x install", &["/x"]),
        ("terraform -chdir=/x plan", &["/x"]),
        ("docker compose --project-directory /x up", &["/x"]),
        ("patch -d /x -p1 -i fix.diff", &["/x"]),
        // Inside a wrapper, a subshell, a shell string.
        ("sudo git -C /x commit", &["/x"]),
        ("nice -n 5 make -C /x", &["/x"]),
        ("timeout 5 git -C /x pull", &["/x"]),
        ("sh -c 'git -C /x add .'", &["/x"]),
        ("(git -C /x stash)", &["/x"]),
        ("echo $(git -C /x rev-parse HEAD)", &["/x"]),
        ("xargs git -C /x rm", &["/x"]),
    ] {
        assert_eq!(dirs(src), want, "{src}");
        assert!(!full(src).unknown_cwd, "{src}");
    }
}

#[test]
fn directory_options_only_known_at_run_time_are_unknown() {
    for src in [
        "git -C \"$D\" status",
        "git -C $(pwd)/../x commit",
        "git --git-dir=\"$G\" log",
        "GIT_DIR=$X git commit",
        "make -C \"$D\"",
        "env -C \"$D\" rm f",
        "npm --prefix \"$P\" install",
        "git -C `pwd` add .",
        "git -C a -C \"$B\" -C c status",
        "git -C '{a,../b}' status",
    ] {
        let a = full(src);
        assert!(a.unknown_cwd, "{src}: {a:?}");
    }
}

#[test]
fn harmless_or_empty_directory_options_add_nothing() {
    for src in [
        "git -C \"\" status",
        "git status",
        "git -c core.quotepath=off log",
        "make test",
        "env FOO=1 ls",
        "npm install",
        "cargo build",
        "go build ./...",
    ] {
        let a = full(src);
        assert!(a.directories.is_empty() && !a.unknown_cwd, "{src}: {a:?}");
    }
}

#[test]
fn programs_that_write_files_by_name_are_writes() {
    for (src, want) in [
        ("rm ../sib/f", &["../sib/f"][..]),
        ("rm -rf -- /x/a /x/b", &["/x/a", "/x/b"]),
        ("unlink /x/f", &["/x/f"]),
        ("rmdir /x/d", &["/x/d"]),
        ("shred -n 3 /x/f", &["/x/f"]),
        ("touch /x/f", &["/x/f"]),
        ("touch -r ref /x/f", &["/x/f"]),
        ("mkdir -p -m 700 /x/d", &["/x/d"]),
        ("truncate -s 0 /x/f", &["/x/f"]),
        ("cp a /x/b", &["/x/b"]),
        ("cp -r a b /x/dir", &["/x/dir"]),
        ("cp -t /x/dir a b", &["/x/dir"]),
        ("cp --target-directory=/x/dir a", &["/x/dir"]),
        ("mv a /x/b", &["a", "/x/b"]),
        ("mv /x/a b", &["/x/a", "b"]),
        ("ln -s target /x/link", &["/x/link"]),
        ("install -m 755 a /x/bin/a", &["/x/bin/a"]),
        ("install -d /x/a /x/b", &["/x/a", "/x/b"]),
        ("tee /x/out", &["/x/out"]),
        ("tee -a a /x/b", &["a", "/x/b"]),
        ("chmod +x /x/s", &["/x/s"]),
        ("chmod -R 755 /x/d", &["/x/d"]),
        ("chown -R me /x/d", &["/x/d"]),
        ("perl -i -pe 's/a/b/' /x/f", &["/x/f"]),
        ("perl -pi -e 's/a/b/' /x/f /x/g", &["/x/f", "/x/g"]),
        ("perl -i.bak -pe 's/a/b/' /x/f", &["/x/f"]),
        ("perl -pie 's/a/b/' /x/f", &["/x/f"]),
        ("ruby -i -pe 'x' /x/f", &["/x/f"]),
        ("ruby -i.orig -ne 'x' /x/f", &["/x/f"]),
        ("patch /x/f.c fix.diff", &["/x/f.c"]),
        ("patch -o /x/out f.c fix.diff", &["/x/out", "f.c"]),
        ("dd if=a of=/x/b", &["/x/b"]),
        ("unzip a.zip -d /x", &["/x"]),
        ("unzip -o -d /x a.zip", &["/x"]),
        ("curl -o /x/f https://e", &["/x/f"]),
        ("curl -sSLo /x/f https://e", &["/x/f"]),
        ("curl --output=/x/f https://e", &["/x/f"]),
        ("curl -O --output-dir /x https://e/f", &["/x"]),
        ("curl -c /x/jar https://e", &["/x/jar"]),
        ("wget -O /x/f https://e", &["/x/f"]),
        ("wget -P /x https://e", &["/x"]),
        ("wget --directory-prefix=/x https://e", &["/x"]),
        ("tar -xzf a.tgz -C /x", &["/x"]),
        ("tar xzf a.tgz -C /x", &["/x"]),
        ("tar czf /x/out.tgz src", &["/x/out.tgz"]),
        ("tar -cf /x/out.tar src", &["/x/out.tar"]),
        ("gzip /x/f", &["/x/f"]),
        ("gunzip /x/f.gz", &["/x/f.gz"]),
        ("zip /x/a.zip f", &["/x/a.zip"]),
        ("sort -o /x/s f", &["/x/s"]),
        ("uniq in /x/out", &["/x/out"]),
        ("rsync -a src/ /x/dst/", &["/x/dst/"]),
        ("rsync -av --delete src /x", &["/x"]),
        ("scp host:f /x/g", &["/x/g"]),
        ("cargo build --target-dir /x/t", &["/x/t"]),
        ("go build -o /x/bin ./cmd", &["/x/bin"]),
        ("cmake -B /x/build", &["/x/build"]),
        ("pip install -t /x/lib pkg", &["/x/lib"]),
        ("cmake -E rm -f /x/f", &["/x/f"]),
        ("cmake -E copy a /x/b", &["a", "/x/b"]),
        ("find /x -delete", &["/x"]),
        ("find /x -name '*.o' -exec rm {} +", &["/x"]),
        ("find /x -execdir rm {} \\;", &["/x"]),
        // GNU builds by their Homebrew names.
        ("grm /x/f", &["/x/f"]),
        ("gcp a /x/b", &["/x/b"]),
        // git subcommands that create files by name.
        ("git clone https://e/r /x/clone", &["/x/clone"]),
        ("git clone -b main https://e/r /x/c", &["/x/c"]),
        ("git worktree add ../wt main", &["../wt"]),
        ("git worktree add -b topic /x/wt", &["/x/wt"]),
        ("git worktree move wt /x/wt", &["/x/wt"]),
        ("git init /x/new", &["/x/new"]),
        ("git diff --output=/x/d.patch", &["/x/d.patch"]),
        ("git format-patch -o /x/p HEAD~1", &["/x/p"]),
        ("git archive -o /x/a.tar HEAD", &["/x/a.tar"]),
        ("git bundle create /x/b.bundle HEAD", &["/x/b.bundle"]),
        ("git -C ../sib diff --output=d", &["../sib/d"]),
        // Through wrappers and nesting.
        ("sudo rm /x/f", &["/x/f"]),
        ("nice rm /x/f", &["/x/f"]),
        ("timeout 5 rm /x/f", &["/x/f"]),
        ("doas rm /x/f", &["/x/f"]),
        ("flock /x/l rm /x/f", &["/x/l", "/x/f"]),
        ("env A=1 touch /x/f", &["/x/f"]),
        ("bash -c 'rm /x/f'", &["/x/f"]),
        ("(cd sub && bash -c 'rm f')", &["f"]),
        ("echo $(rm /x/f)", &["/x/f"]),
        // Globs name files in their fixed folder; plain brace lists too.
        ("rm -f /x/*.o", &["/x"]),
        ("rm ../sib/*", &["../sib"]),
        ("mkdir -p src/{a,b}", &["src/a"]),
        ("rm build/{1..3}.log", &["build/1.log"]),
    ] {
        assert_eq!(writes(src), some(want), "{src}");
    }
}

#[test]
fn write_targets_only_known_at_run_time_are_unknown() {
    for src in [
        "rm \"$X\"",
        "rm -rf $(cat list)",
        "cp a \"$DEST\"",
        "curl -o \"$F\" https://e",
        "ls ../sib | xargs rm",
        "git ls-files | xargs rm -f",
        "find . -print0 | xargs -0 perl -i -pe s/a/b/",
        "patch -p1 < x.diff",
        "patch -p0 -i x.diff",
        "git apply --unsafe-paths x.diff",
        "tar -xPf a.tar",
        "curl -K cfg https://e",
        "rm '{a,../b}'x",
        "rm {a,../b}",
    ] {
        let a = full(src);
        assert!(
            a.unknown_target.is_some() || a.directories.iter().any(|d| d.contains("..")),
            "{src}: {a:?}"
        );
        // A write program's computed path is not a redirect's: it is
        // checked for `external_directory` only.
        assert!(!a.writes.iter().any(|w| w.path.is_none()), "{src}");
    }
}

#[test]
fn reading_programs_name_no_writes() {
    for src in [
        "cat ../sib/f",
        "ls /x",
        "grep -r x /x",
        "gzip -c /x/f",
        "gzip -l /x/f.gz",
        "cp a",
        "ln -s target",
        "sort /x/f",
        "uniq /x/f",
        "curl -o - https://e",
        "curl https://e",
        "wget -q https://e -O -",
        "unzip -l a.zip",
        "tar -tzf a.tgz",
        "tar -xzf a.tgz",
        "git log",
        "git -C /x log",
        "find /x -name '*.rs'",
        "find /x -exec cat {} +",
        "patch --dry-run -p1 -i x.diff",
        "perl -pe 's/a/b/' /x/f",
        "perl -e 'print 1'",
        "ruby -e 'p 1'",
        "rsync host:/a host:/b",
        "scp /x/f host:g",
        "dd if=/x/f",
    ] {
        assert!(writes(src).is_empty(), "{src}: {:?}", writes(src));
    }
}

#[test]
fn checked_writes_by_programs_stay_checked() {
    // The ones the docs listed before: sed -i, awk, find -fprint, flock,
    // script and redirects.
    for (src, want) in [
        ("sed -i 's/a/b/' /x/f", &["/x/f"][..]),
        ("sed -i.bak 's/a/b/' /x/f", &["/x/f", "/x/f.bak"]),
        ("awk '{print > \"/x/f\"}' in", &["/x/f"]),
        ("find . -fprint /x/list", &["/x/list"]),
        ("flock /x/lock true", &["/x/lock"]),
        ("script -q /x/ts true", &["/x/ts"]),
        ("echo x > /x/f", &["/x/f"]),
        ("echo x >> /x/f", &["/x/f"]),
        ("cat a &> /x/f", &["/x/f"]),
    ] {
        assert_eq!(writes(src), some(want), "{src}");
    }
}

#[test]
fn inline_code_names_the_paths_it_quotes() {
    let reached = |src: &str| -> Vec<String> {
        full(src)
            .reached
            .into_iter()
            .filter_map(|w| w.path)
            .collect()
    };
    assert_eq!(
        reached("python3 -c \"import os; os.remove('/x/sib/f')\""),
        ["/x/sib/f"]
    );
    assert_eq!(
        reached("node -e \"require('fs').writeFileSync('../sib/f', 'x')\""),
        ["../sib/f"]
    );
    assert_eq!(reached("perl -e 'open(F, \">/x/f\")'"), ["/x/f"]);
    assert_eq!(
        reached("ruby -e 'File.write(\"~/x\", 1)'"),
        [super::super::home().join("x").display().to_string()]
    );
    for src in [
        "python3 -c 'print(1)'",
        "python3 -c \"print('a b')\"",
        "python3 -c \"open('/dev/null')\"",
        "python3 -c \"print('/')\"",
        "python3 x.py /x/f",
    ] {
        assert!(reached(src).is_empty(), "{src}");
    }
    // Inline code still carries its `code:` marker, and may write.
    let a = full("python3 -c \"open('/x/f','w')\"");
    assert!(a
        .delegated
        .iter()
        .any(|(r, _)| r.starts_with("code:python3")));
    assert!(a.mutating);
}

#[test]
fn commands_are_classified_as_reading_or_writing() {
    for src in [
        "ls",
        "cat f",
        "head -n 5 f",
        "grep -rn x .",
        "rg x",
        "wc -l f",
        "find . -name x",
        "sort f",
        "uniq f",
        "sed -n 1p f",
        "awk '{print $1}' f",
        "git log --oneline",
        "git show HEAD",
        "git diff HEAD~1",
        "git status",
        "git blame f",
        "git grep x",
        "git ls-files",
        "git rev-parse HEAD",
        "git cat-file -p HEAD",
        "git branch",
        "git branch -a",
        "git branch --list 'f*'",
        "git branch --show-current",
        "git tag",
        "git tag -l 'v*'",
        "git remote -v",
        "git config --get user.name",
        "git config user.name",
        "git stash list",
        "git worktree list",
        "git -C /x log",
        "git --version",
        "cd /x",
        "pwd",
    ] {
        assert!(read_only(&words(src)), "{src} reads");
    }
    for src in [
        "rm f",
        "touch f",
        "git checkout main",
        "git switch -c x",
        "git reset --hard",
        "git rebase --onto a b",
        "git commit -am x",
        "git add .",
        "git rm f",
        "git mv a b",
        "git apply x.diff",
        "git am x.mbox",
        "git cherry-pick -n abc",
        "git merge x",
        "git stash",
        "git stash pop",
        "git clean -fd",
        "git restore f",
        "git worktree add ../x",
        "git branch new",
        "git branch -D old",
        "git tag v1",
        "git config user.name x",
        "git remote add o url",
        "git fetch",
        "git pull",
        "git push",
        "git diff --output=x",
        "find . -delete",
        "find . -exec rm {} +",
        "sort -o out f",
        "sed -i s/a/b/ f",
        "make",
        "npm install",
        "python3 x.py",
        "sudo ls",
        "env ls",
        "unknown-tool",
    ] {
        assert!(!read_only(&words(src)), "{src} writes");
    }
    // A line is mutating when any of its commands is.
    assert!(!full("cd /x && ls && git log | head").mutating);
    assert!(full("cd /x && ls && rm f").mutating);
    assert!(full("ls $(touch x)").mutating);
    assert!(full("python3 -c 'print(1)'").mutating);
}

#[test]
fn git_subcommands_that_write_follow_dash_c_outside() {
    // Whatever the subcommand, `-C` outside is a folder to check; the
    // writing ones are not read-only.
    for sub in [
        "checkout x",
        "reset --hard",
        "rebase --onto a b",
        "commit -am x",
        "add .",
        "rm f",
        "mv a b",
        "apply x.diff",
        "am x.mbox",
        "cherry-pick -n a",
        "merge x",
        "stash",
        "clean -fd",
        "restore f",
        "switch -c y",
        "worktree add w",
    ] {
        let src = format!("git -C /x/sib {sub}");
        let a = full(&src);
        assert!(a.directories.contains(&"/x/sib".to_string()), "{src}");
        assert!(a.mutating, "{src}");
    }
}

/// A small deterministic generator, for bounded property tests.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as usize
    }
    fn pick<'a>(&mut self, v: &[&'a str]) -> &'a str {
        v[self.next() % v.len()]
    }
}

/// Wrapping a command that names an outside folder or file in any mix of
/// wrappers, subshells and shell strings never hides it.
#[test]
fn property_wrappers_never_hide_a_target() {
    let wrappers = [
        "sudo {}",
        "nice -n 3 {}",
        "timeout 9 {}",
        "env A=1 {}",
        "doas {}",
        "nohup {}",
        "( {} )",
        "{ {}; }",
        "true && {}",
        "false || {}",
        "if true; then {}; fi",
        "sh -c '{}'",
        "bash -c '{}'",
        "echo $( {} )",
    ];
    let commands = [
        ("git -C /o/x commit -m m", true),
        ("make -C /o/x", true),
        ("env -C /o/x ls", true),
        ("rm /o/x/f", false),
        ("cp a /o/x/f", false),
        ("touch /o/x/f", false),
        ("perl -i -pe s/a/b/ /o/x/f", false),
        ("curl -o /o/x/f u", false),
        ("tar -xf a.tar -C /o/x", true),
    ];
    let mut r = Lcg(7);
    for _ in 0..400 {
        let (cmd, is_dir) = commands[r.next() % commands.len()];
        let mut src = cmd.to_string();
        let depth = 1 + r.next() % 3;
        let mut simple = true;
        for _ in 0..depth {
            let w = r.pick(&wrappers);
            if w.contains('\'') && src.contains('\'') {
                continue;
            }
            // A wrapper runs a simple command, not `if ...` or `( ... )`.
            let prefix = !w.starts_with(['(', '{'])
                && !w.contains(';')
                && !w.contains("&&")
                && !w.contains("||")
                && !w.contains('\'')
                && !w.contains("$(");
            if prefix && !simple {
                continue;
            }
            simple = prefix;
            src = w.replace("{}", &src);
        }
        let a = full(&src);
        let found = if is_dir {
            a.directories.iter().any(|d| d.starts_with("/o/x"))
        } else {
            a.writes
                .iter()
                .any(|w| w.path.as_deref().is_some_and(|p| p.starts_with("/o/x")))
        };
        assert!(found, "{src}: {a:?}");
    }
}

/// Spreading flags, `--` and other operands around a write program's
/// outside target keeps it a write; an inside one stays inside.
#[test]
fn property_write_operands_survive_option_noise() {
    let programs = [
        "rm", "rm -f", "rm -rf", "touch", "mkdir -p", "tee", "tee -a", "shred -u",
    ];
    let noise = ["-v", "", "--", "inside/a", "-f"];
    let mut r = Lcg(42);
    for _ in 0..300 {
        let prog = r.pick(&programs);
        let mut parts: Vec<String> = vec![prog.into()];
        let mut ended = false;
        for _ in 0..r.next() % 4 {
            let n = r.pick(&noise);
            if n == "--" {
                if ended {
                    continue;
                }
                ended = true;
            }
            if !n.is_empty() && !(ended && n.starts_with('-')) {
                parts.push(n.into());
            }
        }
        let outside = r.next().is_multiple_of(2);
        let target = if outside { "../sib/t" } else { "inside/t" };
        parts.push(target.into());
        let src = parts.join(" ");
        let w = writes(&src);
        assert!(w.contains(&Some(target.to_string())), "{src}: {w:?}");
        assert!(
            !w.contains(&None) && full(&src).unknown_target.is_none(),
            "{src}"
        );
    }
}

/// `git -C` chains resolve the way git does: each relative one against the
/// one before, an absolute one starting over.
#[test]
fn property_git_dash_c_chains_resolve_like_git() {
    let parts = ["a", "b", "..", "/abs", "../s", "c/d"];
    let mut r = Lcg(3);
    for _ in 0..200 {
        let n = 1 + r.next() % 4;
        let chosen: Vec<&str> = (0..n).map(|_| r.pick(&parts)).collect();
        let src = format!(
            "git {} status",
            chosen
                .iter()
                .map(|p| format!("-C {p}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let mut want = std::path::PathBuf::new();
        for p in &chosen {
            if p.starts_with('/') {
                want = p.into();
            } else {
                want.push(p);
            }
        }
        let d = dirs(&src);
        assert_eq!(
            d.last().map(std::path::PathBuf::from),
            Some(want.clone()),
            "{src}: {d:?}"
        );
    }
}
