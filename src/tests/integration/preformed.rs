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
            criteria: "1. CRUD operations work\n2. Persistence to JSON\n3. Filter by status\n4. All tests pass",
            specs: vec![(
                "Todo App Technical Spec",
                vec![
                    (
                        "Phase 1: Data Model & Storage",
                        1,
                        vec![
                            ("Todo struct", vec!["src/model.rs"]),
                            ("JSON storage", vec!["src/storage.rs"]),
                        ],
                    ),
                    (
                        "Phase 2: CRUD Operations",
                        2,
                        vec![
                            ("Add command", vec!["src/commands.rs"]),
                            ("List command", vec!["src/commands.rs"]),
                            ("Done command", vec!["src/commands.rs"]),
                            ("Delete command", vec!["src/commands.rs"]),
                        ],
                    ),
                    (
                        "Phase 3: Filtering & CLI",
                        3,
                        vec![
                            ("Filter by status", vec!["src/commands.rs"]),
                            ("CLI entry point", vec!["src/main.rs"]),
                        ],
                    ),
                ],
            )],
        },
    )
    .await;

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
            criteria: "1. Basic arithmetic (+, -, *, /)\n2. Expression parsing with operator precedence\n3. REPL mode\n4. Error handling for division by zero\n5. All tests pass",
            specs: vec![(
                "Calculator Technical Spec",
                vec![
                    (
                        "Phase 1: Arithmetic Engine",
                        1,
                        vec![
                            ("Arithmetic ops", vec!["src/engine.rs"]),
                            ("Error handling", vec!["src/engine.rs"]),
                        ],
                    ),
                    (
                        "Phase 2: Expression Parser",
                        2,
                        vec![
                            ("Tokenizer", vec!["src/parser.rs"]),
                            ("Parser", vec!["src/parser.rs"]),
                            ("Evaluator", vec!["src/parser.rs"]),
                        ],
                    ),
                    (
                        "Phase 3: REPL & CLI",
                        3,
                        vec![
                            ("REPL loop", vec!["src/repl.rs"]),
                            ("CLI entry point", vec!["src/main.rs"]),
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
            criteria: "It works",
            specs: vec![(
                "Spec",
                vec![("Phase 1", 1, vec![("Implement main", vec!["src/main.rs"])])],
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
        json!({"id": work_id, "target-status": "InProgress", "role": "coordinator", "assignee": "agent-impl-1"}),
    )
    .await;
    assert_eq!(result["status"], "InProgress");

    // Verify the work is assigned
    let work_store = stores.works.read().unwrap();
    let work = &work_store[work_id];
    assert_eq!(work.status().to_string(), "InProgress");
    assert_eq!(work.assignee.as_deref(), Some("agent-impl-1"));
}
