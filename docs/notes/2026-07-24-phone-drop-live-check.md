# Phone screenshot drop — live checks

Record for U0 (premise spike) of
`docs/plans/2026-07-24-001-feat-phone-screenshot-drop-plan.md`. U8's live check
appends to this file when the built surface exists.

**Environment.** Claude Code **2.1.219**. fly stable build, pid 5865, feed on
`127.0.0.1:4939`. Ubuntu 24.04, tailscale CLI present. Checks run 2026-07-25.

---

## U0 check 1 — the unprompted read (KTD4, R3)

**Question.** Does a bypass-permissions agent read an absolute path outside its
cwd with no permission prompt, and does the candidate U3 wording actually make
Claude *open* the file rather than treat the path as a string?

**Method.** `~/projects/inbox/u0-premise-check.png` — a generated PNG rendering
the token `GRAPEFRUIT-7731`. The token is the discriminator: a model that treats
the path as an opaque string cannot produce it. Read from a Claude Code session
running **inside a fly pane** (`FLY_PANE_TOKEN` set, `FLY_SOCKET_PATH` =
`/run/user/1000/fly/hook-5865.sock`), cwd `/home/evan/projects/fly` — i.e.
outside the drop directory. Pane argv confirmed `claude
--dangerously-skip-permissions`.

**Result. Confirmed, both halves.**

- No permission prompt. The read of `/home/evan/projects/inbox/…` succeeded
  directly despite being outside cwd.
- The image content came back legibly (`GRAPEFRUIT-7731`), so the file was
  genuinely opened and decoded.

**Consequence.** KTD4's directory choice stands — the drop directory does not
need to live inside the target pane's cwd, and the deferred `--add-dir` item
stays deferred. The plan's default-permission-mode caveat (prompts once) is
untested here and remains as stated: degraded, not broken.

**Caveat on scope.** This confirms the *mechanism* under the permission posture
fly actually spawns with. It does not exercise delivery through the PTY — the
path arrived via a tool call, not as pasted composer text. That end of it is U8's
live check.

## U0 check 1b — prompt wording

The framing carried into U3 is a directive naming the path as an image to read,
with the caption following on its own line:

```
Read the image at <path> — it's a screenshot I dropped from my phone.

<caption>
```

The caption-less case is line one alone. Exact wording is pinned by U3's tests;
U8 re-validates it against a live pane through the real delivery path, since the
composer-paste route differs from a tool call.

---

## U0 check 2 — SSE through the tailscale proxy (R7, KTD9)

**Question.** Do SSE frames arrive incrementally through `tailscale serve`, or
does the proxy buffer them to completion? If they buffer, the live roster has no
transport and U4/U7 need redesign.

**Method.** Mounted the existing feed port behind Serve:

```
tailscale serve --bg --https=8443 http://127.0.0.1:4939
```

Read `/feed` with a bearer token both directly on loopback (baseline) and
through `https://<machine>.<tailnet>.ts.net:8443`, timestamping each line's
arrival. Reading via the tailnet name from the same machine still traverses
`tailscaled`'s proxy, so a second device is not required to answer the buffering
question.

**Result. Confirmed — no buffering.**

| | frame cadence | emit → arrival |
|---|---|---|
| direct loopback | ~1.5s | ~2–3ms |
| through Serve :8443 | ~1.5s | ~3ms |

Frames arrived one at a time at the feed's own publish cadence, indistinguishable
from the direct read. Nothing accumulated and flushed at close.

**Two things this also settled, incidentally:**

- **The proxy forwards `Authorization`.** The frames authenticated, so the header
  survived. A stripped header would have produced a 401 and zero frames. KTD3's
  `fetch`-with-header design has a working transport.
- **KTD9's port claim is real, not hypothetical.** `tailscale serve status`
  showed 443's root mount already taken (`/ → http://127.0.0.1:3000`) before this
  change. A dedicated `--https=8443` mount was necessary, not merely preferred.

**Mount left in place** — the feature needs it and U8 documents it as the setup
step. Remove with `tailscale serve --https=8443 off`.

---

## Verdict

Both premises hold. No plan changes required; U1 proceeds as written.
