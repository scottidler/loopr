# Oracle Vault -> Loopr: Knowledge Extraction Report

## Part 1: The 9 Most Directly Relevant Notes

| # | Note | Relevance to Loopr |
|---|------|-------------------|
| 1 | **Ralph Wiggum Loop Research** (`ralph-wiggum-loop-2.md`) | Loopr's Implementer agent IS an RWL. Fresh context per iteration, git as memory, one task per iteration. This is Loopr's founding pattern. |
| 2 | **Jeffrey Emanuel's Rule of Five** (`jeffrey-emanuel-rule-of-five-agentic-llm.md`) | Loopr's Doc Validator + Coverage Evaluator + Reviewer agent are exactly this - mandatory verification gates before trust. Convergence through iteration. |
| 3 | **The Agent Debate: 10x or Hoax** (`agent-debate-10x-or-hoax.md`) | Master synthesis of 48 notes. Verdict: "The harness determines the outcome, not the model." Loopr IS the harness. |
| 4 | **Stripe's Minions** (`i-studied-stripe-s-ai-agents-vibe-coding-is-already-dead.md`) | Stripe ships 1,300 PRs/week with dedicated agent sandboxes, blueprint engines, and 400+ MCP tools. Closest production analog to what Loopr is building. |
| 5 | **Autoresearch / Agent Loops** (`autoresearch-agent-loops-and-the-future-of-work.md`) | Karpathy's "arena design" pattern: humans define scoring, agents iterate overnight. "The loop is the hero, not the model." |
| 6 | **Will Humans Still Review Code** (`will-humans-still-review-code.md`) | Hard data: +98% more PRs merged, +91% review time. Bottleneck shifted from writing to reviewing. Loopr's Reviewer agent addresses this directly. |
| 7 | **Claude Code Features** (`claude-code-features.md`) | Practical patterns: CLAUDE.md that grows from failures, blocking at submit (not write), session log meta-analysis for improvement loops. |
| 8 | **How AI Agents Remember Things** (`how-ai-agents-remember-things.md`) | Memory doesn't need vector DBs - markdown files and four mechanisms (bootstrap, episodic, semantic, procedural) suffice. |
| 9 | **AGENTS.md Research** (`agents-context-file-value-review`) | ETH Zurich: LLM-generated context files HURT performance (-3%). Human-written ones help marginally (+4%). Only non-inferable details matter. |

---

## Part 2: Actionable Knowledge for What to Do Next

### A. THE BRIDGE IS THE RIGHT PRIORITY (Validated)

Your vault's single strongest signal: **"The harness determines the outcome, not the model."** Loopr's entire value proposition IS being the harness - the FSM spine, the deterministic Integrator, the validation gates. The chat-to-orchestration bridge is the last mile between "working harness" and "working product."

**Specific action**: Get one end-to-end run working. Chat -> Interview -> Plan -> autonomous execution -> merged code. Everything else is refinement. The vault's evidence says:

> "The loop is the hero, not the model." -- Karpathy/Autoresearch

> "Agentic engineering is knowing what will happen in your system so well you don't need to look." -- Stripe

Loopr already has the deterministic guarantees. It just needs the trigger.

---

### B. STEAL FROM STRIPE'S BLUEPRINT ENGINE

Stripe's architecture is the closest production analog. Key patterns Loopr should adopt:

1. **Blueprint Engine** - Stripe combines deterministic code (linters, tests, git) with agent reasoning in workflows. Loopr's Coordinator FSM + Implementer RWL is structurally identical, but Loopr should make the deterministic/LLM boundary even more explicit. The Integrator (no LLM) already does this for Ticks - extend the principle to more of the pipeline.

2. **Limit CI rounds** - Stripe limits agents to 2 rounds of CI for cost control. Loopr's `max_validation_attempts: 3` is close. Consider making this configurable per Work item based on estimated complexity.

3. **Multiple entry points** - Stripe agents kick off from CLI, web, or Slack. Loopr only has TUI chat. Consider adding a CLI-driven goal submission (`loopr run "build feature X"`) for headless/overnight execution - this is the Karpathy "run overnight and review in the morning" pattern.

---

### C. THE INTERVIEW FUNNEL IS YOUR BIGGEST LEVERAGE POINT

The vault's detractor camp converges on one thesis: **specification collapse kills agents**. The intent gap, context amnesia, and blast radius all trace back to poor specs.

> "90% of vibe coding failures trace to two causes: moving so fast you never define what you want, and confusing 'works on my laptop' with production-ready."

> "Spend 30+ minutes on requirements discussion." -- RWL Research

Loopr's Interview funnel state exists but isn't fully wired. This is the highest-leverage piece to complete because:
- Plan quality determines the ceiling for everything downstream
- The Rule of Five says convergence takes 4-5 passes; the Interview is pass 0
- Your Coordinator already generates Plans via LLM, but without the sharpening interview, those Plans will be fuzzy

**Specific action**: Wire the Interview FSM state so the chat LLM asks clarifying questions (acceptance criteria, scope boundaries, what "done" looks like) BEFORE any Plan generation. Binary completion criteria are the key guardrail against thrashing.

---

### D. COVERAGE EVALUATOR IS YOUR SECRET WEAPON - FINISH WIRING IT

The Rule of Five says first drafts are unreliable and convergence requires iteration. Loopr already has:
- Doc Validator (single-doc quality) - pass 1
- Coverage Evaluator (parent -> children completeness) - pass 2
- Reviewer agent (code review) - pass 3

But the **upward feedback / bubble-up logic** is incomplete. When children fail coverage after max attempts, the parent should transition Draft -> revise. This closes the loop - without it, bad Plans produce bad Specs produce bad Work and the system grinds.

**Specific action**: Wire the bubble-up: if coverage evaluation fails N times at any level, escalate to the parent. This is the "regenerate plans - it's cheaper than salvaging" insight from the RWL research.

---

### E. ADD A HEADLESS/OVERNIGHT MODE

Karpathy's autoresearch and Stripe's outloop coding share a pattern: **fire and forget, review later**. Loopr's TUI-centric design currently requires someone watching.

The vault says the highest-value workflow is:
1. Human designs the arena (Plan + acceptance criteria)
2. Agents execute overnight
3. Human reviews results in the morning

**Specific action**: Add `loopr run <goal>` that creates a CoordinatorGoal, starts the daemon, and exits. TUI becomes optional for monitoring. This also makes Loopr testable in CI.

---

### F. LEARNINGS ARE YOUR MEMORY SYSTEM - MAKE THEM FIRST-CLASS

The vault's memory research says you don't need vector DBs - markdown files with four mechanisms (bootstrap, episodic, semantic, procedural) suffice. Loopr already has Learnings with confidence scoring and role-applicability tags. This is structurally identical to what the research recommends.

**Specific action**: Ensure Learnings flow back into agent context assembly. When a Researcher discovers something or a Reviewer gives feedback, that Learning should appear in the next Implementer's system prompt for the same Work item. The "session log meta-analysis" pattern from the Claude Code Features note suggests: after each completed goal, generate a Learning summarizing what worked and what didn't.

---

### G. CONTEXT FILES: LESS IS MORE

The ETH Zurich AGENTS.md research found LLM-generated context files HURT performance. Only human-written, non-inferable details help. For Loopr's agent prompts:

- Don't dump the full codebase overview into agent context
- DO include: custom build commands, non-obvious project conventions, domain-specific vocabulary
- Let agents discover structure through tools (grep, find, read) rather than front-loading it

**Specific action**: Audit your .pmt prompt files. Strip architectural overviews (agents can discover these). Keep only tool instructions, output format requirements, and non-inferable project rules.

---

### H. THE REVIEW BOTTLENECK IS REAL - AUTOMATE IT

Hard data: +98% more PRs, +91% more review time. Loopr's Reviewer agent exists but is single-pass. The Rule of Five says you need 4-5 passes with varying scope (in-the-small bugs -> in-the-large architecture).

**Specific action**: Consider making the Reviewer do multiple passes before approving a Bundle:
1. First pass: bugs, errors, style
2. Second pass: architecture, approach concerns
3. Final pass: convergence check

This matches Emanuel's actual prompts and would produce much higher-quality Bundle approvals.

---

## Summary: Priority Stack

| Priority | Action | Why |
|----------|--------|-----|
| **P0** | Wire end-to-end: Chat -> Interview -> Plan -> Execute -> Merge | Everything else is refinement without this |
| **P1** | Complete Interview FSM with clarifying questions + binary acceptance criteria | Plan quality = ceiling for all downstream work |
| **P2** | Wire coverage evaluator bubble-up logic | Closes the regeneration loop; prevents grinding on bad Plans |
| **P3** | Add `loopr run <goal>` headless mode | Enables overnight execution + CI testing |
| **P4** | Ensure Learnings flow into agent context | Compounds knowledge across iterations |
| **P5** | Multi-pass Reviewer with varying scope | Addresses the review bottleneck with Rule-of-Five convergence |
| **P6** | Audit .pmt prompts per AGENTS.md research | Strip inferable context, keep only non-obvious rules |

The vault's 48 agent notes converge to one thesis: **agents are a 10x multiplier for people who treat them as infrastructure requiring engineering, supervision, and intent alignment**. Loopr is exactly that infrastructure. The spine works. Now connect the last wire.
