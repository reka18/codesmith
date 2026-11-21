//! The built-in language servers and how each is found, ported from
//! OpenCode's `lsp/server.ts`. CodeSmith never downloads a server: each
//! built-in is used only when it is already installed (on `PATH`, or in the
//! project's `node_modules/.bin` for JavaScript tooling).

use super::client::Launch;
use crate::format::which;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// How a server's workspace root is found for a file.
#[derive(Debug, Clone)]
pub enum Root {
    /// The nearest folder (walking up from the file to the project root)
    /// holding one of `include`; the project root when none does. With an
    /// `exclude` marker found, the server does not apply.
    Nearest {
        include: &'static [&'static str],
        exclude: &'static [&'static str],
    },
    /// Like `Nearest`, but the server does not apply without a marker.
    Required(&'static [&'static str]),
    /// `go.work`, else `go.mod`/`go.sum`, else the project root.
    Go,
    /// The nearest crate, widened to its Cargo workspace.
    Rust,
    /// Always the project root.
    Project,
}

/// How a server's command is found once its root is known.
#[derive(Debug, Clone)]
pub enum Spawn {
    /// An executable on PATH, with arguments.
    Exe(&'static str, &'static [&'static str]),
    /// `node_modules/.bin/<bin>` walking up from the root, else PATH.
    Node(&'static str, &'static [&'static str]),
    /// Server-specific detection.
    Custom(fn(&Path, &Path) -> Option<Launch>),
    /// A configured command.
    Fixed(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct Info {
    pub id: String,
    pub extensions: Vec<String>,
    pub root: Root,
    pub spawn: Spawn,
    pub env: std::collections::BTreeMap<String, String>,
    pub initialization: Option<Value>,
}

const LOCKS: &[&str] = &[
    "package-lock.json",
    "bun.lockb",
    "bun.lock",
    "pnpm-lock.yaml",
    "yarn.lock",
];
const JS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"];
const DENO: &[&str] = &["deno.json", "deno.jsonc"];

fn info(id: &str, ext: &[&str], root: Root, spawn: Spawn) -> Info {
    Info {
        id: id.into(),
        extensions: ext.iter().map(|s| s.to_string()).collect(),
        root,
        spawn,
        env: Default::default(),
        initialization: None,
    }
}

fn near(include: &'static [&'static str]) -> Root {
    Root::Nearest {
        include,
        exclude: &[],
    }
}

/// OpenCode's built-in servers, in its order. Its `razor` (needs the VS
/// Code C# extension) and experimental `ty` are left out; configure them as
/// custom servers.
pub fn builtins() -> Vec<Info> {
    use Spawn::*;
    let js_root = || Root::Nearest {
        include: LOCKS,
        exclude: DENO,
    };
    vec![
        info(
            "deno",
            &[".ts", ".tsx", ".js", ".jsx", ".mjs"],
            Root::Required(DENO),
            Exe("deno", &["lsp"]),
        ),
        info("typescript", JS, js_root(), Custom(typescript)),
        info(
            "vue",
            &[".vue"],
            near(LOCKS),
            Custom(|root, project| tsdk_server("vue-language-server", root, project)),
        ),
        info(
            "eslint",
            &[
                ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue",
            ],
            near(LOCKS),
            Custom(eslint),
        ),
        info(
            "oxlint",
            &[
                ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue", ".astro",
                ".svelte",
            ],
            near(&[
                ".oxlintrc.json",
                "package-lock.json",
                "bun.lockb",
                "bun.lock",
                "pnpm-lock.yaml",
                "yarn.lock",
                "package.json",
            ]),
            Custom(oxlint),
        ),
        info(
            "biome",
            &[
                ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".json", ".jsonc",
                ".vue", ".astro", ".svelte", ".css", ".graphql", ".gql", ".html",
            ],
            near(&[
                "biome.json",
                "biome.jsonc",
                "package-lock.json",
                "bun.lockb",
                "bun.lock",
                "pnpm-lock.yaml",
                "yarn.lock",
            ]),
            Node("biome", &["lsp-proxy"]),
        ),
        info("gopls", &[".go"], Root::Go, Exe("gopls", &[])),
        info(
            "ruby-lsp",
            &[".rb", ".rake", ".gemspec", ".ru"],
            near(&["Gemfile"]),
            Exe("rubocop", &["--lsp"]),
        ),
        info(
            "pyright",
            &[".py", ".pyi"],
            near(&[
                "pyproject.toml",
                "setup.py",
                "setup.cfg",
                "requirements.txt",
                "Pipfile",
                "pyrightconfig.json",
            ]),
            Custom(pyright),
        ),
        info(
            "elixir-ls",
            &[".ex", ".exs"],
            near(&["mix.exs", "mix.lock"]),
            Custom(|_, _| {
                which("elixir-ls")
                    .or_else(|| which("language_server.sh"))
                    .map(|b| launch(b, &[]))
            }),
        ),
        info(
            "zls",
            &[".zig", ".zon"],
            near(&["build.zig"]),
            Exe("zls", &[]),
        ),
        info(
            "csharp",
            &[".cs", ".csx"],
            near(&["*.slnx", "*.sln", "*.csproj", "global.json"]),
            Exe("csharp-ls", &[]),
        ),
        info(
            "fsharp",
            &[".fs", ".fsi", ".fsx", ".fsscript"],
            near(&["*.slnx", "*.sln", "*.fsproj", "global.json"]),
            Exe("fsautocomplete", &[]),
        ),
        info(
            "sourcekit-lsp",
            &[".swift", ".objc", ".objcpp"],
            near(&["Package.swift", "*.xcodeproj", "*.xcworkspace"]),
            Custom(sourcekit),
        ),
        info("rust", &[".rs"], Root::Rust, Exe("rust-analyzer", &[])),
        info(
            "clangd",
            &[
                ".c", ".cpp", ".cc", ".cxx", ".c++", ".h", ".hpp", ".hh", ".hxx", ".h++",
            ],
            near(&[
                "compile_commands.json",
                "compile_flags.txt",
                ".clangd",
                "CMakeLists.txt",
                "Makefile",
            ]),
            Exe("clangd", &["--background-index", "--clang-tidy"]),
        ),
        info(
            "svelte",
            &[".svelte"],
            near(LOCKS),
            Node("svelteserver", &["--stdio"]),
        ),
        info(
            "astro",
            &[".astro"],
            near(LOCKS),
            Custom(|root, project| tsdk_server("astro-ls", root, project)),
        ),
        info(
            "jdtls",
            &[".java"],
            near(&[
                "pom.xml",
                "build.gradle",
                "build.gradle.kts",
                ".project",
                ".classpath",
            ]),
            Custom(jdtls),
        ),
        info(
            "kotlin-ls",
            &[".kt", ".kts"],
            near(&[
                "settings.gradle.kts",
                "settings.gradle",
                "build.gradle.kts",
                "build.gradle",
                "pom.xml",
            ]),
            Exe("kotlin-lsp", &["--stdio"]),
        ),
        info(
            "yaml-ls",
            &[".yaml", ".yml"],
            near(LOCKS),
            Node("yaml-language-server", &["--stdio"]),
        ),
        info(
            "lua-ls",
            &[".lua"],
            near(&[
                ".luarc.json",
                ".luarc.jsonc",
                ".luacheckrc",
                ".stylua.toml",
                "stylua.toml",
                "selene.toml",
                "selene.yml",
            ]),
            Exe("lua-language-server", &[]),
        ),
        {
            let mut i = info(
                "php intelephense",
                &[".php"],
                near(&["composer.json", "composer.lock", ".php-version"]),
                Node("intelephense", &["--stdio"]),
            );
            i.initialization = Some(json!({"telemetry": {"enabled": false}}));
            i
        },
        info(
            "prisma",
            &[".prisma"],
            near(&["schema.prisma", "prisma/schema.prisma", "prisma"]),
            Node("prisma", &["language-server"]),
        ),
        info(
            "dart",
            &[".dart"],
            near(&["pubspec.yaml", "analysis_options.yaml"]),
            Exe("dart", &["language-server", "--lsp"]),
        ),
        info(
            "ocaml-lsp",
            &[".ml", ".mli"],
            near(&["dune-project", "dune-workspace", ".merlin", "opam"]),
            Exe("ocamllsp", &[]),
        ),
        info(
            "bash",
            &[".sh", ".bash", ".zsh", ".ksh"],
            Root::Project,
            Node("bash-language-server", &["start"]),
        ),
        info(
            "terraform",
            &[".tf", ".tfvars"],
            near(&[".terraform.lock.hcl", "terraform.tfstate", "*.tf"]),
            Exe("terraform-ls", &["serve"]),
        ),
        info(
            "texlab",
            &[".tex", ".bib"],
            near(&[".latexmkrc", "latexmkrc", ".texlabroot", "texlabroot"]),
            Exe("texlab", &[]),
        ),
        info(
            "dockerfile",
            &[".dockerfile", "Dockerfile"],
            Root::Project,
            Node("docker-langserver", &["--stdio"]),
        ),
        info(
            "gleam",
            &[".gleam"],
            near(&["gleam.toml"]),
            Exe("gleam", &["lsp"]),
        ),
        info(
            "clojure-lsp",
            &[".clj", ".cljs", ".cljc", ".edn"],
            near(&[
                "deps.edn",
                "project.clj",
                "shadow-cljs.edn",
                "bb.edn",
                "build.boot",
            ]),
            Exe("clojure-lsp", &["listen"]),
        ),
        info("nixd", &[".nix"], near(&["flake.nix"]), Exe("nixd", &[])),
        info(
            "tinymist",
            &[".typ", ".typc"],
            near(&["typst.toml"]),
            Exe("tinymist", &[]),
        ),
        info(
            "haskell-language-server",
            &[".hs", ".lhs"],
            near(&["stack.yaml", "cabal.project", "hie.yaml", "*.cabal"]),
            Exe("haskell-language-server-wrapper", &["--lsp"]),
        ),
        info(
            "julials",
            &[".jl"],
            near(&["Project.toml", "Manifest.toml", "*.jl"]),
            Exe(
                "julia",
                &[
                    "--startup-file=no",
                    "--history-file=no",
                    "-e",
                    "using LanguageServer; runserver()",
                ],
            ),
        ),
    ]
}

/// Whether `dir` holds `marker` (`*.ext` matches any file with that suffix).
fn has_marker(dir: &Path, marker: &str) -> bool {
    match marker.strip_prefix('*') {
        Some(suffix) => std::fs::read_dir(dir).is_ok_and(|rd| {
            rd.flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(suffix))
        }),
        None => dir.join(marker).exists(),
    }
}

/// The nearest folder from `start` up to `stop` holding one of `markers`.
pub fn find_up(markers: &[&str], start: &Path, stop: &Path) -> Option<PathBuf> {
    let mut d = Some(start);
    while let Some(cur) = d {
        if markers.iter().any(|m| has_marker(cur, m)) {
            return Some(cur.to_path_buf());
        }
        if cur == stop || !cur.starts_with(stop) {
            return None;
        }
        d = cur.parent();
    }
    None
}

impl Root {
    /// The workspace root for `file`, or `None` when the server does not apply.
    pub fn find(&self, file: &Path, project: &Path) -> Option<PathBuf> {
        let start = file.parent().unwrap_or(project);
        match self {
            Root::Nearest { include, exclude } => {
                if !exclude.is_empty() && find_up(exclude, start, project).is_some() {
                    return None;
                }
                Some(find_up(include, start, project).unwrap_or_else(|| project.to_path_buf()))
            }
            Root::Required(markers) => find_up(markers, start, project),
            Root::Go => Some(
                find_up(&["go.work"], start, project)
                    .or_else(|| find_up(&["go.mod", "go.sum"], start, project))
                    .unwrap_or_else(|| project.to_path_buf()),
            ),
            Root::Rust => {
                let krate = find_up(&["Cargo.toml", "Cargo.lock"], start, project)
                    .unwrap_or_else(|| project.to_path_buf());
                let mut d = Some(krate.as_path());
                while let Some(cur) = d {
                    if !cur.starts_with(project) {
                        break;
                    }
                    let toml = std::fs::read_to_string(cur.join("Cargo.toml")).unwrap_or_default();
                    if toml.contains("[workspace]") {
                        return Some(cur.to_path_buf());
                    }
                    d = cur.parent();
                }
                Some(krate)
            }
            Root::Project => Some(project.to_path_buf()),
        }
    }
}

fn launch(bin: impl Into<PathBuf>, args: &[&str]) -> Launch {
    let mut command = vec![bin.into().display().to_string()];
    command.extend(args.iter().map(|s| s.to_string()));
    Launch {
        command,
        ..Default::default()
    }
}

/// `node_modules/<rel>` walking up from `root` to `project`.
fn node_module(rel: &str, root: &Path, project: &Path) -> Option<PathBuf> {
    let mut d = Some(root);
    while let Some(cur) = d {
        let p = cur.join("node_modules").join(rel);
        if p.exists() {
            return Some(p);
        }
        if cur == project || !cur.starts_with(project) {
            return None;
        }
        d = cur.parent();
    }
    None
}

fn node_bin(bin: &str, root: &Path, project: &Path) -> Option<PathBuf> {
    node_module(&format!(".bin/{bin}"), root, project).or_else(|| which(bin))
}

impl Spawn {
    pub fn detect(&self, root: &Path, project: &Path) -> Option<Launch> {
        match self {
            Spawn::Exe(name, args) => which(name).map(|b| launch(b, args)),
            Spawn::Node(bin, args) => node_bin(bin, root, project).map(|b| launch(b, args)),
            Spawn::Custom(f) => f(root, project),
            Spawn::Fixed(c) => (!c.is_empty()).then(|| Launch {
                command: c.clone(),
                ..Default::default()
            }),
        }
    }
}

/// Needs the project's own `typescript` package, as in OpenCode.
fn typescript(root: &Path, project: &Path) -> Option<Launch> {
    let tsserver = node_module("typescript/lib/tsserver.js", root, project)?;
    let mut l = launch(
        node_bin("typescript-language-server", root, project)?,
        &["--stdio"],
    );
    l.initialization = Some(json!({"tsserver": {"path": tsserver}}));
    Some(l)
}

/// Servers that want the project's TypeScript SDK folder.
fn tsdk_server(bin: &str, root: &Path, project: &Path) -> Option<Launch> {
    let mut l = launch(node_bin(bin, root, project)?, &["--stdio"]);
    if let Some(lib) = node_module("typescript/lib", root, project) {
        l.initialization = Some(json!({"typescript": {"tsdk": lib}}));
    }
    Some(l)
}

/// Needs `eslint` in the project and the VS Code ESLint server installed.
fn eslint(root: &Path, project: &Path) -> Option<Launch> {
    node_module("eslint", root, project)?;
    Some(launch(
        node_bin("vscode-eslint-language-server", root, project)?,
        &["--stdio"],
    ))
}

/// Needs `oxlint` in the project; newer versions serve LSP themselves.
fn oxlint(root: &Path, project: &Path) -> Option<Launch> {
    if let Some(bin) = node_module(".bin/oxlint", root, project) {
        let help = std::process::Command::new(&bin)
            .env_remove(crate::SERVER_TOKEN_VAR)
            .arg("--help")
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        if String::from_utf8_lossy(&help.stdout).contains("--lsp") {
            return Some(launch(bin, &["--lsp"]));
        }
    }
    node_module(".bin/oxc_language_server", root, project).map(|b| launch(b, &[]))
}

/// Points pyright at the project's virtual environment when there is one.
fn pyright(root: &Path, project: &Path) -> Option<Launch> {
    let mut l = launch(node_bin("pyright-langserver", root, project)?, &["--stdio"]);
    let venvs = std::env::var_os("VIRTUAL_ENV")
        .map(PathBuf::from)
        .into_iter()
        .chain([root.join(".venv"), root.join("venv"), project.join(".venv")]);
    for v in venvs {
        for py in [v.join("bin/python"), v.join("Scripts/python.exe")] {
            if py.exists() {
                l.initialization = Some(json!({"pythonPath": py}));
                return Some(l);
            }
        }
    }
    Some(l)
}

fn sourcekit(_: &Path, _: &Path) -> Option<Launch> {
    if let Some(b) = which("sourcekit-lsp") {
        return Some(launch(b, &[]));
    }
    which("xcrun")?;
    let out = std::process::Command::new("xcrun")
        .env_remove(crate::SERVER_TOKEN_VAR)
        .args(["--find", "sourcekit-lsp"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !path.is_empty()).then(|| launch(path, &[]))
}

/// jdtls keeps per-workspace data; give each root its own folder.
fn jdtls(root: &Path, _: &Path) -> Option<Launch> {
    let bin = which("jdtls")?;
    let data = jdtls_data(root).ok()?;
    let mut l = launch(bin, &["-data"]);
    l.command.push(data.display().to_string());
    Some(l)
}

/// jdtls's data folder for `root`, in this user's cache
/// (`<cache>/jdtls/<hash of root>`), created owner-only. Not a fixed name
/// in the shared temporary directory, which another local user could
/// create or link first to read or plant the workspace's index. A folder
/// that is a symlink or someone else's is refused.
fn jdtls_data(root: &Path) -> std::io::Result<PathBuf> {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(root.display().to_string().as_bytes());
    let id: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let base = crate::paths::cache_dir().join("jdtls");
    crate::paths::owned_private_dir(&base)?;
    let data = base.join(id);
    crate::paths::owned_private_dir(&data)?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jdtls_data_is_private_and_per_root() {
        let a = jdtls_data(Path::new("/some/project")).unwrap();
        assert_eq!(a, jdtls_data(Path::new("/some/project")).unwrap());
        assert_ne!(a, jdtls_data(Path::new("/other/project")).unwrap());
        assert!(a.starts_with(crate::paths::cache_dir()), "{}", a.display());
        assert!(!a.starts_with(std::env::temp_dir().join("codesmith-jdtls")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&a).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
            // A symlink planted in its place is refused.
            let planted = jdtls_data(Path::new("/planted")).unwrap();
            std::fs::remove_dir(&planted).unwrap();
            let elsewhere = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(elsewhere.path(), &planted).unwrap();
            assert!(jdtls_data(Path::new("/planted")).is_err());
            std::fs::remove_file(&planted).unwrap();
        }
    }

    #[test]
    fn ids_are_unique_and_match_opencode() {
        let list = builtins();
        let mut ids: Vec<&str> = list.iter().map(|s| s.id.as_str()).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
        for id in ["typescript", "gopls", "rust", "pyright", "clangd", "deno"] {
            assert!(ids.contains(&id), "{id}");
        }
        assert!(list.iter().all(|s| !s.extensions.is_empty()));
    }

    #[test]
    fn nearest_root_and_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("pkg/src")).unwrap();
        std::fs::write(p.join("pkg/yarn.lock"), "").unwrap();
        let file = p.join("pkg/src/a.ts");
        let ts = builtins()
            .into_iter()
            .find(|s| s.id == "typescript")
            .unwrap();
        assert_eq!(ts.root.find(&file, p), Some(p.join("pkg")));
        // Without a marker, the project root.
        let other = p.join("other/b.ts");
        assert_eq!(ts.root.find(&other, p), Some(p.to_path_buf()));
        // A Deno project is left to deno.
        std::fs::write(p.join("pkg/deno.json"), "{}").unwrap();
        assert_eq!(ts.root.find(&file, p), None);
        let deno = builtins().into_iter().find(|s| s.id == "deno").unwrap();
        assert_eq!(deno.root.find(&file, p), Some(p.join("pkg")));
        assert_eq!(deno.root.find(&other, p), None);
    }

    #[test]
    fn rust_root_widens_to_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("crates/a/src")).unwrap();
        std::fs::write(p.join("crates/a/Cargo.toml"), "[package]\nname=\"a\"").unwrap();
        let file = p.join("crates/a/src/lib.rs");
        assert_eq!(Root::Rust.find(&file, p), Some(p.join("crates/a")));
        std::fs::write(p.join("Cargo.toml"), "[workspace]\nmembers=[\"crates/*\"]").unwrap();
        assert_eq!(Root::Rust.find(&file, p), Some(p.to_path_buf()));
    }

    #[test]
    fn glob_markers_and_go_preference() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("svc/app")).unwrap();
        std::fs::write(p.join("svc/app/App.csproj"), "").unwrap();
        assert!(has_marker(&p.join("svc/app"), "*.csproj"));
        assert!(!has_marker(&p.join("svc"), "*.csproj"));
        std::fs::write(p.join("svc/app/go.mod"), "").unwrap();
        std::fs::write(p.join("go.work"), "").unwrap();
        assert_eq!(
            Root::Go.find(&p.join("svc/app/main.go"), p),
            Some(p.to_path_buf())
        );
    }

    #[test]
    fn typescript_needs_the_project_typescript() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert!(typescript(p, p).is_none());
        std::fs::create_dir_all(p.join("node_modules/typescript/lib")).unwrap();
        std::fs::write(p.join("node_modules/typescript/lib/tsserver.js"), "").unwrap();
        std::fs::create_dir_all(p.join("node_modules/.bin")).unwrap();
        std::fs::write(p.join("node_modules/.bin/typescript-language-server"), "").unwrap();
        let l = typescript(p, p).unwrap();
        assert!(l.command[0].ends_with("typescript-language-server"));
        assert!(l.initialization.unwrap()["tsserver"]["path"]
            .as_str()
            .unwrap()
            .ends_with("tsserver.js"));
    }
}
