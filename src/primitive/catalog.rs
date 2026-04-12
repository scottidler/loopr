pub mod context;
pub mod decompose;
pub mod gitaudit;
pub mod mutation;
pub mod reconcile;
pub mod record;
pub mod scoring;

use super::registry::PrimitiveRegistry;

/// Register all catalog primitives into the given registry.
pub fn register_all(registry: &mut PrimitiveRegistry) -> eyre::Result<()> {
    // Phase 2: Pure query primitives
    registry.register(Box::new(record::QueryRecords))?;
    registry.register(Box::new(record::GetRecord))?;
    registry.register(Box::new(reconcile::DetectGoalComplete))?;
    registry.register(Box::new(reconcile::CheckThreshold))?;
    registry.register(Box::new(reconcile::CheckRatio))?;
    registry.register(Box::new(decompose::ClassifyTier))?;
    registry.register(Box::new(context::SelectLearnings))?;
    registry.register(Box::new(context::CompactContext))?;
    registry.register(Box::new(scoring::ComputeScore))?;
    registry.register(Box::new(gitaudit::AuditTickShas))?;
    registry.register(Box::new(gitaudit::AuditMergeAncestry))?;

    // Phase 3: Record mutation primitives
    registry.register(Box::new(mutation::CreateRecord))?;
    registry.register(Box::new(mutation::UpdateRecord))?;
    registry.register(Box::new(mutation::TransitionRecord))?;
    registry.register(Box::new(mutation::CreateWork))?;
    registry.register(Box::new(mutation::TransitionWork))?;
    registry.register(Box::new(mutation::OverrideWork))?;
    registry.register(Box::new(mutation::CreateBundle))?;
    registry.register(Box::new(mutation::CreateTick))?;
    registry.register(Box::new(mutation::CreateLearning))?;
    Ok(())
}
