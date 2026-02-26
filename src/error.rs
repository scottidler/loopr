use thiserror::Error;

#[derive(Error, Debug)]
pub enum LooprError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),

    #[error("invalid transition from {from} to {to} for role {role}")]
    InvalidTransition { from: String, to: String, role: String },
}

pub type Result<T> = std::result::Result<T, LooprError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: LooprError = io_err.into();
        assert!(matches!(err, LooprError::Io(_)));
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn test_serde_json_error_conversion() {
        let json_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let err: LooprError = json_err.into();
        assert!(matches!(err, LooprError::SerdeJson(_)));
    }

    #[test]
    fn test_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LooprError>();
    }
}
