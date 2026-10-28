//! Ollama detection, auto-install, and model pulls.
//!
//! The local-operator loop (see `local_operator.rs`) talks to Ollama on
//! http://localhost:11434, so the app offers to set that up: install
//! Ollama, then pull the operator model. This mirrors the Tailscale
//! install pattern: hidden helpers, a download sanity check, and success
//! verified by the installed binary answering — never claimed on an
//! installer's exit code alone.
//!
//! Installer facts (checked against Ollama's own behavior, not guessed):
//! * The Windows installer is `OllamaSetup.exe`, an Inno Setup installer
//!   downloaded from https://ollama.com/download/OllamaSetup.exe. Inno
//!   Setup's silent flags are `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`
//!   (the same flags Ollama's own updater passes when it upgrades
//!   itself non-interactively). A per-user install: no admin/UAC prompt
//!   is expected, unlike the Tailscale MSI.
//! * It installs to `%LOCALAPPDATA%\Programs\Ollama` and puts `ollama`
//!   on PATH; the CLI is `ollama.exe` in that folder.
//! * If the silent install cannot be verified, the fallback launches the
//!   installer GUI for a one-click install instead of claiming success.

use std::path::PathBuf;

use crate::hidden::{hidden_command, ps_quote};

/// Official Windows installer download.
pub const DOWNLOAD_URL: &str = "https://ollama.com/download/OllamaSetup.exe";

/// Default operator model (fits a 12 GB card; see the local-mode docs).
pub const DEFAULT_MODEL: &str = "qwen3-vl:8b";

/// Locate the `ollama` binary: PATH first, then the standard per-user
/// install location, then the machine-wide location.
pub fn find_cli() -> Option<PathBuf> {
    if let Ok(out) = hidden_command("ollama").arg("--version").output() {
        if out.status.success() {
            return Some(PathBuf::from("ollama"));
        }
    }
    #[cfg(windows)]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let p = PathBuf::from(local)
                .join("Programs")
                .join("Ollama")
                .join("ollama.exe");
            if p.exists() {
                return Some(p);
            }
        }
        let machine = PathBuf::from("C:\\Program Files\\Ollama\\ollama.exe");
        if machine.exists() {
            return Some(machine);
        }
    }
    if let Ok(p) = std::env::var("OLLAMA_CLI") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    None
}

fn run_cli(cli: &PathBuf, args: &[&str]) -> Result<String, String> {
    let out = hidden_command(&cli.to_string_lossy())
        .args(args)
        .output()
        .map_err(|e| format!("cannot run ollama: {}", e))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// `ollama --version` output's first line, if the CLI answers.
pub fn installed_version() -> Option<String> {
    let cli = find_cli()?;
    run_cli(&cli, &["--version"])
        .ok()
        .and_then(|v| v.lines().next().map(|l| l.trim().to_string()))
        .filter(|v| !v.is_empty())
}

/// Parse `ollama list` output into model names (the NAME column).
/// Pure function, unit-tested. The header line is skipped; blank lines
/// are ignored.
pub fn parse_list_output(output: &str) -> Vec<String> {
    let mut models = Vec::new();
    for (i, line) in output.lines().enumerate() {
        if i == 0 {
            continue; // "NAME  ID  SIZE  MODIFIED" header.
        }
        if let Some(name) = line.split_whitespace().next() {
            if !name.is_empty() {
                models.push(name.to_string());
            }
        }
    }
    models
}

/// Models currently pulled, empty when the CLI is missing or the server
/// is not answering (pulls need the server; a missing list is not an
/// error here, it just means "none known").
pub fn models() -> Vec<String> {
    let cli = match find_cli() {
        Some(c) => c,
        None => return Vec::new(),
    };
    match run_cli(&cli, &["list"]) {
        Ok(out) => parse_list_output(&out),
        Err(_) => Vec::new(),
    }
}

/// One-line status for the app's Local AI tab.
pub struct OllamaStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub models: Vec<String>,
}

pub fn status() -> OllamaStatus {
    let version = installed_version();
    OllamaStatus {
        installed: version.is_some(),
        version,
        models: models(),
    }
}

/// Outcome of an install attempt. The GUI fallback is a real outcome,
/// not a failure: the owner completes a one-click install and taps
/// Install again to verify.
pub enum InstallOutcome {
    AlreadyInstalled(String),
    Installed(String),
    GuiLaunched,
}

/// Install Ollama on Windows. Returns the verified version, or a
/// specific error. Non-Windows returns an error (dev machines use their
/// own package manager).
pub fn install_windows() -> Result<InstallOutcome, String> {
    if let Some(v) = installed_version() {
        return Ok(InstallOutcome::AlreadyInstalled(v));
    }
    #[cfg(not(windows))]
    {
        return Err("Ollama auto-install is a Windows flow".to_string());
    }
    #[cfg(windows)]
    {
        let setup = std::env::temp_dir().join("understudy-ollama-setup.exe");
        let setup_arg = setup.to_string_lossy().into_owned();
        let dl = hidden_command("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "Invoke-WebRequest -Uri {} -OutFile {} -UseBasicParsing -TimeoutSec 300",
                    ps_quote(DOWNLOAD_URL),
                    ps_quote(&setup_arg)
                ),
            ])
            .output()
            .map_err(|e| format!("download failed: {}", e))?;
        if !dl.status.success() {
            return Err(format!(
                "Ollama installer download failed: {}",
                String::from_utf8_lossy(&dl.stderr).trim()
            ));
        }
        let size = std::fs::metadata(&setup).map(|m| m.len()).unwrap_or(0);
        if size < 1_000_000 {
            return Err(format!(
                "Ollama installer download looks wrong ({} bytes, expected at least a few MB); check your connection and try again",
                size
            ));
        }
        // Inno Setup silent install. No admin prompt is expected for a
        // per-user install, but the call still waits for completion.
        let inst = hidden_command(&setup_arg)
            .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
            .output()
            .map_err(|e| format!("install failed to start: {}", e))?;
        if inst.status.success() {
            if let Some(v) = installed_version() {
                return Ok(InstallOutcome::Installed(v));
            }
        }
        // Silent install did not verify: fall back to the visible
        // one-click installer instead of claiming anything. The owner
        // completes it, then taps Install again to verify. The launch
        // itself is checked now: previously its result was discarded,
        // so a failed launch still reported "installer opened".
        let gui = hidden_command("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Start-Process {}", ps_quote(&setup_arg)),
            ])
            .output()
            .map_err(|e| format!("could not launch the installer window: {}", e))?;
        if !gui.status.success() {
            return Err(format!(
                "could not launch the installer window: {}",
                String::from_utf8_lossy(&gui.stderr).trim()
            ));
        }
        Ok(InstallOutcome::GuiLaunched)
    }
}

/// Pull a model (hidden `ollama pull`), verified by `ollama list`
/// afterwards. This blocks for the download (multi-GB models take
/// minutes); the engine serves other requests on other threads while
/// it runs, so status polls keep working.
pub fn pull(model: &str) -> Result<(), String> {
    let cli = find_cli().ok_or("Ollama is not installed")?;
    let model = if model.trim().is_empty() {
        DEFAULT_MODEL
    } else {
        model.trim()
    };
    let out = hidden_command(&cli.to_string_lossy())
        .args(["pull", model])
        .output()
        .map_err(|e| format!("ollama pull failed to start: {}", e))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if err.is_empty() {
            return Err(format!(
                "ollama pull {} failed; is the Ollama server running (tray icon)?",
                model
            ));
        }
        return Err(format!("ollama pull {} failed: {}", model, err));
    }
    // Verify: the model must now appear in `ollama list`.
    if models().iter().any(|m| m == model) {
        Ok(())
    } else {
        Err(format!(
            "ollama pull finished but {} is not in `ollama list` — try again",
            model
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_parser_skips_header_and_blanks() {
        let out = "NAME                    ID              SIZE      MODIFIED\nqwen3-vl:8b             abc123          6.1 GB    2 hours ago\n\nllama3.2:3b            def456          2.0 GB    yesterday\n";
        assert_eq!(
            parse_list_output(out),
            vec!["qwen3-vl:8b".to_string(), "llama3.2:3b".to_string()]
        );
    }

    #[test]
    fn list_parser_empty_without_header() {
        assert!(parse_list_output("").is_empty());
        assert!(parse_list_output("NAME  ID  SIZE  MODIFIED\n").is_empty());
    }

    #[test]
    fn download_url_is_official() {
        assert_eq!(
            DOWNLOAD_URL,
            "https://ollama.com/download/OllamaSetup.exe"
        );
    }
}
