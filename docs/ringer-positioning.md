# Loopr next to Ringer and the Nate Jones Conjecture

**Author:** Claude (Fable), 2026-07-12 session on desk.lan; folded here at Scott's ask
**Status:** Note (not a design doc, not a plan: a positioning read to inform future agent-stage decisions)

## Where this came from

2026-07-12 session evaluating Nate B Jones's Ringer
(`~/repos/NateBJones-Projects/ringer`) against our design/build pipeline
(create-design-doc -> review-panel -> how-to-execute-a-plan -> release-driver).
During that session we built a Linux docker worker lane for Ringer
(`ringer/docker/`), wired OpenCode + OpenRouter (GLM-5.2, Kimi, DeepSeek, Qwen),
and ran two real rounds against the gx design doc
(`gx/docs/design/2026-07-12-llm-propose-apply-and-mcp-server.md`, addendum
records the findings):

- a 3-model adversarial-review swarm (~$0.35): 12 raw findings -> 7 confirmed,
  1 fabricated finding refuted by orchestrator verification
- a 16-task verify swarm (new `templates/verify-swarm` kit): every finding
  cross-examined by 2 hostile refuters from rival labs, with the known-false
  finding seeded as a blind canary

Scott asked where loopr's premise lives next to that. This is the answer,
recorded here because it bears directly on the `agents` crate's future.

## The three bets

**Nate's conjecture (proved on his wife's website build): the economics bet.**
Frontier tokens are priced like senior-engineer hours, and most of a build is
not senior-engineer work. Split the roles: the expensive model writes specs and
reviews results; cheap fungible workers (Codex, GLM at ~20-30x cheaper output)
do the typing. Ringer's own README closes with the proof: "a Claude
orchestrator wrote the specs and reviewed the diffs, Codex swarms wrote the
implementation."

**Ringer: the trust bet.** Its invention is not parallelism, it is
institutionalized distrust: an executed check (exit 0) is the ONLY pass, "done"
from a worker is never believed, failures retry once with the check's output
injected, raw logs only, and every attempt lands in a local eval log that
becomes a per-(model, task_type) scoreboard with an untested -> probation ->
proven promotion ladder. Routing is evidence, not vibes. But Ringer
deliberately stops at both ends of the lifecycle: a human or orchestrating
model writes the manifest (no decomposer) and applies the patches (no
integrator). Power tool, not factory.

**Loopr: the autonomy bet.** Goal -> decomposer -> Work DAG -> agents (Ralph
loops in worktrees) -> Bundles -> deterministic non-LLM integrator -> main,
daemon-ticked and reactive. Loopr owns exactly the two ends Ringer refuses to
touch. Its verification lives in typed Rust seams, CI gates, and the
no-LLM-dep integrator rather than per-task hostile checks, and its `llm` crate
is Anthropic-backend: the typing is done with premium-lab tokens, tiered
within Claude. Loopr's premise predates the cheap-minion insight.

One line each: the website is proof it works, Ringer is proof you can trust
it, loopr is the bet you can automate the rest of it.

## The composition (why this matters to loopr)

- Loopr's decomposer/integrator is what Ringer is missing to become a
  pipeline. Ringer's engine pluralism + executed checks + evidence log is what
  loopr's agent stage is missing to get the economics and the distrust.
- The `agents` crate is the seam. It is precisely the slot where the
  conjecture says "fungible cheap CLI + hostile executed check" instead of
  "Claude Ralph loop for everything." If the Work DAG ever dispatches work
  items through ringer-style engine lanes (spec + check per work item,
  scoreboard-routed model), the full conjecture lands in one system: Fable
  decomposes and adjudicates, minions type, nothing is trusted, the
  integrator lands it.
- The tension is philosophical, not technical: loopr v5 is typed-single-binary
  Rust discipline; Ringer is subprocess duct-tape pluralism. The seam is
  clean anyway: an engine is a config block. Ringer already runs Claude Code
  as a worker lane (`[engines.claude]`), so the direction works both ways.
- Verification layering from the gx runs, worth stealing: the executed check
  proves a report is well-formed, never that it is true (GLM produced a
  structurally perfect, factually fabricated finding). Truth takes a second
  layer: orchestrator re-inspection, or cheaper, a verify swarm of rival-lab
  refuters with a seeded canary. Loopr's integrator gate is the analogous
  seat; if agent work ever comes from cheap lanes, it needs this second layer,
  not just CI green.

## Non-decisions

Nothing here changes v5 scope. No engine abstraction rides into `agents` until
there is a design doc against a real observed problem (vision.md's rule).
This note exists so that when that doc gets written, it starts from the
positioning and the run evidence instead of rediscovering it.

## Pointers

- Ringer repo: `~/repos/NateBJones-Projects/ringer` (README, `templates/`,
  `docs/MODEL-NOTES.md`, `docs/TAXONOMY.md`)
- Docker worker lane built this session: `ringer/docker/{Dockerfile,run.sh,config.toml}`
- Verify-swarm kit: `ringer/templates/verify-swarm/`
- gx findings addendum: `gx/docs/design/2026-07-12-llm-propose-apply-and-mcp-server.md`
  (Addendum: Ringer Adversarial-Review Experiment)
- Model scoreboard: `./ringer.py models` (local evidence, per-user by design)
