//! understudy-engine: the control service that runs on the owner's PC.
//!
//! It serves the control API (see `api.rs` and CONTROL_API.md) on the
//! Tailscale interface only, and exits — closing all listeners — when it
//! receives `POST /shutdown` or when its process ends (the Flutter app's
//! Quit button kills this process, which is the kill switch).
//!
//! Usage:
//!   understudy-engine [--port 18769] [--data-dir <dir>] [--bind <ip>]
//!
//! * `--bind` is restricted: only 127.0.0.1 or the machine's current
//!   Tailscale IPv4 address is accepted. 0.0.0.0 and any other address are
//!   refused, because this service must never listen on a public network.
//! * On first run a bearer token is generated and stored in the data dir.
//!   It is printed ONCE to stdout so the owner can copy it into the
//!   controller that will operate this PC. It is never printed again and
//!   must never be committed anywhere.

mod api;
mod auth;
mod capture;
mod cmd;
mod files;
mod input;
mod json;
mod log;
mod monitors;
mod png;
mod process;
mod tailscale;
#[cfg(windows)]
mod win32;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_PORT: u16 = 18769;

fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("Understudy");
        }
        PathBuf::from("Understudy")
    }
    #[cfg(not(windows))]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".understudy");
        }
        PathBuf::from(".understudy")
    }
}

fn default_file_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            return vec![PathBuf::from(profile)];
        }
        vec![PathBuf::from("C:\\")]
    }
    #[cfg(not(windows))]
    {
        if let Ok(home) = std::env::var("HOME") {
            return vec![PathBuf::from(home)];
        }
        vec![PathBuf::from("/tmp")]
    }
}

fn print_usage() {
    eprintln!("understudy-engine [--port PORT] [--data-dir DIR] [--bind IP]");
    eprintln!("  --port      control API port (default {})", DEFAULT_PORT);
    eprintln!("  --data-dir  token + action log directory (default: per-OS app data)");
    eprintln!("  --bind      127.0.0.1 or this machine's Tailscale IPv4 only");
}

fn main() {
    let mut port = DEFAULT_PORT;
    let mut data_dir = default_data_dir();
    let mut bind_override: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                port = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                    eprintln!("bad --port value");
                    std::process::exit(2);
                });
            }
            "--data-dir" => {
                data_dir = match args.next() {
                    Some(v) => PathBuf::from(v),
                    None => {
                        eprintln!("missing --data-dir value");
                        std::process::exit(2);
                    }
                };
            }
            "--bind" => {
                bind_override = Some(match args.next() {
                    Some(v) => v,
                    None => {
                        eprintln!("missing --bind value");
                        std::process::exit(2);
                    }
                });
            }
            "--help" | "-h" => {
                print_usage();
                return;
            }
            other => {
                eprintln!("unknown argument: {}", other);
                print_usage();
                std::process::exit(2);
            }
        }
    }

    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("cannot create data dir {}: {}", data_dir.display(), e);
        std::process::exit(1);
    }

    let (token, is_new) = match auth::load_or_create(&data_dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("cannot load auth token: {}", e);
            std::process::exit(1);
        }
    };
    if is_new {
        // Printed exactly once, on first run, so the owner can pair a
        // controller. Never logged to the action log.
        println!("UNDERSTUDY FIRST-RUN TOKEN (copy it into your controller, then keep it private):");
        println!("{}", token);
    } else {
        println!("Understudy: bearer token loaded from the data directory.");
    }

    // Resolve the bind address: explicit override (validated), else the
    // Tailscale IPv4 address, else loopback. Never 0.0.0.0.
    let ts = tailscale::status();
    let bind_ip = match bind_override {
        Some(ip) => {
            let allowed = ip == "127.0.0.1"
                || ts.ip.as_deref() == Some(ip.as_str());
            if !allowed {
                eprintln!(
                    "refusing --bind {}: only 127.0.0.1 or this machine's Tailscale IP ({}) is allowed",
                    ip,
                    ts.ip.clone().unwrap_or_else(|| "none".to_string())
                );
                std::process::exit(2);
            }
            ip
        }
        None => ts.ip.clone().unwrap_or_else(|| "127.0.0.1".to_string()),
    };
    if bind_ip == "0.0.0.0" {
        eprintln!("refusing to bind 0.0.0.0: Understudy never listens on a public interface");
        std::process::exit(2);
    }

    let addr = format!("{}:{}", bind_ip, port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    // Connection info for the local Flutter UI to discover this engine.
    let conn = format!(
        "{{\"ip\":{},\"port\":{},\"version\":{},\"tailscale_installed\":{},\"tailscale_ip\":{}}}",
        crate::json::quote(&bind_ip),
        port,
        crate::json::quote(api::VERSION),
        ts.installed,
        ts.ip
            .clone()
            .map(|ip| crate::json::quote(&ip))
            .unwrap_or_else(|| "null".to_string()),
    );
    let _ = std::fs::write(data_dir.join("connection.json"), conn);

    let mons = monitors::enumerate();
    println!(
        "Understudy engine v{} listening on {} (Tailscale-only{}), {} monitor(s) detected.",
        api::VERSION,
        addr,
        if ts.ip.is_some() { "" } else { " — Tailscale not connected, loopback only" },
        mons.len()
    );
    for m in &mons {
        println!("  {}", m);
    }
    println!("Data directory: {}", data_dir.display());
    println!("Action log: {}", data_dir.join("actions.log").display());
    println!("Kill switch: POST /shutdown with the bearer token, or end this process.");

    let state = Arc::new(api::State::new(
        data_dir.clone(),
        token,
        default_file_roots(),
    ));
    log::append(&data_dir, "engine_started", true, &addr);
    if let Err(e) = api::serve(listener, Arc::clone(&state)) {
        eprintln!("server error: {}", e);
        std::process::exit(1);
    }
    log::append(&data_dir, "engine_stopped", true, "shutdown requested");
    println!("Understudy engine stopped.");
}
