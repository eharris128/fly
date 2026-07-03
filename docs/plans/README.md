# Design plans index

`docs/plans/` is the full design for **fly**. The code is cross-referenced back
to these docs by ID in doc comments (see `CLAUDE.md` → "Conventions"). This index
maps each plan to what it added and the primary code that implements it, so you
can go from a `KTD`/`R`/`U` reference in the source to the doc that defines it.

## How the IDs work (read this first)

IDs are **scoped per plan** — every plan restarts its own `KTD<n>` (key technical
decision), `R<n>` (requirement), and `U<n>` (unit) numbering. So `KTD7` in the
foundation plan is a *different* decision from `KTD7` in the automations plan.
**Always resolve an ID against the plan the file belongs to** (use the "Primary
code" column below, or the plan reference in the file's own doc comment). Some
plans continue a running range from an earlier related plan (e.g. the
notification-parity plan starts at `U16`); the per-plan doc is always the
authority.

Plans whose upstream requirements were captured in a brainstorm session link to
`docs/brainstorms/` via an `origin:` header.

All plans below are **implemented in the current tree** (file paths are live);
a plan header's own `status:` field may lag behind the code.

## Plans (chronological)

| Plan (`docs/plans/…`) | What it added | Primary code |
|---|---|---|
| `2026-06-16-001-feat-fly-agent-terminal` | **Foundation** — PTY panes, tabs/splits, the attention pipeline, the authenticated hook socket, the `fly` CLI | `pty/`, `stream/`, `state/{lifecycle,attention,policy,manager}`, `hooks/`, `notify/`, `cli/{notify,hooks}`, `config/`, `session/mod.rs`, `App.svelte`, `lib/{layout,keymap,config,serialize}.ts`, `Terminal.svelte` |
| `2026-06-18-001-feat-hotkey-menu` | Cheat-sheet overlay + close-tab chord | `lib/HotkeyMenu.svelte`, `lib/keymap.ts` |
| `2026-06-19-001-feat-notification-parity-suppression` | Notification panel, badges, and the suppression policy (`U16`+) | `state/policy.rs`, `notify/`, `lib/notifications.ts`, `lib/NotificationPanel.svelte` |
| `2026-06-22-001-feat-tab-workspace-qol` | Workspaces, sidebar tree, control bar, tab rename | `lib/workspaces.ts`, `lib/Sidebar.svelte`, `lib/ControlBar.svelte`, `lib/layout.ts` |
| `2026-06-22-002-feat-agent-dashboard-home` | Agent dashboard (`leader d`): output-activity signal + `/usage` gauges | `state/activity.rs`, `usage/`, `lib/home.ts`, `lib/HomeView.svelte` |
| `2026-06-23-001-feat-resume-agents` | Durable per-leaf resume store + clean-exit marker + replay argv | `session/resume.rs`, `lib/resume.ts` |
| `2026-06-23-002-feat-dashboard-running-state` | `running · N tasks` status for busy-but-quiet agents | `lib/home.ts`, `cwd/` (`/proc` task probe) |
| `2026-06-23-003-fix-resume-session-selection` | Transcript-derived session-id capture (version-skew-proof) | `session/transcript.rs` |
| `2026-06-30-001-feat-dashboard-home-base` | Dashboard "home base" + the attention-triage nudge | `lib/HomeView.svelte`, `lib/home.ts`, `lib/nudge.ts`, `lib/NudgeOverlay.svelte` |
| `2026-07-01-001-feat-reason-typed-attention-triage` | `Reason`-typed attention (`Reason::Alert` end-to-end) + triage badge | `state/attention.rs`, `hooks/`, `lib/notifications.ts` |
| `2026-07-01-002-feat-automations` | Cron-scheduled agent/script runs (`U1`–`U12`) | `automations/`, `cli/automation.rs`, `lib/automations.ts`, `lib/automation-panes.ts` |
| `2026-07-02-001-feat-session-handoff` | Fresh agent handed a stale pane's previous session (`leader f`/`F`) | `session/handoff.rs`, `lib/handoff.ts` |
| `2026-07-03-001-fix-session-pane-attribution` | Trust-ranked (`Poll < Hook < Pick`) SessionStart capture, poll abstention on same-cwd ambiguity, session pick-list, reset/re-pick (`leader g`) | `session/{resume,transcript,handoff}.rs`, `cli/{hooks,notify}.rs`, `hooks/protocol.rs`, `lib/session-picker.ts`, `lib/SessionPicker.svelte` |

## Brainstorms

`docs/brainstorms/` holds the requirements captured before a plan was written —
the upstream `origin:` for the dashboard-home-base, reason-typed-triage, and
session-handoff plans. Start from the plan; drop to the brainstorm for the "why".
