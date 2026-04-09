#![allow(clippy::unwrap_used, unused_imports)]

use serde_json::json;

use super::fixtures::*;

#[tokio::test]
async fn test_preformed_todo_app_plan() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    let (plan_id, spec_results) = inject_preformed_plan(
        &stores,
        &tx,
        &wm,
        &ic,
        PlanInput {
            title: "CLI Todo App",
            desc: "Build a command-line todo application with add, list, done, delete, and filter commands. Persist todos to a JSON file.",
            criteria: "1. CRUD operations work\n2. Persistence to JSON\n3. Filter by status\n4. All tests pass",
            specs: vec![(
                "Todo App Technical Spec",
                "Full technical specification for the CLI todo app",
                vec![
                    (
                        "Phase 1: Data Model & Storage",
                        "Implement Todo struct and JSON file persistence",
                        1,
                        vec![
                            (
                                "Todo struct",
                                "Define Todo with id, title, done, created_at fields",
                                vec!["src/model.rs"],
                            ),
                            (
                                "JSON storage",
                                "Read/write todos to a JSON file on disk",
                                vec!["src/storage.rs"],
                            ),
                        ],
                    ),
                    (
                        "Phase 2: CRUD Operations",
                        "Implement add, list, done, delete commands",
                        2,
                        vec![
                            ("Add command", "Add a new todo with a title", vec!["src/commands.rs"]),
                            (
                                "List command",
                                "List all todos with status indicators",
                                vec!["src/commands.rs"],
                            ),
                            (
                                "Done command",
                                "Mark a todo as completed by ID",
                                vec!["src/commands.rs"],
                            ),
                            ("Delete command", "Remove a todo by ID", vec!["src/commands.rs"]),
                        ],
                    ),
                    (
                        "Phase 3: Filtering & CLI",
                        "Add filter support and wire up CLI arg parsing",
                        3,
                        vec![
                            (
                                "Filter by status",
                                "Filter todos by all/active/done",
                                vec!["src/commands.rs"],
                            ),
                            (
                                "CLI entry point",
                                "Parse args and dispatch to commands",
                                vec!["src/main.rs"],
                            ),
                        ],
                    ),
                ],
            )],
        },
    ).await;

    // Verify hierarchy counts
    assert_eq!(stores.plans.read().unwrap().len(), 1);
    assert_eq!(stores.specs.read().unwrap().len(), 1);
    assert_eq!(stores.phases.read().unwrap().len(), 3);
    assert_eq!(stores.works.read().unwrap().len(), 8);

    // Verify plan is active
    let plans = stores.plans.read().unwrap();
    assert_eq!(plans[&plan_id].status().to_string(), "active");

    // Verify spec->plan relationship
    let (ref spec_id, ref phases) = spec_results[0];
    let specs = stores.specs.read().unwrap();
    assert_eq!(&specs[spec_id].parent_id, &plan_id);

    // Verify phase->spec relationships and ordering
    let phase_store = stores.phases.read().unwrap();
    for (phase_id, _) in phases.iter() {
        let phase = &phase_store[phase_id];
        assert_eq!(&phase.parent_id, spec_id);
        assert_eq!(phase.status().to_string(), "active");
    }

    // Verify work->phase relationships
    let work_store = stores.works.read().unwrap();
    let (ref phase1_id, ref phase1_works) = phases[0];
    assert_eq!(phase1_works.len(), 2);
    for wid in phase1_works {
        assert_eq!(work_store[wid].parent_id, *phase1_id);
    }

    let (ref phase2_id, ref phase2_works) = phases[1];
    assert_eq!(phase2_works.len(), 4);
    for wid in phase2_works {
        assert_eq!(work_store[wid].parent_id, *phase2_id);
    }

    // All works should be Ready (auto-promoted from Draft since acceptance_criteria present)
    for work in work_store.values() {
        assert_eq!(work.status().to_string(), "Ready");
    }
}

#[tokio::test]
async fn test_preformed_calculator_app_plan() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    let (plan_id, spec_results) = inject_preformed_plan(
        &stores,
        &tx,
        &wm,
        &ic,
        PlanInput {
            title: "Calculator CLI",
            desc: "Build a command-line calculator supporting basic arithmetic, expression parsing, and a REPL mode.",
            criteria: "1. Basic arithmetic (+, -, *, /)\n2. Expression parsing with operator precedence\n3. REPL mode\n4. Error handling for division by zero\n5. All tests pass",
            specs: vec![(
                "Calculator Technical Spec",
                "Technical specification for the CLI calculator",
                vec![
                    (
                        "Phase 1: Arithmetic Engine",
                        "Implement core arithmetic operations with error handling",
                        1,
                        vec![
                            (
                                "Arithmetic ops",
                                "Implement add, subtract, multiply, divide with f64",
                                vec!["src/engine.rs"],
                            ),
                            (
                                "Error handling",
                                "Handle division by zero and overflow gracefully",
                                vec!["src/engine.rs"],
                            ),
                        ],
                    ),
                    (
                        "Phase 2: Expression Parser",
                        "Parse and evaluate mathematical expressions",
                        2,
                        vec![
                            (
                                "Tokenizer",
                                "Tokenize input string into numbers and operators",
                                vec!["src/parser.rs"],
                            ),
                            (
                                "Parser",
                                "Recursive descent parser with operator precedence",
                                vec!["src/parser.rs"],
                            ),
                            (
                                "Evaluator",
                                "Evaluate parsed AST to produce a result",
                                vec!["src/parser.rs"],
                            ),
                        ],
                    ),
                    (
                        "Phase 3: REPL & CLI",
                        "Interactive REPL mode and CLI entry point",
                        3,
                        vec![
                            ("REPL loop", "Read-eval-print loop with history", vec!["src/repl.rs"]),
                            (
                                "CLI entry point",
                                "Parse args: expression mode vs REPL mode",
                                vec!["src/main.rs"],
                            ),
                        ],
                    ),
                ],
            )],
        },
    ).await;

    // Verify hierarchy counts
    assert_eq!(stores.plans.read().unwrap().len(), 1);
    assert_eq!(stores.specs.read().unwrap().len(), 1);
    assert_eq!(stores.phases.read().unwrap().len(), 3);
    assert_eq!(stores.works.read().unwrap().len(), 7);

    // Verify everything is active/ready
    let plans = stores.plans.read().unwrap();
    assert_eq!(plans[&plan_id].status().to_string(), "active");

    let phase_store = stores.phases.read().unwrap();
    for phase in phase_store.values() {
        assert_eq!(phase.status().to_string(), "active");
    }

    let work_store = stores.works.read().unwrap();
    for work in work_store.values() {
        assert_eq!(work.status().to_string(), "Ready");
    }

    // Verify phases exist under spec
    let (_, ref phases) = spec_results[0];
    for (phase_id, _) in phases.iter() {
        assert!(phase_store.contains_key(phase_id));
    }
}

#[tokio::test]
async fn test_preformed_plan_work_can_transition_to_in_progress() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    let (_, spec_results) = inject_preformed_plan(
        &stores,
        &tx,
        &wm,
        &ic,
        PlanInput {
            title: "Tiny App",
            desc: "A minimal app for testing work transitions",
            criteria: "It works",
            specs: vec![(
                "Spec",
                "The spec",
                vec![(
                    "Phase 1",
                    "The only phase",
                    1,
                    vec![("Implement main", "Write main.rs", vec!["src/main.rs"])],
                )],
            )],
        },
    )
    .await;

    let work_id = &spec_results[0].1[0].1[0];

    // Transition work: Ready -> InProgress
    let result = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": work_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-impl-1"}),
    )
    .await;
    assert_eq!(result["status"], "InProgress");

    // Verify the work is assigned
    let work_store = stores.works.read().unwrap();
    let work = &work_store[work_id];
    assert_eq!(work.status().to_string(), "InProgress");
    assert_eq!(work.assignee.as_deref(), Some("agent-impl-1"));
}
