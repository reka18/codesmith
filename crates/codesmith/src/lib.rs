//! CodeSmith: a terminal coding agent (a Rust port of OpenCode).
//!
//! Besides the `codesmith` binary, this crate lets you build your own
//! CodeSmith with native plugins compiled in:
//!
//! ```no_run
//! use std::sync::Arc;
//! use codesmith::plugin::Plugin;
//!
//! struct Hello;
//! impl Plugin for Hello {
//!     fn id(&self) -> &str { "hello" }
//! }
//!
//! fn main() {
//!     codesmith::main(vec![Arc::new(Hello)]);
//! }
//! ```

// The example's `fn main` is the point: it is the plugin binary's main.
// clippy 1.91 (the workspace rust-version) calls it needless.
#![allow(clippy::needless_doctest_main)]

mod acp;
mod busctl;
mod claude_token;
mod cli;
mod links;
mod render;
mod serve;
mod tui;
mod upgrade;

pub use codesmith_core as core;
pub use codesmith_core::plugin;
use std::sync::Arc;

pub(crate) use cli::{all_commands_with, connect_mcp};

/// Run the CodeSmith command line with these plugins installed (after the
/// built-in ones, such as `/goal`), then exit.
pub fn main(plugins: Vec<Arc<dyn plugin::Plugin>>) -> ! {
    // Code Mode programs run in a child copy of this binary.
    codesmith_core::codemode::child_main_if_requested();
    // So does the shell daemon (`__shelld`), which `serve` starts.
    #[cfg(unix)]
    codesmith_core::daemon::child_main_if_requested();
    // And the local bus's broker (`__busd`), which the first session on the
    // bus starts.
    #[cfg(unix)]
    codesmith_core::bus::child_main_if_requested();
    // Before any thread starts: `serve` alone gets the server token, and no
    // child process inherits it.
    cli::take_server_token();
    cli::take_shell_handoff();
    let mut all = plugin::builtin();
    all.extend(plugins);
    plugin::install(all);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("starting the async runtime");
    let code = runtime.block_on(cli::entry());
    drop(runtime);
    std::process::exit(code)
}
