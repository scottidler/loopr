# context

Prompt assembly — the single source of truth for rendering `domain` records, persisted history from `store`, and `tools` schemas into the `Message` vectors consumed by `llm`. Shared between every LLM-calling crate (`decomposer`, `agents`).

## In scope

- `ContextBuilder` — the main entry point. Takes typed inputs (Plan, Work, Bundle, prior Verdicts, Tool registry snapshot, conversation history) and produces a ready-to-send `Vec<Message>`.
- Template loading: reads `.pmt` files from the three-layer chain `.loopr/prompts/` → `~/.config/loopr/prompts/` → baked-in via `include_dir!()`. Caches compiled templates.
- handlebars-rust as the templating engine. Partials registered from `partials/` for SSOT chunks (`section-ac`, `section-context`, etc.).
- Token budgeting: drops oldest history / largest records first when the rendered context would exceed the budget. Emits telemetry spans with final token counts.
- Tool-schema rendering into system prompts: takes a `&[ToolSchema]` slice from the `tools` crate (or wherever the registry lives) and produces the canonical "tools you have access to" block.
- Per-role and per-stage prompt entry points: `build_for_plan(goal)`, `build_for_decompose(plan)`, `build_for_implementer(work, tools_snapshot)`, `build_for_reviewer(bundle)`, etc. Callers get typed APIs instead of passing raw context dicts.

## Out of scope

- **LLM transport.** `context` builds `Message` vectors; `llm` sends them. No network here.
- **Tool execution.** `tools` owns execution. `context` only renders tool *schemas*; it doesn't call them.
- **Ralph loops or orchestration decisions.** Those are in `agents` and `decomposer`.
- **Prompt content authoring.** Prompts are authored by humans and live in `.loopr/prompts/`; this crate loads and renders them.

## Rule

`context` is shared across multiple LLM-calling crates. If you find yourself implementing prompt assembly inside `agents` or `decomposer` directly, extract it into a `context::build_for_<role>` entry point so the other crate can share it.

This crate was extracted after the Architect's Round 3 finding: originally `ContextBuilder` lived in `agents`, but `decomposer` also calls LLMs and couldn't reach `agents` (reverse pipeline dep). Putting assembly into its own crate under both consumers solves the upstream-starvation problem without introducing cycles.

## Dependencies

`domain` (record types), `store` (to fetch historical context like prior Bundles), `tools` (for `ToolSchema`), `telemetry` (for span emission), workspace-shared (`serde`, `eyre`, `handlebars`, `include_dir`). Added via `cargo add` when first consumed.

Notably does NOT depend on `llm` — `context` produces ready-to-send Messages; `llm` consumes them. Keeps the prompt-assembly layer independent of which LLM backend is in use.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): "Prompts" section for the three-layer override model and themed directory structure
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
