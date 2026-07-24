---
date: 2026-07-24
topic: phone-screenshot-drop
---

# Phone screenshot drop

## Summary

fly serves a phone-facing upload page over the user's tailnet. From a phone the
user picks a target agent from fly's live roster, attaches a screenshot, writes
a caption, and sends. fly stores the image and delivers its path plus the
caption into that agent's pane, so the agent starts on the bug immediately.

---

## Problem Frame

Bugs get caught on the phone, away from the desk. Today the screenshot travels
by email-to-self: send it, come back to the machine, download it, then type a
description of the wanted behavior change into a fly pane. The image and the
description are both needed — the description is doing real work, not just
labeling the picture.

The cost is the round trip. Everything waits until the user is physically back
at the machine, and by then the moment that produced the screenshot has to be
reconstructed from memory.

fly already carries most of the machinery. `src-tauri/src/feed/` is a
bearer-authenticated HTTP surface that publishes a live agent roster over SSE
and writes text into a chosen agent's PTY. It is text-only and bound to
loopback. The gap between it and this brief is an image-accepting route and a
way to reach the listener from a phone.

---

## Key Decisions

**fly serves the phone surface itself.** A share-sheet shortcut posting to a
fixed agent would be a smaller change, but choosing a target from the live
roster needs a real surface, and the shortcut path is iOS-only with no way to
report back that the drop landed. A page fly serves works on any phone and can
show the result.

**The image reaches the agent as a filesystem path.** A pane is a PTY; there is
no side channel into a running agent. fly writes the uploaded image to disk and
delivers the path together with the caption as pane input. Claude Code reads
image files by path, so this is the whole mechanism.

**Reachability comes from tailscale's proxy, not a new bind.** `tailscale serve`
placed in front of the existing loopback listener supplies TLS and tailnet-only
reachability without fly opening a non-loopback socket. fly's stated posture —
the bearer token, not the bind address, is the boundary — carries over
unchanged.

**Upload-only.** The page sends images and reports what happened to them. It
does not read replies, send free text, or answer permission prompts, even
though the feed already serves the data each of those would need.

**Permission behavior is untouched.** `feed.allowPermissionAnswers` stays off by
default. An agent that hits a permission prompt after a drop blocks until the
user is back at the machine.

**No retention policy.** Uploaded images stay on disk until the user removes
them. Screenshots are small and infrequent, and any automatic eviction risks
deleting an image a session still wants to look at.

---

## Key Flows

- F1. Successful drop
  - **Trigger:** User catches a bug on the phone and opens the upload page.
  - **Actors:** A1, A2, A3
  - **Steps:** Page authenticates and lists the currently running agents with
    their working directory and status. User taps a target, attaches a
    screenshot from the camera roll, types a caption, sends. fly stores the
    image, delivers path plus caption to the target pane, and reports success.
  - **Outcome:** The agent is working on the screenshot before the user puts the
    phone down.
  - **Covered by:** R1, R2, R3, R4, R5, R8

- F2. Drop onto a blocked agent
  - **Trigger:** The chosen agent is already stopped at a permission prompt.
  - **Actors:** A1, A2, A3
  - **Steps:** fly refuses the delivery and the page tells the user that agent
    is waiting on an answer at the machine. Nothing is stored or queued.
  - **Outcome:** The user knows the drop did not land and why.
  - **Covered by:** R9, R10

---

## Actors

- A1. The user, on a phone, outside the local network segment but inside the
  tailnet.
- A2. fly, serving the page and owning delivery into a pane.
- A3. The target agent — a live Claude Code session in a fly pane.

---

## Requirements

**Capture and delivery**

- R1. A drop carries an image and a caption. The caption is optional; the image
  is not.
- R2. fly persists the uploaded image to a durable location on this machine
  that the target agent can read. fly never deletes it.
- R3. Delivery into the pane presents the stored image path and the caption
  together as one agent-visible input.

**Phone surface**

- R4. The page lists the agents currently available to receive a drop, with
  enough identifying detail (working directory, status) to tell two sessions
  apart from a phone.
- R5. The page reports the outcome of a send — landed, refused, or failed —
  rather than completing silently.
- R6. The page works in a mobile browser without an installed app, and its
  image picker reaches the camera roll.
- R7. The roster the page shows reflects fly's live state, not a snapshot from
  page load.

**Reachability and authentication**

- R8. The phone reaches fly over the tailnet without fly binding a listener
  beyond loopback.
- R9. Every request from the phone is authenticated by the same bearer token
  that guards the rest of the feed. An unauthenticated request is refused
  without disclosing whether the agent key exists.

**Failure behavior**

- R10. A drop targeting an agent that is stopped at a permission prompt is
  refused rather than queued, and the refusal is distinguishable from an
  authentication failure or an unknown agent.
- R11. A drop targeting an agent that is no longer running is refused with a
  reason the page can show.
- R12. An image larger than the accepted size is refused with a reason, not
  truncated.

---

## Acceptance Examples

- AE1. Blocked agent
  - **Covers R10.**
  - **Given** the target agent is stopped at a permission prompt,
  - **When** the user sends a screenshot to it,
  - **Then** nothing reaches the pane, no image is retained, and the page says
    that agent is waiting on an answer at the machine.

- AE2. Agent exits mid-upload
  - **Covers R11.**
  - **Given** the user selected an agent that has since exited,
  - **When** the upload completes,
  - **Then** the send is refused and the page shows the agent is gone, rather
    than delivering into a replacement pane that reused the slot.

- AE3. Caption omitted
  - **Covers R1, R3.**
  - **Given** the user attaches an image and leaves the caption empty,
  - **When** they send,
  - **Then** the agent receives the image path alone and the drop is not
    refused for the missing caption.

---

## Scope Boundaries

- Reading agent replies, the conversation tail, or pending questions from the
  phone. The feed already serves all of it; the page deliberately does not.
- Sending free text from the phone. The caption rides an image; there is no
  text-only send.
- Answering permission prompts remotely. `feed.allowPermissionAnswers` stays
  off.
- Dispatching a new agent or automation run from a screenshot. Drops go to a
  live pane only.
- A self-hosted collaboration relay (the shape `block/buzz` occupies). It is a
  reference for the shared-room pattern, not a dependency; running Postgres,
  Redis, and object storage to move one screenshot is the wrong trade.
- A file-sync watcher (Taildrop into a watched folder). Cheaper, but a dropped
  file carries no target selection, which is the requirement that drove the
  design.

---

## Dependencies / Assumptions

- The phone and this machine share a tailnet, and `tailscale serve` can front
  the loopback listener. Assumed from the user's setup; not verified in this
  repo.
- Claude Code reads an image when given its filesystem path in a prompt.
- fly's feed is enabled by default and its bearer token is minted on first run,
  so no new secret-provisioning surface is required —
  `src-tauri/src/config/schema.rs` and `src-tauri/src/config/mod.rs`.
- **The token leaves this machine.** Today it never departs loopback. Under this
  brief it lives in a phone browser and travels the tailnet, and it grants PTY
  write access to running agents. This is a real change to the security posture
  documented in `src-tauri/src/feed/server.rs`, accepted here on the grounds
  that the tailnet is personal and single-user.

---

## Outstanding Questions

**Deferred to planning**

- The accepted image size ceiling. The existing text input route caps bodies at
  64 KiB, which no screenshot fits, so the upload path needs its own cap.
- Whether the page is a static asset embedded in the binary or generated per
  request.
- How the page holds the bearer token between visits, and what happens when a
  saved token stops working.
- Whether the drop is refused for an unrelated in-flight interaction beyond the
  permission-prompt case in R11.

---

## Sources

- `src-tauri/src/feed/server.rs` — the existing routes, the loopback bind, the
  64 KiB input cap, and the refusal precedence an upload route should mirror.
- `src-tauri/src/feed/wire.rs` — the roster shape the phone page would render.
- `src-tauri/src/config/schema.rs` — `feed.enabled` defaults on;
  `allowPermissionAnswers` defaults off.
- `docs/plans/2026-07-04-001-feat-agent-state-local-feed-plan.md` — the feed's
  original design and its token-is-the-boundary reasoning.
- `docs/plans/2026-07-11-002-feat-hook-ask-channel-plan.md` — how fly learns an
  agent is stopped at a permission prompt, which R11 depends on.
- https://github.com/block/buzz — the relay-and-shared-room pattern this brief
  deliberately does not adopt.
</content>
</invoke>
