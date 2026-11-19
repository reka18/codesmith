//! Where skeptical keeps things. XDG-style on every platform, overridable by
//! environment variables, so a fresh install needs nothing configured.

use std::path::PathBuf;

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

/// `$SKEPTICAL_CONFIG_DIR`, else `$XDG_CONFIG_HOME/skeptical`, else `~/.config/skeptical`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("SKEPTICAL_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("skeptical"))
}

/// `$XDG_DATA_HOME/skeptical`, else `~/.local/share/skeptical`.
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("skeptical")
}

/// Full tool outputs that were too large to show inline.
pub fn tool_output_dir() -> PathBuf {
    data_dir().join("tool-output")
}

/// A private scratch directory the agent may use without asking.
pub fn tmp_dir() -> PathBuf {
    data_dir().join("tmp")
}
