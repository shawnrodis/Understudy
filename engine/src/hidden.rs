//! Helpers for spawning child processes on Windows without flashing a
//! console window, plus PowerShell argument quoting.
//!
//! Shared by the Tailscale and Ollama install flows: both run helpers from
//! a windowless parent, and without CREATE_NO_WINDOW each console helper
//! gets a brand-new console window flashed on the owner's screen.

use std::process::Command;

/// Spawn a helper process with no visible console window on Windows.
/// Without CREATE_NO_WINDOW, a console helper spawned from a windowless
/// parent gets a brand-new console window flashed on screen.
#[cfg(windows)]
pub fn hidden_command(program: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut c = Command::new(program);
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

#[cfg(not(windows))]
pub fn hidden_command(program: &str) -> Command {
    Command::new(program)
}

/// Single-quote a string for PowerShell, doubling embedded quotes.
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
