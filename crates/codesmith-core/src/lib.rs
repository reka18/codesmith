//! codesmith's agent core: config, permissions, tools, prompts and the
//! session loop. Ported from OpenCode (MIT); see LICENSE.

pub mod agent;
pub mod approvals;
pub mod attachment;
pub mod auth;
pub mod automation;
pub mod blob;
pub mod bus;
pub mod catalog;
pub mod child_env;
pub mod codemode;
pub mod codemode_catalog;
pub mod command;
pub mod compaction;
pub mod config;
#[cfg(unix)]
pub mod daemon;
pub mod events;
pub mod export;
pub mod folder;
pub mod fork;
pub mod form;
pub mod format;
pub mod generate;
pub mod git;
pub mod hg;
pub mod home;
pub mod instruction_state;
pub mod instructions;
pub mod lsp;
pub mod markdown;
pub mod mcp;
pub mod media;
pub mod model_spec;
pub mod nearby;
pub mod paths;
pub mod per_repo;
pub mod permission;
pub mod plugin;
pub mod prompt;
pub mod providers;
#[cfg(unix)]
pub mod pty;
pub mod question;
pub mod reference;
pub mod remote;
pub mod repository;
pub mod retry;
pub mod review;
pub mod runner;
pub mod session;
pub mod shell_select;
pub mod skill;
pub mod skill_mention;
pub mod skill_remote;
pub mod snapshot;
pub mod stats;
pub mod store;
pub mod title;
pub mod tool;
pub mod unicode;
pub mod user_shell;
pub mod vcs;
pub mod video;
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
