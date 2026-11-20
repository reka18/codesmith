//! Formatting files after the agent writes or edits them, as in OpenCode:
//! opt-in with `"formatter": true` (or an object of overrides), each
//! built-in formatter enabled only when the project uses it.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// `formatter` in codesmith.json.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Setting {
    Enabled(bool),
    Custom(BTreeMap<String, Entry>),
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Entry {
    pub disabled: bool,
    /// Command with `$FILE` for the path; replaces detection.
    pub command: Option<Vec<String>>,
    pub environment: BTreeMap<String, String>,
    pub extensions: Option<Vec<String>>,
}

/// How a formatter decides whether it applies here.
#[derive(Debug, Clone, PartialEq)]
enum Detect {
    /// The executable is on PATH.
    Exe(&'static str, &'static [&'static str]),
    /// The executable is on PATH and one of these files is found walking up.
    ExeWithFile(
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
    ),
    /// A package.json walking up depends on the package; run its node_modules/.bin binary.
    Npm {
        package: &'static str,
        bin: &'static str,
        args: &'static [&'static str],
        config: &'static [&'static str],
    },
    Ruff,
    Air,
    Uv,
    Pint,
    Fixed(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct Info {
    pub name: String,
    pub extensions: Vec<String>,
    pub environment: BTreeMap<String, String>,
    detect: Detect,
}

const WEB: &[&str] = &[
    ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".html", ".htm", ".css", ".scss",
    ".sass", ".less", ".vue", ".svelte", ".json", ".jsonc", ".yaml", ".yml", ".toml", ".xml",
    ".md", ".mdx", ".graphql", ".gql",
];
const JS: &[&str] = &[".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts"];
const RUBY: &[&str] = &[".rb", ".rake", ".gemspec", ".ru"];

fn info(name: &str, ext: &[&str], detect: Detect) -> Info {
    let mut environment = BTreeMap::new();
    if matches!(detect, Detect::Npm { .. }) {
        environment.insert("BUN_BE_BUN".into(), "1".into());
    }
    Info {
        name: name.into(),
        extensions: ext.iter().map(|s| s.to_string()).collect(),
        environment,
        detect,
    }
}

/// OpenCode's built-in formatters, in its order (the first that applies wins).
pub fn builtins() -> Vec<Info> {
    use Detect::*;
    vec![
        info("gofmt", &[".go"], Exe("gofmt", &["-w", "$FILE"])),
        info(
            "mix",
            &[".ex", ".exs", ".eex", ".heex", ".leex", ".neex", ".sface"],
            Exe("mix", &["format", "$FILE"]),
        ),
        info(
            "oxfmt",
            JS,
            Npm {
                package: "oxfmt",
                bin: "oxfmt",
                args: &["$FILE"],
                config: &[],
            },
        ),
        info(
            "prettier",
            WEB,
            Npm {
                package: "prettier",
                bin: "prettier",
                args: &["--write", "$FILE"],
                config: &[],
            },
        ),
        info(
            "biome",
            WEB,
            Npm {
                package: "@biomejs/biome",
                bin: "biome",
                args: &["format", "--write", "$FILE"],
                config: &["biome.json", "biome.jsonc"],
            },
        ),
        info("zig", &[".zig", ".zon"], Exe("zig", &["fmt", "$FILE"])),
        info(
            "clang-format",
            &[
                ".c", ".cc", ".cpp", ".cxx", ".c++", ".h", ".hh", ".hpp", ".hxx", ".h++", ".ino",
                ".C", ".H",
            ],
            ExeWithFile("clang-format", &["-i", "$FILE"], &[".clang-format"]),
        ),
        info("ktlint", &[".kt", ".kts"], Exe("ktlint", &["-F", "$FILE"])),
        info("ruff", &[".py", ".pyi"], Ruff),
        info("air", &[".R"], Air),
        info("uv", &[".py", ".pyi"], Uv),
        info("rubocop", RUBY, Exe("rubocop", &["--autocorrect", "$FILE"])),
        info("standardrb", RUBY, Exe("standardrb", &["--fix", "$FILE"])),
        info(
            "htmlbeautifier",
            &[".erb", ".html.erb"],
            Exe("htmlbeautifier", &["$FILE"]),
        ),
        info("dart", &[".dart"], Exe("dart", &["format", "$FILE"])),
        info(
            "ocamlformat",
            &[".ml", ".mli"],
            ExeWithFile("ocamlformat", &["-i", "$FILE"], &[".ocamlformat"]),
        ),
        info(
            "terraform",
            &[".tf", ".tfvars"],
            Exe("terraform", &["fmt", "$FILE"]),
        ),
        info(
            "latexindent",
            &[".tex"],
            Exe("latexindent", &["-w", "-s", "$FILE"]),
        ),
        info("gleam", &[".gleam"], Exe("gleam", &["format", "$FILE"])),
        info("shfmt", &[".sh", ".bash"], Exe("shfmt", &["-w", "$FILE"])),
        info("nixfmt", &[".nix"], Exe("nixfmt", &["$FILE"])),
        info("rustfmt", &[".rs"], Exe("rustfmt", &["$FILE"])),
        info("pint", &[".php"], Pint),
        info("ormolu", &[".hs"], Exe("ormolu", &["-i", "$FILE"])),
        info(
            "cljfmt",
            &[".clj", ".cljs", ".cljc", ".edn"],
            Exe("cljfmt", &["fix", "--quiet", "$FILE"]),
        ),
        info("dfmt", &[".d"], Exe("dfmt", &["-i", "$FILE"])),
    ]
}

/// The formatters a config enables: none unless `formatter` is set.
pub fn resolve(setting: Option<&Setting>) -> Vec<Info> {
    match setting {
        None | Some(Setting::Enabled(false)) => vec![],
        Some(Setting::Enabled(true)) => builtins(),
        Some(Setting::Custom(map)) => {
            let mut list = builtins();
            for (name, e) in map {
                if e.disabled {
                    list.retain(|f| &f.name != name);
                    continue;
                }
                let base = list.iter().position(|f| &f.name == name);
                let builtin = base.map(|i| list[i].clone());
                let mut environment = builtin
                    .as_ref()
                    .map(|b| b.environment.clone())
                    .unwrap_or_default();
                environment.extend(e.environment.clone());
                let f = Info {
                    name: name.clone(),
                    extensions: e
                        .extensions
                        .clone()
                        .or_else(|| builtin.as_ref().map(|b| b.extensions.clone()))
                        .unwrap_or_default(),
                    environment,
                    detect: match (&e.command, &builtin) {
                        (Some(c), _) => Detect::Fixed(c.clone()),
                        (None, Some(b)) => b.detect.clone(),
                        (None, None) => Detect::Fixed(vec![]),
                    },
                };
                match base {
                    Some(i) => list[i] = f,
                    None => list.push(f),
                }
            }
            list
        }
    }
}

/// An executable on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.is_file()
            && p.metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// `target` in `dir` and each parent up to `root`, nearest first.
fn find_up(target: &str, dir: &Path, root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut d = Some(dir);
    while let Some(cur) = d {
        let p = cur.join(target);
        if p.exists() {
            out.push(p);
        }
        if cur == root {
            break;
        }
        d = cur.parent();
    }
    out
}

fn has_dependency(package_json: &Path, dep: &str) -> bool {
    let Ok(v) = std::fs::read_to_string(package_json)
        .map(|t| serde_json::from_str::<serde_json::Value>(&t).unwrap_or_default())
    else {
        return false;
    };
    ["dependencies", "devDependencies"]
        .iter()
        .any(|k| v[k].get(dep).is_some())
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn with_args(bin: impl Into<PathBuf>, args: &[&str]) -> Vec<String> {
    let mut v = vec![bin.into().display().to_string()];
    v.extend(args.iter().map(|s| s.to_string()));
    v
}

fn detect(d: &Detect, dir: &Path, root: &Path) -> Option<Vec<String>> {
    match d {
        Detect::Fixed(c) => (!c.is_empty()).then(|| c.clone()),
        Detect::Exe(name, args) => which(name).map(|b| with_args(b, args)),
        Detect::ExeWithFile(name, args, files) => {
            if files.iter().all(|f| find_up(f, dir, root).is_empty()) {
                return None;
            }
            which(name).map(|b| with_args(b, args))
        }
        Detect::Npm {
            package,
            bin,
            args,
            config,
        } => {
            if !config.is_empty() && config.iter().all(|f| find_up(f, dir, root).is_empty()) {
                return None;
            }
            let pkgs = find_up("package.json", dir, root);
            if config.is_empty() && !pkgs.iter().any(|p| has_dependency(p, package)) {
                return None;
            }
            find_up(&format!("node_modules/.bin/{bin}"), dir, root)
                .into_iter()
                .find(|p| is_executable(p))
                .map(|b| with_args(b, args))
        }
        Detect::Ruff => {
            let bin = which("ruff")?;
            for cfg in ["pyproject.toml", "ruff.toml", ".ruff.toml"] {
                if let Some(f) = find_up(cfg, dir, root).first() {
                    if cfg != "pyproject.toml" || read(f).contains("[tool.ruff]") {
                        return Some(with_args(bin, &["format", "$FILE"]));
                    }
                }
            }
            for dep in ["requirements.txt", "pyproject.toml", "Pipfile"] {
                if find_up(dep, dir, root)
                    .first()
                    .is_some_and(|f| read(f).contains("ruff"))
                {
                    return Some(with_args(bin, &["format", "$FILE"]));
                }
            }
            None
        }
        Detect::Air => {
            let bin = which("air")?;
            let out = std::process::Command::new(&bin)
                .env_remove(crate::SERVER_TOKEN_VAR)
                .arg("--help")
                .stdin(std::process::Stdio::null())
                .output()
                .ok()?;
            let first = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            (out.status.success() && first.contains("R language") && first.contains("formatter"))
                .then(|| with_args(bin, &["format", "$FILE"]))
        }
        Detect::Uv => {
            let bin = which("uv")?;
            let ok = std::process::Command::new(&bin)
                .env_remove(crate::SERVER_TOKEN_VAR)
                .args(["format", "--help"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            ok.then(|| with_args(bin, &["format", "--", "$FILE"]))
        }
        Detect::Pint => {
            for f in find_up("composer.json", dir, root) {
                let v: serde_json::Value = serde_json::from_str(&read(&f)).unwrap_or_default();
                if ["require", "require-dev"]
                    .iter()
                    .any(|k| v[k].get("laravel/pint").is_some())
                {
                    return Some(vec!["./vendor/bin/pint".into(), "$FILE".into()]);
                }
            }
            None
        }
    }
}

/// A resolved formatter command (`None`: not available here).
type Command = Option<Vec<String>>;

/// Formatters for one session, with each one's command resolved once.
pub struct Formatter {
    list: Vec<Info>,
    dir: PathBuf,
    root: PathBuf,
    /// (formatter, folder) → resolved command.
    cache: Mutex<HashMap<(String, PathBuf), Command>>,
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default()
}

impl Formatter {
    pub fn new(setting: Option<&Setting>, dir: PathBuf, root: PathBuf) -> Formatter {
        Formatter {
            list: resolve(setting),
            dir,
            root,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.list.is_empty()
    }

    /// Detection walks up from the file's folder to the project root, so a
    /// package in a monorepo uses its own formatter setup.
    fn command(&self, f: &Info, dir: &Path) -> Option<Vec<String>> {
        let key = (f.name.clone(), dir.to_path_buf());
        if let Some(c) = self.cache.lock().unwrap().get(&key) {
            return c.clone();
        }
        let start = if dir.starts_with(&self.root) {
            dir
        } else {
            self.root.as_path()
        };
        let c = detect(&f.detect, start, &self.root);
        self.cache.lock().unwrap().insert(key, c.clone());
        c
    }

    /// Format a file in place; returns the formatter's name if one succeeded.
    pub async fn file(&self, path: &Path) -> Option<String> {
        let ext = extension(path);
        for f in self.list.iter().filter(|f| f.extensions.contains(&ext)) {
            let dir = path.parent().unwrap_or(&self.dir);
            let Some(cmd) = tokio::task::block_in_place(|| self.command(f, dir)) else {
                continue;
            };
            let file = path.display().to_string();
            let cmd: Vec<String> = cmd.iter().map(|a| a.replace("$FILE", &file)).collect();
            let run = tokio::process::Command::new(&cmd[0])
                .env_remove(crate::SERVER_TOKEN_VAR)
                .args(&cmd[1..])
                .current_dir(&self.dir)
                .envs(&f.environment)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status();
            match tokio::time::timeout(Duration::from_secs(30), run).await {
                Ok(Ok(s)) if s.success() => return Some(f.name.clone()),
                _ => continue,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_unless_configured() {
        assert!(resolve(None).is_empty());
        assert!(resolve(Some(&Setting::Enabled(false))).is_empty());
        assert_eq!(
            resolve(Some(&Setting::Enabled(true))).len(),
            26,
            "OpenCode's 26 built-ins"
        );
    }

    #[test]
    fn config_overrides() {
        let s: Setting = serde_json::from_value(serde_json::json!({
            "rustfmt": {"disabled": true},
            "prettier": {"environment": {"X": "1"}},
            "mine": {"command": ["fmt-it", "$FILE"], "extensions": [".foo"]}
        }))
        .unwrap();
        let l = resolve(Some(&s));
        assert!(!l.iter().any(|f| f.name == "rustfmt"));
        let p = l.iter().find(|f| f.name == "prettier").unwrap();
        assert_eq!(
            (
                p.environment["X"].as_str(),
                p.environment["BUN_BE_BUN"].as_str()
            ),
            ("1", "1"),
            "builtin environment is kept and extended"
        );
        assert!(p.extensions.contains(&".tsx".to_string()));
        let m = l.last().unwrap();
        assert_eq!(
            (m.name.as_str(), m.extensions.clone()),
            ("mine", vec![".foo".to_string()])
        );
        assert_eq!(
            detect(&m.detect, Path::new("/"), Path::new("/")),
            Some(vec!["fmt-it".into(), "$FILE".into()])
        );
    }

    #[test]
    fn npm_formatters_need_the_dependency_and_binary() {
        let d = tempfile::tempdir().unwrap();
        let (root, sub) = (d.path().to_path_buf(), d.path().join("pkg/src"));
        std::fs::create_dir_all(&sub).unwrap();
        let prettier = builtins()
            .into_iter()
            .find(|f| f.name == "prettier")
            .unwrap();
        assert_eq!(detect(&prettier.detect, &sub, &root), None);
        std::fs::write(
            root.join("package.json"),
            r#"{"devDependencies": {"prettier": "^3"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect(&prettier.detect, &sub, &root),
            None,
            "dependency without an installed binary"
        );
        let bin = root.join("node_modules/.bin/prettier");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            detect(&prettier.detect, &sub, &root),
            Some(vec![
                bin.display().to_string(),
                "--write".into(),
                "$FILE".into()
            ])
        );
    }

    #[test]
    fn config_file_gates() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        assert_eq!(
            detect(
                &Detect::ExeWithFile("sh", &["$FILE"], &[".clang-format"]),
                r,
                r
            ),
            None
        );
        std::fs::write(r.join(".clang-format"), "").unwrap();
        assert!(detect(
            &Detect::ExeWithFile("sh", &["$FILE"], &[".clang-format"]),
            r,
            r
        )
        .is_some());
        std::fs::write(
            r.join("composer.json"),
            r#"{"require-dev": {"laravel/pint": "1"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect(&Detect::Pint, r, r),
            Some(vec!["./vendor/bin/pint".into(), "$FILE".into()])
        );
    }

    #[test]
    fn detection_starts_at_the_files_folder() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        let pkg = root.join("packages/web");
        std::fs::create_dir_all(pkg.join("node_modules/.bin")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"dependencies": {"prettier": "3"}}"#,
        )
        .unwrap();
        let bin = pkg.join("node_modules/.bin/prettier");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let f = Formatter::new(Some(&Setting::Enabled(true)), root.clone(), root.clone());
        let p = f.list.iter().find(|i| i.name == "prettier").unwrap();
        assert!(
            f.command(p, &pkg).is_some(),
            "a nested package's prettier is found"
        );
        assert!(
            f.command(p, &root).is_none(),
            "but not for files outside that package"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn formats_with_the_first_working_formatter() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        let s: Setting = serde_json::from_value(serde_json::json!({
            "broken": {"command": ["sh", "-c", "exit 3"], "extensions": [".txt"]},
            "upper": {"command": ["sh", "-c", "tr a-z A-Z < \"$0\" > \"$0.tmp\" && mv \"$0.tmp\" \"$0\"", "$FILE"], "extensions": [".txt"]}
        }))
        .unwrap();
        let f = Formatter::new(Some(&s), r.clone(), r.clone());
        let file = r.join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();
        assert_eq!(
            f.file(&file).await.as_deref(),
            Some("upper"),
            "a failing formatter falls through"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "HELLO\n");
        assert_eq!(
            f.file(&r.join("b.rs")).await,
            None,
            "no custom formatter for .rs and rustfmt is only used if installed"
        );
    }
}
