# Understudy — security notes

## Read this first

Understudy grants **full user-level control** of the PC it runs on: screen
capture, mouse/keyboard injection, app launching and killing, file transfer,
and arbitrary command execution as the engine's user. That is its purpose —
the owner asked for an AI operator that can do any possible thing on his own
PC at his direction. It is also why every protection below is load-bearing,
not decorative.

## For your own machines only

Understudy is for operating **your own machines with your own permission**.
Never install it on, point it at, or use it against someone else's PC. The
owner's permission is the only authorization this tool recognizes, and it
does not transfer to any other person or machine.

## The protections

1. **Tailscale-only listening.** The engine binds to the machine's Tailscale
   IPv4 address (or loopback when Tailscale is down). It refuses `0.0.0.0`
   and any bind address that is not loopback or the machine's own Tailscale
   IP. The API is reachable only from the owner's tailnet, never from the
   open internet.
2. **Bearer-token auth on every endpoint.** No unauthenticated endpoints
   exist — not even status. The token lives only on the controlled PC
   (`%APPDATA%\Understudy\auth.token`) and in the paired controller.
3. **Complete local action log.** Every request, including rejected ones, is
   appended to `%APPDATA%\Understudy\actions.log` on the controlled PC and
   viewable in the app. Tokens, Tailscale auth keys, and file contents are
   never written to the log.
4. **Kill switch.** Tray → Quit (or `POST /shutdown`, or ending the engine
   process) stops the engine and closes all listeners immediately. There is
   no background mode that survives quitting, and no auto-restart.
5. **Scoped file access.** File transfer is confined to allowed roots
   (default: the user's profile directory). Traversal escapes are rejected.
6. **No baked-in secrets.** The repo contains no tokens, auth keys, or
   credentials of any kind. The engine generates its token at runtime, on
   the controlled PC, on first run.

## If the token leaks

1. Quit the app (kill switch) to close the listener immediately.
2. Delete `%APPDATA%\Understudy\auth.token`.
3. Relaunch: a fresh token is generated and printed once. Re-pair the
   controller with the new token; the old one stops working.
4. Review `%APPDATA%\Understudy\actions.log` for anything unrecognized.

## What this tool does not protect against

- **Anyone with the token and tailnet access can operate the PC.** Guard
  the token like a password, and guard tailnet membership the same way.
  Tailscale's own device approval and key expiry are part of this posture.
- **Malware or another user on the same PC.** The engine runs as the
  owner's user; anything else running as that user can read the token file
  and the log. That is inherent to a same-user agent — it is not a
  sandbox, and it must not be presented as one.
- **The owner being tricked into approving an action.** The action log
  records what happened; it cannot judge whether the owner meant it. The
  controller side (the AI operator) is responsible for acting only at the
  owner's direction.

## Reporting

This is a personal, open-source project (GPL-3.0). Security issues with the
code itself can be reported through the GitHub repo's issue tracker.
