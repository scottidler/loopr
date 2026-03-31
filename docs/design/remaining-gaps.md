# Remaining Gaps - All Resolved

All gaps previously tracked in this file have been addressed across Items #2, #3, and #4 (see `next-steps.md`).

| Gap | Resolution |
|-----|-----------|
| #16: Work `depends_on` cycle detection | Item #4: Pipeline Hardening (BFS cycle detection) |
| #10: Tool SIGTERM -> SIGKILL escalation | Item #2: Runner Lane Architecture (`killpg()` with SIGTERM->5s->SIGKILL) |
| #11: Agent session wall-clock timeouts | Item #4: Pipeline Hardening (`tokio::time::timeout` in executor.rs) |
| #22: Bundle `max_loc_changed` enforcement | Wired in daemon-hardening-config-audit (bundle create/update handlers) |
| Upward feedback / bubble-up logic | Item #3: Semantic Bubble-Up (`ReviseParent`, `bubble_up_count`) |
| Coverage gate in Coordinator loop | Item #3: Semantic Bubble-Up (wired into decision tree) |
| Auto-lock on WriteFile | Item #4: Pipeline Hardening (auto-acquisition in executor) |
| Lock cleanup on agent exit | Item #4: Pipeline Hardening (guaranteed release in `run_agent_task` cleanup) |
| Collaborative Plan interview IPC | Chat funnel handles this via interview_mode; IPC handlers exist |
