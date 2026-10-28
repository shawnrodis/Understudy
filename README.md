![Understudy icon](assets/icon.png)

# Understudy

**Understudy exists so that Muse AI, acting at the owner's direction, can operate the owner's Windows PC across all its monitors.**

An understudy steps in and performs your role when you can't be there. That is what this does: you ask Muse to do something on your PC, and Muse does it — through a documented API, not by a human watching a screen and clicking. Screenshots in, reasoned actions out.

## What this is — and what it is not

- **It is an AI-operator agent for your own PC.** The control API is the primary interface. Every capability is fully operable headlessly through the API: per-monitor screenshots, mouse/keyboard injection, app and process control, file transfer, and command execution.
- **It is not a remote-desktop viewer.** There is no screen-sharing viewer and no manual driving UI in v1. The Flutter app is a monitor, not a controller: it shows connection status, the connected indicator, the action log, and the kill switch, and it walks Tailscale setup. If manual control is ever wanted, that is a separate later decision.
- **It is for your own machines, with your own permission.** See [SECURITY.md](SECURITY.md). Never point it at someone else's PC.

## The security model, stated plainly

This tool grants **full user-level control** of the PC it runs on. That is the point — and it is why the guardrails are not optional:

- **Tailscale-only listening.** The engine binds to the machine's Tailscale IPv4 address (or loopback when Tailscale is down). It never binds `0.0.0.0` and never listens on a public interface. A `--bind` override is accepted only for loopback or the machine's own Tailscale IP; anything else is refused.
- **Key auth.** Every API endpoint requires `Authorization: Bearer <token>`. The token is generated on first run, stored only on the controlled PC (`%APPDATA%\Understudy\auth.token`), and printed once so the owner can pair a controller. There are no unauthenticated endpoints. If a token ever leaks, the Status screen's **Rotate pairing token** button generates a fresh one and kills the old one immediately (`POST /auth/rotate`); the new token is re-read from `auth.token`, never returned over the API.
- **Single instance.** The app enforces exactly one running copy: launching it again raises the existing window and exits — no duplicate windows, engines, or taskbar icons. The engine itself is fully windowless (no console window, no taskbar button of its own). If stale duplicate icons from an older version are still showing, quit each old copy once (tray → Quit); no new duplicates can appear after that.
- **Full action log.** Every request — including rejected ones — is appended to a local JSON-lines log (`%APPDATA%\Understudy\actions.log`), viewable in the app. Tokens, auth keys, and file contents are never logged.
- **Kill switch.** Closing the app (tray → Quit) stops the engine and closes all listeners immediately. `POST /shutdown` does the same from the API side. Ending the engine process ends everything regardless.
- **Scoped file access.** File transfer is restricted to allowed roots (default: the user's profile directory). Paths escaping those roots are rejected.

## Setup

1. Run the installer once (`Understudy_Setup_<version>.exe` from the GitHub release).
2. On first launch, if Tailscale is missing, the app downloads the official Tailscale MSI and installs it hidden — **one Windows admin (UAC) prompt will appear; approve it**. The install runs with no console window, and success is only reported after the install is verified (`tailscale.exe` present and answering); any failure — download, declined prompt, bad installer — is shown right on the Tailscale tab with its reason. Then sign in once: either tap through the browser sign-in, or paste an auth key from your Tailscale admin console. After that the connection is headless. (If automatic version detection ever fails, type a version number explicitly in the version field.)
3. The engine stores its first-run pairing token in `%APPDATA%\Understudy\auth.token`. Copy it into the controller that will operate this PC (for Muse, hand it over in chat once). The app's Status screen shows the base URL too, and **Copy connection details for the AI** puts one paste-ready block on the clipboard — base URL, bearer token, the header to send, reachability, and the session workflow — so a controller never has to go hunting in `connection.json`. The Status screen can reveal the token again later, and rotate it if it ever leaks.
4. Operate the PC through the API: see [CONTROL_API.md](CONTROL_API.md) for the complete contract.

**Tray:** closing the window tucks the app into the system tray — click the tray icon (left or right) for **Open** and **Quit** (the kill switch, which stops the engine). The Status tab has a Tray section showing the setup diagnostics if the menu ever looks wrong.

**Local mode (no Tailscale needed):** the app's **Local AI** tab runs a
vision model ON this PC (Ollama) that operates the engine over
loopback. Tap **Install Ollama**, then **Pull model**
(`qwen3-vl:8b` by default — fits a 12 GB card), type a task, and press
**Start**; the log shows every step live, and **Stop** kills the loop
immediately. Every action goes through the engine, so it lands in the
action log like any controller's. The loop, CLI flags, and action JSON
contract are documented in [CONTROL_API.md](CONTROL_API.md) ("Local
operator"). Install and pull buttons never silently do nothing: while
one runs, its message card shows a live spinner, and every outcome —
installed, already installed, installer window opened, or the failure
reason — is reported on the tab.

**SmartScreen note:** the installer is unsigned, so Windows SmartScreen will prompt on first run. That is expected — the owner's other unsigned apps behave the same way, and he accepts that.

## Updates

The tray app checks the `shawnrodis/Understudy` GitHub Releases API on
launch, at most once per 24h (throttled by a stamp file recording the
last completed check in the data dir), silently: no update shows
nothing, and a failed check does not consume the day. The Status
tab also has **Check for updates** for a manual check, which always
reports its result. When a newer release exists, the Status tab shows
its version and release notes with a **Download & install** button.
Nothing is ever installed silently: tapping it downloads the new
installer, then quits this app (engine included) and launches the
installer elevated — one UAC prompt, expected — so it can replace the
running files. If the UAC prompt is declined, just relaunch the app and
try again. If the app quits but no installer appears, the delayed
launch logs its outcome to `%TEMP%\Understudy_update_launch.log`
(waiting → app exited / wait timed out → launched / FAILED). And a
failed launch never means a trip to GitHub: once an installer is
downloaded, the Status tab keeps an **Open installer** button for the
file already in `%TEMP%`, including after a relaunch.

## Architecture

```
Flutter tray app (app/)            Rust engine (engine/)
  - status, connected indicator      - control API (HTTP/JSON, Tailscale-only)
  - action log view                  - per-monitor capture (GDI BitBlt, DPI-aware)
  - kill switch (Quit)               - SendInput mouse/keyboard injection
  - Tailscale setup flow             - app/process control, file transfer, commands
  - spawns + watches the engine      - bearer-token auth, local action log
```

- **Coordinates are per-monitor and DPI-aware.** The API speaks per-monitor logical (DIP) coordinates measured from that monitor's own top-left corner. The engine converts through each monitor's scale factor and origin to virtual-desktop physical pixels, then to the absolute range `SendInput` expects. The desktop is never treated as one stretched virtual screen — this matters on a mixed-DPI desk (e.g. a 2560×1440 landscape panel next to a 2160×3840 portrait panel at a different scale).
- **Capture in v1** uses Win32 GDI `BitBlt` per monitor (real screenshots, DPI-aware). DXGI Desktop Duplication is the planned upgrade for high-rate frame streaming; the capture function already returns raw RGBA, so that backend slots in behind the same signature.
- **No cloud dependency beyond Tailscale itself** (the transport the owner chose). No paywall, no sign-in wall, no account system of its own.

## Building

The Linux VM cannot build Flutter Windows targets, so Windows builds run in GitHub Actions (see `.github/workflows/`):

- **CI** (`ci.yml`): engine unit tests on Linux, engine type-check for the Windows target, Flutter analyze.
- **Release** (`release-windows.yml`): on a version tag (or manual dispatch), builds the Rust engine and the Flutter app on `windows-latest`, stages them, builds the NSIS installer, uploads it as a workflow artifact, and attaches it to the GitHub release.

To cut a release: `git tag v0.1.7 && git push origin v0.1.7` — the workflow builds the installer and attaches it to the release.

To build the engine locally (any platform with Rust):

```
cargo test --manifest-path engine/Cargo.toml
```

The engine is zero-dependency Rust (standard library only), so `cargo test` needs no network.

## Repository layout

```
engine/                 Rust control engine (zero dependencies)
  src/monitors.rs       per-monitor enumeration + DPI-aware coordinate math
  src/input.rs          SendInput mouse/keyboard injection
  src/capture.rs        per-monitor screenshots + PNG encoding
  src/process.rs        app/process control
  src/files.rs          scoped file transfer + base64
  src/cmd.rs            command execution
  src/tailscale.rs      detection, auto-install, login
  src/api.rs            HTTP control API server
  src/auth.rs           bearer token generation/storage
  src/log.rs            local action log
app/                    Flutter tray app (status, log, kill switch, setup)
installer/              NSIS installer script
assets/                 app icon (PNG + multi-size ICO + source art)
CONTROL_API.md          the complete control API contract
SECURITY.md             security notes and token-handling rules
```

## Field tests

Real operator runs, with timings and an honest verdict, live in
[FIELD_TESTS.md](FIELD_TESTS.md).

## License

GPL-3.0. See [LICENSE](LICENSE).
