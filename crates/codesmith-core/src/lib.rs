//! codesmith's agent core: config, permissions, tools, prompts and the
//! session loop. Ported from OpenCode (MIT); see LICENSE.

pub mod agent;
pub mod auth;
pub mod command;
pub mod compaction;
pub mod config;
pub mod format;
pub mod instructions;
pub mod mcp;
pub mod paths;
pub mod permission;
pub mod prompt;
pub mod question;
pub mod review;
pub mod session;
pub mod skill;
pub mod snapshot;
pub mod store;
pub mod title;
pub mod tool;
pub mod wildcard;
