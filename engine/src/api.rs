//! The control API: HTTP/JSON over a Tailscale-only listener.
//!
//! This is the PRIMARY interface of Understudy. Every capability —
//! per-monitor screenshots, mouse/keyboard injection, app/process control,
//! file transfer, command execution — is fully operable headlessly through
//! these endpoints; no step requires clicking around in the Flutter UI.
//! The Flutter UI only monitors (status, action log), offers the kill
//! switch, and walks Tailscale setup.
//!
//! Transport rules (enforced by `main.rs`, not just documented):
//! * The listener binds to the Tailscale IPv4 address (or 127.0.0.1 when
//!   Tailscale is down), never to 0.0.0.0 or a public interface.
//! * Every endpoint requires `Authorization: Bearer <token>` (see
//!   `auth.rs`). There are no unauthenticated endpoints.
//! * Every request — including rejected ones — is appended to the local
//!   action log. Tokens, auth keys, and file contents are never logged.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::{bearer_token, tokens_equal};
use crate::json::{self, quote, Value};
use crate::{capture, cmd, files, input, log, monitors, process, tailscale};

pub const VERSION: &str = "0.1.0";

/// Shared server state.
pub struct State {
    pub data_dir: PathBuf,
    pub token: String,
    pub file_roots: Vec<PathBuf>,
    pub shutdown: AtomicBool,
    pub requests: AtomicU64,
    pub last_request_unix: AtomicU64,
    pub started_unix: u64,
}

impl State {
    pub fn new(data_dir: PathBuf, token: String, file_roots: Vec<PathBuf>) -> State {
        State {
            data_dir,
            token,
            file_roots,
            shutdown: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            last_request_unix: AtomicU64::new(0),
            started_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

fn json_resp(status: u16, body: &str) -> Response {
    Response {
        status,
        reason: reason_for(status),
        content_type: "application/json",
        body: body.as_bytes().to_vec(),
    }
}

fn png_resp(bytes: Vec<u8>) -> Response {
    Response {
        status: 200,
        reason: "OK",
        content_type: "image/png",
        body: bytes,
    }
}

fn raw_resp(bytes: Vec<u8>, filename: &str) -> Response {
    // Filename is sent as a header by the caller path; kept simple here.
    let _ = filename;
    Response {
        status: 200,
        reason: "OK",
        content_type: "application/octet-stream",
        body: bytes,
    }
}

fn err(status: u16, message: &str) -> Response {
    json_resp(status, &format!("{{\"ok\":false,\"error\":{}}}", quote(message)))
}

fn ok_json(inner: &str) -> Response {
    json_resp(200, &format!("{{\"ok\":true,{}}}", inner))
}

fn reason_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "OK",
    }
}

fn monitor_json(m: &monitors::Monitor) -> String {
    format!(
        "{{\"id\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"scale\":{},\"is_primary\":{}}}",
        m.id, m.x, m.y, m.width, m.height, m.scale, m.is_primary
    )
}

/// Percent-decode a query value (`+` -> space, `%XX` -> byte).
pub fn percent_decode(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            return it.next().map(percent_decode);
        }
    }
    None
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json_body(&self) -> Result<Value, String> {
        if self.body.trim().is_empty() {
            return Ok(Value::Obj(Default::default()));
        }
        json::parse(&self.body)
    }
}

/// Route one authenticated request. Returns the response; the caller logs.
fn route(state: &Arc<State>, req: &Request) -> Response {
    let mons = monitors::enumerate();
    let method = req.method.as_str();
    let path = req.path.as_str();

    match (method, path) {
        ("GET", "/status") => {
            let last = state.last_request_unix.load(Ordering::Relaxed);
            let active = last > 0 && now_unix().saturating_sub(last) <= 30;
            ok_json(&format!(
                "\"version\":{},\"bind\":\"tailscale-or-loopback\",\"monitors\":{},\"uptime_s\":{},\"requests\":{},\"session_active\":{},\"last_request_unix\":{}",
                quote(VERSION),
                mons.len(),
                now_unix().saturating_sub(state.started_unix),
                state.requests.load(Ordering::Relaxed),
                active,
                last
            ))
        }
        ("GET", "/monitors") => {
            let list: Vec<String> = mons.iter().map(monitor_json).collect();
            ok_json(&format!("\"monitors\":[{}]", list.join(",")))
        }
        ("GET", p) if p.starts_with("/monitors/") && p.ends_with("/screenshot") => {
            let id_part = &p["/monitors/".len()..p.len() - "/screenshot".len()];
            let id: u32 = match id_part.parse() {
                Ok(v) => v,
                Err(_) => return err(400, "bad monitor id"),
            };
            match capture::capture_monitor_png(&mons, id) {
                Ok((w, h, png_bytes)) => {
                    // Width/height ride along in a trailing JSON comment? No:
                    // they are derivable from the PNG itself. Keep it pure.
                    let _ = (w, h);
                    png_resp(png_bytes)
                }
                Err(e) => err(501, &e),
            }
        }
        ("POST", "/input/move") => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            let (monitor, x, y) = match point_fields(&body) {
                Ok(v) => v,
                Err(e) => return err(400, &e),
            };
            match input::resolve_point(&mons, monitor, x, y)
                .and_then(input::move_to)
            {
                Ok(()) => ok_json(&format!(
                    "\"moved\":{{\"monitor\":{},\"x\":{},\"y\":{}}}",
                    monitor, x, y
                )),
                Err(e) => err(400, &e),
            }
        }
        ("POST", "/input/click") => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            let (monitor, x, y) = match point_fields(&body) {
                Ok(v) => v,
                Err(e) => return err(400, &e),
            };
            let button = match body
                .get("button")
                .and_then(Value::as_str)
                .unwrap_or("left")
            {
                s => match input::Button::from_str(s) {
                    Some(b) => b,
                    None => return err(400, "button must be left, right, or middle"),
                },
            };
            let double = body.opt_bool("double", false);
            match input::resolve_point(&mons, monitor, x, y)
                .and_then(|p| input::click(p, button, double))
            {
                Ok(()) => ok_json("\"clicked\":true"),
                Err(e) => err(400, &e),
            }
        }
        ("POST", "/input/scroll") => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            let (monitor, x, y) = match point_fields(&body) {
                Ok(v) => v,
                Err(e) => return err(400, &e),
            };
            let delta = match body.req_num("delta") {
                Ok(v) => v as i32,
                Err(e) => return err(400, &e),
            };
            match input::resolve_point(&mons, monitor, x, y)
                .and_then(|p| input::scroll(p, delta))
            {
                Ok(()) => ok_json("\"scrolled\":true"),
                Err(e) => err(400, &e),
            }
        }
        ("POST", "/input/type") => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            let text = match body.req_str("text") {
                Ok(t) => t,
                Err(e) => return err(400, &e),
            };
            match input::type_text(text) {
                Ok(()) => ok_json("\"typed\":true"),
                Err(e) => err(501, &e),
            }
        }
        ("POST", "/input/keys") => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            let keys = match body.get("keys").and_then(Value::as_arr) {
                Some(a) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect::<Vec<String>>(),
                None => return err(400, "keys must be an array of strings"),
            };
            if keys.is_empty() {
                return err(400, "keys must not be empty");
            }
            match input::key_chord(&keys) {
                Ok(()) => ok_json("\"pressed\":true"),
                Err(e) => err(501, &e),
            }
        }
        ("GET", "/apps") => match process::list_processes() {
            Ok(list) => {
                let items: Vec<String> = list
                    .iter()
                    .map(|p| {
                        format!(
                            "{{\"pid\":{},\"name\":{}}}",
                            p.pid,
                            quote(&p.name)
                        )
                    })
                    .collect();
                ok_json(&format!("\"processes\":[{}]", items.join(",")))
            }
            Err(e) => err(500, &e),
        },
        ("POST", "/apps/launch") => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            let path = match body.req_str("path") {
                Ok(p) => p,
                Err(e) => return err(400, &e),
            };
            let args: Vec<String> = body
                .get("args")
                .and_then(Value::as_arr)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default();
            match process::launch_app(path, &args) {
                Ok(pid) => ok_json(&format!("\"pid\":{}", pid)),
                Err(e) => err(500, &e),
            }
        }
        ("POST", "/apps/kill") => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            let pid = match body.req_num("pid") {
                Ok(v) => v as u32,
                Err(e) => return err(400, &e),
            };
            match process::kill_process(pid) {
                Ok(()) => ok_json("\"killed\":true"),
                Err(e) => err(500, &e),
            }
        }
        ("GET", "/files/download") => {
            let requested = match query_param(&req.query, "path") {
                Some(p) => p,
                None => return err(400, "missing ?path="),
            };
            match files::resolve_in_scope(&requested, &state.file_roots)
                .and_then(|p| files::read_file(&p))
            {
                Ok(bytes) => raw_resp(bytes, "download"),
                Err(e) => err(400, &e),
            }
        }
        ("POST", "/files/upload") => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            let path = match body.req_str("path") {
                Ok(p) => p,
                Err(e) => return err(400, &e),
            };
            let b64 = match body.req_str("base64") {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            let data = match files::b64_decode(b64) {
                Ok(d) => d,
                Err(e) => return err(400, &e),
            };
            match files::resolve_in_scope(path, &state.file_roots)
                .and_then(|p| files::write_file(&p, &data))
            {
                Ok(()) => ok_json(&format!("\"bytes\":{}", data.len())),
                Err(e) => err(400, &e),
            }
        }
        ("POST", "/cmd") => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            let command = match body.req_str("command") {
                Ok(c) => c,
                Err(e) => return err(400, &e),
            };
            let args: Vec<String> = body
                .get("args")
                .and_then(Value::as_arr)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default();
            let cwd = body.get("cwd").and_then(Value::as_str);
            match cmd::run(command, &args, cwd) {
                Ok(out) => ok_json(&format!(
                    "\"stdout\":{},\"stderr\":{},\"exit_code\":{}",
                    quote(&out.stdout),
                    quote(&out.stderr),
                    out.exit_code
                )),
                Err(e) => err(500, &e),
            }
        }
        ("GET", "/log") => {
            let limit = query_param(&req.query, "limit")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(100)
                .min(1000);
            let entries = log::read_last(&state.data_dir, limit);
            let items: Vec<String> = entries
                .iter()
                .map(|v| {
                    let ts = v.get("ts").and_then(Value::as_f64).unwrap_or(0.0) as u64;
                    let action = v.get("action").and_then(Value::as_str).unwrap_or("");
                    let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    let detail = v.get("detail").and_then(Value::as_str).unwrap_or("");
                    format!(
                        "{{\"ts\":{},\"action\":{},\"ok\":{},\"detail\":{}}}",
                        ts,
                        quote(action),
                        ok,
                        quote(detail)
                    )
                })
                .collect();
            ok_json(&format!("\"entries\":[{}]", items.join(",")))
        }
        ("GET", "/tailscale/status") => {
            let s = tailscale::status();
            ok_json(&format!(
                "\"installed\":{},\"logged_in\":{},\"ip\":{}",
                s.installed,
                s.logged_in,
                s.ip.map(|ip| quote(&ip)).unwrap_or("null".to_string())
            ))
        }
        ("POST", "/tailscale/install") => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            let version = body
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let version = if version.trim().is_empty() {
                match fetch_stable_version() {
                    Some(v) => v,
                    None => {
                        return err(
                            400,
                            "could not resolve the latest Tailscale version; pass {\"version\": \"x.y.z\"}",
                        )
                    }
                }
            } else {
                version
            };
            match tailscale::install_windows(&version) {
                Ok(()) => ok_json(&format!("\"installed_version\":{}", quote(&version))),
                Err(e) => err(501, &e),
            }
        }
        ("POST", "/tailscale/login") => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err(400, &e),
            };
            // The auth key is deliberately NOT logged (see route logging
            // below: login actions log only whether a key was supplied).
            let key = body.get("authkey").and_then(Value::as_str);
            match tailscale::login(key) {
                Ok(()) => ok_json("\"login_started\":true"),
                Err(e) => err(500, &e),
            }
        }
        ("POST", "/shutdown") => {
            state.shutdown.store(true, Ordering::Relaxed);
            ok_json("\"shutting_down\":true")
        }
        _ => err(404, "unknown endpoint"),
    }
}

fn point_fields(body: &Value) -> Result<(u32, f64, f64), String> {
    let monitor = body.req_num("monitor")? as u32;
    let x = body.req_num("x")?;
    let y = body.req_num("y")?;
    Ok((monitor, x, y))
}

/// Resolve the latest stable Tailscale version via PowerShell on Windows.
/// Returns None on other platforms or on fetch failure.
fn fetch_stable_version() -> Option<String> {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "(Invoke-WebRequest -Uri '{}' -UseBasicParsing).Content.Trim()",
                    tailscale::VERSION_URL
                ),
            ])
            .output()
            .ok()?;
        if out.status.success() {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !v.is_empty() && v.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                return Some(v);
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn loggable_detail(req: &Request, path: &str) -> String {
    // Never log auth keys, tokens, or file contents.
    if path == "/tailscale/login" {
        let has_key = req
            .json_body()
            .ok()
            .and_then(|b| b.get("authkey").and_then(Value::as_str).map(|_| ()))
            .is_some();
        return format!("authkey_supplied={}", has_key);
    }
    if path == "/files/upload" {
        return req
            .json_body()
            .ok()
            .and_then(|b| b.req_str("path").ok().map(|p| p.to_string()))
            .map(|p| format!("path={}", p))
            .unwrap_or_default();
    }
    if path == "/cmd" {
        return req
            .json_body()
            .ok()
            .and_then(|b| b.req_str("command").ok().map(|c| c.to_string()))
            .map(|c| format!("command={}", c))
            .unwrap_or_default();
    }
    if !req.query.is_empty() {
        return format!("query={}", req.query);
    }
    req.body.chars().take(300).collect()
}

/// Handle one connection: read one request, route, respond, close.
fn handle_conn(state: Arc<State>, mut stream: TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    // Read headers first.
    let mut header_end: Option<usize> = None;
    let mut total_read = 0usize;
    while header_end.is_none() && total_read < 1_048_576 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                total_read += n;
                if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
                    header_end = Some(pos + 4);
                }
            }
            Err(_) => return,
        }
    }
    let header_end = match header_end {
        Some(p) => p,
        None => return,
    };
    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = header_text.lines();
    let request_line = match lines.next() {
        Some(l) => l.to_string(),
        None => return,
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let content_len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body_bytes = buf[header_end..].to_vec();
    while body_bytes.len() < content_len && total_read < 16_777_216 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                body_bytes.extend_from_slice(&chunk[..n]);
                total_read += n;
            }
            Err(_) => break,
        }
    }
    body_bytes.truncate(content_len);
    let body = String::from_utf8_lossy(&body_bytes).into_owned();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let req = Request {
        method: method.clone(),
        path: path.clone(),
        query,
        headers,
        body,
    };

    state.requests.fetch_add(1, Ordering::Relaxed);
    state.last_request_unix.store(now_unix(), Ordering::Relaxed);

    // Auth: every endpoint requires the bearer token.
    let authed = req
        .header("Authorization")
        .and_then(bearer_token)
        .map(|t| tokens_equal(t, &state.token))
        .unwrap_or(false);
    let resp = if !authed {
        log::append(&state.data_dir, "auth_failed", false, &format!("{} {}", method, path));
        err(401, "missing or invalid bearer token")
    } else {
        let r = route(&state, &req);
        let ok = r.status < 400;
        let detail = loggable_detail(&req, &path);
        log::append(
            &state.data_dir,
            &format!("{} {}", method, path),
            ok,
            &detail,
        );
        r
    };

    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp.status,
        resp.reason,
        resp.content_type,
        resp.body.len()
    )
    .into_bytes();
    out.extend_from_slice(&resp.body);
    let _ = stream.write_all(&out);
    let _ = stream.flush();
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Serve until `state.shutdown` is set. The listener is non-blocking so the
/// shutdown flag is honored promptly; the Flutter app's Quit and the
/// `POST /shutdown` endpoint both set it, and closing the app's process
/// ends the listener regardless.
pub fn serve(listener: TcpListener, state: Arc<State>) -> Result<(), String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("cannot set listener nonblocking: {}", e))?;
    loop {
        if state.shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let st = Arc::clone(&state);
                std::thread::spawn(move || handle_conn(st, stream));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let _ = e;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a+b%20c%2F"), "a b c/");
        assert_eq!(percent_decode("C%3A%5CUsers%5CShawn"), "C:\\Users\\Shawn");
    }

    #[test]
    fn query_param_lookup() {
        assert_eq!(
            query_param("path=C%3A%5Cx.txt&limit=5", "path"),
            Some("C:\\x.txt".to_string())
        );
        assert_eq!(query_param("path=C%3A%5Cx.txt", "limit"), None);
    }
}
