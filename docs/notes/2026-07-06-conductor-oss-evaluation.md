# Conductor (conductor-oss) — reference, not a dependency

Created: 2026-07-06

Evaluation prompted by coming across https://github.com/conductor-oss/conductor
and asking whether it holds lessons for how fly models agent interactions, or is
something to adopt into the workstream.

## Decision

**Derive patterns from it; do not take it as a dependency.** Revisit only if the
`automations` subsystem ever becomes multi-step (chained/branching runs).

## What Conductor is

A JVM (Java 21+) distributed workflow-orchestration **server**. Runs as a service
with external datastores (Redis / Postgres / Cassandra + Elasticsearch/OpenSearch
for search) and usually a message broker (Kafka / NATS / SQS); language-agnostic
workers poll it for tasks. Workflows are declarative JSON DAGs (`SWITCH`,
`DO_WHILE`, `FORK_JOIN`, sub-workflows). It solves durable, long-lived,
high-scale orchestration with orchestration state deliberately separated from
business logic — "billions of executions at Netflix/Tesla/JPM".

## Why not a dependency

Category mismatch with fly's identity. fly is a **single-binary local desktop
app** for one user — no server, no cluster, no external datastore, no worker
fleet. Adopting Conductor would mean "install a JVM + Redis + Elasticsearch to
run your terminal", inverting the standalone-binary premise. Conductor's weight
class (distributed, server-hosted, datastore-backed, scale-first) is the opposite
of fly's (local, single-binary, one-user).

## Lessons worth deriving (patterns fly already reaches for)

- **Orchestration separated from business logic.** Conductor's headline pitch is
  something fly already does at the right scale — the pure state machines
  (`state/lifecycle.rs`, `state/attention.rs`, `state/activity.rs`) with injected
  seams are orchestration-separated-from-logic expressed as tested Rust rather
  than a JSON DAG engine.
- **Correlation-ID external completion (the sharp one).** Conductor's
  wait-for-external-signal / human-in-the-loop task, completed via a correlation
  id, is structurally identical to the `feed-pending-question` design: an agent
  pauses on AskUserQuestion and the answer is completed via `askedAt` →
  `ifAskedAt` + the answered latch (idempotent, correlation-keyed). We arrived at
  this independently; Conductor is battle-tested confirmation the shape is right
  — an agent-waiting-on-input is a first-class **waiting state with a correlation
  id**, not an ad-hoc read+write.
- **Durable task lifecycle.** The automations run-row state machine (claim →
  persist-before-run → close, R2) is a miniature of Conductor's durable task
  lifecycle. Same principle, right-sized.

## Held in reserve (do not act on now)

- **Replay/retry as a first-class affordance.** Conductor treats "rerun this
  task / restart from here" as core. Automations already have retry-on-interrupt;
  a "re-run this run / resume from failure" affordance is a Conductor-validated
  direction *if* automations grow richer.
- **DAG-as-data.** Today each automation is a single agent/script run — one task,
  not a workflow — so branching/fork/loop machinery is pure overhead. If
  automations ever become multi-step (run agent → branch on output → run
  another), model that as durable data-driven workflow state, not imperative glue.
  This is the one future trigger where Conductor-thinking earns its keep.

## Bottom line

Reference architecture, not a build target. It confirms fly's instincts (state
separated from logic, correlation-ID external completion) and supplies mature
vocabulary for the agent-interaction work, but nothing here changes the
`feed-pending-question` plan — if anything it reinforces the `askedAt`/`ifAskedAt`
correlation design.
