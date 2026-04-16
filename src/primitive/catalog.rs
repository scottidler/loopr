pub mod agent;
pub mod bundle;
pub mod conflict;
pub mod context;
pub mod decompose;
pub mod event;
pub mod gitaudit;
pub mod integration;
pub mod lock;
pub mod mutation;
pub mod reconcile;
pub mod record;
pub mod scoring;
pub mod sweep;
pub mod tooling;
pub mod work;
pub mod worktree;

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

    // Phase 4: Agent and worktree primitives
    registry.register(Box::new(agent::SpawnAgent))?;
    registry.register(Box::new(agent::StopAgent))?;
    registry.register(Box::new(agent::PauseAgent))?;
    registry.register(Box::new(agent::ResumeAgent))?;
    registry.register(Box::new(agent::InjectContext))?;
    registry.register(Box::new(worktree::CreateWorktree))?;
    registry.register(Box::new(worktree::CleanupWorktree))?;
    registry.register(Box::new(worktree::DeleteAgentBranch))?;
    registry.register(Box::new(worktree::RefreshWorktree))?;

    // Phase 5: Integration and complex primitives
    registry.register(Box::new(integration::IntegrateTick))?;
    registry.register(Box::new(integration::MergeBranches))?;
    registry.register(Box::new(integration::RunValidation))?;
    registry.register(Box::new(integration::CreateIntegrationBranch))?;
    registry.register(Box::new(integration::MergeIntegrationToMain))?;
    registry.register(Box::new(integration::DeleteIntegrationBranch))?;
    registry.register(Box::new(sweep::PromoteRecord))?;
    registry.register(Box::new(sweep::CompleteRecord))?;
    registry.register(Box::new(sweep::SweepToDone))?;
    registry.register(Box::new(sweep::SweepStuckInreview))?;
    registry.register(Box::new(sweep::Escalate))?;
    registry.register(Box::new(decompose::Decompose))?;
    registry.register(Box::new(decompose::ValidateDocument))?;
    registry.register(Box::new(decompose::EvaluateCoverage))?;
    registry.register(Box::new(decompose::RatifyHierarchy))?;
    registry.register(Box::new(decompose::AbandonChildren))?;
    registry.register(Box::new(decompose::ReDecompose))?;
    registry.register(Box::new(work::ClaimNextWork))?;
    registry.register(Box::new(work::ResetWork))?;
    registry.register(Box::new(work::RetryWork))?;
    registry.register(Box::new(work::AbandonWork))?;
    registry.register(Box::new(work::IncrementFailureCount))?;
    registry.register(Box::new(work::IncrementAttemptCount))?;
    registry.register(Box::new(bundle::RejectBundle))?;
    registry.register(Box::new(bundle::SupersedeBundles))?;
    registry.register(Box::new(lock::AcquireLock))?;
    registry.register(Box::new(lock::ReleaseLock))?;
    registry.register(Box::new(context::BuildContext))?;
    registry.register(Box::new(context::BuildStateSummary))?;
    registry.register(Box::new(gitaudit::AuditBranches))?;
    registry.register(Box::new(conflict::CombineConflictingWorks))?;
    registry.register(Box::new(event::EmitEvent))?;
    registry.register(Box::new(tooling::RegisterValidationTools))?;
    Ok(())
}
