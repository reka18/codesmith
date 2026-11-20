//! codesmith's agent core: config, permissions, tools, prompts and the
//! session loop. Ported from OpenCode (MIT); see LICENSE.

pub mod agent;
pub mod auth;
pub mod automation;
pub mod catalog;
pub mod codemode;
pub mod codemode_catalog;
pub mod command;
pub mod compaction;
pub mod config;
#[cfg(unix)]
pub mod daemon;
pub mod fork;
pub mod format;
pub mod instruction_state;
pub mod instructions;
pub mod lsp;
pub mod markdown;
pub mod mcp;
pub mod media;
pub mod paths;
pub mod permission;
pub mod plugin;
pub mod prompt;
pub mod providers;
#[cfg(unix)]
pub mod pty;
pub mod question;
pub mod reference;
pub mod repository;
pub mod retry;
pub mod review;
pub mod runner;
pub mod session;
pub mod skill;
pub mod skill_remote;
pub mod snapshot;
pub mod stats;
pub mod store;
pub mod title;
pub mod tool;
pub mod user_shell;
pub mod warming;
pub mod watch;
pub mod wildcard;
pub mod worktree;

/// The environment variable that carries the local server's bearer token.
/// No child process CodeSmith starts may see it, as it would let the child
/// drive the server (OpenCode keeps its server password out of sessions the
/// same way): `codesmith` takes it out of its own environment at startup,
/// and every spawn removes it again.
pub const SERVER_TOKEN_VAR: &str = "CODESMITH_SERVER_TOKEN";
