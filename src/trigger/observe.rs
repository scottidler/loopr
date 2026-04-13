use std::collections::HashMap;
use std::sync::RwLock;

use serde::Serialize;
use serde_json::Value;

use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;

/// Key-value filter conditions. Keys are field names (kebab-case accepted, converted to
/// snake_case for JSON lookup). The special key `"terminal"` is a computed property that
/// checks the FSM terminal status for the evaluated collection.
pub type Filter = HashMap<String, Value>;

// ─── ObservationCtx ──────────────────────────────────────────────────────────

/// Read-only view of runtime state for trigger evaluation.
///
/// This is the ONLY legal path for triggers to inspect state. No direct store
/// writes, no IPC calls, no filesystem access.
pub struct ObservationCtx<'a> {
    pub stores: &'a Stores,
    /// Events emitted since the last tick.
    pub event_bus: &'a [DaemonEvent],
    /// Current timestamp in milliseconds.
    pub now: i64,
}

impl<'a> ObservationCtx<'a> {
    pub fn new(stores: &'a Stores, event_bus: &'a [DaemonEvent], now: i64) -> Self {
        Self { stores, event_bus, now }
    }

    /// Get a record by collection and ID, returned as a JSON value.
    /// Returns `None` if the collection name is unknown or the record does not exist.
    pub fn get_record(&self, collection: &str, id: &str) -> Option<Value> {
        match collection {
            "work" => lookup_one(&self.stores.works, id),
            "plan" => lookup_one(&self.stores.plans, id),
            "spec" => lookup_one(&self.stores.specs, id),
            "phase" => lookup_one(&self.stores.phases, id),
            "bundle" => lookup_one(&self.stores.bundles, id),
            "tick" => lookup_one(&self.stores.ticks, id),
            "lock" => lookup_one(&self.stores.locks, id),
            "session" => lookup_one(&self.stores.agent_sessions, id),
            _ => None,
        }
    }

    /// Count records in a collection that match all filter conditions.
    ///
    /// The special filter key `"terminal"` is a computed property that fires the FSM
    /// `is_terminal` check rather than a direct field lookup.
    pub fn count(&self, collection: &str, filter: &Filter) -> usize {
        self.all_records(collection)
            .into_iter()
            .filter(|record| self.matches_filter(collection, record, filter))
            .count()
    }

    /// Get a u32 numeric field from a record. Accepts kebab-case or snake_case field names.
    pub fn get_field_u32(&self, collection: &str, id: &str, field: &str) -> Option<u32> {
        let record = self.get_record(collection, id)?;
        let key = normalize_field(field);
        record.get(&key).and_then(|v| v.as_u64()).map(|v| v as u32)
    }

    /// Get a timestamp field (milliseconds since epoch) from a record.
    pub fn get_field_timestamp(&self, collection: &str, id: &str, field: &str) -> Option<i64> {
        let record = self.get_record(collection, id)?;
        let key = normalize_field(field);
        record.get(&key).and_then(|v| v.as_i64())
    }

    /// Check whether any event in the current tick's event bus matches `event_type` and
    /// all conditions in `match_filter`.
    pub fn has_event(&self, event_type: &str, match_filter: &Filter) -> bool {
        self.event_bus.iter().any(|event| {
            event.event == event_type
                && match_filter
                    .iter()
                    .all(|(k, expected)| event.data.get(k).map(|v| flexible_eq(v, expected)).unwrap_or(false))
        })
    }

    /// Get all records in `child_collection` whose `parent_id` field equals `parent_id`.
    pub fn children(&self, parent_id: &str, child_collection: &str) -> Vec<Value> {
        self.all_records(child_collection)
            .into_iter()
            .filter(|record| {
                record
                    .get("parent_id")
                    .and_then(|v| v.as_str())
                    .map(|pid| pid == parent_id)
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Get all record IDs in a collection.
    pub fn record_ids(&self, collection: &str) -> Vec<String> {
        self.all_records(collection)
            .into_iter()
            .filter_map(|r| r.get("id").and_then(|v| v.as_str()).map(str::to_owned))
            .collect()
    }

    /// Count children of a parent that match all filter conditions.
    /// Handles the `terminal` computed filter the same way `count()` does.
    pub fn count_children(&self, parent_id: &str, child_collection: &str, filter: &Filter) -> usize {
        self.children(parent_id, child_collection)
            .into_iter()
            .filter(|child| self.matches_filter(child_collection, child, filter))
            .count()
    }

    /// Get a numeric field from a record as f64. Handles integer and float JSON values.
    /// Accepts kebab-case or snake_case field names.
    pub fn get_field_numeric(&self, collection: &str, id: &str, field: &str) -> Option<f64> {
        let record = self.get_record(collection, id)?;
        let key = normalize_field(field);
        record.get(&key).and_then(|v| v.as_f64())
    }

    // ─── Private helpers ─────────────────────────────────────────────────────

    fn all_records(&self, collection: &str) -> Vec<Value> {
        match collection {
            "work" => collect_all(&self.stores.works),
            "plan" => collect_all(&self.stores.plans),
            "spec" => collect_all(&self.stores.specs),
            "phase" => collect_all(&self.stores.phases),
            "bundle" => collect_all(&self.stores.bundles),
            "tick" => collect_all(&self.stores.ticks),
            "lock" => collect_all(&self.stores.locks),
            "session" => collect_all(&self.stores.agent_sessions),
            _ => Vec::new(),
        }
    }

    fn matches_filter(&self, collection: &str, record: &Value, filter: &Filter) -> bool {
        filter.iter().all(|(key, expected)| match key.as_str() {
            "terminal" => {
                let status_str = record.get("status").and_then(|v| v.as_str()).unwrap_or("");
                let fsm_state = pascal_to_kebab(status_str);
                let is_terminal = self.stores.fsm.is_terminal(collection, &fsm_state).unwrap_or(false);
                expected.as_bool().map(|b| is_terminal == b).unwrap_or(false)
            }
            _ => {
                let json_key = normalize_field(key);
                record.get(&json_key).map(|v| flexible_eq(v, expected)).unwrap_or(false)
            }
        })
    }
}

// ─── StateQueryRegistry ──────────────────────────────────────────────────────

/// Function type for built-in state queries.
/// Args: (ctx, collection, record_id, params) -> bool
pub type StateQueryFn = Box<dyn Fn(&ObservationCtx<'_>, &str, &str, &HashMap<String, Value>) -> bool + Send + Sync>;

/// Registry of named state query functions.
/// Built-ins are registered at startup; new queries can be added as Rust functions.
pub struct StateQueryRegistry {
    queries: HashMap<String, StateQueryFn>,
}

impl StateQueryRegistry {
    /// Build a registry populated with all 10 built-in state queries.
    pub fn with_builtins() -> Self {
        let mut r = Self {
            queries: HashMap::new(),
        };

        // all-children-terminal: all children of the scoped record are in terminal-statuses.
        r.register(
            "all-children-terminal",
            Box::new(|ctx, _collection, id, params| {
                let child_col = str_param(params, "child-collection");
                let terminal_statuses = string_list_param(params, "terminal-statuses");
                let children = ctx.children(id, child_col);
                !children.is_empty()
                    && children.iter().all(|c| {
                        let status = c.get("status").and_then(|v| v.as_str()).unwrap_or("");
                        terminal_statuses.iter().any(|ts| ts.eq_ignore_ascii_case(status))
                    })
            }),
        );

        // all-children-done: strict version - all children have status "done".
        r.register(
            "all-children-done",
            Box::new(|ctx, _collection, id, params| {
                let child_col = str_param(params, "child-collection");
                let children = ctx.children(id, child_col);
                !children.is_empty()
                    && children.iter().all(|c| {
                        c.get("status")
                            .and_then(|v| v.as_str())
                            .map(|s| s.eq_ignore_ascii_case("done"))
                            .unwrap_or(false)
                    })
            }),
        );

        // all-deps-terminal: all dep IDs (from dep-field on the scoped record) are terminal.
        // Dep records are assumed to be in the same collection as the scope.
        r.register(
            "all-deps-terminal",
            Box::new(|ctx, collection, id, params| {
                let dep_field = str_param(params, "dep-field");
                let terminal_statuses = string_list_param(params, "terminal-statuses");
                let json_key = normalize_field(dep_field);
                let record = match ctx.get_record(collection, id) {
                    Some(r) => r,
                    None => return false,
                };
                let dep_ids = match record.get(&json_key).and_then(|v| v.as_array()) {
                    Some(arr) => arr
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect::<Vec<_>>(),
                    None => return true, // No dep field: vacuously satisfied.
                };
                dep_ids.is_empty()
                    || dep_ids.iter().all(|dep_id| {
                        let dep = match ctx.get_record(collection, dep_id) {
                            Some(r) => r,
                            None => return false,
                        };
                        let status = dep.get("status").and_then(|v| v.as_str()).unwrap_or("");
                        terminal_statuses.iter().any(|ts| ts.eq_ignore_ascii_case(status))
                    })
            }),
        );

        // all-deps-done: all dep IDs have status "done".
        r.register(
            "all-deps-done",
            Box::new(|ctx, collection, id, params| {
                let dep_field = str_param(params, "dep-field");
                let json_key = normalize_field(dep_field);
                let record = match ctx.get_record(collection, id) {
                    Some(r) => r,
                    None => return false,
                };
                let dep_ids = match record.get(&json_key).and_then(|v| v.as_array()) {
                    Some(arr) => arr
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect::<Vec<_>>(),
                    None => return true, // No dep field: vacuously satisfied.
                };
                dep_ids.is_empty()
                    || dep_ids.iter().all(|dep_id| {
                        ctx.get_record(collection, dep_id)
                            .and_then(|dep| dep.get("status").and_then(|v| v.as_str()).map(|s| s.to_owned()))
                            .map(|s| s.eq_ignore_ascii_case("done"))
                            .unwrap_or(false)
                    })
            }),
        );

        // parent-active: the parent record of the scoped record has status "active".
        r.register(
            "parent-active",
            Box::new(|ctx, collection, id, _params| {
                let record = match ctx.get_record(collection, id) {
                    Some(r) => r,
                    None => return false,
                };
                let parent_id = match record.get("parent_id").and_then(|v| v.as_str()) {
                    Some(pid) => pid.to_owned(),
                    None => return false,
                };
                // Determine parent collection from scoped collection name.
                let parent_col = parent_collection(collection);
                ctx.get_record(parent_col, &parent_id)
                    .and_then(|p| p.get("status").and_then(|v| v.as_str()).map(str::to_owned))
                    .map(|s| s.eq_ignore_ascii_case("active"))
                    .unwrap_or(false)
            }),
        );

        // has-children: at least one child record exists in child-collection.
        r.register(
            "has-children",
            Box::new(|ctx, _collection, id, params| {
                let child_col = str_param(params, "child-collection");
                !ctx.children(id, child_col).is_empty()
            }),
        );

        // has-no-children: zero child records exist in child-collection. Complement of has-children.
        r.register(
            "has-no-children",
            Box::new(|ctx, _collection, id, params| {
                let child_col = str_param(params, "child-collection");
                ctx.children(id, child_col).is_empty()
            }),
        );

        // no-active-sessions: no Running or Paused agent sessions for this record ID.
        r.register(
            "no-active-sessions",
            Box::new(|ctx, _collection, id, _params| {
                ctx.stores
                    .agent_sessions
                    .read()
                    .ok()
                    .map(|sessions| {
                        !sessions.values().any(|s| {
                            let is_active = matches!(
                                s.status(),
                                crate::agents::AgentStatus::Starting
                                    | crate::agents::AgentStatus::Running
                                    | crate::agents::AgentStatus::WaitingForLlm
                                    | crate::agents::AgentStatus::Paused
                            );
                            let matches_record = s.work_id.as_deref() == Some(id) || s.target_id.as_deref() == Some(id);
                            is_active && matches_record
                        })
                    })
                    .unwrap_or(false)
            }),
        );

        // field-equals: a specific field on the record equals a given value.
        r.register(
            "field-equals",
            Box::new(|ctx, collection, id, params| {
                let field = str_param(params, "field");
                let expected = match params.get("value") {
                    Some(v) => v,
                    None => return false,
                };
                let record = match ctx.get_record(collection, id) {
                    Some(r) => r,
                    None => return false,
                };
                let json_key = normalize_field(field);
                record.get(&json_key).map(|v| flexible_eq(v, expected)).unwrap_or(false)
            }),
        );

        // field-is-true: a boolean field on the record is true.
        r.register(
            "field-is-true",
            Box::new(|ctx, collection, id, params| {
                let field = str_param(params, "field");
                let record = match ctx.get_record(collection, id) {
                    Some(r) => r,
                    None => return false,
                };
                let json_key = normalize_field(field);
                record.get(&json_key).and_then(|v| v.as_bool()).unwrap_or(false)
            }),
        );

        r
    }

    /// Register a custom state query function.
    pub fn register(&mut self, name: &str, f: StateQueryFn) {
        self.queries.insert(name.to_owned(), f);
    }

    /// Evaluate a named state query. Returns false if the query name is unknown.
    pub fn evaluate(
        &self,
        name: &str,
        ctx: &ObservationCtx<'_>,
        collection: &str,
        id: &str,
        params: &HashMap<String, Value>,
    ) -> bool {
        match self.queries.get(name) {
            Some(f) => f(ctx, collection, id, params),
            None => {
                tracing::warn!("unknown state query: '{}'", name);
                false
            }
        }
    }

    /// Return the names of all registered queries.
    pub fn names(&self) -> Vec<&str> {
        self.queries.keys().map(String::as_str).collect()
    }
}

// ─── GuardConditionRegistry ──────────────────────────────────────────────────

/// Function type for guard conditions.
/// Args: (ctx, collection, record_id) -> bool
pub type GuardConditionFn = Box<dyn Fn(&ObservationCtx<'_>, &str, &str) -> bool + Send + Sync>;

/// Registry of named guard condition functions.
/// Guards evaluate synchronously on FSM transition attempts and must be fast.
pub struct GuardConditionRegistry {
    conditions: HashMap<String, GuardConditionFn>,
}

impl GuardConditionRegistry {
    /// Build a registry with all built-in guard conditions.
    pub fn with_builtins() -> Self {
        let mut r = Self {
            conditions: HashMap::new(),
        };

        // no-active-sessions: no Running/Paused agent sessions for this record.
        r.register(
            "no-active-sessions",
            Box::new(|ctx, _collection, id| {
                ctx.stores
                    .agent_sessions
                    .read()
                    .ok()
                    .map(|sessions| {
                        !sessions.values().any(|s| {
                            let is_active = matches!(
                                s.status(),
                                crate::agents::AgentStatus::Starting
                                    | crate::agents::AgentStatus::Running
                                    | crate::agents::AgentStatus::WaitingForLlm
                                    | crate::agents::AgentStatus::Paused
                            );
                            let matches_record = s.work_id.as_deref() == Some(id) || s.target_id.as_deref() == Some(id);
                            is_active && matches_record
                        })
                    })
                    .unwrap_or(false)
            }),
        );

        // deps-satisfied: all dependencies of the record are in a terminal state.
        r.register(
            "deps-satisfied",
            Box::new(|ctx, collection, id| {
                let record = match ctx.get_record(collection, id) {
                    Some(r) => r,
                    None => return false,
                };
                let dep_ids = match record.get("dependencies").and_then(|v| v.as_array()) {
                    Some(arr) => arr
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect::<Vec<_>>(),
                    None => return true, // No dependencies field: vacuously satisfied.
                };
                dep_ids.iter().all(|dep_id| {
                    ctx.get_record(collection, dep_id)
                        .and_then(|dep| dep.get("status").and_then(|v| v.as_str()).map(str::to_owned))
                        .map(|s| {
                            ctx.stores
                                .fsm
                                .is_terminal(collection, &pascal_to_kebab(&s))
                                .unwrap_or(false)
                        })
                        .unwrap_or(false)
                })
            }),
        );

        // validation-passed: a ValidationReport with verdict=pass exists for this record.
        // Note: ValidationReports are stored in TaskStore, not in-memory. This guard always
        // returns true until Phase 4 wires it into the FSM interpreter with TaskStore access.
        r.register(
            "validation-passed",
            Box::new(|_ctx, _collection, _id| {
                // TODO(Phase 4): query TaskStore for ValidationReport where target_id == id
                // and verdict == "pass". For now, permissive (don't block transitions).
                true
            }),
        );

        // all-ac-passing: all acceptance criteria on the record are satisfied.
        // Note: There is no per-criterion satisfaction tracking in the current domain model.
        // This guard is vacuously true when the AC list is absent or empty (no criteria
        // to fail - same semantics as deps-satisfied on an empty deps list).
        // TODO(future): wire to a real AC satisfaction tracker when per-criterion status exists.
        r.register(
            "all-ac-passing",
            Box::new(|ctx, collection, id| {
                let record = match ctx.get_record(collection, id) {
                    Some(r) => r,
                    None => return false,
                };
                // Permissive stub: always true when record exists. An empty AC list is
                // vacuously satisfied (no criteria to fail), and a non-empty list cannot
                // be evaluated without per-criterion satisfaction tracking, which does not
                // yet exist in the domain model. Replace with a real check when it does.
                let _ = record.get("acceptance_criteria");
                true
            }),
        );

        r
    }

    /// Register a custom guard condition function.
    pub fn register(&mut self, name: &str, f: GuardConditionFn) {
        self.conditions.insert(name.to_owned(), f);
    }

    /// Evaluate a named guard condition. Returns false if the name is unknown.
    pub fn evaluate(&self, name: &str, ctx: &ObservationCtx<'_>, collection: &str, id: &str) -> bool {
        match self.conditions.get(name) {
            Some(f) => f(ctx, collection, id),
            None => {
                tracing::warn!("unknown guard condition: '{}'", name);
                false
            }
        }
    }

    /// Return the names of all registered conditions.
    pub fn names(&self) -> Vec<&str> {
        self.conditions.keys().map(String::as_str).collect()
    }
}

// ─── Internal utilities ──────────────────────────────────────────────────────

/// Look up a single record by ID from a std::sync::RwLock-protected HashMap.
fn lookup_one<T: Serialize>(store: &RwLock<HashMap<String, T>>, id: &str) -> Option<Value> {
    store.read().ok()?.get(id).and_then(|r| serde_json::to_value(r).ok())
}

/// Collect all records from a std::sync::RwLock-protected HashMap as JSON values.
fn collect_all<T: Serialize>(store: &RwLock<HashMap<String, T>>) -> Vec<Value> {
    store
        .read()
        .ok()
        .map(|m| m.values().filter_map(|r| serde_json::to_value(r).ok()).collect())
        .unwrap_or_default()
}

/// Normalize a field name from kebab-case to snake_case for JSON key lookup.
fn normalize_field(field: &str) -> String {
    field.replace('-', "_")
}

/// Flexible equality: string values are compared case-insensitively; other types use `==`.
pub(crate) fn flexible_eq(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::String(a), Value::String(e)) => a.eq_ignore_ascii_case(e),
        _ => actual == expected,
    }
}

/// Convert PascalCase status string to kebab-case for FSM lookup.
/// "Done" -> "done", "InProgress" -> "in-progress", "InReview" -> "in-review"
fn pascal_to_kebab(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('-');
        }
        result.push(ch.to_ascii_lowercase());
    }
    result
}

/// Return the parent collection name for a given child collection.
/// Works belong to phases, phases belong to specs, specs belong to plans.
fn parent_collection(collection: &str) -> &str {
    match collection {
        "work" => "phase",
        "phase" => "spec",
        "spec" => "plan",
        _ => collection,
    }
}

/// Extract a string parameter value from a params map. Returns "" if missing or not a string.
fn str_param<'a>(params: &'a HashMap<String, Value>, key: &str) -> &'a str {
    params.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

/// Extract a list of strings from a params map. Returns empty Vec if missing or not an array.
fn string_list_param(params: &HashMap<String, Value>, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}
