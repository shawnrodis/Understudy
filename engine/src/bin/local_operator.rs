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
//! HTTP notes: responses may use Content-Length or chunked framing
//! (Ollama's Go server switches to chunked above ~2 KB) — the client
//! decodes both before parsing. Ollama's `/api/chat` with
//! `stream: false` returns one JSON object, not a stream of lines.
//! A failed chat call retries once on the same screenshot, then counts
//! as a failed step; bad replies are logged with status + body snippet
//! instead of exiting opaque.
//!
//! Parse failures are diagnosable, not opaque: the raw model text is
//! logged (operator stdout, truncated to ~500 chars, and POSTed to the
//! engine's /log/note so it also lands in the engine action log), a
//! reply cut off mid-object (unbalanced braces) retries the chat once
//! instead of counting truncated garbage, and every executed action is
//! logged with its full details (monitor, normalized + logical
//! coordinates, origin) — a "click -> ok" that achieves nothing must
//! still say exactly where it clicked. Pointer actions always execute
//! on the screenshotted monitor: the model only ever sees that one
//! screenshot, so its coordinates are meaningless anywhere else, and
//! its monitor field is logged, not followed.

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
    // A chunked body is NOT JSON: it starts with a hex chunk-size
    // line, which is exactly the "unexpected input at 0" the v0.1.5
    // field run died on. Go's net/http (Ollama's server) sends a
    // Content-Length only for small bodies and switches to chunked
    // framing above ~2 KB — so step 1's short reply parsed fine and
    // step 2's longer reply failed at position 0. Chunked framing
    // must be decoded before anyone parses the body.
    let chunked = headers.lines().any(|l| match l.split_once(':') {
        Some((k, v)) => {
            k.trim().eq_ignore_ascii_case("Transfer-Encoding")
                && v.to_lowercase().contains("chunked")
        }
        None => false,
    });
    let mut body_bytes = buf[header_end..].to_vec();
    if chunked {
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => body_bytes.extend_from_slice(&chunk[..n]),
                Err(e) => return Err(format!("response body read failed: {}", e)),
            }
        }
        body_bytes = decode_chunked(&body_bytes)?;
    } else {
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
    }
    Ok((status, body_bytes))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Decode a chunked transfer-encoding body into its payload bytes.
/// Chunk-size lines are hex, optionally with `;extension` suffixes;
/// the terminating zero-size chunk's trailers (if any) are ignored.
fn decode_chunked(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let rel = find_subslice(&data[pos..], b"\r\n")
            .ok_or("chunked: missing chunk-size line")?;
        let line_end = pos + rel;
        let line = std::str::from_utf8(&data[pos..line_end])
            .map_err(|_| "chunked: bad chunk-size encoding")?;
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| format!("chunked: bad chunk size {:?}", size_str))?;
        pos = line_end + 2;
        if size == 0 {
            break;
        }
        if pos + size > data.len() {
            return Err("chunked: truncated chunk".to_string());
        }
        out.extend_from_slice(&data[pos..pos + size]);
        pos += size;
        if data.len() < pos + 2 || &data[pos..pos + 2] != b"\r\n" {
            return Err("chunked: missing CRLF after chunk".to_string());
        }
        pos += 2;
    }
    Ok(out)
}

/// First ~200 chars of a raw reply body, single-line, for error logs.
/// A bad reply must never again be reported without showing what the
/// server actually sent.
fn snippet(body: &[u8]) -> String {
    String::from_utf8_lossy(body)
        .chars()
        .take(200)
        .collect::<String>()
        .replace('\n', " ")
        .replace('\r', "")
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

/// One /api/chat call: returns the message content, or a descriptive
/// error that always says what the server actually sent — HTTP status
/// plus Ollama's {"error": ...} field, or the first ~200 chars of the
/// raw body when the reply is not parseable JSON. A bad reply used to
/// surface as just "unexpected input at 0" and kill the run; it now
/// carries the evidence with it.
fn chat_step(ollama_host: &str, model: &str, prompt: &str, shot: &[u8]) -> Result<String, String> {
    let chat_body = format!(
        "{{\"model\":{},\"messages\":[{{\"role\":\"user\",\"content\":{},\"images\":[{}]}}],\"stream\":false,\"options\":{{\"temperature\":0.1,\"num_ctx\":8192}}}}",
        json::quote(model),
        json::quote(prompt),
        json::quote(&base64(shot)),
    );
    let (st, chat) = http(
        ollama_host,
        "POST",
        "/api/chat",
        Some(chat_body.as_bytes()),
        None,
        "application/json",
    )
    .map_err(|e| format!("chat request failed: {}", e))?;
    if st != 200 {
        let msg = json::parse(&String::from_utf8_lossy(&chat))
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| snippet(&chat));
        return Err(format!("ollama HTTP {}: {}", st, msg));
    }
    let reply = json::parse(&String::from_utf8_lossy(&chat)).map_err(|e| {
        format!(
            "bad /api/chat reply (HTTP 200): {}; body starts: {}",
            e,
            snippet(&chat)
        )
    })?;
    Ok(reply
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string())
}

/// Best-effort: record a parse failure in the engine action log (as
/// an "operator-note" entry, never an executed input) so it is visible
/// in the app's Action log tab, not just this process's stdout.
/// Failures to post the note are ignored — logging must never fail
/// the step itself.
fn note_parse_failure(engine: &Engine, detail: &str) {
    let body = format!("{{\"text\":{}}}", json::quote(detail));
    let _ = engine.post_json("/log/note", &body);
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

/// Why an action parse failed. Truncated means the reply was cut off
/// mid-object (unbalanced braces) — worth one fresh chat retry, since
/// the model never finished its answer. Anything else is Invalid: the
/// model produced a complete object that is not a valid action.
#[derive(Debug, PartialEq)]
enum ParseFail {
    Truncated(String),
    Invalid(String),
}

impl ParseFail {
    fn message(&self) -> &str {
        match self {
            ParseFail::Truncated(m) | ParseFail::Invalid(m) => m,
        }
    }
}

/// Parse the model's action JSON. Tolerates fences and prose wrappers
/// (a fenced block anywhere wins; else the first `{...}` object), but
/// validation is strict afterwards: the action must be known,
/// coordinates must be numbers (clamped to 0–1000), and `type`/`keys`
/// must carry non-empty payloads. Anything failing validation is a
/// failed step and is NEVER executed — tolerance covers the wrapping,
/// never the action itself.
fn parse_action(text: &str) -> Result<Action, ParseFail> {
    let cleaned = strip_fences(text.trim());
    let json_text = match extract_json_object(&cleaned) {
        Extracted::Found(s) => s,
        Extracted::NoBrace => {
            return Err(ParseFail::Invalid(
                "no JSON object in model reply".to_string(),
            ))
        }
        Extracted::Unbalanced => {
            return Err(ParseFail::Truncated(
                "model reply ends mid-object (unbalanced braces)".to_string(),
            ))
        }
    };
    let v = json::parse(&json_text)
        .map_err(|e| ParseFail::Invalid(format!("bad action JSON: {}", e)))?;
    let action = v.get("action").and_then(Value::as_str).ok_or_else(|| {
        ParseFail::Invalid("action JSON has no 'action' field".to_string())
    })?;
    // The model's monitor field is parsed (and validated when present)
    // but the caller executes pointer actions on the screenshotted
    // monitor: the model only ever sees that one screenshot, so its
    // coordinates are meaningless anywhere else. See run().
    let monitor = match v.get("monitor") {
        None => 0,
        Some(m) => m.as_f64().ok_or_else(|| {
            ParseFail::Invalid("action 'monitor' must be a number".to_string())
        })? as u32,
    };
    let coord = |key: &str| -> Result<f64, ParseFail> {
        let n = v.get(key).and_then(Value::as_f64).ok_or_else(|| {
            ParseFail::Invalid(format!("action '{}' must be a number", key))
        })?;
        Ok(n.clamp(0.0, 1000.0))
    };
    match action {
        "move" => Ok(Action::Move {
            monitor,
            x: coord("x")?,
            y: coord("y")?,
        }),
        "click" => Ok(Action::Click {
            monitor,
            x: coord("x")?,
            y: coord("y")?,
        }),
        "scroll" => Ok(Action::Scroll {
            monitor,
            x: coord("x")?,
            y: coord("y")?,
            delta: v.get("delta").and_then(Value::as_f64).unwrap_or(0.0) as i64,
        }),
        "type" => {
            let text = v.get("text").and_then(Value::as_str).ok_or_else(|| {
                ParseFail::Invalid("type action needs a 'text' string".to_string())
            })?;
            if text.is_empty() {
                return Err(ParseFail::Invalid(
                    "type action needs non-empty 'text'".to_string(),
                ));
            }
            Ok(Action::Type {
                text: text.to_string(),
            })
        }
        "keys" => {
            let arr = v.get("keys").and_then(Value::as_arr).ok_or_else(|| {
                ParseFail::Invalid("keys action needs a 'keys' array".to_string())
            })?;
            if arr.is_empty() {
                return Err(ParseFail::Invalid(
                    "keys action needs a non-empty 'keys' array".to_string(),
                ));
            }
            let mut keys = Vec::new();
            for k in arr {
                match k.as_str() {
                    Some(s) => keys.push(s.to_string()),
                    None => {
                        return Err(ParseFail::Invalid(
                            "keys array must contain only strings".to_string(),
                        ))
                    }
                }
            }
            Ok(Action::Keys { keys })
        }
        "done" => Ok(Action::Done {
            reason: v
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }),
        other => Err(ParseFail::Invalid(format!("unknown action: {}", other))),
    }
}

/// Remove markdown fences around the action object, wherever they sit:
/// a ```json ... ``` block anywhere wins; else a bare ``` ... ```
/// block; else the whole text (leading/trailing prose is handled by
/// extraction). A reply cut off inside a fence still yields its
/// partial object, which extraction reports as Unbalanced.
fn strip_fences(text: &str) -> String {
    if let Some(start) = text.find("```json") {
        let after = &text[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
        return after.trim().to_string();
    }
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
        return after.trim().to_string();
    }
    text.trim().to_string()
}

/// What the first-`{...}` search found. NoBrace = no object at all
/// (the model answered in prose); Unbalanced = an object starts but
/// never closes (a truncated reply).
#[derive(Debug, PartialEq)]
enum Extracted {
    Found(String),
    NoBrace,
    Unbalanced,
}

/// First balanced `{...}` in the text. String-aware: braces inside
/// quoted strings (with `\"` escapes) do not affect depth, so a
/// `type` text containing braces cannot break the span. Only the
/// first object is returned; trailing text after it is ignored.
fn extract_json_object(text: &str) -> Extracted {
    let start = match text.find('{') {
        Some(i) => i,
        None => return Extracted::NoBrace,
    };
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            match c {
                // Skip the escaped byte: a \" must not end the string.
                b'\\' => {
                    i += 2;
                    continue;
                }
                b'"' => in_string = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Extracted::Found(text[start..=i].to_string());
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    Extracted::Unbalanced
}

/// First ~500 chars of a model reply, single-line, for failure logs.
/// A bad action must never again be reported without showing what the
/// model actually returned — prose, malformed JSON, or a wrong scale
/// (e.g. 0–100 coordinates where 0–1000 was asked) are all visible
/// here. The same text is POSTed to the engine's /log/note.
fn action_snippet(text: &str) -> String {
    text.chars()
        .take(500)
        .collect::<String>()
        .replace('\n', " ")
        .replace('\r', "")
}

/// One-line detail for an executed pointer action: the monitor it ran
/// on, normalized + logical coordinates, and that monitor's origin —
/// plus the model's requested monitor when it differs, since a click
/// routed to the wrong screen reports "ok" while doing nothing
/// visible. This line is what separates a grounding error from a
/// mapping error on the next failure.
fn pointer_detail(
    kind: &str,
    target: &TargetMonitor,
    model_monitor: u32,
    x: f64,
    y: f64,
    px: f64,
    py: f64,
) -> String {
    let mut s = format!(
        "{} monitor {} x={} y={} px=({},{}) origin=({},{})",
        kind, target.id, x, y, px, py, target.x, target.y
    );
    if model_monitor != target.id {
        s.push_str(&format!(" [model said monitor {}]", model_monitor));
    }
    s
}

/// Convert normalized (0–1000) coordinates to a monitor's logical
/// pixels, clamped to that monitor's bounds.
fn normalized_to_pixels(x: f64, y: f64, width: f64, height: f64) -> (f64, f64) {
    (
        (x / 1000.0 * width).clamp(0.0, (width - 1.0).max(0.0)),
        (y / 1000.0 * height).clamp(0.0, (height - 1.0).max(0.0)),
    )
}

fn prompt_for(task: &str, monitor_id: u32, mon_w: f64, mon_h: f64) -> String {
    format!(
        "You are operating this Windows PC through screenshots. Task: {task}\n\
        This screenshot is monitor {monitor_id} ({mon_w}x{mon_h} logical pixels).\n\
        Output ONLY a single JSON object. No prose, no markdown fences, no\n\
        explanation, no apology, no trailing text. Start your reply with {{ and\n\
        end with }}. Use exactly this shape:\n\
        {{\"action\": \"move\"|\"click\"|\"scroll\"|\"type\"|\"keys\"|\"done\", \
        \"monitor\": <id>, \"x\": <0-1000>, \"y\": <0-1000>, \"text\": \"...\", \
        \"keys\": [\"...\"], \"delta\": <scroll amount>, \"reason\": \"...\"}}\n\
        x and y are normalized 0-1000 across the screenshot (0,0 is the \
        top-left). Use \"done\" only when the task is complete, with a \
        short reason. Do one small step per reply; never claim done for \
        a step you have not seen take effect. For system-level operations\n\
        (opening Start, showing the desktop, switching windows), prefer a\n\
        keys action (for example keys [\"win\"]) over grounding a click — a\n\
        keypress is more reliable than a pixel target."
    )
}

/// One monitor's geometry as the operator needs it: id, virtual-desktop
/// origin (physical px), LOGICAL dims (/monitors reports physical size
/// + scale, while the engine API takes logical/DIP coordinates — using
/// physical dims here used to double-scale on HiDPI monitors), scale,
/// and the primary flag.
struct TargetMonitor {
    id: u32,
    x: i32,
    y: i32,
    logical_w: f64,
    logical_h: f64,
    scale: f64,
    is_primary: bool,
}

/// Pick the operator's target monitor: `--monitor <id>` wins, else the
/// monitor flagged primary (never a fixed id — Windows enumeration
/// order shifts between runs, which is why a v0.1.5 run logged monitor
/// 0 and a v0.1.6 run monitor 1 for the same landscape panel), else
/// the first monitor. Returns None when the wanted id is absent or
/// there are no monitors at all.
fn select_monitor(mons: &[Value], wanted: Option<u32>) -> Option<TargetMonitor> {
    let read = |m: &Value| -> Option<TargetMonitor> {
        let scale = m.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
        let scale = if scale > 0.0 { scale } else { 1.0 };
        Some(TargetMonitor {
            id: m.get("id").and_then(Value::as_f64)? as u32,
            x: m.get("x").and_then(Value::as_f64)? as i32,
            y: m.get("y").and_then(Value::as_f64)? as i32,
            logical_w: m.get("width").and_then(Value::as_f64)? / scale,
            logical_h: m.get("height").and_then(Value::as_f64)? / scale,
            scale,
            is_primary: m
                .get("is_primary")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    };
    if let Some(id) = wanted {
        return mons.iter().find_map(|m| {
            let t = read(m)?;
            if t.id == id {
                Some(t)
            } else {
                None
            }
        });
    }
    if let Some(m) = mons
        .iter()
        .find(|m| m.get("is_primary").and_then(Value::as_bool).unwrap_or(false))
    {
        return read(m);
    }
    mons.first().and_then(read)
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
        .map_err(|e| format!("bad /api/tags reply: {}; body starts: {}", e, snippet(&tags_body)))?;
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
    let target = match select_monitor(mons_arr, monitor_wanted) {
        Some(t) => t,
        None => {
            return Err(match monitor_wanted {
                Some(id) => format!("no monitor {}", id),
                None => "no monitors".to_string(),
            })
        }
    };
    let monitor_id = target.id;
    let (mon_w, mon_h) = (target.logical_w, target.logical_h);
    println!(
        "local-operator: task {:?} on monitor {} at ({},{}) {}x{} logical, scale {:.2}, {}",
        task,
        target.id,
        target.x,
        target.y,
        target.logical_w,
        target.logical_h,
        target.scale,
        if target.is_primary {
            "primary"
        } else {
            "not flagged primary"
        }
    );

    let prompt = prompt_for(&task, monitor_id, mon_w, mon_h);
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
        // A failed chat call used to kill the run immediately (the
        // `?` on the parse). It now retries once on the same fresh
        // screenshot, and only then counts as a failed step — a
        // transient Ollama hiccup no longer ends the task.
        let content = match chat_step(&ollama_host, &model, &prompt, &shot) {
            Ok(c) => c,
            Err(e1) => {
                println!("step {}: chat failed ({}), retrying once", step, e1);
                match chat_step(&ollama_host, &model, &prompt, &shot) {
                    Ok(c) => c,
                    Err(e2) => {
                        failures += 1;
                        println!(
                            "step {}: chat failed again ({}), failure {}",
                            step, e2, failures
                        );
                        if failures >= MAX_CONSECUTIVE_FAILURES {
                            return Err(format!(
                                "{} consecutive chat failures",
                                failures
                            ));
                        }
                        continue;
                    }
                }
            }
        };
        let action = match parse_action(&content) {
            Ok(a) => a,
            Err(ParseFail::Truncated(msg)) => {
                // A truncated reply means the model never finished —
                // retry the chat once on the same screenshot (the
                // screen has not changed, since no action ran) instead
                // of counting truncated garbage. Only the retry's
                // outcome counts as a failure.
                println!(
                    "step {}: truncated action ({}), raw: {}, retrying chat once",
                    step,
                    msg,
                    action_snippet(&content)
                );
                note_parse_failure(
                    &engine,
                    &format!("truncated action ({}): {}", msg, action_snippet(&content)),
                );
                match chat_step(&ollama_host, &model, &prompt, &shot) {
                    Ok(c2) => match parse_action(&c2) {
                        Ok(a) => a,
                        Err(e2) => {
                            failures += 1;
                            println!(
                                "step {}: still unparseable after retry ({}), raw: {}, failure {}",
                                step,
                                e2.message(),
                                action_snippet(&c2),
                                failures
                            );
                            note_parse_failure(
                                &engine,
                                &format!(
                                    "unparseable action after retry ({}): {}",
                                    e2.message(),
                                    action_snippet(&c2)
                                ),
                            );
                            if failures >= MAX_CONSECUTIVE_FAILURES {
                                return Err(format!(
                                    "{} consecutive unparseable actions",
                                    failures
                                ));
                            }
                            continue;
                        }
                    },
                    Err(e2) => {
                        failures += 1;
                        println!(
                            "step {}: chat retry failed ({}), failure {}",
                            step, e2, failures
                        );
                        if failures >= MAX_CONSECUTIVE_FAILURES {
                            return Err(format!("{} consecutive chat failures", failures));
                        }
                        continue;
                    }
                }
            }
            Err(ParseFail::Invalid(msg)) => {
                failures += 1;
                println!(
                    "step {}: unparseable action ({}), raw: {}, failure {}",
                    step,
                    msg,
                    action_snippet(&content),
                    failures
                );
                note_parse_failure(
                    &engine,
                    &format!(
                        "unparseable action ({}): {}",
                        msg,
                        action_snippet(&content)
                    ),
                );
                if failures >= MAX_CONSECUTIVE_FAILURES {
                    return Err(format!("{} consecutive unparseable actions", failures));
                }
                continue;
            }
        };
        failures = 0;
        let (detail, result) = match &action {
            Action::Move { monitor, x, y } => {
                let (px, py) = normalized_to_pixels(*x, *y, mon_w, mon_h);
                let body = format!(
                    "{{\"monitor\":{},\"x\":{},\"y\":{}}}",
                    monitor_id, px, py
                );
                (
                    pointer_detail("move", &target, *monitor, *x, *y, px, py),
                    engine.post_json("/input/move", &body),
                )
            }
            Action::Click { monitor, x, y } => {
                let (px, py) = normalized_to_pixels(*x, *y, mon_w, mon_h);
                let body = format!(
                    "{{\"monitor\":{},\"x\":{},\"y\":{},\"button\":\"left\"}}",
                    monitor_id, px, py
                );
                (
                    pointer_detail("click", &target, *monitor, *x, *y, px, py),
                    engine.post_json("/input/click", &body),
                )
            }
            Action::Scroll {
                monitor,
                x,
                y,
                delta,
            } => {
                let (px, py) = normalized_to_pixels(*x, *y, mon_w, mon_h);
                let body = format!(
                    "{{\"monitor\":{},\"x\":{},\"y\":{},\"delta\":{}}}",
                    monitor_id, px, py, delta
                );
                (
                    format!(
                        "{} delta={}",
                        pointer_detail("scroll", &target, *monitor, *x, *y, px, py),
                        delta
                    ),
                    engine.post_json("/input/scroll", &body),
                )
            }
            Action::Type { text } => {
                let body = format!("{{\"text\":{}}}", json::quote(text));
                (
                    format!(
                        "type {:?}",
                        text.chars().take(200).collect::<String>()
                    ),
                    engine.post_json("/input/type", &body),
                )
            }
            Action::Keys { keys } => {
                let arr = keys
                    .iter()
                    .map(|k| json::quote(k))
                    .collect::<Vec<_>>()
                    .join(",");
                let body = format!("{{\"keys\":[{}]}}", arr);
                (
                    format!("keys {:?}", keys),
                    engine.post_json("/input/keys", &body),
                )
            }
            Action::Done { reason } => {
                println!("done: {}", reason);
                return Ok(());
            }
        };
        match result {
            Ok((st, _)) if (200..300).contains(&st) => {
                println!("step {}: {} -> ok", step, detail);
            }
            Ok((st, body)) => {
                println!(
                    "step {}: {} -> engine HTTP {}: {}",
                    step,
                    detail,
                    st,
                    String::from_utf8_lossy(&body).chars().take(200).collect::<String>()
                );
            }
            Err(e) => {
                println!("step {}: {} -> error: {}", step, detail, e);
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

    #[test]
    fn chunked_decoding_matches_known_framing() {
        // "Wikipedia" in two chunks, then the zero terminator.
        let framed = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(framed).unwrap(), b"Wikipedia");
    }

    #[test]
    fn chunked_decoding_tolerates_extensions_and_trailers() {
        let framed = b"5;ext=1\r\nHello\r\n0\r\nX-Trailer: y\r\n\r\n";
        assert_eq!(decode_chunked(framed).unwrap(), b"Hello");
    }

    #[test]
    fn chunked_decoding_rejects_bad_frames() {
        assert!(decode_chunked(b"zz\r\nabc\r\n0\r\n\r\n").is_err());
        assert!(decode_chunked(b"5\r\nHi\r\n0\r\n\r\n").is_err());
        assert!(decode_chunked(b"").is_err());
    }

    #[test]
    fn chunked_ollama_envelope_parses_after_decode() {
        // Regression test for the v0.1.5 field failure: a chat reply
        // above Go's ~2 KB Content-Length threshold arrives chunked,
        // and parsing the raw framing fails at position 0.
        let envelope = r#"{"model":"qwen3-vl:8b","message":{"role":"assistant","content":"{\"action\":\"click\",\"x\":500,\"y\":250}"},"done":true}"#;
        let framed = format!("{:x}\r\n{}\r\n0\r\n\r\n", envelope.len(), envelope);
        assert!(json::parse(&String::from_utf8_lossy(framed.as_bytes())).is_err());
        let decoded = decode_chunked(framed.as_bytes()).unwrap();
        let v = json::parse(&String::from_utf8_lossy(&decoded)).unwrap();
        let content = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap();
        assert!(parse_action(content).is_ok());
    }

    #[test]
    fn snippet_truncates_and_flattens() {
        let long = "a".repeat(300);
        let body = format!("{}\nsecond line", long);
        let s = snippet(body.as_bytes());
        assert_eq!(s.chars().count(), 200);
        assert!(!s.contains('\n'));
    }

    #[test]
    fn action_parses_fences_anywhere_and_prose() {
        // Fenced block mid-reply with prose on both sides.
        let fenced = parse_action(
            "Here is the next step:\n```json\n{\"action\":\"click\",\"x\":100,\"y\":200}\n```\nLet me know.",
        )
        .unwrap();
        assert_eq!(
            fenced,
            Action::Click {
                monitor: 0,
                x: 100.0,
                y: 200.0
            }
        );
        // Bare prose before/after, no fences at all.
        let prose = parse_action(
            "Sure! {\"action\":\"keys\",\"keys\":[\"win\"]} Hope that helps.",
        )
        .unwrap();
        assert_eq!(
            prose,
            Action::Keys {
                keys: vec!["win".to_string()]
            }
        );
        // Trailing text after the object, no fences.
        let trailing =
            parse_action("{\"action\":\"done\",\"reason\":\"open\"} all done!").unwrap();
        assert_eq!(
            trailing,
            Action::Done {
                reason: "open".to_string()
            }
        );
    }

    #[test]
    fn action_parses_braces_inside_strings() {
        // The old brace counter broke on braces inside string values.
        let a = parse_action(
            r#"{"action":"type","text":"hello {world} and {friends}"}"#,
        )
        .unwrap();
        assert_eq!(
            a,
            Action::Type {
                text: "hello {world} and {friends}".to_string()
            }
        );
    }

    #[test]
    fn action_distinguishes_truncated_from_malformed() {
        // Cut off mid-object: unbalanced braces -> Truncated.
        assert_eq!(
            parse_action("{\"action\":\"click\",\"x\":500,").unwrap_err(),
            ParseFail::Truncated(
                "model reply ends mid-object (unbalanced braces)".to_string()
            )
        );
        // Complete object, bad JSON: trailing comma -> Invalid.
        assert!(matches!(
            parse_action("{\"action\":\"click\",\"x\":500,}").unwrap_err(),
            ParseFail::Invalid(_)
        ));
        // Complete object, unquoted key -> Invalid.
        assert!(matches!(
            parse_action("{action:\"click\"}").unwrap_err(),
            ParseFail::Invalid(_)
        ));
        // No object at all (prose reply) -> Invalid, not Truncated.
        assert!(matches!(
            parse_action("I can see the Start button.").unwrap_err(),
            ParseFail::Invalid(_)
        ));
    }

    #[test]
    fn action_validation_rules() {
        // Pointer actions need numeric x and y.
        assert!(parse_action(r#"{"action":"click","x":500}"#).is_err());
        assert!(parse_action(r#"{"action":"click","x":"500","y":100}"#).is_err());
        // Out-of-range coordinates clamp to 0–1000, they do not fail.
        let clamped = parse_action(r#"{"action":"move","x":1500,"y":-20}"#).unwrap();
        assert_eq!(
            clamped,
            Action::Move {
                monitor: 0,
                x: 1000.0,
                y: 0.0
            }
        );
        // type/keys need non-empty payloads.
        assert!(parse_action(r#"{"action":"type","text":""}"#).is_err());
        assert!(parse_action(r#"{"action":"type"}"#).is_err());
        assert!(parse_action(r#"{"action":"keys","keys":[]}"#).is_err());
        assert!(parse_action(r#"{"action":"keys","keys":["win",7]}"#).is_err());
        // Monitor must be numeric when present.
        assert!(parse_action(r#"{"action":"click","monitor":"1","x":1,"y":1}"#).is_err());
    }

    #[test]
    fn action_snippet_truncates_to_500_and_flattens() {
        let long = format!("{}\nsecond line", "b".repeat(600));
        let s = action_snippet(&long);
        assert_eq!(s.chars().count(), 500);
        assert!(!s.contains('\n'));
    }

    #[test]
    fn select_monitor_prefers_primary_flag_not_id_order() {
        let portrait = json::parse(
            r#"{"id":0,"x":2560,"y":-556,"width":2160,"height":3840,"scale":1.5,"is_primary":false}"#,
        )
        .unwrap();
        let landscape = json::parse(
            r#"{"id":1,"x":0,"y":0,"width":2560,"height":1440,"scale":1.0,"is_primary":true}"#,
        )
        .unwrap();
        // Primary wins even when it is not first in the list.
        let t = select_monitor(&[portrait.clone(), landscape.clone()], None).unwrap();
        assert_eq!(t.id, 1);
        assert_eq!((t.x, t.y), (0, 0));
        assert_eq!((t.logical_w, t.logical_h), (2560.0, 1440.0));
        assert!(t.is_primary);
        // No primary flagged -> first monitor.
        let plain = json::parse(
            r#"{"id":5,"x":0,"y":0,"width":1920,"height":1080,"scale":1.0,"is_primary":false}"#,
        )
        .unwrap();
        let t = select_monitor(&[plain], None).unwrap();
        assert_eq!(t.id, 5);
        assert!(!t.is_primary);
        // --monitor picks by id; unknown id -> None.
        let t =
            select_monitor(&[portrait.clone(), landscape.clone()], Some(0)).unwrap();
        assert_eq!(t.id, 0);
        // Logical dims divide physical size by scale (no double-scaling).
        assert_eq!((t.logical_w, t.logical_h), (1440.0, 2560.0));
        assert_eq!(t.scale, 1.5);
        assert!(select_monitor(&[portrait, landscape], Some(9)).is_none());
        assert!(select_monitor(&[], None).is_none());
    }

    #[test]
    fn prompt_forbids_prose_and_prefers_keys_for_system_ops() {
        let p = prompt_for("Open start menu", 1, 2560.0, 1440.0);
        assert!(p.contains("ONLY a single JSON object"));
        assert!(p.contains("no markdown fences"));
        assert!(p.contains("monitor 1"));
        assert!(p.contains("keys [\"win\"]"));
    }
}
