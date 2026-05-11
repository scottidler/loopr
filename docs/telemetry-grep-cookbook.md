# Telemetry grep cookbook

Common questions an operator asks of `events.log` and the grep that
answers them. The instrumentation behind each pattern is the
comprehensive-telemetry sweep:
[2026-05-09-comprehensive-telemetry.md](design/2026-05-09-comprehensive-telemetry.md).

The events file lives at:

```
~/.local/share/loopr/sessions/<session-id>/targets/<target-slug>/runs/<process-id>/events.log
```

It is line-oriented JSONL; pipe through `jq` to project specific fields.

## Tools

**Which tool calls did the implementer make on `wk-XXX`?**

```
grep '"name":"tool\.' events.log \
  | jq -r 'select(.spans[]? | (.work_id // "") == "wk-XXX")
           | "\(.span.name) \(.span.path // .span.pattern // .span.command_chars // "-")"'
```

The `tool.<name>` span carries `tool_name`, `lane`, `path`/`pattern`/`command_chars`,
`working_dir`. Each tool call's success path emits a `"tool: ok"` debug event
carrying `elapsed_ms` and a per-tool size metric (`bytes` for
read/write/edit/bash; `match_count` for glob/grep).

**Which lane was a bash command routed to, and how long did it take?**

```
grep '"message":"tool: ok"' events.log \
  | jq 'select(.span.name == "tool.bash")
        | {lane: .span.lane, elapsed_ms: .fields.elapsed_ms, exit_code: .fields.exit_code}'
```

**Did the router dispatch a tool through bwrap?**

```
grep '"message":"router: dispatched"' events.log \
  | jq '{lane: .fields.lane, sandbox: .fields.sandbox, timeout_secs: .fields.timeout_secs}'
```

## Lifeguard

**Did the lifeguard fire on action_hash 0xYYY?**

```
grep '"message":"lifeguard: action observed"' events.log \
  | jq -r 'select(.fields.action_hash == "0xYYY") | .fields.action_count' \
  | sort | uniq -c
```

The escalation message itself is at `WARN`/`ERROR` level and embeds
`action_kind=...`, `action_hash=...`, `action_count=...`,
`max_repeat=...` directly in the message string for cross-level
visibility.

**How many parse failures piled up in the implementer loop?**

```
grep '"message":"lifeguard: parse failure recorded"' events.log \
  | jq '.fields.consecutive_parse_failures' \
  | sort -n | tail -1
```

## Integrator

**What was the integrator's last phase before failure?**

```
grep '"name":"integrator.integrate"' events.log \
  | grep '"message":"integrator: phase begin"' \
  | jq -r '.fields.phase' \
  | tail -1
```

The phases run `preflight` → `git_sequence` → `validation` → `commit`. A
stalled integration's last visible phase is the answer.

**Which bundles got force-failed in this run?**

```
grep '"message":"integrator: failing all bundles"' events.log \
  | jq '{bundle_count: .fields.bundle_count, error: .fields.error}'
```

## Worktrees

**Which seq + branch did `wk-XXX` land on?**

```
grep '"message":"worktree: allocated"' events.log \
  | jq 'select(.spans[]? | (.work_id // "") == "wk-XXX")
        | {seq: .fields.seq, branch: .fields.branch, base_sha: .fields.base_sha}'
```

## Bundle composition manifest

**What did this implementer change for `bd-ZZZ`?**

```
grep '"message":"implementer produced bundle"' events.log \
  | jq 'select(.fields.bundle_id == "bd-ZZZ")
        | {paths_added, paths_modified, paths_deleted, patch_id, diff_bytes}'
```

`patch_id` is a stable identifier from `git patch-id --stable`. Diffs over
1 MiB are recorded as `"oversize"` with `diff_bytes` carrying the byte count
so a runaway implementer is visible without paying the patch-id compute.

## Reviewer

**Which ACs did the reviewer verify on `bd-ZZZ`?**

```
grep '"message":"reviewer: ac evaluated"' events.log \
  | jq 'select(.fields.bundle_id == "bd-ZZZ")
        | {criterion: .fields.criterion, status: .fields.status, evidence: .fields.evidence}'
```

The roll-up is one event per review:

```
grep '"message":"reviewer: ac roll-up"' events.log \
  | jq '{bundle: .fields.bundle_id, count: .fields.ac_count,
         verified: .fields.ac_verified, failed: .fields.ac_failed,
         skipped: .fields.ac_skipped}'
```

Per-AC results are synthesized from the parsed `Verdict` plus the Work's
acceptance criteria; the LLM does not currently emit structured per-AC
verification (see Phase 5 of the design doc).

## Director

**Which Bundles did the Director route through accept?**

```
grep '"name":"director.accept_bundle"' events.log \
  | jq '{plan_id: .span.plan_id, bundle_id: .span.bundle_id}'
```

**How many times did the Director restart, and why?**

```
grep '"message":"director restart"' events.log \
  | jq '{restart: .fields.restart, reason: .fields.restart_reason, error: .fields.error}'
```

`restart_reason` is a closed enum: `llm_retryable`, `parse_failure`,
`context_failure`, `store_failure`, `id_failure`, `lifeguard_escalation`,
`need_help`.

**Which Plans are currently in escalation (Conservative or NeedsOperator)?**

```
grep '"message":"director.mode_change"' events.log \
  | jq 'select(.fields.to == "Conservative" or .fields.to == "NeedsOperator")
        | {plan_id: .fields.plan_id, from: .fields.from, to: .fields.to,
           trigger: .fields.trigger, iteration: .fields.iteration}'
```

`trigger` is a closed enum: `same_action`, `no_progress`, `escalation`,
`recovered`, `operator_note`. Recovery to Normal is also a `mode_change`
event with `to == "Normal"`; grep for `to=Normal` to find Plans the
operator pulled out of escalation via `loopr director chat`.

**Which Plans were Stalled by the NeedsOperator grace timeout?**

```
grep '"message":"director: NeedsOperator grace exceeded' events.log \
  | jq '{plan_id: .fields.plan_id, needs_operator_iters: .fields.needs_operator_iters,
         grace: .fields.grace}'
```

**Which operator notes did the Director observe but not act on (idempotent edge)?**

```
grep '"message":"director: operator note observed; mode unchanged' events.log \
  | jq '{plan_id: .fields.plan_id, iteration: .fields.iteration,
         mode: .fields.mode, note_count: .fields.note_count}'
```

**Which Plans have a live Director right now?**

The per-iteration `DirectorStatusSnapshot` sidecar (`docs/design/2026-05-12-director-phase-2-followups.md` Item 3) is the authoritative answer for the running daemon; query it per-Plan via the CLI:

```
loopr director status pl-<plan-id>
```

For an after-the-fact reconstruction from `events.log`, grep `director iteration start` and partition by the highest-numbered iteration per `plan_id` (the highest iteration for each Plan is the freshest evidence of liveness; an old iteration with no successor means the Director task exited):

```
grep '"message":"director iteration start"' events.log \
  | jq -r '"\(.fields.plan_id) \(.fields.iteration)"' \
  | sort -k1,1 -k2,2n \
  | awk '{p=$1; v=$2} END {print p, v}'  # adapt for multi-plan grouping
```

## Per-record summaries

**What was the terminal state and attempt count for every Work in this run?**

```
grep '"message":"work: terminal-state summary"' events.log \
  | jq '{work_id: .fields.work_id, terminal_state: .fields.terminal_state,
         attempts: .fields.attempt_count}'
```

**Did the Plan complete, and what's the breakdown of child Work outcomes?**

```
grep '"message":"plan: terminal-state summary"' events.log \
  | jq '{plan_id: .fields.plan_id, terminal: .fields.terminal_state,
         total: .fields.total_works, done: .fields.works_done,
         failed: .fields.works_failed, blocked: .fields.works_blocked}'
```

## Cross-stage correlation

Every event under a Work-scoped span carries `work_id` somewhere in the
ancestry — either on the immediate `span` or in the `spans[]` array. To
follow one Work through every stage:

```
grep '"work_id":"wk-XXX"' events.log
```

The per-Work fanout file at `<run-dir>/work/wk-XXX.log` materializes the
same data as a pretty-formatted log filtered to that Work; for ad-hoc
queries, prefer the JSON `events.log` plus `jq`.
