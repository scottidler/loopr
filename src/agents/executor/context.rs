// Reserved for ExecutionContext<'a> - the bundling struct described in the design doc.
// Currently, action handlers receive AgentContext directly from execute_action().
// When handler signatures are refactored to reduce parameter lists, ExecutionContext
// will be introduced here to bundle Stores, AgentContext, EventTx, and WorktreeManager.
