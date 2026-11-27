//! End-to-end scenarios: the real `codesmith` binary driven through
//! multi-turn coding tasks against a scripted model, across every front
//! end. They assert the files on disk, the session store, what the model
//! was sent and what the front end showed, because modules that pass their
//! own tests can still break each other once combined.
//!
//! Run them with
//!
//! ```text
//! cargo test -p codesmith --test e2e
//! cargo test -p codesmith --test e2e -- acp     # one group
//! ```
//!
//! Each test gets its own temp folder (a git project, `HOME`, config and
//! data folders, session database) and its own model server, so they run
//! in parallel and never read or write the user's real config, sessions
//! or `~/.claude`. Tests that need `git`, `python3` (the MCP fixtures) or
//! `script(1)` (the UI) pass with a "skipped" note when those are missing.
//!
//! - `harness`: the isolated world and the binary runner.
//! - `model`: the scripted OpenAI-compatible model (matchers → replies,
//!   every request recorded).
//! - `run_cli`: `run` (a read → edit → test fix, permission rules, piped
//!   input, commands), `session` diff/undo/redo/export/import, `stats`.
//! - `limits`: output-limit continuation, the step limit, retries, quota,
//!   content filter.
//! - `delegation`: the explore subagent, skills, custom commands, `review`.
//! - `acp`: `codesmith acp` over stdin/stdout, through replay.
//! - `serve`: `codesmith serve` over HTTP and SSE.
//! - `pty`: `codesmith serve` terminals over HTTP and WebSockets.
//! - `mcp`: stdio MCP servers with and without Code Mode, MCP prompts.
//! - `automation`: `automation add`/`run-due`/`log`.
//! - `run_bridge`: `run` as a headless agent for a bridge: the prompt on
//!   stdin alone, `--effort`, `--tools`, `--no-tools`, `--no-mcp`,
//!   `--usage-log`.
//! - `bridge`: signing in for a tailbus bridge (`auth login --home` and
//!   `--to-file`) and `bridge init` in a pseudo-terminal.
//! - `bifrost`: `run` on a fake Bifrost gateway: its model list per key,
//!   request ids, the usage log's costs and run total, and a gateway that
//!   serves only Bifrost's integration routes.
//! - `worktree`: `run -w`.
//! - `tui`: the interactive UI in a pseudo-terminal.
//! - `catalog`: the models.dev catalog: listing, resolution, background
//!   refresh.
//! - `swap`: `codesmith models swap` and a provider's `swap` setting.
//! - `repo_config`: a repository's config is never read (in `run`, `serve`,
//!   `acp`, automations and the UI, against a loopback recorder), per-repo
//!   files apply, and `codesmith trust` is gone.
//! - `folder`: starting outside a repository: the warning, the question
//!   at a terminal, the refusal in the home folder or `/` without one.
//! - `oc_cli`: OpenCode's CLI tests that need the binary (see PARITY.md).

mod harness;
mod model;

mod acp;
mod automation;
mod bifrost;
mod bridge;
mod catalog;
mod delegation;
mod folder;
mod limits;
mod mcp;
mod oc_cli;
mod pty;
mod repo_config;
mod run_bridge;
mod run_cli;
mod serve;
mod swap;
mod tui;
mod worktree;
