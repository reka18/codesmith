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

mod acp;
mod cli;
mod render;
mod serve;
mod tui;

pub use codesmith_core as core;
pub use codesmith_core::plugin;
use std::sync::Arc;

pub(crate) use cli::{all_commands, connect_mcp};

/// Run the CodeSmith command line with these plugins installed, then exit.
pub fn main(plugins: Vec<Arc<dyn plugin::Plugin>>) -> ! {
    plugin::install(plugins);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("starting the async runtime");
    let code = runtime.block_on(cli::entry());
    drop(runtime);
    std::process::exit(code)
}
