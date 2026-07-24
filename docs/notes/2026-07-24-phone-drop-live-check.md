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

---

# Operator setup (U8)

Three steps, all required. The feature does not work with any of them skipped,
and two of them fail *silently* if you skip them.

## 1. Put `tailscale serve` in front of the feed port

```
tailscale serve --bg --https=8443 http://127.0.0.1:4939
```

- **`--bg` is mandatory.** Without it the mount is a foreground session that
  dies with the terminal, and the phone silently stops being able to connect.
- **A dedicated port, not `--set-path` on 443.** On this machine 443's root
  mount is already taken by another service. Beyond that, a `--set-path` mount
  **strips the prefix** before proxying, which would break any root-absolute URL
  in the page. A root mount on its own port removes that whole class of bug.
- MagicDNS and HTTPS certificates must be enabled for the tailnet.
- Your machine and tailnet names appear in **public Certificate Transparency
  logs** as a consequence of issuing the certificate. That is inherent to
  tailnet HTTPS, not to this feature.
- Remove with `tailscale serve --https=8443 off`.

**Never use `tailscale funnel`.** The same port cannot be Serve and Funnel at
once, last command wins, and a stray funnel invocation converts this private
mount — which writes into your agents' terminals — into a publicly reachable
one. It must not appear in any script or note about this feature.

fly neither configures nor detects the mount. If it is absent the phone simply
cannot connect, with no diagnostic from fly.

## 2. Retrieve the bearer token

No in-app surface displays it. Read it from the config file:

```
python3 -c "import json;print(json.load(open('$HOME/.config/fly/config.json'))['feed']['token'])"
```

(For a dev-flavor build, `~/.config/fly-dev/config.json`.) Paste it into the
page once; it is kept in `localStorage`. WebKit purges script-writable storage
after roughly seven days without interaction, so expect to re-paste
occasionally.

There is currently **no rotate path** anywhere in the codebase. Removing the
device from the tailnet is the real revocation.

## 3. Set `feed.expectedTailnetLogin`

```jsonc
// ~/.config/fly/config.json
{ "feed": { "expectedTailnetLogin": "you@example.com" } }
```

**The identity check is off until this is set**, so following steps 1–2 alone
ships it inert. Be precise about what it buys, because it is less than it looks:
`tailscale serve` stamps the *tailnet user* who owns the device, not the device,
so on a personal single-user tailnet the realistic leak path — the token pasted
into one of your own phones — passes the check unchanged. It defends against a
token used from a device belonging to a *different* tailnet user, and does
nothing about a local process forging the header. The bearer token remains the
boundary.

## Diagnosing a token-entry loop

If the page keeps asking for the token even though the token is right, the
likely cause is a mistyped `expectedTailnetLogin`. **The wire response is
deliberately identical to a bad token** — a bare 401 with no body — so the only
way to tell them apart is fly's stderr:

```
phone drop refused: tailnet login "..." does not match the configured "..."
```

Grep app stderr for `does not match the configured`. If that line is absent, the
token really is wrong.

---

# U8 live check — backend path, from this machine (2026-07-25)

Run against a `pnpm flavor:dev` build (feed on `127.0.0.1:4940`), driving a real
`claude --dangerously-skip-permissions` pane. This covers everything except the
iOS-specific questions: in particular it exercises the `lib.rs` delivery
closure, which is the one piece with no unit coverage.

**Result: the full path works.** A drop to a live agent returned
`200 {"ok":true,"path":…}`, the image landed `0600` in a `0700` directory with
bytes identical to the source, the composed prompt appeared in the composer, and
Claude **read the image with no permission prompt** and reported its contents —
`Read image (789 bytes)` → "The image reads GRAPEFRUIT-773". The triage nudge
fired too, so the delivered drop clears pane attention as intended.

Also confirmed live:

| check | result |
|---|---|
| `GET /` unauthenticated | 200, `text/html`, 22984 bytes |
| shell inertness (R19) | zero occurrences of the token or any agent key |
| `paneId` on the roster (U4) | present — `paneId=4` for the live agent |
| `publishedAt` on the frame (U4) | present and advancing |
| wrong `pane` → guard one | `409 {"error":"paneChanged"}` |
| unknown agent | `404 {"error":"unknownAgent"}` |
| non-image body | `415 {"error":"badFormat"}` |
| no token | bare `401`, empty body |
| directory after four refusals | **empty** — no residue, no temp files |

## Finding: a drop during agent startup is accepted but silently discarded

The first attempt was sent while the pane's `claude` was still initializing. It
returned **200** and the image was stored, but nothing ever appeared in the
composer — Claude Code discards input that arrives before its composer is
ready.

Neither guard can catch this and neither is wrong to miss it: the pane id
matches, and the foreground process genuinely *is* an agent. The roster's status
was `working`, which is the same value a mid-turn agent shows, and KTD5
deliberately does not block on `working` because Claude queues mid-turn input.
The difference between "queues it" and "discards it" is not visible in anything
fly currently observes.

Impact is small — the window is the few seconds between spawn and a ready
composer, and a user picking an agent from a roster is unlikely to hit it — but
the failure is **silent and reports success**, which is the shape this plan
otherwise works hard to avoid. Not fixed here; recorded as the honest boundary
of the 200. A follow-up would need a readiness signal distinct from `working`.

---

## Still to verify live (needs a phone)

The backend path above is verified. These three are iOS-specific and remain
**not done**:

1. **iOS delivers a camera-roll screenshot as PNG, not HEIC.** The page warns on
   a HEIC selection rather than blocking it, so the failure mode is a warning
   and a possibly-unreadable image, not a broken flow. Worth confirming because
   it determines whether the warning is rare or constant.
2. **The whole path end to end from a phone** — pick, caption, send, and the
   prompt appearing in the pane.
3. **A refusal renders legibly on a small screen outdoors.** Each refusal code
   has its own message; the question is whether they read clearly at arm's
   length in daylight.
