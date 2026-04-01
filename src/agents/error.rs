use serde::{Deserialize, Serialize};

/// Typed agent errors for eyre downcasting at the executor level.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("assembled context ({tokens} tokens) exceeds model input limit ({limit} tokens)")]
    ContextOverflow { tokens: usize, limit: usize },

    #[error("exhausted parse retries (attempts: {attempts})")]
    ParseExhausted { attempts: u32 },
}

/// Serializable error kind for session state and Coordinator dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorKind {
    ContextOverflow,
    ParseExhausted,
    LlmTransient,
    ToolFailure,
    Unknown,
}

/// Classify an eyre::Report into an AgentErrorKind.
pub fn classify_error(err: &eyre::Report) -> AgentErrorKind {
    if let Some(agent_err) = err.downcast_ref::<AgentError>() {
        match agent_err {
            AgentError::ContextOverflow { .. } => AgentErrorKind::ContextOverflow,
            AgentError::ParseExhausted { .. } => AgentErrorKind::ParseExhausted,
        }
    } else {
        let err_str = format!("{err:?}");
        if err_str.contains("status: 429") || err_str.contains("timed out") {
            AgentErrorKind::LlmTransient
        } else {
            AgentErrorKind::Unknown
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use eyre::eyre;

    #[test]
    fn test_classify_context_overflow() {
        let err: eyre::Report = AgentError::ContextOverflow {
            tokens: 250000,
            limit: 200000,
        }
        .into();
        assert_eq!(classify_error(&err), AgentErrorKind::ContextOverflow);
    }

    #[test]
    fn test_classify_parse_exhausted() {
        let err: eyre::Report = AgentError::ParseExhausted { attempts: 3 }.into();
        assert_eq!(classify_error(&err), AgentErrorKind::ParseExhausted);
    }

    #[test]
    fn test_classify_transient_429() {
        let err = eyre!("HTTP error: status: 429 Too Many Requests");
        assert_eq!(classify_error(&err), AgentErrorKind::LlmTransient);
    }

    #[test]
    fn test_classify_transient_timeout() {
        let err = eyre!("request timed out");
        assert_eq!(classify_error(&err), AgentErrorKind::LlmTransient);
    }

    #[test]
    fn test_classify_unknown() {
        let err = eyre!("something unexpected");
        assert_eq!(classify_error(&err), AgentErrorKind::Unknown);
    }

    #[test]
    fn test_agent_error_kind_serde_roundtrip() {
        for kind in [
            AgentErrorKind::ContextOverflow,
            AgentErrorKind::ParseExhausted,
            AgentErrorKind::LlmTransient,
            AgentErrorKind::ToolFailure,
            AgentErrorKind::Unknown,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let restored: AgentErrorKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, restored);
        }
    }
}
