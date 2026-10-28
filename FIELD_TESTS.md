# Field tests

Real operator runs against Understudy v0.1.4 on a Windows PC (two monitors:
2560×1440 primary + portrait secondary), 2026-10-28. One sample each — these
are field notes, not benchmarks. No screenshots, tokens, or addresses are
included here; timings and method only.

## Test 1: open the Start menu (vision path)

Two different agents, same task, same API.

**Agent A — Freebuff (MiMo v2.6 Flash Slow), running on the PC itself.**

- Result: Start menu opened.
- Time: ~10 minutes.
- How: fetched the screenshot, wrote a script to downscale it, scanned pixel
  columns for the blue Windows logo, calculated bounds, sent move + click,
  verified with a second screenshot.
- Why so slow: a slow model variant running a vision loop, a hand-written
  pixel scan for a button instead of direct grounding, and ~14 MB raw
  screenshots per round trip.

**Agent B — Muse, operating remotely over Tailscale.**

- Result: Start menu opened, verified by follow-up screenshot.
- Time: ~2 minutes from go-ahead; ~1 minute for the control loop itself
  (status → monitors → screenshot → ground → click → verify screenshot).
  The other minute was a proxy misconfiguration on the controller side
  (the tunnel pointed at the wrong local proxy), fixed mid-session.
- How: screenshot, visual grounding to locate the Start button
  (cross-checked on a zoomed taskbar crop, because the first grounding was
  ~40 px off center), one click, verification screenshot. No scripts, no
  pixel scanning.
- Note: the two full screenshots dominated the loop time (~7 s each to
  download at ~14.7 MB).

## Test 2: close the Start menu (keys path)

Same session, menu already open. One call: `POST /input/keys ["win"]`.
Menu closed, verified by follow-up screenshot.

- Key call wall-clock: 10.1 s on this sample, but a status call immediately
  after took 0.38 s, so the 10 s looks like approval/wait time on that call
  rather than engine slowness. One sample cannot separate the two.
- Structural point: one call, zero screenshots, zero grounding.

## Verdict

It works, and the grounded remote run was roughly 5–10× faster than the
pixel-scanning run — but it is still not practical for everyday use.
Pressing a single button should not need a multi-step vision loop. What
would change that, in order:

1. **Keys and commands first.** For known targets (Start, shortcuts,
   launching apps), deterministic input beats vision on every axis: one
   call, no downloads.
2. **Compressed capture.** Vision rounds are dominated by ~14 MB raw frames;
   DXGI + JPEG (or smaller crops) would cut each round trip to well under
   a second.
3. **Vision reserved for genuinely unknown UI** — the case where the
   operator does not already know where something is.

Setup cost (tailnet join, approvals, token handoff) is one-time per
controller and is excluded from the times above; it is the price of the
first session, not of each action.
