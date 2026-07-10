---
name: fly-monitor-handoff
description: Hand a long-running experiment off to a fly monitor automation — a parked, sparse re-check that delivers one PASS/FAIL verdict and retires — so this Claude session can close instead of blocking on a Monitor call for hours. Use when an experiment (training run, long build, soak test, data job) will finish far in the future and the user wants to stop babysitting it.
---

# fly monitor handoff

<!-- monitor-handoff U8 (R10) of
     docs/plans/2026-07-10-001-feat-monitor-handoff-plan.md.
     Install-by-copy: copy this directory to ~/.claude/skills/fly-monitor-handoff/
     (per-user) or <project>/.claude/skills/fly-monitor-handoff/ (per-project).
     No installer machinery — deliberate v1 roughness. -->

You are a Claude session running inside a **fly** pane, watching a long
experiment. Instead of parking this session on a blocking wait, you will
register a **monitor**: a cron-scheduled check that a smaller model runs
sparsely, which reports one machine-readable verdict and then retires. On
FAIL, fly stores a failure bundle and offers the user a one-click pickup that
spawns a fresh recovery session pointed at your transcript — so capture what
that future session needs **now**.

Registration only works from inside a fly pane (the `fly automation` CLI
talks to the app over the pane's authenticated socket). If `fly automation
list` errors with a socket/token message, say so and stop — do not improvise.

## Steps

1. **Summarize the experiment.** In one short paragraph: what is running,
   where (cwd, log files, process names, dashboards), and the expected finish
   window. You will embed this in the check prompt — the checking model knows
   nothing you don't write down.

2. **Write a self-contained check prompt.** It must include:
   - How to tell whether the experiment is **finished**, **still running**,
     or **failed** — exact commands/files to inspect (e.g. `tail -n 50
     train.log`, `ls checkpoints/`, a pid to probe). Concrete, copy-runnable.
   - What success and failure look like, concretely.
   - The instruction: **if the experiment is not finished, say it is still
     running and stop — emit no verdict block.**
   - The verdict-block contract, verbatim (see below).

3. **Choose the schedule.** Sparse — the check costs a model run each tick:
   - `--cron`: a **recurring** expression (never a one-shot); every 4–6 hours
     is typical (`0 */6 * * *`). fly clamps anything under 5 minutes.
   - `--not-before`: the earliest plausible finish time, so no checks burn
     before the experiment could possibly be done. RFC3339
     (`2026-07-12T09:00:00Z`) or local `"YYYY-MM-DD HH:MM"`. A past time is
     fine (it becomes a no-op floor).

4. **Check for duplicates.** Run `fly automation list` and look for an
   existing monitor with the name you are about to use. If one exists,
   pick a distinct name (this is the v1 duplicate-registration guard).

5. **Register.** From this pane:

   ```bash
   fly automation create \
     --name "<short monitor name>" \
     --cron "0 */6 * * *" \
     --monitor \
     --not-before "<earliest plausible finish>" \
     --prompt "<the check prompt from step 2>"
   ```

   `--model` / `--effort` are optional — monitors default to **sonnet at
   xhigh**. The cwd defaults to this pane's cwd; the checks run there.
   Monitors default `--retry-on-interrupt` on (an app-restart-interrupted
   check re-runs once).

6. **Confirm the output.**
   - **Created** (an id is printed): registration captured this session's
     pickup pointers and fly will close this tab automatically — tell the
     user the monitor is registered and that **fly must be left running for
     checks to fire; missed ticks are not caught up**, then stop. Do not
     start new work: the tab is about to close.
   - **Refused** with "could not capture pickup pointers": this pane has no
     qualified session record (fly needs your transcript for the pickup
     path). Report the error verbatim and leave the tab open — do not retry
     blindly and do not fall back to a non-monitor automation.

## The verdict-block contract

Embed the following in the check prompt **verbatim**. It is the parser's
contract — the authoritative copy lives in
`src-tauri/src/automations/verdict.rs::VERDICT_BLOCK_SPEC`, and this skill
quotes it exactly; edit both together, nowhere else. Anything that is not
exactly one well-formed block is treated as "not done yet" (the check stays
silent and the monitor keeps waiting), so precision here is what makes the
monitor work at all.

````text
End your final message with exactly one fenced verdict block:

```verdict
PASS
<free-text note — one or more lines>
```

The first line inside the fence must be exactly PASS or FAIL — uppercase,
alone on its line. Every line after it up to the closing fence is the
free-text result note. Emit exactly one such block. If the experiment is not
finished yet, emit NO verdict block at all — say it is still running and stop.
````

## Caveats worth telling the user

- **fly must be running** for checks to fire; there is no catch-up for
  missed ticks. The not-before floor composes with the recurring cron, so a
  floor that passes while fly is closed still yields a check at the next
  tick after launch.
- **Busy directories blur the check's output.** fly attributes a check's
  final message by cwd; a second fresh Claude session in the same cwd during
  a check makes that tick's verdict unreadable. For noisy directories,
  recommend a dedicated cwd for the experiment. Three consecutive unreadable
  checks ring a "monitor broken" alert instead of failing silently.
- On PASS the user gets a notification with your note. On FAIL they also get
  a durable failure bundle (verdict, evidence, pointers back to this
  session) and a one-click pickup on the fly dashboard.
