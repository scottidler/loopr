use std::fs;
use std::sync::OnceLock;

use log::info;

// Compile-time defaults — baked into the binary data segment, zero runtime cost
const DEFAULT_COORDINATOR: &str = include_str!("../prompts/coordinator.pmt");
const DEFAULT_IMPLEMENTER: &str = include_str!("../prompts/implementer.pmt");
const DEFAULT_REVIEWER: &str = include_str!("../prompts/reviewer.pmt");
const DEFAULT_RESEARCHER: &str = include_str!("../prompts/researcher.pmt");
const DEFAULT_VALIDATOR_SCHEMA: &str = include_str!("../prompts/validator-schema.pmt");
const DEFAULT_VALIDATOR_PLAN: &str = include_str!("../prompts/validator-plan.pmt");
const DEFAULT_VALIDATOR_SPEC: &str = include_str!("../prompts/validator-spec.pmt");
const DEFAULT_VALIDATOR_PHASE: &str = include_str!("../prompts/validator-phase.pmt");
const DEFAULT_GENERATION_PLAN: &str = include_str!("../prompts/generation-plan.pmt");
const DEFAULT_GENERATION_SPEC: &str = include_str!("../prompts/generation-spec.pmt");
const DEFAULT_GENERATION_PHASE: &str = include_str!("../prompts/generation-phase.pmt");
const DEFAULT_GENERATION_WORKITEM: &str = include_str!("../prompts/generation-workitem.pmt");

pub struct PromptStore {
    pub coordinator: String,
    pub implementer: String,
    pub reviewer: String,
    pub researcher: String,
    pub validator_schema: String,
    pub validator_plan: String,
    pub validator_spec: String,
    pub validator_phase: String,
    pub generation_plan: String,
    pub generation_spec: String,
    pub generation_phase: String,
    pub generation_workitem: String,
}

static STORE: OnceLock<PromptStore> = OnceLock::new();

/// Initialize the global prompt store. Call once at startup after config load.
/// Checks ~/.config/loopr/prompts/ for overrides.
pub fn init() {
    let overrides_dir = dirs::config_dir().map(|d| d.join("loopr/prompts"));

    let load = |filename: &str, default: &str| -> String {
        if let Some(ref dir) = overrides_dir {
            let path = dir.join(filename);
            match fs::read_to_string(&path) {
                Ok(content) if !content.trim().is_empty() => {
                    info!("prompt override loaded: {}", path.display());
                    return content;
                }
                Ok(_) => {
                    // Empty file — fall back to default
                }
                Err(_) => {
                    // File not found — expected, use default
                }
            }
        }
        default.to_string()
    };

    // OnceLock::set returns Err if already initialized — harmless no-op
    let _ = STORE.set(PromptStore {
        coordinator: load("coordinator.pmt", DEFAULT_COORDINATOR),
        implementer: load("implementer.pmt", DEFAULT_IMPLEMENTER),
        reviewer: load("reviewer.pmt", DEFAULT_REVIEWER),
        researcher: load("researcher.pmt", DEFAULT_RESEARCHER),
        validator_schema: load("validator-schema.pmt", DEFAULT_VALIDATOR_SCHEMA),
        validator_plan: load("validator-plan.pmt", DEFAULT_VALIDATOR_PLAN),
        validator_spec: load("validator-spec.pmt", DEFAULT_VALIDATOR_SPEC),
        validator_phase: load("validator-phase.pmt", DEFAULT_VALIDATOR_PHASE),
        generation_plan: load("generation-plan.pmt", DEFAULT_GENERATION_PLAN),
        generation_spec: load("generation-spec.pmt", DEFAULT_GENERATION_SPEC),
        generation_phase: load("generation-phase.pmt", DEFAULT_GENERATION_PHASE),
        generation_workitem: load("generation-workitem.pmt", DEFAULT_GENERATION_WORKITEM),
    });
}

/// Initialize with compiled-in defaults only (no filesystem). For tests.
pub fn init_defaults() {
    let _ = STORE.set(PromptStore {
        coordinator: DEFAULT_COORDINATOR.to_string(),
        implementer: DEFAULT_IMPLEMENTER.to_string(),
        reviewer: DEFAULT_REVIEWER.to_string(),
        researcher: DEFAULT_RESEARCHER.to_string(),
        validator_schema: DEFAULT_VALIDATOR_SCHEMA.to_string(),
        validator_plan: DEFAULT_VALIDATOR_PLAN.to_string(),
        validator_spec: DEFAULT_VALIDATOR_SPEC.to_string(),
        validator_phase: DEFAULT_VALIDATOR_PHASE.to_string(),
        generation_plan: DEFAULT_GENERATION_PLAN.to_string(),
        generation_spec: DEFAULT_GENERATION_SPEC.to_string(),
        generation_phase: DEFAULT_GENERATION_PHASE.to_string(),
        generation_workitem: DEFAULT_GENERATION_WORKITEM.to_string(),
    });
}

/// Get the global prompt store. Panics if init() was not called.
pub fn store() -> &'static PromptStore {
    STORE
        .get()
        .expect("prompts::init() must be called before prompts::store()")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_defaults_non_empty() {
        init_defaults();
        let s = store();
        assert!(!s.coordinator.is_empty());
        assert!(!s.implementer.is_empty());
        assert!(!s.reviewer.is_empty());
        assert!(!s.researcher.is_empty());
        assert!(!s.validator_schema.is_empty());
        assert!(!s.validator_plan.is_empty());
        assert!(!s.validator_spec.is_empty());
        assert!(!s.validator_phase.is_empty());
        assert!(!s.generation_plan.is_empty());
        assert!(!s.generation_spec.is_empty());
        assert!(!s.generation_phase.is_empty());
        assert!(!s.generation_workitem.is_empty());
    }

    #[test]
    fn test_defaults_match_include_str() {
        init_defaults();
        let s = store();
        assert_eq!(s.coordinator, DEFAULT_COORDINATOR);
        assert_eq!(s.implementer, DEFAULT_IMPLEMENTER);
        assert_eq!(s.reviewer, DEFAULT_REVIEWER);
        assert_eq!(s.researcher, DEFAULT_RESEARCHER);
        assert_eq!(s.validator_schema, DEFAULT_VALIDATOR_SCHEMA);
        assert_eq!(s.validator_plan, DEFAULT_VALIDATOR_PLAN);
        assert_eq!(s.validator_spec, DEFAULT_VALIDATOR_SPEC);
        assert_eq!(s.validator_phase, DEFAULT_VALIDATOR_PHASE);
        assert_eq!(s.generation_plan, DEFAULT_GENERATION_PLAN);
        assert_eq!(s.generation_spec, DEFAULT_GENERATION_SPEC);
        assert_eq!(s.generation_phase, DEFAULT_GENERATION_PHASE);
        assert_eq!(s.generation_workitem, DEFAULT_GENERATION_WORKITEM);
    }

    #[test]
    fn test_placeholder_assertions() {
        init_defaults();
        let s = store();
        // Researcher prompt must contain {query} placeholder
        assert!(s.researcher.contains("{query}"));
        // Validator templates must contain their placeholders
        assert!(s.validator_plan.contains("{title}"));
        assert!(s.validator_plan.contains("{description}"));
        assert!(s.validator_plan.contains("{acceptance_criteria}"));
        assert!(s.validator_plan.contains("{schema}"));
        assert!(s.validator_spec.contains("{title}"));
        assert!(s.validator_spec.contains("{plan_title}"));
        assert!(s.validator_spec.contains("{schema}"));
        assert!(s.validator_phase.contains("{title}"));
        assert!(s.validator_phase.contains("{order}"));
        assert!(s.validator_phase.contains("{spec_title}"));
        assert!(s.validator_phase.contains("{schema}"));
    }

    #[test]
    fn test_override_from_temp_dir() {
        // This test uses a fresh OnceLock via a subprocess-like pattern.
        // Since OnceLock can only be set once per process, we test the load closure directly.
        let dir = std::env::temp_dir().join(format!("loopr-pmt-override-{}", crate::id::generate_id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("coordinator.pmt"), "CUSTOM COORDINATOR PROMPT").unwrap();

        let overrides_dir = Some(dir.clone());
        let load = |filename: &str, default: &str| -> String {
            if let Some(ref dir) = overrides_dir {
                let path = dir.join(filename);
                match fs::read_to_string(&path) {
                    Ok(content) if !content.trim().is_empty() => {
                        return content;
                    }
                    _ => {}
                }
            }
            default.to_string()
        };

        assert_eq!(load("coordinator.pmt", "default"), "CUSTOM COORDINATOR PROMPT");
        assert_eq!(load("implementer.pmt", "default"), "default");
    }

    #[test]
    fn test_empty_override_falls_back() {
        let dir = std::env::temp_dir().join(format!("loopr-pmt-empty-{}", crate::id::generate_id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("coordinator.pmt"), "   \n  ").unwrap();

        let overrides_dir = Some(dir.clone());
        let load = |filename: &str, default: &str| -> String {
            if let Some(ref dir) = overrides_dir {
                let path = dir.join(filename);
                match fs::read_to_string(&path) {
                    Ok(content) if !content.trim().is_empty() => {
                        return content;
                    }
                    _ => {}
                }
            }
            default.to_string()
        };

        assert_eq!(load("coordinator.pmt", "default value"), "default value");
    }
}
