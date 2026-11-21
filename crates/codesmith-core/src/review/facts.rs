//! Facts about a repository gathered from git for `/calibrate`: which files
//! exist, how they group into areas, what changes most, and where bug fixes
//! have landed.

use super::target::git;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Area {
    pub name: String,
    /// Globs covering the area.
    pub paths: Vec<String>,
    pub files: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub files: Vec<String>,
    pub areas: Vec<Area>,
    /// Files by commits touching them in the last year.
    pub churn: Vec<(String, usize)>,
    /// Bug-fix commits: (short hash, subject, files).
    pub fixes: Vec<(String, String, Vec<String>)>,
    pub instructions: Vec<String>,
    pub manifests: Vec<String>,
}

const MAX_AREAS: usize = 8;
const MANIFESTS: [&str; 12] = [
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "requirements.txt",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "CMakeLists.txt",
    "Makefile",
    "Package.swift",
];
const INSTRUCTIONS: [&str; 4] = [
    "AGENTS.md",
    "CLAUDE.md",
    "CONTRIBUTING.md",
    "CONVENTIONS.md",
];

fn top(path: &str, depth: usize) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    (parts.len() > depth).then(|| parts[..depth].join("/"))
}

/// Group files into at most `MAX_AREAS` areas, splitting the largest folder
/// into its subfolders while one area holds more than 40% of the files.
pub fn areas(files: &[String]) -> Vec<Area> {
    let mut groups: BTreeMap<String, usize> = BTreeMap::new(); // prefix -> count ("" = root files)
    for f in files {
        *groups.entry(top(f, 1).unwrap_or_default()).or_default() += 1;
    }
    loop {
        let total = files.len().max(1);
        let Some((big, n)) = groups
            .iter()
            .filter(|(k, _)| !k.is_empty())
            .max_by_key(|(_, n)| **n)
            .map(|(k, n)| (k.clone(), *n))
        else {
            break;
        };
        let depth = big.matches('/').count() + 1;
        let mut sub: BTreeMap<String, usize> = BTreeMap::new();
        for f in files.iter().filter(|f| f.starts_with(&format!("{big}/"))) {
            *sub.entry(top(f, depth + 1).unwrap_or_else(|| big.clone()))
                .or_default() += 1;
        }
        let splits = sub.keys().filter(|k| **k != big).count();
        if n * 100 / total <= 40 || splits < 2 || groups.len() - 1 + sub.len() > MAX_AREAS * 2 {
            break;
        }
        groups.remove(&big);
        groups.extend(sub);
    }
    // Keep the largest areas; fold the rest into "other".
    let mut v: Vec<(String, usize)> = groups.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut out: Vec<Area> = Vec::new();
    let mut other: (Vec<String>, usize) = (vec![], 0);
    for (i, (k, n)) in v.into_iter().enumerate() {
        let glob = if k.is_empty() {
            "*".to_string()
        } else {
            format!("{k}/**")
        };
        if i < MAX_AREAS - 1 && !k.is_empty() {
            out.push(Area {
                name: k,
                paths: vec![glob],
                files: n,
            });
        } else {
            other.0.push(glob);
            other.1 += n;
        }
    }
    if other.1 > 0 {
        out.push(Area {
            name: if other.0 == ["*"] {
                "root files".into()
            } else {
                "everything else".into()
            },
            paths: other.0,
            files: other.1,
        });
    }
    out
}

pub fn gather(root: &Path) -> Facts {
    let files: Vec<String> = git(root, &["ls-files"])
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let mut churn: BTreeMap<String, usize> = BTreeMap::new();
    for l in git(
        root,
        &[
            "log",
            "--since=1.year",
            "--name-only",
            "--format=",
            "-n",
            "2000",
        ],
    )
    .unwrap_or_default()
    .lines()
    .filter(|l| !l.is_empty())
    {
        *churn.entry(l.to_string()).or_default() += 1;
    }
    let mut churn: Vec<(String, usize)> = churn
        .into_iter()
        .filter(|(f, _)| files.contains(f))
        .collect();
    churn.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    churn.truncate(25);
    let log = git(
        root,
        &[
            "log",
            "-n",
            "300",
            "-i",
            "-E",
            "--grep=(fix|bug|regress|revert|crash|broke|hotfix)",
            "--name-only",
            "--format=@@%h %s",
        ],
    )
    .unwrap_or_default();
    let mut fixes: Vec<(String, String, Vec<String>)> = Vec::new();
    for l in log.lines().filter(|l| !l.is_empty()) {
        if let Some(h) = l.strip_prefix("@@") {
            let (hash, subject) = h.split_once(' ').unwrap_or((h, ""));
            fixes.push((hash.into(), subject.into(), vec![]));
        } else if let Some(last) = fixes.last_mut() {
            last.2.push(l.to_string());
        }
    }
    let name = |f: &String| f.rsplit('/').next().unwrap_or(f).to_string();
    Facts {
        areas: areas(&files),
        instructions: files
            .iter()
            .filter(|f| INSTRUCTIONS.contains(&name(f).as_str()))
            .cloned()
            .collect(),
        manifests: files
            .iter()
            .filter(|f| MANIFESTS.contains(&name(f).as_str()) && f.matches('/').count() <= 2)
            .take(30)
            .cloned()
            .collect(),
        churn,
        fixes,
        files,
    }
}

fn in_area(a: &Area, f: &str) -> bool {
    a.paths.iter().any(|p| {
        p == "*" && !f.contains('/')
            || p.strip_suffix("/**")
                .is_some_and(|d| f.starts_with(&format!("{d}/")))
    })
}

impl Facts {
    pub fn render(&self) -> String {
        let mut ext: BTreeMap<String, usize> = BTreeMap::new();
        for f in &self.files {
            let e = f
                .rsplit('/')
                .next()
                .and_then(|n| n.rsplit_once('.'))
                .map(|(_, e)| e.to_lowercase())
                .unwrap_or_else(|| "(none)".into());
            *ext.entry(e).or_default() += 1;
        }
        let mut ext: Vec<_> = ext.into_iter().collect();
        ext.sort_by_key(|e| std::cmp::Reverse(e.1));
        let mut s = format!(
            "{} tracked files. By extension: {}\n",
            self.files.len(),
            ext.iter()
                .take(12)
                .map(|(e, n)| format!("{e} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        s.push_str(&format!(
            "Areas: {}\n",
            self.areas
                .iter()
                .map(|a| format!("{} ({} files)", a.name, a.files))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if !self.manifests.is_empty() {
            s.push_str(&format!("Build manifests: {}\n", self.manifests.join(", ")));
        }
        if !self.instructions.is_empty() {
            s.push_str(&format!(
                "Project instruction files: {}\n",
                self.instructions.join(", ")
            ));
        }
        if !self.churn.is_empty() {
            s.push_str(&format!(
                "Most-changed files (commits in the last year): {}\n",
                self.churn
                    .iter()
                    .take(15)
                    .map(|(f, n)| format!("{f} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.fixes.is_empty() {
            s.push_str(&format!(
                "{} recent bug-fix commits, for example:\n",
                self.fixes.len()
            ));
            for (h, subj, _) in self.fixes.iter().take(15) {
                s.push_str(&format!("- {h} {subj}\n"));
            }
        }
        s
    }

    /// The facts that concern one area.
    pub fn for_area(&self, a: &Area) -> String {
        let churn: Vec<String> = self
            .churn
            .iter()
            .filter(|(f, _)| in_area(a, f))
            .take(10)
            .map(|(f, n)| format!("{f} ({n})"))
            .collect();
        let fixes: Vec<String> = self
            .fixes
            .iter()
            .filter(|(_, _, fs)| fs.iter().any(|f| in_area(a, f)))
            .take(12)
            .map(|(h, s, _)| format!("- {h} {s}"))
            .collect();
        let mut s = format!("{} files.\n", a.files);
        if !churn.is_empty() {
            s.push_str(&format!("Most-changed: {}\n", churn.join(", ")));
        }
        if fixes.is_empty() {
            s.push_str("No bug-fix commits found touching this area.\n");
        } else {
            s.push_str(&format!(
                "Bug-fix commits touching it:\n{}\n",
                fixes.join("\n")
            ));
        }
        let inst: Vec<&String> = self.instructions.iter().filter(|f| in_area(a, f)).collect();
        if !inst.is_empty() {
            s.push_str(&format!(
                "Instruction files here: {}\n",
                inst.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(spec: &[(&str, usize)]) -> Vec<String> {
        spec.iter()
            .flat_map(|(p, n)| (0..*n).map(move |i| format!("{p}/f{i}.rs")))
            .collect()
    }

    #[test]
    fn a_dominant_folder_is_split() {
        let mut f = files(&[
            ("crates/core/src", 50),
            ("crates/cli/src", 30),
            ("crates/ai/src", 10),
            ("docs", 5),
        ]);
        f.push("Cargo.toml".into());
        let a = areas(&f);
        let names: Vec<&str> = a.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "crates/core",
                "crates/cli",
                "crates/ai",
                "docs",
                "root files"
            ]
        );
        assert_eq!(a[0].paths, ["crates/core/**"]);
        assert_eq!(
            a.iter().map(|a| a.files).sum::<usize>(),
            f.len(),
            "every file is in exactly one area"
        );
    }

    #[test]
    fn many_folders_are_capped() {
        let spec: Vec<(String, usize)> = (0..12).map(|i| (format!("d{i:02}"), 12 - i)).collect();
        let spec: Vec<(&str, usize)> = spec.iter().map(|(p, n)| (p.as_str(), *n)).collect();
        let a = areas(&files(&spec));
        assert_eq!(a.len(), MAX_AREAS);
        assert_eq!(a.last().unwrap().name, "everything else");
        assert_eq!(a.last().unwrap().paths.len(), 12 - (MAX_AREAS - 1));
    }

    #[test]
    fn facts_from_a_repository() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let run = |a: &[&str]| {
            assert!(crate::git::fixture()
                .args(a)
                .current_dir(r)
                .output()
                .unwrap()
                .status
                .success())
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(r.join("src")).unwrap();
        std::fs::write(r.join("src/lib.rs"), "x").unwrap();
        std::fs::write(r.join("Cargo.toml"), "x").unwrap();
        std::fs::write(r.join("AGENTS.md"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "start"]);
        std::fs::write(r.join("src/lib.rs"), "y").unwrap();
        run(&["commit", "-q", "-am", "Fix off-by-one in parser"]);
        let f = gather(r);
        assert_eq!(f.files.len(), 3);
        assert_eq!(f.manifests, ["Cargo.toml"]);
        assert_eq!(f.instructions, ["AGENTS.md"]);
        assert_eq!(f.churn[0], ("src/lib.rs".to_string(), 2));
        assert_eq!(f.fixes.len(), 1);
        assert_eq!(f.fixes[0].2, ["src/lib.rs"]);
        let src = f.areas.iter().find(|a| a.name == "src").unwrap();
        assert!(f.for_area(src).contains("Fix off-by-one in parser"));
        assert!(f.render().contains("Most-changed files"));
    }
}
