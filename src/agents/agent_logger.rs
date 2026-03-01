use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::agents::AgentType;

/// Per-agent logger that writes to both a dedicated log file and the main daemon log.
///
/// Each agent session gets its own log file at:
/// `~/.local/share/loopr/logs/agents/agent-<type>-<session_id>.log`
///
/// Every log method writes a formatted line to the per-agent file AND calls
/// the corresponding `log::*!` macro with a `[session_id]` prefix.
pub struct AgentLogger {
    session_id: String,
    agent_type: AgentType,
    writer: Mutex<BufWriter<File>>,
    file_path: PathBuf,
}

impl AgentLogger {
    /// Create a new AgentLogger, opening the per-agent log file.
    pub fn new(agent_type: AgentType, session_id: &str) -> eyre::Result<Self> {
        let log_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("loopr")
            .join("logs")
            .join("agents");

        fs::create_dir_all(&log_dir)?;

        let file_name = format!("agent-{}-{}.log", agent_type, session_id);
        let file_path = log_dir.join(&file_name);

        let file = OpenOptions::new().create(true).append(true).open(&file_path)?;

        Ok(Self {
            session_id: session_id.to_string(),
            agent_type,
            writer: Mutex::new(BufWriter::new(file)),
            file_path,
        })
    }

    /// Create an AgentLogger from a pre-opened file (for tests).
    #[doc(hidden)]
    pub fn _new_for_test(agent_type: AgentType, session_id: &str, file: File, file_path: PathBuf) -> Self {
        Self {
            session_id: session_id.to_string(),
            agent_type,
            writer: Mutex::new(BufWriter::new(file)),
            file_path,
        }
    }

    /// Path to the per-agent log file.
    pub fn file_path(&self) -> &PathBuf {
        &self.file_path
    }

    pub fn debug(&self, msg: &str) {
        self.write_line("DEBUG", msg);
        log::debug!("[{}] {}", self.session_id, msg);
    }

    pub fn info(&self, msg: &str) {
        self.write_line("INFO", msg);
        log::info!("[{}] {}", self.session_id, msg);
    }

    pub fn warn(&self, msg: &str) {
        self.write_line("WARN", msg);
        log::warn!("[{}] {}", self.session_id, msg);
    }

    pub fn error(&self, msg: &str) {
        self.write_line("ERROR", msg);
        log::error!("[{}] {}", self.session_id, msg);
    }

    fn write_line(&self, level: &str, msg: &str) {
        let now = chrono::Local::now();
        let line = format!(
            "[{} {:5} agent:{}:{}] {}\n",
            now.format("%Y-%m-%d %H:%M:%S%.3f"),
            level,
            self.agent_type,
            self.session_id,
            msg
        );
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn test_logger(agent_type: AgentType, suffix: &str) -> (AgentLogger, PathBuf) {
        let tmp_dir = std::env::temp_dir().join(format!("loopr_agent_logger_test_{}", suffix));
        let _ = fs::create_dir_all(&tmp_dir);
        let file_path = tmp_dir.join(format!("agent-{}-test123.log", agent_type));
        // Remove stale file from previous test run
        let _ = fs::remove_file(&file_path);

        let file = OpenOptions::new().create(true).append(true).open(&file_path).unwrap();

        let logger = AgentLogger {
            session_id: "test123".to_string(),
            agent_type,
            writer: Mutex::new(BufWriter::new(file)),
            file_path,
        };
        (logger, tmp_dir)
    }

    #[test]
    fn test_agent_logger_creates_file() {
        let (logger, tmp) = test_logger(AgentType::Implementer, "creates");
        logger.info("hello");
        assert!(logger.file_path().exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_agent_logger_file_naming() {
        let (logger, tmp) = test_logger(AgentType::Implementer, "naming");
        let name = logger.file_path().file_name().unwrap().to_string_lossy();
        assert_eq!(name, "agent-implementer-test123.log");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_agent_logger_writes_formatted_line() {
        let (logger, tmp) = test_logger(AgentType::Reviewer, "formatted");
        logger.info("test message");

        let mut contents = String::new();
        File::open(logger.file_path())
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();

        assert!(contents.contains("INFO"));
        assert!(contents.contains("agent:reviewer:test123"));
        assert!(contents.contains("test message"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_agent_logger_all_levels() {
        let (logger, tmp) = test_logger(AgentType::Coordinator, "levels");
        logger.debug("d");
        logger.info("i");
        logger.warn("w");
        logger.error("e");

        let mut contents = String::new();
        File::open(logger.file_path())
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();

        assert!(contents.contains("DEBUG"));
        assert!(contents.contains("INFO"));
        assert!(contents.contains("WARN"));
        assert!(contents.contains("ERROR"));
        let _ = fs::remove_dir_all(&tmp);
    }
}
