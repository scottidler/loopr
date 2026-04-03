//! YAML manifest support for deterministic plan injection.
//!
//! When `--plan` points to a `.yaml` or `.yml` file, the manifest is deserialized
//! directly into Plan/Spec/Phase/Work TaskStore records, bypassing LLM decomposition.

use std::collections::HashMap;

use serde::Deserialize;

use crate::domain::phase::Phase;
use crate::domain::plan::Plan;
use crate::domain::spec::Spec;
use crate::domain::work::Work;
use crate::id;

/// Top-level manifest structure.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub goal: String,
    pub plan: ManifestPlan,
}

#[derive(Debug, Deserialize)]
pub struct ManifestPlan {
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub spec: ManifestSpec,
}

#[derive(Debug, Deserialize)]
pub struct ManifestSpec {
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub phases: Vec<ManifestPhase>,
}

#[derive(Debug, Deserialize)]
pub struct ManifestPhase {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "validation-commands")]
    pub validation_commands: Vec<String>,
    pub works: Vec<ManifestWork>,
}

#[derive(Debug, Deserialize)]
pub struct ManifestWork {
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub resource_tags: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
}

/// Resolved manifest ready for TaskStore insertion.
pub struct ResolvedManifest {
    pub goal: String,
    pub plan: Plan,
    pub spec: Spec,
    pub phases: Vec<Phase>,
    pub works: Vec<Work>,
}

/// Parse a YAML manifest string and resolve logical keys to real IDs.
pub fn parse_manifest(yaml: &str) -> eyre::Result<ResolvedManifest> {
    let manifest: Manifest = serde_yaml::from_str(yaml)?;

    // Create Plan
    let plan = Plan::new(
        manifest.plan.title.clone(),
        manifest.plan.description.clone(),
        String::new(),
    );

    // Create Spec under the Plan
    let spec = Spec::new(
        plan.id.clone(),
        manifest.plan.spec.title.clone(),
        manifest.plan.spec.description.clone(),
    );

    let mut all_phases = Vec::new();
    let mut all_works = Vec::new();
    // Map logical keys to generated Work IDs for dependency resolution
    let mut key_to_id: HashMap<String, String> = HashMap::new();

    for (order, manifest_phase) in manifest.plan.spec.phases.iter().enumerate() {
        let mut phase = Phase::new(
            spec.id.clone(),
            manifest_phase.title.clone(),
            manifest_phase.description.clone(),
            (order + 1) as u32,
        );
        phase.validation_commands = manifest_phase.validation_commands.clone();

        // First pass: create Work records and build the key -> ID map
        for mw in &manifest_phase.works {
            let mut work = Work::new(phase.id.clone(), mw.title.clone(), mw.description.clone());
            work.resource_tags = mw.resource_tags.clone();
            work.acceptance_criteria = mw.acceptance_criteria.clone();
            // Mark as Ready (not Draft) so the worker pool can pick them up
            work.force_status(crate::domain::work::WorkStatus::Ready);
            work.updated_at = id::now_millis();
            key_to_id.insert(mw.key.clone(), work.id.clone());
            all_works.push((work, mw.dependencies.clone()));
        }

        all_phases.push(phase);
    }

    // Second pass: resolve dependency keys to real IDs
    let resolved_works: Vec<Work> = all_works
        .into_iter()
        .map(|(mut work, dep_keys)| {
            work.dependencies = dep_keys
                .iter()
                .filter_map(|key| {
                    let resolved = key_to_id.get(key).cloned();
                    if resolved.is_none() {
                        log::warn!("manifest: unresolved dependency key '{}' in work '{}'", key, work.title);
                    }
                    resolved
                })
                .collect();
            work
        })
        .collect();

    Ok(ResolvedManifest {
        goal: manifest.goal,
        plan,
        spec,
        phases: all_phases,
        works: resolved_works,
    })
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_manifest_basic() {
        let yaml = r#"
goal: "Build a todo app"
plan:
  title: "Todo App"
  description: "A simple todo app"
  spec:
    title: "Core Implementation"
    phases:
      - title: "Phase 1"
        works:
          - key: "model"
            title: "Create model"
            description: "Build the data model"
            resource_tags: ["todo.py"]
            acceptance_criteria: ["model works"]
          - key: "cli"
            title: "Create CLI"
            description: "Build the CLI"
            dependencies: ["model"]
            acceptance_criteria: ["cli works"]
"#;
        let resolved = parse_manifest(yaml).unwrap();
        assert_eq!(resolved.goal, "Build a todo app");
        assert_eq!(resolved.works.len(), 2);
        // Second work should depend on first
        assert_eq!(resolved.works[1].dependencies.len(), 1);
        assert_eq!(resolved.works[1].dependencies[0], resolved.works[0].id);
        // Works should be Ready status
        assert_eq!(resolved.works[0].status(), crate::domain::work::WorkStatus::Ready);
    }

    #[test]
    fn test_parse_manifest_unresolved_dep_is_skipped() {
        let yaml = r#"
goal: "Test"
plan:
  title: "Test"
  spec:
    title: "Spec"
    phases:
      - title: "Phase 1"
        works:
          - key: "a"
            title: "Work A"
            dependencies: ["nonexistent"]
"#;
        let resolved = parse_manifest(yaml).unwrap();
        assert!(resolved.works[0].dependencies.is_empty());
    }

    #[test]
    fn test_parse_manifest_multiple_phases() {
        let yaml = r#"
goal: "Multi-phase"
plan:
  title: "Multi"
  spec:
    title: "Spec"
    phases:
      - title: "Phase 1"
        works:
          - key: "a"
            title: "Work A"
      - title: "Phase 2"
        works:
          - key: "b"
            title: "Work B"
            dependencies: ["a"]
"#;
        let resolved = parse_manifest(yaml).unwrap();
        assert_eq!(resolved.phases.len(), 2);
        assert_eq!(resolved.phases[0].order, 1);
        assert_eq!(resolved.phases[1].order, 2);
        // Cross-phase dependency resolves
        assert_eq!(resolved.works[1].dependencies.len(), 1);
        assert_eq!(resolved.works[1].dependencies[0], resolved.works[0].id);
    }

    #[test]
    fn test_parse_manifest_with_validation_commands() {
        let yaml = r#"
goal: "Test validation"
plan:
  title: "Plan"
  spec:
    title: "Spec"
    phases:
      - title: "Phase 1"
        validation-commands:
          - "python -c 'import todo'"
        works:
          - key: "a"
            title: "Work A"
      - title: "Phase 2"
        works:
          - key: "b"
            title: "Work B"
"#;
        let resolved = parse_manifest(yaml).unwrap();
        assert_eq!(resolved.phases[0].validation_commands, vec!["python -c 'import todo'"]);
        assert!(resolved.phases[1].validation_commands.is_empty());
    }
}
