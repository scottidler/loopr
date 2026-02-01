# Prompt Templates

**Author:** Scott A. Idler
**Date:** 2026-01-31
**Status:** Implementation Spec

---

## Summary

Prompt templates define what the LLM sees for each loop type. They use Handlebars syntax for variable interpolation. This directory contains the default templates for each loop type.

## Templates

| File | Loop Type | Description |
|------|-----------|-------------|
| [plan.md](plan.md) | PlanLoop | Creates high-level plans from user tasks |
| [spec.md](spec.md) | SpecLoop | Creates detailed specs from plans |
| [phase.md](phase.md) | PhaseLoop | Creates phase documents from specs |
| [code.md](code.md) | CodeLoop | Implements code from phase specs |

---

## Template System

### Format: Handlebars

Templates use [Handlebars](https://handlebarsjs.com/) syntax:

```handlebars
{{variable}}           - Insert variable value
{{#if condition}}      - Conditional block
{{#each array}}        - Iteration
{{{raw_html}}}         - Unescaped output (for markdown)
```

### Template Locations

```
~/.config/loopr/prompts/    # User customizations (override defaults)
  ├── plan.md
  ├── spec.md
  ├── phase.md
  └── code.md

<embedded in binary>        # Defaults (fallback)
  └── prompts/*.md
```

**Loading priority:** User templates override embedded defaults.

### Template Rendering

```rust
impl PromptRenderer {
    pub fn render(&self, loop_record: &Loop) -> Result<String> {
        let template_path = self.resolve_template(&loop_record.prompt_path)?;
        let template = fs::read_to_string(&template_path)?;

        let mut handlebars = Handlebars::new();
        handlebars.register_escape_fn(handlebars::no_escape);  // Preserve markdown

        // Build context from loop fields
        let mut context = loop_record.context.clone();
        context["progress"] = json!(loop_record.progress);
        context["iteration"] = json!(loop_record.iteration);

        Ok(handlebars.render_template(&template, &context)?)
    }
}
```

---

## Variables by Loop Type

### Common Variables (All Loop Types)

| Variable | Type | Description |
|----------|------|-------------|
| `iteration` | u32 | Current iteration number (0-indexed) |
| `progress` | String | Accumulated feedback from previous iterations |
| `max_iterations` | u32 | Maximum allowed iterations |

### PlanLoop Variables

| Variable | Type | Description |
|----------|------|-------------|
| `task` | String | User's original task description |

### SpecLoop Variables

| Variable | Type | Description |
|----------|------|-------------|
| `plan_id` | String | Parent plan's ID |
| `plan_content` | String | Full content of plan.md |
| `spec_name` | String | Name/title of this spec from plan |

### PhaseLoop Variables

| Variable | Type | Description |
|----------|------|-------------|
| `spec_id` | String | Parent spec's ID |
| `spec_content` | String | Full content of spec.md |
| `phase_number` | u32 | Phase index (1-indexed) |
| `phase_name` | String | Name of this phase from spec |
| `phases_total` | u32 | Total phases in this spec |

### CodeLoop Variables

| Variable | Type | Description |
|----------|------|-------------|
| `phase_id` | String | Parent phase's ID |
| `phase_content` | String | Full content of phase.md |
| `task` | String | Specific coding task from phase |

---

## Progress Accumulation

The `progress` field accumulates feedback across iterations:

```
Iteration 1 fails:
  progress = "## Iteration 1 Failed\n- Missing ## Overview section\n- No success criteria defined"

Iteration 2 fails:
  progress = "## Iteration 1 Failed\n- Missing ## Overview section\n- No success criteria defined\n\n---\n\n## Iteration 2 Failed\n- Overview too vague\n- Success criteria not measurable"

Iteration 3 succeeds:
  (progress is preserved but iteration completes)
```

The progress is injected into the prompt via `{{#if progress}}` block, so fresh iterations without prior failures get a cleaner prompt.

---

## Customization

### Per-Project Templates

Projects can override templates in `.loopr/prompts/`:

```
my-project/
├── .loopr/
│   └── prompts/
│       └── code.md    # Custom CodeLoop prompt for this project
└── src/
```

### Template Variables via Context

The `context` field in Loop records can include arbitrary variables:

```rust
let loop = Loop {
    context: json!({
        "task": "Add OAuth",
        "language": "rust",      // Custom variable
        "framework": "axum",     // Custom variable
    }),
    ..
};
```

Templates can reference these:

```handlebars
{{#if language}}
Use {{language}} conventions and idioms.
{{/if}}

{{#if framework}}
This project uses the {{framework}} framework.
{{/if}}
```

---

## References

- [../loop.md](../loop.md) - Loop iteration model
- [../domain-types.md](../domain-types.md) - Loop struct definition
- [../implementation-patterns.md](../implementation-patterns.md) - Handlebars usage
- [Handlebars Documentation](https://handlebarsjs.com/)
