use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::agents::AgentKind;

/// Per-agent logger that writes to both a dedicated log file and the main daemon log.
///
/// When a session directory is provided, writes to `{session_dir}/agents/{type}-{id}.log`.
/// Falls back to `~/.local/share/loopr/logs/agents/agent-{type}-{id}.log` otherwise.
///
/// Every log method writes a formatted line to the per-agent file AND calls
/// the corresponding `log::*!` macro with a `[session_id]` prefix.
pub struct AgentLogger {
    session_id: String,
    agent_type: AgentKind,
    writer: Mutex<BufWriter<File>>,
    file_path: PathBuf,
}

impl AgentLogger {
    /// Create a new AgentLogger, opening the per-agent log file.
    /// When `session_dir` is provided, writes to `{session_dir}/agents/`.
    /// Otherwise falls back to the legacy path.
    pub fn new(agent_type: AgentKind, session_id: &str, session_dir: Option<&Path>) -> eyre::Result<Self> {
        let log_dir = if let Some(dir) = session_dir {
            dir.join("agents")
        } else {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("loopr")
                .join("logs")
                .join("agents")
        };

        fs::create_dir_all(&log_dir)?;

        let file_name = format!("{}-{}.log", agent_type, session_id);
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
    pub fn _new_for_test(agent_type: AgentKind, session_id: &str, file: File, file_path: PathBuf) -> Self {
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
        log::debug!("[{}:{}] {}", self.agent_type, self.session_id, msg);
    }

    pub fn info(&self, msg: &str) {
        self.write_line("INFO", msg);
        log::info!("[{}:{}] {}", self.agent_type, self.session_id, msg);
    }

    pub fn warn(&self, msg: &str) {
        self.write_line("WARN", msg);
        log::warn!("[{}:{}] {}", self.agent_type, self.session_id, msg);
    }

    pub fn error(&self, msg: &str) {
        self.write_line("ERROR", msg);
        log::error!("[{}:{}] {}", self.agent_type, self.session_id, msg);
    }

    /// Write a per-iteration conversation file alongside the agent log.
    /// Only writes when log level is DEBUG or lower. File is named
    /// `{agent_id}.iter-{N}.md` in the same directory as the agent log.
    pub fn write_iter_file(
        &self,
        iteration: u32,
        work_id: Option<&str>,
        system_prompt: &str,
        user_message: &str,
        response: &str,
    ) {
        if !log::log_enabled!(log::Level::Debug) {
            return;
        }
        let iter_path = self.file_path.with_extension(format!("iter-{iteration}.md"));
        let work_line = work_id.unwrap_or("(none)");
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let content = format!(
            "# Agent: {}-{}\n# Type: {:?}\n# Work: {}\n# Iteration: {}\n# Timestamp: {}\n\n## System Prompt\n{}\n\n## User Message\n{}\n\n## Response\n{}\n",
            self.agent_type,
            self.session_id,
            self.agent_type,
            work_line,
            iteration,
            now,
            system_prompt,
            user_message,
            response,
        );
        if let Err(e) = fs::write(&iter_path, content.as_bytes()) {
            log::warn!(
                "[{}:{}] failed to write iter file {}: {}",
                self.agent_type,
                self.session_id,
                iter_path.display(),
                e
            );
        } else {
            self.debug(&format!(
                "LLM exchange -> iter-{}.md ({} chars prompt, {} chars response)",
                iteration,
                system_prompt.len() + user_message.len(),
                response.len()
            ));
        }
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;
    use std::io::Read;

    fn test_logger(agent_type: AgentKind, suffix: &str) -> (AgentLogger, TestDir) {
        let tmp_dir = TestDir::new(&format!("loopr_agent_logger_test_{suffix}"));
        let file_path = tmp_dir.join(format!("{}-test123.log", agent_type));
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
        let (logger, _tmp) = test_logger(AgentKind::Implementer, "creates");
        logger.info("hello");
        assert!(logger.file_path().exists());
    }

    #[test]
    fn test_agent_logger_file_naming() {
        let (logger, _tmp) = test_logger(AgentKind::Implementer, "naming");
        let name = logger.file_path().file_name().unwrap().to_string_lossy();
        assert_eq!(name, "implementer-test123.log");
    }

    #[test]
    fn test_agent_logger_session_scoped() {
        let tmp_dir = TestDir::new("loopr_agent_logger_test_session");
        let session_dir = tmp_dir.join("session1");
        fs::create_dir_all(session_dir.join("agents")).unwrap();

        let logger = AgentLogger::new(AgentKind::Implementer, "ag01", Some(&session_dir)).unwrap();
        logger.info("session scoped");

        assert!(logger.file_path().exists());
        assert!(logger.file_path().starts_with(session_dir.join("agents")));
        let name = logger.file_path().file_name().unwrap().to_string_lossy();
        assert_eq!(name, "implementer-ag01.log");
    }

    #[test]
    fn test_agent_logger_legacy_fallback() {
        let logger = AgentLogger::new(AgentKind::Reviewer, "ag02", None).unwrap();
        assert!(logger.file_path().exists());
        let name = logger.file_path().file_name().unwrap().to_string_lossy();
        assert_eq!(name, "reviewer-ag02.log");
    }

    #[test]
    fn test_agent_logger_writes_formatted_line() {
        let (logger, _tmp) = test_logger(AgentKind::Reviewer, "formatted");
        logger.info("test message");

        let mut contents = String::new();
        File::open(logger.file_path())
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();

        assert!(contents.contains("INFO"));
        assert!(contents.contains("agent:reviewer:test123"));
        assert!(contents.contains("test message"));
    }

    #[test]
    fn test_agent_logger_all_levels() {
        let (logger, _tmp) = test_logger(AgentKind::Coordinator, "levels");
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
    }
}
