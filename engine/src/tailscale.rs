//! Tailscale detection, auto-install, and login.
//!
//! First-launch flow (driven from the Flutter setup screen, which polls
//! `GET /tailscale/status`):
//! 1. Detect: is `tailscale` on PATH, or at the standard install location?
//! 2. If missing: download the official stable MSI and install it with
//!    `msiexec` via an elevated PowerShell launch. This raises one Windows
//!    admin/UAC prompt — the UI says so. The latest stable version is
//!    resolved from the stable package listing at
//!    `https://pkgs.tailscale.com/stable/` (its version dropdown's first
//!    versioned entry is the latest release); the MSI is then
//!    `https://pkgs.tailscale.com/stable/tailscale-setup-{version}-amd64.msi`
//!    (the documented stable MSI pattern, verified live).
//!    NOTE: there is NO `VERSION` file on the stable track — v0.1.0
//!    resolved versions from a `VERSION` URL that 404s, so a blank-version
//!    install died before any download started. That is fixed here.
//! 3. Login (one-time, by the owner): `tailscale up` opens the browser SSO
//!    flow, or `tailscale up --authkey <key>` uses a key pasted from the
//!    Tailscale admin console. After that the connection is headless.
//! 4. The engine then binds its listener to the Tailscale IPv4 address
//!    (`tailscale ip -4`), never to a public interface.
//!
//! Every helper process here runs HIDDEN (CREATE_NO_WINDOW on Windows):
//! without it, each console helper flashes a blank console window on the
//! owner's screen — the exact symptom reported for "Install Tailscale".
//!
//! Nothing here phones home to anyone except Tailscale itself (the
//! transport the owner chose). Auth keys are passed to the local
//! `tailscale` binary only and are never logged.

use std::path::PathBuf;

use crate::hidden::{hidden_command, ps_quote};

/// Stable package listing page (used for latest-version resolution).
pub const LISTING_URL: &str = "https://pkgs.tailscale.com/stable/";

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
    if let Ok(out) = hidden_command("tailscale").arg("version").output() {
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
    let out = hidden_command(&cli.to_string_lossy())
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
/// `device_name` is the machine's Tailscale device name (from
/// `tailscale status --json`): it is what a controller needs alongside
/// the IP to identify this PC. It is None when the name cannot be
/// determined — callers must show "unknown", never a fabricated name.
#[derive(Debug)]
pub struct TsStatus {
    pub installed: bool,
    pub logged_in: bool,
    pub ip: Option<String>,
    pub device_name: Option<String>,
}

/// Extract the device name from `tailscale status --json` output:
/// prefer `Self.HostName`; fall back to the first label of
/// `Self.DNSName` (e.g. "shawn-pc" from "shawn-pc.tailxxx.ts.net").
/// Pure function, unit-tested. Returns None on any parse failure or
/// when neither field is present — never a guessed name.
pub fn extract_device_name(status_json: &str) -> Option<String> {
    let v = crate::json::parse(status_json).ok()?;
    let self_obj = v.get("Self")?;
    if let Some(name) = self_obj.get("HostName").and_then(|h| h.as_str()) {
        let name = name.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    if let Some(dns) = self_obj.get("DNSName").and_then(|d| d.as_str()) {
        let first = dns.split('.').next().unwrap_or("").trim();
        if !first.is_empty() {
            return Some(first.to_string());
        }
    }
    None
}

/// `tailscale status --json` device name, if determinable.
pub fn device_name(cli: &PathBuf) -> Option<String> {
    let out = run_cli(cli, &["status", "--json"]).ok()?;
    extract_device_name(&out)
}

/// Installed Tailscale version, if the CLI answers. The first line of
/// `tailscale version` is the version number (a leading "tailscale
/// version" prefix, if present, is stripped).
pub fn installed_version() -> Option<String> {
    let cli = find_cli()?;
    let out = run_cli(&cli, &["version"]).ok()?;
    let first = out.lines().next()?.trim();
    let v = first.strip_prefix("tailscale version").unwrap_or(first).trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

pub fn status() -> TsStatus {
    match find_cli() {
        None => TsStatus {
            installed: false,
            logged_in: false,
            ip: None,
            device_name: None,
        },
        Some(cli) => {
            let ip = tailscale_ip(&cli);
            let device_name = device_name(&cli);
            TsStatus {
                installed: true,
                logged_in: ip.is_some(),
                ip,
                device_name,
            }
        }
    }
}

fn is_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Extract the latest stable version from the pkgs listing HTML: the
/// version dropdown's first versioned `<option value="x.y.z">` is the
/// latest release (the very first option is literally "latest" and is
/// skipped). Pure function, unit-tested.
pub fn parse_stable_version_from_listing(html: &str) -> Option<String> {
    let bytes = html.as_bytes();
    let mut i = 0;
    while i + 13 <= bytes.len() {
        if &html[i..i + 13] == "option value=" {
            let mut j = i + 13;
            while j < bytes.len()
                && (bytes[j] == b' '
                    || bytes[j] == b'='
                    || bytes[j] == b'"'
                    || bytes[j] == b'\'')
            {
                j += 1;
            }
            let start = j;
            while j < bytes.len()
                && bytes[j] != b'"'
                && bytes[j] != b'\''
                && bytes[j] != b' '
                && bytes[j] != b'>'
            {
                j += 1;
            }
            let value = &html[start..j];
            if is_version(value) {
                return Some(value.to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// Resolve the latest stable version from the package listing (Windows
/// only; hidden PowerShell fetch). Returns None on any fetch/parse
/// failure — callers must then ask the owner for an explicit version
/// instead of failing silently.
pub fn resolve_stable_version() -> Option<String> {
    #[cfg(not(windows))]
    {
        return None;
    }
    #[cfg(windows)]
    {
        let out = hidden_command("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "(Invoke-WebRequest -Uri {} -UseBasicParsing).Content",
                    ps_quote(LISTING_URL)
                ),
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let html = String::from_utf8_lossy(&out.stdout);
        parse_stable_version_from_listing(&html)
    }
}

/// Install Tailscale on Windows: download the stable MSI via hidden
/// PowerShell and install it elevated. Returns the VERIFIED installed
/// version (from `tailscale version` after install), or a specific error.
/// Returns an error on non-Windows (the engine's Windows installer flow
/// is the real path; dev machines use their own package manager).
pub fn install_windows(version: &str) -> Result<String, String> {
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
        // Download hidden, with -UseBasicParsing: without it, Windows
        // PowerShell falls back to the IE engine, which fails outright on
        // systems where IE is disabled — a fast, silent failure.
        let dl = hidden_command("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "Invoke-WebRequest -Uri {} -OutFile {} -UseBasicParsing",
                    ps_quote(&url),
                    ps_quote(&msi_arg)
                ),
            ])
            .output()
            .map_err(|e| format!("download failed: {}", e))?;
        if !dl.status.success() {
            return Err(format!(
                "Tailscale MSI download failed: {}",
                String::from_utf8_lossy(&dl.stderr).trim()
            ));
        }
        // The download must be a real MSI (~35 MB), not an error page.
        let size = std::fs::metadata(&msi).map(|m| m.len()).unwrap_or(0);
        if size < 1_000_000 {
            return Err(format!(
                "Tailscale MSI download looks wrong ({} bytes, expected tens of MB); check your connection and try again",
                size
            ));
        }
        // Install elevated: Start-Process -Verb RunAs raises the one UAC
        // prompt, -Wait blocks until msiexec finishes, and the exit code
        // is reported. A declined prompt makes Start-Process throw.
        // /qb = basic progress UI (not a console window).
        let script = format!(
            "$p = Start-Process -FilePath 'msiexec.exe' -ArgumentList '/i',{},'/qb' -Verb RunAs -Wait -PassThru; $p.ExitCode",
            ps_quote(&msi_arg)
        );
        let inst = hidden_command("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .map_err(|e| format!("install failed: {}", e))?;
        if !inst.status.success() {
            let err = String::from_utf8_lossy(&inst.stderr);
            if err.contains("canceled")
                || err.contains("cancelled")
                || err.contains("1223")
            {
                return Err(
                    "Tailscale install cancelled — the admin prompt was declined"
                        .to_string(),
                );
            }
            return Err(format!(
                "Tailscale install failed to start: {}",
                err.trim()
            ));
        }
        let code: i32 = String::from_utf8_lossy(&inst.stdout)
            .trim()
            .parse()
            .unwrap_or(-1);
        if code != 0 && code != 3010 {
            return Err(format!(
                "Tailscale installer exited with code {} (0 = success, 3010 = success, reboot needed)",
                code
            ));
        }
        // Verify: the CLI must now exist and answer. Success is never
        // claimed on the installer's exit code alone.
        match find_cli() {
            Some(cli) => match run_cli(&cli, &["version"]) {
                Ok(v) => Ok(v
                    .lines()
                    .next()
                    .unwrap_or(version)
                    .trim()
                    .to_string()),
                Err(_) => Err(
                    "installer finished but tailscale.exe does not answer yet — try again in a moment"
                        .to_string(),
                ),
            },
            None => Err(
                "installer finished but tailscale.exe was not found — the install may need a retry"
                    .to_string(),
            ),
        }
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
    fn listing_parser_finds_latest_version() {
        // Snippet shaped like the real stable listing's version dropdown.
        let html = r#"<select name="v" id="v">
    <option value="latest" selected>latest</option>
    <option value="1.104.2" >1.104.2</option>
    <option value="1.104.1" >1.104.1</option>
    <option value="1.102.5" >1.102.5</option>
  </select>"#;
        assert_eq!(
            parse_stable_version_from_listing(html).as_deref(),
            Some("1.104.2")
        );
    }

    #[test]
    fn listing_parser_returns_none_without_versions() {
        assert_eq!(parse_stable_version_from_listing("<html>no options</html>"), None);
        assert_eq!(
            parse_stable_version_from_listing(r#"<option value="latest">latest</option>"#),
            None
        );
    }

    #[test]
    fn listing_parser_rejects_non_versions() {
        // A "1.2" two-part value is not a full version; keep scanning.
        let html = r#"<option value="1.2">1.2</option><option value="1.104.2">1.104.2</option>"#;
        assert_eq!(
            parse_stable_version_from_listing(html).as_deref(),
            Some("1.104.2")
        );
    }

    #[test]
    fn device_name_prefers_hostname() {
        let json = r#"{"Self": {"HostName": "Shawn-PC", "DNSName": "shawn-pc.tail1234.ts.net", "TailscaleIPs": ["100.64.0.5"]}}"#;
        assert_eq!(
            extract_device_name(json).as_deref(),
            Some("Shawn-PC")
        );
    }

    #[test]
    fn device_name_falls_back_to_dns_label() {
        let json = r#"{"Self": {"DNSName": "shawn-pc.tail1234.ts.net"}}"#;
        assert_eq!(
            extract_device_name(json).as_deref(),
            Some("shawn-pc")
        );
    }

    #[test]
    fn device_name_returns_none_when_unknown() {
        assert_eq!(extract_device_name("{}"), None);
        assert_eq!(extract_device_name("not json at all"), None);
        assert_eq!(
            extract_device_name(r#"{"Self": {"HostName": ""}}"#),
            None
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
