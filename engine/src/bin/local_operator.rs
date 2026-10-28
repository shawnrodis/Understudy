//! Local operator: run an Ollama vision model ON the PC to operate the
//! Understudy engine, with no Tailscale needed for this loop.
//!
//! The loop is deliberately simple and boring:
//!   1. GET /monitors from the engine (loopback or the tailnet address
//!      recorded in connection.json — verbatim, never a forced
//!      127.0.0.1, because the engine binds the Tailscale IP when the
//!      tailnet is up).
//!   2. GET a downscaled JPEG screenshot of the target monitor
//!      (`?width=1280&format=jpeg` — a few tens of KB, not the 15 MB
//!      raw PNG a remote controller downloads).
//!   3. POST it to Ollama at http://localhost:11434 (`/api/chat`,
//!      `stream: false`) with the task prompt, and parse back one JSON
//!      action: move / click / scroll / type / keys / done, with x/y in
//!      0–1000 normalized space converted to that monitor's logical
//!      pixels using the /monitors dimensions.
//!   4. Execute the action through the engine API (so it lands in the
//!      engine action log exactly like any controller's action), then
//!      repeat until done, failed, stopped (the app kills this process),
//!      or `--max-steps`.
//!
//! Std-only, like the engine. It prints one log line per step to stdout
//! (the Flutter app shows these live); the app stops it by killing the
//! process. The model is NOT auto-pulled: if it is missing, the run
//! exits with a clear error telling the owner to pull it first.
//!
//! HTTP notes: both the engine and Ollama answer non-streaming
//! requests with a Content-Length, so the client reads exactly that
//! many bytes after the headers. Ollama's `/api/chat` with
//! `stream: false` returns one JSON object, not a stream of lines.

#[path = "../json.rs"]
mod json;

use json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const DEFAULT_MODEL: &str = "qwen3-vl:8b";
const DEFAULT_MAX_STEPS: u32 = 50;
const STEP_DELAY_MS: u64 = 300;
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

// ---------------------------------------------------------------- HTTP

/// Minimal HTTP client. `host` is "ip:port"; `auth` is a bearer token
/// for the engine (None for Ollama). Returns (status, body bytes).
fn http(
    host: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    auth: Option<&str>,
    content_type: &str,
) -> Result<(u16, Vec<u8>), String> {
    let mut stream = TcpStream::connect(host)
        .map_err(|e| format!("cannot connect to {}: {}", host, e))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(300)))
        .ok();
    let mut req = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", method, path, host);
    if let Some(t) = auth {
        req.push_str(&format!("Authorization: Bearer {}\r\n", t));
    }
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            content_type,
            b.len()
        ));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("request write failed: {}", e))?;
    if let Some(b) = body {
        stream
            .write_all(b)
            .map_err(|e| format!("request body write failed: {}", e))?;
    }
    // Read headers, then exactly Content-Length bytes (or to EOF when
    // the server does not send one).
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) => break None,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    break Some(pos + 4);
                }
            }
            Err(e) => return Err(format!("response read failed: {}", e)),
        }
    };
    let header_end = match header_end {
        Some(p) => p,
        None => return Err("response had no headers".to_string()),
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let status: u16 = headers
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let content_length: Option<usize> = headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim().eq_ignore_ascii_case("Content-Length") {
            v.trim().parse().ok()
        } else {
            None
        }
    });
    let mut body_bytes = buf[header_end..].to_vec();
    match content_length {
        Some(len) => {
            while body_bytes.len() < len {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => body_bytes.extend_from_slice(&chunk[..n]),
                    Err(e) => return Err(format!("response body read failed: {}", e)),
                }
            }
            body_bytes.truncate(len);
        }
        None => loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => body_bytes.extend_from_slice(&chunk[..n]),
                Err(e) => return Err(format!("response body read failed: {}", e)),
            }
        },
    }
    Ok((status, body_bytes))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Base64 (standard alphabet, padded) for the JPEG → Ollama `images`
/// array. Small, self-contained, no dependency.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ------------------------------------------------------------ data dir

fn default_data_dir() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return std::path::PathBuf::from(appdata).join("Understudy");
        }
    }
    std::env::temp_dir().join("understudy")
}

struct Engine {
    host: String,
    token: String,
}

impl Engine {
    fn get(&self, path: &str) -> Result<(u16, Vec<u8>), String> {
        http(&self.host, "GET", path, None, Some(&self.token), "")
    }
    fn post_json(&self, path: &str, body: &str) -> Result<(u16, Vec<u8>), String> {
        http(
            &self.host,
            "POST",
            path,
            Some(body.as_bytes()),
            Some(&self.token),
            "application/json",
        )
    }
}

// ------------------------------------------------------------- actions

/// One parsed model action. x/y arrive normalized (0–1000) and are
/// converted to the target monitor's logical pixels by the caller.
#[derive(Debug, PartialEq)]
enum Action {
    Move { monitor: u32, x: f64, y: f64 },
    Click { monitor: u32, x: f64, y: f64 },
    Scroll { monitor: u32, x: f64, y: f64, delta: i64 },
    Type { text: String },
    Keys { keys: Vec<String> },
    Done { reason: String },
}

/// Parse the model's action JSON. Tolerates ```json fences and a
/// surrounding prose wrapper (the first `{...}` wins). Returns an
/// error string on anything malformed — the caller counts it as a
/// failed step.
fn parse_action(text: &str) -> Result<Action, String> {
    let cleaned = strip_fences(text.trim());
    let json_text = extract_json_object(&cleaned).ok_or("no JSON object in model reply")?;
    let v = json::parse(&json_text).map_err(|e| format!("bad action JSON: {}", e))?;
    let action = v
        .get("action")
        .and_then(Value::as_str)
        .ok_or("action JSON has no 'action' field")?;
    let monitor = v.get("monitor").and_then(Value::as_f64).unwrap_or(0.0) as u32;
    let x = v.get("x").and_then(Value::as_f64).unwrap_or(0.0);
    let y = v.get("y").and_then(Value::as_f64).unwrap_or(0.0);
    match action {
        "move" => Ok(Action::Move { monitor, x, y }),
        "click" => Ok(Action::Click { monitor, x, y }),
        "scroll" => Ok(Action::Scroll {
            monitor,
            x,
            y,
            delta: v.get("delta").and_then(Value::as_f64).unwrap_or(0.0) as i64,
        }),
        "type" => Ok(Action::Type {
            text: v
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }),
        "keys" => {
            let keys = v
                .get("keys")
                .and_then(Value::as_arr)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default();
            Ok(Action::Keys { keys })
        }
        "done" => Ok(Action::Done {
            reason: v
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }),
        other => Err(format!("unknown action: {}", other)),
    }
}

fn strip_fences(text: &str) -> String {
    let t = text.strip_prefix("```json").unwrap_or(text);
    let t = t.strip_prefix("```").unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_string()
}

/// First balanced `{...}` in the text, by brace depth (strings are
/// not tracked — action JSON has no braces inside string values in
/// practice, and a malformed reply is a failed step, not a crash).
fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let mut depth = 0i32;
    for (i, c) in text[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..start + i + 1].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Convert normalized (0–1000) coordinates to a monitor's logical
/// pixels, clamped to that monitor's bounds.
fn normalized_to_pixels(x: f64, y: f64, width: f64, height: f64) -> (f64, f64) {
    (
        (x / 1000.0 * width).clamp(0.0, (width - 1.0).max(0.0)),
        (y / 1000.0 * height).clamp(0.0, (height - 1.0).max(0.0)),
    )
}

fn prompt_for(task: &str) -> String {
    format!(
        "You are operating this Windows PC through screenshots. Task: {task}\n\
        Reply with exactly one JSON object and nothing else, in this shape:\n\
        {{\"action\": \"move\"|\"click\"|\"scroll\"|\"type\"|\"keys\"|\"done\", \
        \"monitor\": <id>, \"x\": <0-1000>, \"y\": <0-1000>, \"text\": \"...\", \
        \"keys\": [\"...\"], \"delta\": <scroll amount>, \"reason\": \"...\"}}\n\
        x and y are normalized 0-1000 across the screenshot (0,0 is the \
        top-left). Use \"done\" only when the task is complete, with a \
        short reason. Do one small step per reply; never claim done for \
        a step you have not seen take effect."
    )
}

// ---------------------------------------------------------------- main

fn run() -> Result<(), String> {
    let mut data_dir: Option<String> = None;
    let mut task: Option<String> = None;
    let mut model = DEFAULT_MODEL.to_string();
    let mut ollama = "http://localhost:11434".to_string();
    let mut max_steps = DEFAULT_MAX_STEPS;
    let mut monitor_wanted: Option<u32> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or_else(|| format!("missing value for {}", a));
        match a.as_str() {
            "--data-dir" => data_dir = Some(val()?),
            "--task" => task = Some(val()?),
            "--model" => model = val()?,
            "--ollama" => ollama = val()?,
            "--max-steps" => {
                max_steps = val()?.parse().map_err(|_| "bad --max-steps")?
            }
            "--monitor" => {
                monitor_wanted = Some(val()?.parse().map_err(|_| "bad --monitor")?)
            }
            "--help" => {
                println!("local-operator --task <task> [--data-dir <dir>] [--model <model>] [--ollama <url>] [--max-steps <n>] [--monitor <id>]");
                return Ok(());
            }
            other => return Err(format!("unknown argument: {}", other)),
        }
    }
    let task = task.ok_or("--task is required")?;
    let data_dir = data_dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_data_dir);
    let conn_text = std::fs::read_to_string(data_dir.join("connection.json"))
        .map_err(|e| format!("cannot read connection.json in {}: {} (is the Understudy engine running?)", data_dir.display(), e))?;
    let conn = json::parse(&conn_text).map_err(|e| format!("bad connection.json: {}", e))?;
    let ip = conn
        .get("ip")
        .and_then(Value::as_str)
        .ok_or("connection.json has no ip")?;
    let port = conn
        .get("port")
        .and_then(Value::as_f64)
        .ok_or("connection.json has no port")? as u16;
    let token = std::fs::read_to_string(data_dir.join("auth.token"))
        .map_err(|e| format!("cannot read auth.token: {}", e))?;
    let engine = Engine {
        host: format!("{}:{}", ip, port),
        token: token.trim().to_string(),
    };
    let ollama_host = ollama
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string();

    // Pre-flight: Ollama must answer, and the model must be present.
    let (st, tags_body) = http(&ollama_host, "GET", "/api/tags", None, None, "")
        .map_err(|e| format!("cannot reach Ollama at {}: {}", ollama, e))?;
    if st != 200 {
        return Err(format!("Ollama at {} answered HTTP {}", ollama, st));
    }
    let tags = json::parse(&String::from_utf8_lossy(&tags_body))
        .map_err(|e| format!("bad /api/tags reply: {}", e))?;
    let have_model = tags
        .get("models")
        .and_then(Value::as_arr)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("name").and_then(Value::as_str))
                .any(|n| n == model)
        })
        .unwrap_or(false);
    if !have_model {
        return Err(format!(
            "model {} is not pulled in Ollama — pull it from the app's Local AI tab first",
            model
        ));
    }

    // Monitors, once (dimensions do not change mid-run in practice).
    let (st, mons_body) = engine.get("/monitors")?;
    if st != 200 {
        return Err(format!("GET /monitors failed: HTTP {}", st));
    }
    let mons = json::parse(&String::from_utf8_lossy(&mons_body))
        .map_err(|e| format!("bad /monitors reply: {}", e))?;
    let mons_arr = mons
        .get("monitors")
        .and_then(Value::as_arr)
        .ok_or("/monitors reply has no monitors")?;
    let pick = |id: u32| -> Option<(f64, f64, bool)> {
        mons_arr.iter().find_map(|m| {
            let mid = m.get("id").and_then(Value::as_f64)? as u32;
            if mid == id {
                Some((
                    m.get("width").and_then(Value::as_f64).unwrap_or(0.0),
                    m.get("height").and_then(Value::as_f64).unwrap_or(0.0),
                    m.get("is_primary").and_then(Value::as_bool).unwrap_or(false),
                ))
            } else {
                None
            }
        })
    };
    let (monitor_id, mon_w, mon_h) = match monitor_wanted {
        Some(id) => {
            let (w, h, _) = pick(id).ok_or_else(|| format!("no monitor {}", id))?;
            (id, w, h)
        }
        None => {
            let primary = mons_arr.iter().find(|m| {
                m.get("is_primary").and_then(Value::as_bool).unwrap_or(false)
            });
            let m = primary.or_else(|| mons_arr.first()).ok_or("no monitors")?;
            (
                m.get("id").and_then(Value::as_f64).unwrap_or(0.0) as u32,
                m.get("width").and_then(Value::as_f64).unwrap_or(0.0),
                m.get("height").and_then(Value::as_f64).unwrap_or(0.0),
            )
        }
    };
    println!(
        "local-operator: task {:?} on monitor {} ({}x{}), model {}",
        task, monitor_id, mon_w, mon_h, model
    );

    let prompt = prompt_for(&task);
    let mut failures = 0u32;
    for step in 1..=max_steps {
        let (st, shot) = engine.get(&format!(
            "/monitors/{}/screenshot?width=1280&format=jpeg",
            monitor_id
        ))?;
        if st != 200 {
            failures += 1;
            println!("step {}: screenshot failed (HTTP {}), failure {}", step, st, failures);
            if failures >= MAX_CONSECUTIVE_FAILURES {
                return Err(format!("{} consecutive screenshot failures", failures));
            }
            continue;
        }
        let chat_body = format!(
            "{{\"model\":{},\"messages\":[{{\"role\":\"user\",\"content\":{},\"images\":[{}]}}],\"stream\":false,\"options\":{{\"temperature\":0.1,\"num_ctx\":8192}}}}",
            json::quote(&model),
            json::quote(&prompt),
            json::quote(&base64(&shot)),
        );
        let (st, chat) = http(
            &ollama_host,
            "POST",
            "/api/chat",
            Some(chat_body.as_bytes()),
            None,
            "application/json",
        )?;
        if st != 200 {
            failures += 1;
            println!("step {}: ollama failed (HTTP {}), failure {}", step, st, failures);
            if failures >= MAX_CONSECUTIVE_FAILURES {
                return Err(format!("{} consecutive Ollama failures", failures));
            }
            continue;
        }
        let reply = json::parse(&String::from_utf8_lossy(&chat))
            .map_err(|e| format!("bad /api/chat reply: {}", e))?;
        let content = reply
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let action = match parse_action(&content) {
            Ok(a) => a,
            Err(e) => {
                failures += 1;
                println!("step {}: unparseable action ({}), failure {}", step, e, failures);
                if failures >= MAX_CONSECUTIVE_FAILURES {
                    return Err(format!("{} consecutive unparseable actions", failures));
                }
                continue;
            }
        };
        failures = 0;
        let (label, result) = match &action {
            Action::Move { monitor, x, y } => {
                let (px, py) = normalized_to_pixels(*x, *y, mon_w, mon_h);
                let body = format!(
                    "{{\"monitor\":{},\"x\":{},\"y\":{}}}",
                    monitor, px, py
                );
                ("move", engine.post_json("/input/move", &body))
            }
            Action::Click { monitor, x, y } => {
                let (px, py) = normalized_to_pixels(*x, *y, mon_w, mon_h);
                let body = format!(
                    "{{\"monitor\":{},\"x\":{},\"y\":{},\"button\":\"left\"}}",
                    monitor, px, py
                );
                ("click", engine.post_json("/input/click", &body))
            }
            Action::Scroll { monitor, x, y, delta } => {
                let (px, py) = normalized_to_pixels(*x, *y, mon_w, mon_h);
                let body = format!(
                    "{{\"monitor\":{},\"x\":{},\"y\":{},\"delta\":{}}}",
                    monitor, px, py, delta
                );
                ("scroll", engine.post_json("/input/scroll", &body))
            }
            Action::Type { text } => {
                let body = format!("{{\"text\":{}}}", json::quote(text));
                ("type", engine.post_json("/input/type", &body))
            }
            Action::Keys { keys } => {
                let arr = keys
                    .iter()
                    .map(|k| json::quote(k))
                    .collect::<Vec<_>>()
                    .join(",");
                let body = format!("{{\"keys\":[{}]}}", arr);
                ("keys", engine.post_json("/input/keys", &body))
            }
            Action::Done { reason } => {
                println!("done: {}", reason);
                return Ok(());
            }
        };
        match result {
            Ok((st, _)) if (200..300).contains(&st) => {
                println!("step {}: {} -> ok", step, label);
            }
            Ok((st, body)) => {
                println!(
                    "step {}: {} -> engine HTTP {}: {}",
                    step,
                    label,
                    st,
                    String::from_utf8_lossy(&body).chars().take(200).collect::<String>()
                );
            }
            Err(e) => {
                println!("step {}: {} -> error: {}", step, label, e);
            }
        }
        std::thread::sleep(Duration::from_millis(STEP_DELAY_MS));
    }
    Err(format!("max steps ({}) reached without done", max_steps))
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            println!("error: {}", e);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn action_parses_click_and_done() {
        let a = parse_action(r#"{"action":"click","monitor":0,"x":500,"y":250}"#).unwrap();
        assert_eq!(a, Action::Click { monitor: 0, x: 500.0, y: 250.0 });
        let d = parse_action("Here you go:\n```json\n{\"action\":\"done\",\"reason\":\"menu open\"}\n```").unwrap();
        assert_eq!(d, Action::Done { reason: "menu open".to_string() });
    }

    #[test]
    fn action_rejects_unknown_and_malformed() {
        assert!(parse_action(r#"{"action":"teleport"}"#).is_err());
        assert!(parse_action("no json here").is_err());
    }

    #[test]
    fn normalized_conversion_clamps() {
        assert_eq!(normalized_to_pixels(500.0, 500.0, 2560.0, 1440.0), (1280.0, 720.0));
        assert_eq!(normalized_to_pixels(1200.0, -50.0, 2560.0, 1440.0), (2559.0, 0.0));
    }
}
