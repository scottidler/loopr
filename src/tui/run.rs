pub mod events;
pub mod ipc;
pub mod render;

use std::io;
use std::path::Path;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::tui::app::App;

pub use self::render::draw;
pub use self::render::role_actions;

/// Restore the terminal to normal mode. Called on both clean exit and panic.
/// Uses raw escape sequences as a fallback to guarantee mouse tracking stops.
fn restore_terminal() {
    // Hard-disable all mouse tracking modes with raw escape sequences.
    // This must happen BEFORE leaving raw mode so the sequences are sent properly.
    let _ = io::Write::write_all(&mut io::stdout(), b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l");
    let _ = io::Write::flush(&mut io::stdout());
    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// Run the TUI, connecting to the daemon at the given socket path.
pub async fn run_tui(socket_path: &Path) -> eyre::Result<()> {
    // Connect to daemon
    let mut client = crate::ipc::client::IpcClient::connect(socket_path)
        .await
        .map_err(|e| eyre::eyre!("Failed to connect to daemon: {e}"))?;
    let handshake_resp = client
        .handshake(crate::version())
        .await
        .map_err(|e| eyre::eyre!("Handshake failed: {e}"))?;

    // Extract session_id from handshake response
    let handshake_result = handshake_resp.result.as_ref();
    let version_match = handshake_result
        .and_then(|r| r.get("version_match"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !version_match {
        return Err(eyre::eyre!(
            "daemon version mismatch (ours={}, theirs={:?})",
            crate::version(),
            handshake_result.and_then(|r| r.get("server_version")),
        ));
    }
    let session_id = handshake_result
        .and_then(|r| r.get("session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Install panic hook that restores the terminal before printing the panic.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original_hook(info);
    }));

    // Create app state
    let mut app = App::new();
    app.connection = crate::tui::app::ConnectionStatus::Connected;
    app.session_id = session_id;

    // Run event loop; capture result so we always restore the terminal
    let result = events::event_loop(&mut terminal, &mut app, Some(client), socket_path).await;

    restore_terminal();

    if !app.session_id.is_empty() {
        eprintln!(
            "loopr session {} ended. Run `loopr diagnose dump` for diagnostics.",
            app.session_id
        );
    }

    result
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use crate::tui::app::{App, FunnelState};

    #[test]
    fn test_system_prompt_chat_state() {
        crate::prompts::init_defaults();
        let prompt = crate::domain::chat::system_prompt_for_chat(FunnelState::Chat, false, None);
        assert!(prompt.contains("Loopr development orchestrator"));
        assert!(!prompt.contains("clarifying questions"));
    }

    #[test]
    fn test_system_prompt_interview_state() {
        crate::prompts::init_defaults();
        let prompt = crate::domain::chat::system_prompt_for_chat(FunnelState::Interview, false, None);
        assert!(prompt.contains("Loopr development orchestrator"));
        assert!(prompt.contains("clarifying questions"));
    }

    #[test]
    fn test_system_prompt_draft_request() {
        crate::prompts::init_defaults();
        let prompt = crate::domain::chat::system_prompt_for_chat(FunnelState::PlanDraft, true, None);
        assert!(prompt.contains("structured plan"));
        assert!(!prompt.contains("refine"));
    }

    #[test]
    fn test_system_prompt_plan_refine() {
        crate::prompts::init_defaults();
        let prompt = crate::domain::chat::system_prompt_for_chat(FunnelState::PlanDraft, false, None);
        assert!(prompt.contains("refine"));
        assert!(!prompt.contains("structured plan"));
    }

    #[test]
    fn test_system_prompt_executing_state() {
        crate::prompts::init_defaults();
        let prompt = crate::domain::chat::system_prompt_for_chat(FunnelState::Executing, false, Some("2 Works active"));
        assert!(prompt.contains("orchestration pipeline"));
        assert!(prompt.contains("2 Works active"));
    }

    #[test]
    fn test_canonical_messages_lifecycle() {
        let mut app = App::new();
        assert!(app.canonical_messages.is_empty());

        // Simulate user message
        app.canonical_messages.push(crate::tools::types::Message {
            role: "user".to_string(),
            content: vec![crate::tools::types::ContentBlock::Text {
                text: "hello".to_string(),
            }],
        });
        assert_eq!(app.canonical_messages.len(), 1);
        assert_eq!(app.canonical_messages[0].role, "user");

        // Simulate assistant response with tool use
        app.canonical_messages.push(crate::tools::types::Message {
            role: "assistant".to_string(),
            content: vec![crate::tools::types::ContentBlock::Text {
                text: "hi there".to_string(),
            }],
        });
        assert_eq!(app.canonical_messages.len(), 2);

        // Clear resets both
        app.chat_history.clear();
        app.canonical_messages.clear();
        assert!(app.canonical_messages.is_empty());
    }
}
