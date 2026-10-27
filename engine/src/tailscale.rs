//! Tailscale detection, auto-install, and login.
//!
//! First-launch flow (driven from the Flutter setup screen, which polls
//! `GET /tailscale/status`):
//! 1. Detect: is `tailscale` on PATH, or at the standard install location?
//! 2. If missing: download the official stable MSI and install it with
//!    `msiexec`. This raises one Windows admin/UAC prompt — the UI says so.
//!    The latest stable version is resolved from
//!    `https://pkgs.tailscale.com/stable/VERSION`, then the MSI is
//!    `https://pkgs.tailscale.com/stable/tailscale-setup-{version}-amd64.msi`
//!    (the documented stable MSI pattern).
//! 3. Login (one-time, by the owner): `tailscale up` opens the browser SSO
//!    flow, or `tailscale up --authkey <key>` uses a key pasted from the
//!    Tailscale admin console. After that the connection is headless.
//! 4. The engine then binds its listener to the Tailscale IPv4 address
//!    (`tailscale ip -4`), never to a public interface.
//!
//! Nothing here phones home to anyone except Tailscale itself (the
//! transport the owner chose). Auth keys are passed to the local
//! `tailscale` binary only and are never logged.

use std::path::PathBuf;
use std::process::Command;

pub const VERSION_URL: &str = "https://pkgs.tailscale.com/stable/VERSION";

/// MSI URL for a resolved stable version.
pub fn msi_url(version: &str) -> String {
    format!(
        "https://pkgs.tailscale.com/stable/tailscale-setup-{}-amd64.msi",
        version.trim()
    )
}

/// Standard Windows install location of the Tailscale CLI.
pub fn default_cli_path() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from("C:\\Program Files\\Tailscale\\tailscale.exe")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/usr/local/bin/tailscale")
    }
}

/// Locate the `tailscale` binary: PATH first, then the standard location.
pub fn find_cli() -> Option<PathBuf> {
    if let Ok(out) = Command::new("tailscale").arg("version").output() {
        if out.status.success() {
            return Some(PathBuf::from("tailscale"));
        }
    }
    let std = default_cli_path();
    if std.exists() {
        return Some(std);
    }
    // Also honor a TAILSCALE_CLI override (useful for testing).
    if let Ok(p) = std::env::var("TAILSCALE_CLI") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    None
}

fn run_cli(cli: &PathBuf, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cli)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run tailscale: {}", e))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// `tailscale ip -4`, if the client is up. Returns None when not connected.
pub fn tailscale_ip(cli: &PathBuf) -> Option<String> {
    run_cli(cli, &["ip", "-4"]).ok().filter(|s| {
        !s.is_empty() && s.starts_with("100.") // CGNAT range Tailscale uses
    })
}

/// One-line status for the setup screen: installed? logged in? IP?
#[derive(Debug)]
pub struct TsStatus {
    pub installed: bool,
    pub logged_in: bool,
    pub ip: Option<String>,
}

pub fn status() -> TsStatus {
    match find_cli() {
        None => TsStatus {
            installed: false,
            logged_in: false,
            ip: None,
        },
        Some(cli) => {
            let ip = tailscale_ip(&cli);
            TsStatus {
                installed: true,
                logged_in: ip.is_some(),
                ip,
            }
        }
    }
}

/// Install Tailscale on Windows: download the stable MSI via PowerShell
/// and run it with msiexec. Returns an error on non-Windows (the engine's
/// Windows installer flow is the real path; dev machines use their own
/// package manager).
pub fn install_windows(version: &str) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = version;
        return Err("Tailscale auto-install is a Windows flow".to_string());
    }
    #[cfg(windows)]
    {
        let url = msi_url(version);
        let msi = std::env::temp_dir().join("understudy-tailscale-setup.msi");
        let msi_arg = msi.to_string_lossy().into_owned();
        // Download with PowerShell (present on every supported Windows).
        let dl = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Invoke-WebRequest -Uri '{}' -OutFile '{}'",
                    url, msi_arg
                ),
            ])
            .output()
            .map_err(|e| format!("download failed: {}", e))?;
        if !dl.status.success() {
            return Err(format!(
                "Tailscale MSI download failed: {}",
                String::from_utf8_lossy(&dl.stderr)
            ));
        }
        // msiexec /i raises the UAC prompt (admin install). /qb = basic UI.
        let inst = Command::new("msiexec")
            .args(["/i", &msi_arg, "/qb"])
            .output()
            .map_err(|e| format!("msiexec failed: {}", e))?;
        if !inst.status.success() {
            return Err("Tailscale MSI install failed (did you decline the admin prompt?)".to_string());
        }
        Ok(())
    }
}

/// Bring Tailscale up. With an auth key: headless. Without: opens the
/// browser SSO flow for the owner to complete once.
pub fn login(auth_key: Option<&str>) -> Result<(), String> {
    let cli = find_cli().ok_or("tailscale is not installed")?;
    let mut args: Vec<&str> = vec!["up"];
    // NOTE: the auth key is passed only to the local binary, never logged.
    let mut owned: Vec<String> = Vec::new();
    if let Some(key) = auth_key {
        owned.push("--authkey".to_string());
        owned.push(key.to_string());
        args = vec!["up", "--authkey", owned[1].as_str()];
    }
    run_cli(&cli, &args).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msi_url_pattern() {
        assert_eq!(
            msi_url("1.88.4"),
            "https://pkgs.tailscale.com/stable/tailscale-setup-1.88.4-amd64.msi"
        );
        assert_eq!(
            msi_url(" 1.88.4\n"),
            "https://pkgs.tailscale.com/stable/tailscale-setup-1.88.4-amd64.msi"
        );
    }

    #[test]
    fn status_reports_missing_cli() {
        // With no tailscale on PATH in this environment, status must not crash.
        let s = status();
        if find_cli().is_none() {
            assert!(!s.installed);
            assert!(!s.logged_in);
            assert!(s.ip.is_none());
        }
    }
}
