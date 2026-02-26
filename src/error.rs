use thiserror::Error;

#[derive(Error, Debug)]
pub enum LooprError {
    #[error("invalid transition from {from} to {to} for role {role}")]
    InvalidTransition {
        from: String,
        to: String,
        role: String,
    },

    #[error("record not found: {collection}/{id}")]
    NotFound { collection: String, id: String },

    #[error("stale bundle: base_tick_id {base_tick_id} is behind latest published tick {latest_tick_id}")]
    StaleBundleBase {
        base_tick_id: String,
        latest_tick_id: String,
    },

    #[error("invariant violated: {0}")]
    InvariantViolation(String),

    #[error("duplicate lock: resource {resource} already locked by {holder_id}")]
    DuplicateLock {
        resource: String,
        holder_id: String,
    },

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("daemon error: {0}")]
    Daemon(String),

    #[error("worktree error: {0}")]
    Worktree(String),

    #[error("config error: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, LooprError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invalid_transition_error_display() {
        let err = LooprError::InvalidTransition {
            from: "Draft".to_string(),
            to: "Done".to_string(),
            role: "Implementer".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "invalid transition from Draft to Done for role Implementer"
        );
    }

    #[test]
    fn test_not_found_error_display() {
        let err = LooprError::NotFound {
            collection: "plans".to_string(),
            id: "plan-123".to_string(),
        };
        assert_eq!(err.to_string(), "record not found: plans/plan-123");
    }

    #[test]
    fn test_stale_bundle_error_display() {
        let err = LooprError::StaleBundleBase {
            base_tick_id: "tick-1".to_string(),
            latest_tick_id: "tick-5".to_string(),
        };
        assert!(err.to_string().contains("stale bundle"));
        assert!(err.to_string().contains("tick-1"));
        assert!(err.to_string().contains("tick-5"));
    }

    #[test]
    fn test_invariant_violation_display() {
        let err = LooprError::InvariantViolation("description must not be empty".to_string());
        assert_eq!(
            err.to_string(),
            "invariant violated: description must not be empty"
        );
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: LooprError = io_err.into();
        assert!(matches!(err, LooprError::Io(_)));
    }
}
