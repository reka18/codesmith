# Language servers and formatters

Both are **off by default**, as in OpenCode. Turn them on per project or
globally. Nothing is ever downloaded: CodeSmith uses only the tools you have
installed.

Because a project's config can turn these on, and they run programs from the
project (`node_modules/.bin`, for example), read a repository's config before
you use CodeSmith in it. See
[project config is trusted](configuration.md#files-and-precedence).

## Formatters

With `"formatter": true`, files the agent writes or edits are formatted
afterwards. When a formatter changes the file, the tool result tells the
model to read it again.

CodeSmith has OpenCode's 26 built-in formatters. Each turns on only when the
project uses it, and the first one that applies to a file wins:

- when the program is installed: `gofmt`, `mix`, `zig`, `ktlint`,
  `rubocop`, `standardrb`, `htmlbeautifier`, `dart`, `terraform`,
  `latexindent`, `gleam`, `shfmt`, `nixfmt`, `rustfmt`, `ormolu`, `cljfmt`,
  `dfmt`, and `air` and `uv` (when the installed version can format);
- when the program is installed and its config file exists: `clang-format`
  (`.clang-format`), `ocamlformat` (`.ocamlformat`), and `ruff` (`ruff.toml`,
  `.ruff.toml`, or a `[tool.ruff]` section in `pyproject.toml`);
- when a `package.json` above the file depends on them and they are
  installed in `node_modules`: `prettier` and `oxfmt`; `biome` needs a
  `biome.json[c]` instead of the dependency;
- when `composer.json` requires `laravel/pint`: `pint`.

Detection starts at the edited file's folder, so each package of a monorepo
uses its own setup.

An object instead of `true` turns formatting on and changes individual
formatters:

```jsonc
"formatter": {
  "rustfmt": { "disabled": true },
  "prettier": { "environment": { "NODE_OPTIONS": "--max-old-space-size=4096" } },
  "sqlfmt": { "command": ["sqlfmt", "$FILE"], "extensions": [".sql"] }
}
```

| Field | Meaning |
|---|---|
| `disabled` | Turn this formatter off. |
| `command` | The command, with `$FILE` for the path. Replaces detection. |
| `environment` | Extra environment variables. |
| `extensions` | File extensions it handles (required for a new formatter). |

`"formatter": false`, or leaving it out, turns formatting off.

## Language servers

With `"lsp": true`, the agent hears from your language servers after it
writes or edits a file:

1. The file is sent to the servers for its type (`didOpen`, then
   `didChange`).
2. The errors they publish within three seconds are added to the tool
   result, up to 20 per file, so the model fixes them in the next step.
   `write` also lists errors in up to five other files. Warnings and hints
   are left out.

Servers use memory and can lag behind the files, which is why they are off
by default.

The built-in servers follow OpenCode's list. Their ids (the keys you use in
config) are:

`deno`, `typescript`, `vue`, `eslint`, `oxlint`, `biome`, `gopls`, `ruby-lsp`,
`pyright`, `elixir-ls`, `zls`, `csharp`, `fsharp`, `sourcekit-lsp`, `rust`,
`clangd`, `svelte`, `astro`, `jdtls`, `kotlin-ls`, `yaml-ls`, `lua-ls`,
`prisma`, `dart`, `ocaml-lsp`, `bash`, `terraform`, `texlab`, `dockerfile`,
`gleam`, `clojure-lsp`, `nixd`, `tinymist`, `haskell-language-server`,
`julials`.

Each server is used only when its program is on `PATH`, or, for JavaScript
tooling, in the project's `node_modules/.bin`. The TypeScript server also
needs the project's own `typescript`. A server starts the first time a
matching file is touched, in the nearest folder with its project file
(`Cargo.toml` widened to the Cargo workspace, `go.mod`, `pyproject.toml`, a
lockfile, and so on). A server that fails to start is not retried, and an
edit waits at most five seconds for one that is still starting.

An object turns language servers on, keeps the built-ins, and changes or adds
servers:

```jsonc
"lsp": {
  "typescript": { "disabled": true },
  "rust": { "command": ["rust-analyzer"], "env": { "RUST_LOG": "error" } },
  "custom": {
    "command": ["custom-lsp-server", "--stdio"],
    "extensions": [".custom"],
    "initialization": { "preferences": { "strict": true } }
  }
}
```

| Field | Meaning |
|---|---|
| `disabled` | Turn this server off. |
| `command` | The command to start it. Required for a new server. |
| `extensions` | File extensions it handles. Required for a new server. |
| `env` | Extra environment variables. |
| `initialization` | Sent as `initializationOptions`, and returned for `workspace/configuration`. |

`"lsp": false`, or leaving it out, turns language servers off.

### The `lsp` tool

With language servers on, the agent also gets an `lsp` tool for
`goToDefinition`, `findReferences`, `hover`, `documentSymbol`,
`workspaceSymbol`, `goToImplementation` and call hierarchies. It is checked
with the `lsp` permission action and the file's path.
