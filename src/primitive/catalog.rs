pub mod context;
pub mod decompose;
pub mod gitaudit;
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
    Ok(())
}
