# Arize Phoenix Docs

Local mirror of the [Arize Phoenix](https://arize.com/docs/phoenix) documentation, fetched from
`https://arizeai-433a7140.mintlify.app/docs/phoenix/**` (Mintlify source). 254 markdown files.

## Tatari deployment

`ai-evals-svc` is Tatari's Phoenix-backed evaluation service. Endpoints (pinned by Jeffrey Xu in #C0ASQ462D1S):

| Env | URL |
|---|---|
| Dev | https://ai-evals-svc.test.tatari.dev/ |
| Staging | https://ai-evals-svc.staging.tatari.dev/ |
| Production | https://ai-evals-svc.prod.tatari.dev/ |

When instrumenting Tatari apps, point `PHOENIX_COLLECTOR_ENDPOINT` at the appropriate env URL above rather than running a local Phoenix instance.

---

**Phoenix vs. Arize AX** — these docs cover **Phoenix** (open-source, `phoenix.otel`, `PHOENIX_API_KEY`).
Arize AX is a separate product (`arize.otel`, `ARIZE_SPACE_ID` + `ARIZE_API_KEY`) with its own docs
at `docs.arize.com/arize`. Do not conflate them.

## Most important docs

### Instrumentation — start here

| File | What it covers |
|---|---|
| `tracing/how-to-tracing/setup-tracing/setup-using-phoenix-otel.md` | `phoenix.otel.register()` — the canonical one-call setup for Python and TypeScript. Sets endpoint, project, and OTLP protocol. |
| `tracing/how-to-tracing/setup-tracing/instrument.md` | Manual instrumentation helpers: `@trace`, `using_session`, `using_metadata`, context managers, span kind constants. Combines with or replaces auto-instrumentors. |
| `get-started/get-started-tracing.md` | End-to-end quickstart: spin up Phoenix Cloud, build an agent, send a first trace. |
| `tracing/llm-traces.md` | Conceptual overview — what traces/spans capture (latency, tokens, exceptions, retrieved docs, embeddings). |

### Auto-instrumentation by framework

- **Python frameworks:** `integrations/python/<framework>/<framework>-tracing.md`
  - LlamaIndex, LangChain, LangGraph, DSPy, CrewAI, AutoGen, Pydantic AI, OpenAI Agents SDK, Agno, Haystack, MCP, Smolagents
- **LLM providers:** `integrations/llm-providers/<provider>/<provider>-tracing.md`
  - OpenAI, Anthropic, Bedrock, VertexAI, Groq, MistralAI, LiteLLM, Google GenAI
- **TypeScript:** `integrations/typescript/<name>.md`
  - Claude Agent SDK, LangChain, Mastra, Vercel AI SDK, BeeAI, MCP, TanStack AI

### Spans and metadata

| File | What it covers |
|---|---|
| `tracing/how-to-tracing/add-metadata/customize-spans.md` | Attach custom attributes, tags, user IDs to auto-instrumented spans. |
| `tracing/how-to-tracing/add-metadata/instrumenting-prompt-templates-and-prompt-variables.md` | Track prompt templates and variable substitutions as span attributes. |
| `tracing/how-to-tracing/setup-tracing/setup-projects.md` | Route traces to named projects via env var or code. |
| `tracing/how-to-tracing/setup-tracing/setup-sessions.md` | Group related traces into conversation sessions. |

### Feedback and evaluation on traces

| File | What it covers |
|---|---|
| `tracing/how-to-tracing/feedback-and-annotations/capture-feedback.md` | Attach human or programmatic feedback to spans via the SDK. |
| `tracing/how-to-tracing/feedback-and-annotations/evaluating-phoenix-traces.md` | Run LLM evals against exported spans; log results back to Phoenix. |
| `tracing/how-to-tracing/feedback-and-annotations/llm-evaluations.md` | LLM-as-a-judge evaluators wired directly into the trace pipeline. |
| `evaluation/llm-evals.md` | Evaluation overview — SDK evals, dataset evaluators, human labels. |
| `evaluation/pre-built-metrics/faithfulness.md` | Faithfulness evaluator (RAG). Siblings: `document-relevance.md`, `correctness.md`, `tool-calling-eval.md`, `toxicity.md`, etc. |

### Import/export and querying

| File | What it covers |
|---|---|
| `tracing/how-to-tracing/importing-and-exporting-traces/extract-data-from-spans.md` | `px.Client().get_spans_dataframe()` — pull spans into pandas for offline eval or analysis. |
| `tracing/how-to-tracing/importing-and-exporting-traces/importing-existing-traces.md` | Ingest OTLP traces from other sources. |
| `tracing/how-to-tracing/importing-and-exporting-traces/exporting-annotated-spans.md` | Export spans with human annotations for fine-tuning datasets. |

### Advanced tracing

| File | What it covers |
|---|---|
| `tracing/how-to-tracing/advanced/suppress-tracing.md` | Disable tracing for specific code paths. |
| `tracing/how-to-tracing/advanced/masking-span-attributes.md` | Redact PII or sensitive fields from spans before export. |
| `tracing/how-to-tracing/advanced/modifying-spans.md` | Post-process span attributes with a processor. |
| `tracing/how-to-tracing/advanced/multimodal-tracing.md` | Trace image and audio inputs. |

### Datasets and experiments

| File | What it covers |
|---|---|
| `datasets-and-experiments/overview-datasets.md` | What datasets are; creating from traces, CSV, or code. |
| `datasets-and-experiments/how-to-experiments/run-experiments.md` | Run experiments from Playground or SDK; compare versions. |
| `datasets-and-experiments/how-to-experiments/how-to-dataset-evaluators.md` | Attach evaluators to datasets for automatic scoring. |

## Directory layout

```
docs/arize/
├── index.md                          # What is Phoenix? (root overview)
├── get-started/                      # Quickstarts (Python + TypeScript)
├── tracing/
│   ├── llm-traces.md                 # Conceptual overview
│   ├── llm-traces/                   # Sessions, projects, metrics, annotation UI
│   ├── how-to-tracing/               # Setup, metadata, manual instrumentation, feedback, advanced
│   │   ├── setup-tracing/            # register(), instrument helpers, projects, sessions
│   │   ├── add-metadata/             # Span attributes, prompt templates
│   │   ├── feedback-and-annotations/ # Capture feedback, LLM evals, UI annotation
│   │   ├── importing-and-exporting-traces/
│   │   └── advanced/                 # Suppress, mask, modify, multimodal
│   └── tutorial/                     # First traces, sessions, annotations walkthrough
├── integrations/
│   ├── python/                       # Per-framework instrumentors (LlamaIndex, LangChain, …)
│   ├── typescript/                   # TS-specific integrations (Vercel, Mastra, MCP, …)
│   ├── llm-providers/                # Provider-level instrumentation (OpenAI, Anthropic, …)
│   ├── java/                         # Spring AI, LangChain4j, Arconia
│   ├── platforms/                    # Dify, Flowise, LangFlow, PromptFlow
│   └── evaluation-integrations/      # Ragas, Cleanlab, MLflow, UQLM
├── evaluation/
│   ├── llm-evals.md                  # Evaluation overview
│   ├── pre-built-metrics/            # Faithfulness, relevance, correctness, toxicity, tool-calling, …
│   ├── how-to-evals/                 # Batch eval, code evaluators, custom LLM evaluators, LLM config
│   ├── server-evals/                 # Server-side evaluators attached to datasets
│   └── tutorials/
├── datasets-and-experiments/
│   ├── overview-datasets.md
│   ├── how-to-datasets/
│   ├── how-to-experiments/
│   └── tutorial/
├── prompt-engineering/
│   ├── overview-prompts/             # Prompt management, playground, span replay, prompts in code
│   └── tutorial/
├── cookbook/                         # End-to-end worked examples by topic
│   ├── agent-workflow-patterns/      # AutoGen, CrewAI, LangGraph, OpenAI Agents, Smolagents
│   ├── evaluation/                   # RAG eval, agent eval, custom evaluator
│   ├── tracing/                      # Agentic RAG, synthetic datasets, structured extraction
│   ├── prompt-engineering/           # Chain-of-thought, few-shot, ReAct, prompt optimization
│   └── datasets-and-experiments/
├── settings/                         # RBAC, API keys, secrets, data retention, custom providers
├── self-hosting.md
├── phoenix-cloud.md
├── production-guide.md
├── user-guide.md
├── environments.md
└── release-notes.md
```
