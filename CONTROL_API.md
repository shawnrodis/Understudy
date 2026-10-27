# Understudy Control API — contract (v0.1.0)

This document is the complete contract for operating an Understudy-controlled
PC. It is written so that a future Muse session can operate the PC through
this API **without guessing and without touching the Flutter UI** — the API
is the primary interface. The Flutter app only monitors, logs, and stops the
engine; it is never required to perform any action below.

Base URL: `http://<tailscale-ip>:18769` (port overridable with the engine's
`--port` flag). When Tailscale is down, the engine listens on `127.0.0.1`
instead; it never listens on `0.0.0.0` or a public interface.

## Authentication

Every endpoint requires this header — there are no unauthenticated endpoints:

```
Authorization: Bearer <token>
```

- The token is generated on the controlled PC on first engine run and stored
  at `%APPDATA%\Understudy\auth.token` (Linux dev path: `~/.understudy/`).
- It is printed once to the engine's stdout on first run. The owner copies it
  into the controller once. The Flutter app's Status screen can reveal it
  again later (same user, same machine).
- Missing or wrong token → `401 {"ok":false,"error":"missing or invalid bearer token"}`
  and the attempt is appended to the action log.
- Never put the token in a URL, a repo, a screenshot, or a log message.

## Response envelope

- Success: `{"ok":true, ...endpoint fields}` with HTTP 200.
- Failure: `{"ok":false,"error":"<message>"}` with HTTP 400 (bad request),
  401 (auth), 404 (unknown endpoint), 500 (operation failed), or 501 (the
  capability requires Windows and this host is not Windows).
- `GET /monitors/{id}/screenshot` returns raw `image/png` bytes on success
  (no JSON envelope); on failure it returns the JSON error envelope.
- `GET /files/download` returns raw bytes (`application/octet-stream`) on
  success; on failure the JSON error envelope.

## Coordinate system (read this before sending any pointer action)

The API speaks **per-monitor logical (DIP) coordinates**:

- Every pointer endpoint takes `monitor` (an integer id from `GET /monitors`)
  plus `x`, `y` measured **from that monitor's own top-left corner, in that
  monitor's scaled (logical) space**.
- The engine converts: logical × monitor scale → physical pixels on that
  monitor → plus the monitor's virtual-desktop origin → the 0..=65535
  absolute range `SendInput` expects. Points outside the monitor are
  rejected with HTTP 400; they are never clamped onto another monitor.
- A monitor's logical size is `width / scale` by `height / scale`. Example
  desk (a landscape primary next to a portrait panel at 150%):
  - Monitor 0: origin (0, 0), 2560×1440 physical, scale 1.0 → logical 2560×1440.
  - Monitor 1: origin (2560, 0), 2160×3840 physical, scale 1.5 → logical 1440×2560.
  - So logical (720, 1280) on monitor 1 is physical (1080, 1920) on that
    panel, i.e. virtual-desktop (3640, 1920).
- **Workflow for acting on a screen:** `GET /monitors` → pick the monitor →
  `GET /monitors/{id}/screenshot` → read positions off the screenshot in that
  monitor's logical space → send pointer actions with that monitor id and
  those coordinates. Never mix coordinates from one monitor's screenshot
  into another monitor's actions.

## Endpoints

### GET /status

Health + session state.

Response:

```json
{
  "ok": true,
  "version": "0.1.0",
  "bind": "tailscale-or-loopback",
  "monitors": 2,
  "uptime_s": 3600,
  "requests": 128,
  "session_active": true,
  "last_request_unix": 1761580000
}
```

`session_active` is true when any authenticated request arrived in the last
30 seconds — the Flutter app's connected indicator reads this field.

### GET /monitors

```json
{
  "ok": true,
  "monitors": [
    {"id": 0, "x": 0, "y": 0, "width": 2560, "height": 1440, "scale": 1.0, "is_primary": true},
    {"id": 1, "x": 2560, "y": 0, "width": 2160, "height": 3840, "scale": 1.5, "is_primary": false}
  ]
}
```

`x`/`y` are the virtual-desktop origin in physical pixels; `width`/`height`
are physical pixels; `scale` is physical pixels per logical pixel.

### GET /monitors/{id}/screenshot

Returns a PNG of exactly that monitor (physical-pixel size). Example:
`GET /monitors/1/screenshot` → PNG bytes, 2160×3840 for the desk above.

### POST /input/move

```json
{"monitor": 1, "x": 720, "y": 1280}
```

Moves the mouse to that point. Response: `{"ok":true,"moved":{"monitor":1,"x":720,"y":1280}}`.

### POST /input/click

```json
{"monitor": 1, "x": 720, "y": 1280, "button": "left", "double": false}
```

- `button`: `"left"` (default), `"right"`, or `"middle"`.
- `double`: optional, default `false`. When true, two clicks are sent.
- The mouse is moved to the point first, then the click is sent.

### POST /input/scroll

```json
{"monitor": 1, "x": 720, "y": 1280, "delta": 3}
```

`delta` is wheel clicks; positive scrolls up, negative scrolls down.

### POST /input/type

```json
{"text": "Hello, world"}
```

Types text via Unicode keystrokes (layout-independent). No monitor or
position is involved; the text goes to wherever focus is. Move + click
first if focus matters.

### POST /input/keys

```json
{"keys": ["ctrl", "l"]}
```

Presses a chord: all keys down in order, then up in reverse order.
Recognized names: `ctrl`, `alt`, `shift`, `win`, `enter`, `tab`, `esc`,
`space`, `backspace`, `delete`, `up`, `down`, `left`, `right`, `home`,
`end`, `pageup`, `pagedown`, `f1`–`f12`, and any single alphanumeric
character. Unknown names → HTTP 400.

### GET /apps

```json
{"ok": true, "processes": [{"pid": 1234, "name": "notepad.exe"}]}
```

### POST /apps/launch

```json
{"path": "C:\\Windows\\System32\\notepad.exe", "args": []}
```

`args` is optional. Response: `{"ok":true,"pid":1234}`. The path is executed
as the engine's user; quote handling for arguments containing spaces is done
by the engine.

### POST /apps/kill

```json
{"pid": 1234}
```

Terminates that process. Response: `{"ok":true,"killed":true}`.

### GET /files/download?path=<path>

Returns the file's raw bytes. `path` is URL-encoded. The path must resolve
inside an allowed file root (default: the user's profile directory);
escapes are rejected with HTTP 400.

### POST /files/upload

```json
{"path": "Documents/report.txt", "base64": "aGVsbG8="}
```

- `path` may be relative (anchored at the first allowed root) or absolute
  (must still be inside an allowed root). Parent directories are created.
- `base64` is standard base64 of the file contents.
- Response: `{"ok":true,"bytes":5}`.

### POST /cmd

```json
{"command": "dir", "args": [], "cwd": "C:\\Users\\Shawn"}
```

Runs the command as the engine's user (via `cmd /C` on Windows) and returns
stdout, stderr, and the exit code:

```json
{"ok": true, "stdout": "...", "stderr": "", "exit_code": 0}
```

`args` and `cwd` are optional. This is full user-level command execution by
design — see SECURITY.md. The full command text is action-logged.

### GET /log?limit=100

Returns the most recent action-log entries (newest last, default 100, max
1000):

```json
{"ok": true, "entries": [{"ts": 1761580000, "action": "POST /input/click", "ok": true, "detail": "..."}]}
```

Auth keys are never in `detail` (login entries record only whether a key
was supplied); tokens and file contents are never logged.

### GET /tailscale/status

```json
{"ok": true, "installed": true, "logged_in": true, "ip": "100.64.0.5"}
```

`ip` is `null` when Tailscale is installed but not connected.

### POST /tailscale/install

```json
{"version": "1.88.4"}
```

Downloads the official stable MSI for that version and installs it with
`msiexec` (one Windows admin prompt). `version` may be omitted or empty:
the engine then resolves the latest stable version from
`https://pkgs.tailscale.com/stable/VERSION` (Windows only). On non-Windows
hosts this endpoint returns 501 — install Tailscale with the host's package
manager instead.

### POST /tailscale/login

```json
{"authkey": "tskey-auth-..."}
```

- With `authkey`: headless login via `tailscale up --authkey`.
- Without `authkey` (empty body `{}`): starts `tailscale up`, which opens
  the browser SSO flow for the owner to complete once.
- The key is passed only to the local `tailscale` binary and is never
  logged. Response: `{"ok":true,"login_started":true}`.

### POST /shutdown

```json
{}
```

Stops the engine: the listener closes and the process exits. This is the
API-side kill switch. The Flutter app's Quit button calls this and then
exits itself; ending the engine process directly has the same effect.
Response: `{"ok":true,"shutting_down":true}` (best effort — the connection
may close as the server stops).

## Error behavior

- Unknown monitor id, or a point outside the named monitor → 400, and no
  input is sent. Re-read `GET /monitors` and the matching screenshot.
- A screenshot on a non-Windows host → 501 (capture is a Windows
  capability; the endpoint contract is unchanged).
- Malformed JSON or missing required fields → 400 naming the field.
- Any 401 response means the token is missing, wrong, or stale (a new token
  is generated only if `auth.token` was deleted — in that case the owner
  must re-pair by copying the new first-run token).

## Operating notes for the AI operator

1. Always start a session with `GET /status` and `GET /monitors`.
2. Screenshot before acting: never click blind on coordinates from memory.
3. Prefer `/cmd` and `/apps/*` for deterministic work; reserve injected
   input for GUI-only steps.
4. After destructive actions (kill, command, upload), verify with a read
   (`GET /apps`, a screenshot, or a follow-up command) before reporting
   success.
5. When finished, leave the engine running (the owner controls its
   lifetime from the tray app) unless the owner asked for shutdown.
