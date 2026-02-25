# Loops View Rendering Options

**Author:** Scott A. Idler
**Date:** 2026-02-02
**Status:** Options for Review
**Related:** [2026-02-02-plan-command-and-loop-cascade.md](2026-02-02-plan-command-and-loop-cascade.md)

---

## Key Relationships

Understanding the data model is critical for visualization:

- **Loop PRODUCES artifacts**: 1 Loop → 1+ artifacts (one-to-many)
- **Loop WORKS ON artifact**: 1 Loop → 1 artifact (one-to-one)

Flow example:
```
p-a1b2 (produces 2 artifacts)
  ├── plan-oauth-auth.md ──→ s-c3d4 (works on this, produces 3 artifacts)
  │                            ├── spec-google.md ──→ h-1111
  │                            ├── spec-github.md ──→ h-2222
  │                            └── spec-token.md  ──→ h-3333
  │
  └── plan-infra.md ───────→ s-d5e6 (works on this, produces 1 artifact)
                               └── spec-database.md ──→ h-4444

**Loop ID format:** `{type}-{4char}` where type is `p` (plan), `s` (spec), `h` (phase), `c` (code)
```

---

## Option A: Unified Tree (Shows producing and spawning)

Loop and artifacts in one tree. Each artifact spawns exactly one child loop (─►). Each loop can have multiple artifact children.

```
p-a1b2 [Complete] ─────────────────────── produced 2 artifacts
├── plan-oauth-auth.md [Active]
│   └─► s-c3d4 [Complete] ─────────────── produced 3 artifacts
│       ├── spec-google.md [Active]
│       │   └─► h-1111 [Running] ──────── produced 2 artifacts
│       │       ├── phase-google-001.md [Complete]
│       │       └── phase-google-002.md [Running]
│       │
│       ├── spec-github.md [Active]
│       │   └─► h-2222 [Pending]
│       │
│       └── spec-token.md [Active]
│           └─► h-3333 [Pending]
│
└── plan-infra.md [Active]
    └─► s-d5e6 [Running] ─────────────── produced 1 artifact
        └── spec-database.md [Draft]
```

**Pros:**
- Shows full hierarchy in one view
- Clear visual distinction between loops (─►) and artifacts (──)

**Cons:**
- Can get deeply nested
- Mixed concerns (loops and artifacts together)

---

## Option B: Artifact-First with Loop Annotations

Artifacts as primary nodes. Shows which loop produced each, and which artifact that loop was working on.

```
plan-oauth-auth.md [Active]              ← produced by p-a1b2
├── spec-google.md [Active]              ← produced by s-c3d4 (works on plan-oauth-auth)
│   ├── phase-google-001.md [Complete]   ← produced by h-1111 (works on spec-google)
│   └── phase-google-002.md [Running]    ← produced by h-1111
│
├── spec-github.md [Active]              ← produced by s-c3d4
│   └── (h-2222 pending)
│
└── spec-token.md [Active]               ← produced by s-c3d4
    └── (h-3333 pending)

plan-infra.md [Active]                   ← produced by p-a1b2
└── spec-database.md [Draft]             ← produced by s-d5e6 (works on plan-infra)
```

**Pros:**
- Artifact-centric (what the user cares about)
- Loop IDs as metadata, not primary structure

**Cons:**
- Loop status less prominent
- Harder to see loop-level operations

---

## Option C: Two-Column with Clear 1:1 Working Relationship

Explicit columns showing what each loop works on (1:1) and what it produced (1:many).

```
LOOP                WORKS ON              PRODUCED
──────────────────────────────────────────────────────────────
p-a1b2 [●]          (conversation)        ┬─ plan-oauth-auth.md
                                          └─ plan-infra.md

s-c3d4 [●]          plan-oauth-auth.md    ┬─ spec-google.md
                                          ├─ spec-github.md
                                          └─ spec-token.md

s-d5e6 [◐]          plan-infra.md         └─ spec-database.md

h-1111 [◐]          spec-google.md        ┬─ phase-google-001.md
                                          └─ phase-google-002.md

h-2222 [○]          spec-github.md        (pending)

h-3333 [○]          spec-token.md         (pending)
```

**Pros:**
- Very clear about relationships
- Easy to scan loop status
- Shows 1:1 (works on) and 1:many (produced) explicitly

**Cons:**
- Loses hierarchical nesting visually
- May need scrolling for large hierarchies

---

## Option D: Breadcrumb Navigation

Navigate the artifact→loop→artifact chain. Shows artifacts at current level only.

```
Path: p-a1b2 → plan-oauth-auth.md → s-c3d4 → spec-google.md → h-1111

Loop: h-1111 [Running]
Works on: spec-google.md
Produced:
  ┌──────────────────────────────────────────┐
  │ ● phase-google-001.md    [Complete]      │
  │ ◐ phase-google-002.md    [Running]       │
  └──────────────────────────────────────────┘

[← Back to spec-google.md] [↑ Up to s-c3d4]
```

**Pros:**
- Clean, focused view
- Works well for deep hierarchies
- Clear navigation model

**Cons:**
- Can't see siblings without navigating
- No overview of full tree

---

## Option E: Expanded Detail View

Collapsible sections with full context visible. Shows the "works on" relationship explicitly.

```
▼ p-a1b2 [Complete] works on: (conversation)
  │ Produced 2 artifacts:
  │
  │  ┌─ plan-oauth-auth.md [Active]
  │  │  └─► s-c3d4 [Complete] works on: plan-oauth-auth.md
  │  │      │ Produced 3 artifacts:
  │  │      │  ┌─ spec-google.md [Active]
  │  │      │  │  └─► h-1111 [Running] works on: spec-google.md
  │  │      │  │      │ Produced 2 artifacts:
  │  │      │  │      │  ├─ phase-google-001.md [Complete]
  │  │      │  │      │  └─ phase-google-002.md [Running]
  │  │      │  │
  │  │      │  ├─ spec-github.md [Active]
  │  │      │  │  └─► h-2222 [Pending]
  │  │      │  │
  │  │      │  └─ spec-token.md [Active]
  │  │      │     └─► h-3333 [Pending]
  │  │
  │  └─ plan-infra.md [Active]
  │     └─► s-d5e6 [Running] works on: plan-infra.md
  │         │ Produced 1 artifact:
  │         │  └─ spec-database.md [Draft]
```

**Pros:**
- Full context always visible
- Explicit "works on" relationship shown
- Collapsible for managing complexity

**Cons:**
- Verbose
- Deep indentation

---

## Option F: Split View (Loops left, Artifacts right, lines connecting)

Two panes with visual connections between related items.

```
LOOPS                              ARTIFACTS
─────────────────────────────────────────────────────────────
p-a1b2 [●]─────────────────────┬── plan-oauth-auth.md
         │                     └── plan-infra.md
         │                              │
         │    ┌─────────────────────────┘
         │    │
s-c3d4 [●]────┼──(works on plan-oauth)─┬── spec-google.md
         │    │                        ├── spec-github.md
         │    │                        └── spec-token.md
         │    │                                 │
s-d5e6 [◐]────┘──(works on plan-infra)─── spec-database.md
         │
h-1111 [◐]───────(works on spec-google)┬── phase-001.md
                                       └── phase-002.md
```

**Pros:**
- Clear separation of loops and artifacts
- Visual connection lines
- Both views synchronized

**Cons:**
- Complex to render in TUI
- Lines can get tangled with many items

---

## Status Symbols

| Symbol | Meaning |
|--------|---------|
| ● | Complete |
| ◐ | Running |
| ○ | Pending |
| ✗ | Failed |
| ⊘ | Invalidated |

---

## Recommendation

**Option A (Unified Tree)** or **Option E (Expanded Detail)** seem best for:
- Showing full hierarchy
- Clear loop→artifact→loop chain
- Working in a TUI context

**Option C (Two-Column)** is good for:
- Flat list operations
- Quick status scanning
- Batch operations on loops

Consider implementing Option A as default with Option D (Breadcrumb) for deep navigation.
