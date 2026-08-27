# AGENTS.md

This repo's agent guide lives in **[`CLAUDE.md`](./CLAUDE.md)** — read it first.
It is the single source of truth for architecture, build/test commands, the
environment gotchas (they will bite), and conventions.

This file exists only so non-Claude tools (Codex, Cursor, Zed, …) that look for
`AGENTS.md` find that guide. Do not duplicate content here — update `CLAUDE.md`
instead, and this pointer stays correct.

Quick map for the impatient:

- **What it is / how to run it, and the environment gotchas** → `CLAUDE.md`
  ("What this is", "Commands", "Environment gotchas").
- **How the pieces fit** → `CLAUDE.md` "Architecture" (the attention pipeline is
  the core feature and spans many files).
- **The design docs the code cross-references by ID** → `docs/plans/`, indexed by
  [`docs/plans/README.md`](./docs/plans/README.md). IDs (KTD/R/U) are **scoped
  per plan** — resolve one against the plan its file belongs to.
- **The security boundary** → `core/src/hooks/` has its own scoped
  `CLAUDE.md`; read it before touching the socket.
- **The shipped shell** → `electron/` (with its own `README.md` for the dev
  loop and packaging); its wire contract is
  [`docs/core-protocol.md`](./docs/core-protocol.md), edited only together
  with `core/src/control/` and `electron/protocol.js`.
- **Human-facing orientation** → [`README.md`](./README.md); update it when a
  change moves the product story.
