use std::fmt;

use serde::{Deserialize, Serialize};

use crate::agents::{AgentSession, AgentType};
use crate::domain::bundle::Bundle;
use crate::domain::learning::Learning;
use crate::domain::lock::Lock;
use crate::domain::phase::Phase;
use crate::domain::plan::Plan;
use crate::domain::role::Role;
use crate::domain::spec::Spec;
use crate::domain::tick::Tick;
use crate::domain::work::Work;

/// Whether the TUI is in normal mode or capturing text input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Normal keybinding mode.
    Normal,
    /// Capturing goal text input (g key).
    GoalInput,
}

/// An IPC action queued by a keybinding for the event loop to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcAction {
    SetGoal(String),
    PauseAgent(String),
    ResumeAgent(String),
    StopAgent(String),
    NewRecord { collection: String },
    TransitionRecord { collection: String, id: String },
}

/// The six TUI views, cycled with Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum View {
    Dashboard,
    Works,
    Bundles,
    Ticks,
    Learnings,
    Locks,
    Agents,
}

impl View {
    /// All views in tab order.
    pub const ALL: [View; 7] = [
        View::Dashboard,
        View::Works,
        View::Bundles,
        View::Ticks,
        View::Learnings,
        View::Locks,
        View::Agents,
    ];

    /// Next view in tab cycle.
    pub fn next(self) -> View {
        let idx = Self::ALL.iter().position(|v| *v == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// Previous view in tab cycle.
    pub fn prev(self) -> View {
        let idx = Self::ALL.iter().position(|v| *v == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

impl fmt::Display for View {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            View::Dashboard => write!(f, "Dashboard"),
            View::Works => write!(f, "Works"),
            View::Bundles => write!(f, "Bundles"),
            View::Ticks => write!(f, "Ticks"),
            View::Learnings => write!(f, "Learnings"),
            View::Locks => write!(f, "Locks"),
            View::Agents => write!(f, "Agents"),
        }
    }
}

/// Cached state synced from daemon events.
#[derive(Debug, Default)]
pub struct AppState {
    pub plans: Vec<Plan>,
    pub specs: Vec<Spec>,
    pub phases: Vec<Phase>,
    pub works: Vec<Work>,
    pub bundles: Vec<Bundle>,
    pub ticks: Vec<Tick>,
    pub learnings: Vec<Learning>,
    pub locks: Vec<Lock>,
    pub agent_sessions: Vec<AgentSession>,
}

/// Connection state to the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    Disconnected,
}

impl fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectionStatus::Connected => write!(f, "Connected"),
            ConnectionStatus::Disconnected => write!(f, "Disconnected"),
        }
    }
}

/// Core TUI application state.
pub struct App {
    pub current_view: View,
    pub current_role: Role,
    pub connection: ConnectionStatus,
    pub state: AppState,
    pub selected_index: usize,
    pub show_help: bool,
    pub should_quit: bool,
    pub input_mode: InputMode,
    pub goal_input: String,
    pub pending_ipc: Option<IpcAction>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            current_view: View::Dashboard,
            current_role: Role::Coordinator,
            connection: ConnectionStatus::Disconnected,
            state: AppState::default(),
            selected_index: 0,
            show_help: false,
            should_quit: false,
            input_mode: InputMode::Normal,
            goal_input: String::new(),
            pending_ipc: None,
        }
    }

    /// Find the session ID of a coordinator agent with the given status.
    pub fn find_coordinator_session(&self, running: bool) -> Option<String> {
        use crate::agents::AgentStatus;
        let target = if running { AgentStatus::Running } else { AgentStatus::Paused };
        self.state
            .agent_sessions
            .iter()
            .find(|s| s.agent_type == AgentType::Coordinator && s.status == target)
            .map(|s| s.id.clone())
    }

    /// Switch to next view (Tab).
    pub fn next_view(&mut self) {
        self.current_view = self.current_view.next();
        self.selected_index = 0;
    }

    /// Switch to previous view (Shift+Tab).
    pub fn prev_view(&mut self) {
        self.current_view = self.current_view.prev();
        self.selected_index = 0;
    }

    /// Cycle role: Coordinator → Integrator → Implementer → Reviewer → Researcher → Coordinator.
    pub fn cycle_role(&mut self) {
        self.current_role = match self.current_role {
            Role::Coordinator => Role::Integrator,
            Role::Integrator => Role::Implementer,
            Role::Implementer => Role::Reviewer,
            Role::Reviewer => Role::Researcher,
            Role::Researcher => Role::Coordinator,
        };
    }

    /// Move selection down in the current list (j / Down).
    pub fn select_next(&mut self) {
        let max = self.current_list_len();
        if max > 0 {
            self.selected_index = (self.selected_index + 1).min(max - 1);
        }
    }

    /// Move selection up in the current list (k / Up).
    pub fn select_prev(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(1);
    }

    /// Toggle help overlay (?).
    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    /// Map current view to an IPC collection name, if applicable.
    pub fn view_collection(&self) -> Option<String> {
        match self.current_view {
            View::Works => Some("work".to_string()),
            View::Bundles => Some("bundle".to_string()),
            View::Ticks => Some("tick".to_string()),
            View::Learnings => Some("learning".to_string()),
            View::Locks => Some("lock".to_string()),
            _ => None, // Dashboard and Agents have no single collection
        }
    }

    /// Get the ID of the currently selected record, if any.
    pub fn selected_record_id(&self) -> Option<String> {
        match self.current_view {
            View::Works => self.state.works.get(self.selected_index).map(|w| w.id.clone()),
            View::Bundles => self.state.bundles.get(self.selected_index).map(|b| b.id.clone()),
            View::Ticks => self.state.ticks.get(self.selected_index).map(|t| t.id.clone()),
            View::Learnings => self.state.learnings.get(self.selected_index).map(|l| l.id.clone()),
            View::Locks => self.state.locks.get(self.selected_index).map(|l| l.id.clone()),
            _ => None,
        }
    }

    /// Number of items in the current view's list.
    pub fn current_list_len(&self) -> usize {
        match self.current_view {
            View::Dashboard => 0,
            View::Works => self.state.works.len(),
            View::Bundles => self.state.bundles.len(),
            View::Ticks => self.state.ticks.len(),
            View::Learnings => self.state.learnings.len(),
            View::Locks => self.state.locks.len(),
            View::Agents => self.state.agent_sessions.len(),
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_view_next_cycles() {
        assert_eq!(View::Dashboard.next(), View::Works);
        assert_eq!(View::Works.next(), View::Bundles);
        assert_eq!(View::Bundles.next(), View::Ticks);
        assert_eq!(View::Ticks.next(), View::Learnings);
        assert_eq!(View::Learnings.next(), View::Locks);
        assert_eq!(View::Locks.next(), View::Agents);
        assert_eq!(View::Agents.next(), View::Dashboard);
    }

    #[test]
    fn test_view_prev_cycles() {
        assert_eq!(View::Dashboard.prev(), View::Agents);
        assert_eq!(View::Agents.prev(), View::Locks);
        assert_eq!(View::Locks.prev(), View::Learnings);
        assert_eq!(View::Learnings.prev(), View::Ticks);
        assert_eq!(View::Ticks.prev(), View::Bundles);
        assert_eq!(View::Bundles.prev(), View::Works);
        assert_eq!(View::Works.prev(), View::Dashboard);
    }

    #[test]
    fn test_view_display() {
        assert_eq!(View::Dashboard.to_string(), "Dashboard");
        assert_eq!(View::Works.to_string(), "Works");
        assert_eq!(View::Bundles.to_string(), "Bundles");
        assert_eq!(View::Ticks.to_string(), "Ticks");
        assert_eq!(View::Learnings.to_string(), "Learnings");
        assert_eq!(View::Locks.to_string(), "Locks");
        assert_eq!(View::Agents.to_string(), "Agents");
    }

    #[test]
    fn test_view_all_order() {
        assert_eq!(View::ALL.len(), 7);
        assert_eq!(View::ALL[0], View::Dashboard);
        assert_eq!(View::ALL[4], View::Learnings);
        assert_eq!(View::ALL[5], View::Locks);
        assert_eq!(View::ALL[6], View::Agents);
    }

    #[test]
    fn test_connection_status_display() {
        assert_eq!(ConnectionStatus::Connected.to_string(), "Connected");
        assert_eq!(ConnectionStatus::Disconnected.to_string(), "Disconnected");
    }

    #[test]
    fn test_app_new_defaults() {
        let app = App::new();
        assert_eq!(app.current_view, View::Dashboard);
        assert_eq!(app.current_role, Role::Coordinator);
        assert_eq!(app.connection, ConnectionStatus::Disconnected);
        assert_eq!(app.selected_index, 0);
        assert!(!app.show_help);
        assert!(!app.should_quit);
    }

    #[test]
    fn test_app_next_view_resets_selection() {
        let mut app = App::new();
        app.selected_index = 5;
        app.next_view();
        assert_eq!(app.current_view, View::Works);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_app_prev_view_resets_selection() {
        let mut app = App::new();
        app.selected_index = 5;
        app.prev_view();
        assert_eq!(app.current_view, View::Agents);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_app_cycle_role() {
        let mut app = App::new();
        assert_eq!(app.current_role, Role::Coordinator);
        app.cycle_role();
        assert_eq!(app.current_role, Role::Integrator);
        app.cycle_role();
        assert_eq!(app.current_role, Role::Implementer);
        app.cycle_role();
        assert_eq!(app.current_role, Role::Reviewer);
        app.cycle_role();
        assert_eq!(app.current_role, Role::Researcher);
        app.cycle_role();
        assert_eq!(app.current_role, Role::Coordinator);
    }

    #[test]
    fn test_app_select_next_empty() {
        let mut app = App::new();
        app.current_view = View::Works;
        app.select_next(); // no items, should not panic
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_app_select_next_with_items() {
        let mut app = App::new();
        app.current_view = View::Works;
        app.state.works.push(Work::new("ph1".into(), "t1".into(), "d1".into()));
        app.state.works.push(Work::new("ph1".into(), "t2".into(), "d2".into()));
        app.state.works.push(Work::new("ph1".into(), "t3".into(), "d3".into()));

        assert_eq!(app.selected_index, 0);
        app.select_next();
        assert_eq!(app.selected_index, 1);
        app.select_next();
        assert_eq!(app.selected_index, 2);
        app.select_next(); // at end, should clamp
        assert_eq!(app.selected_index, 2);
    }

    #[test]
    fn test_app_select_prev() {
        let mut app = App::new();
        app.selected_index = 2;
        app.select_prev();
        assert_eq!(app.selected_index, 1);
        app.select_prev();
        assert_eq!(app.selected_index, 0);
        app.select_prev(); // at start, should not underflow
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_app_toggle_help() {
        let mut app = App::new();
        assert!(!app.show_help);
        app.toggle_help();
        assert!(app.show_help);
        app.toggle_help();
        assert!(!app.show_help);
    }

    #[test]
    fn test_app_current_list_len() {
        let mut app = App::new();
        assert_eq!(app.current_list_len(), 0); // Dashboard

        app.current_view = View::Works;
        assert_eq!(app.current_list_len(), 0);

        app.state.works.push(Work::new("ph1".into(), "t1".into(), "d1".into()));
        assert_eq!(app.current_list_len(), 1);

        app.current_view = View::Bundles;
        assert_eq!(app.current_list_len(), 0);

        app.current_view = View::Ticks;
        app.state.ticks.push(Tick::new(1));
        assert_eq!(app.current_list_len(), 1);
    }

    #[test]
    fn test_app_state_default_empty() {
        let state = AppState::default();
        assert!(state.plans.is_empty());
        assert!(state.specs.is_empty());
        assert!(state.phases.is_empty());
        assert!(state.works.is_empty());
        assert!(state.bundles.is_empty());
        assert!(state.ticks.is_empty());
        assert!(state.learnings.is_empty());
        assert!(state.locks.is_empty());
        assert!(state.agent_sessions.is_empty());
    }

    #[test]
    fn test_view_serde_roundtrip() {
        for view in View::ALL {
            let json = serde_json::to_string(&view).unwrap();
            let deserialized: View = serde_json::from_str(&json).unwrap();
            assert_eq!(view, deserialized);
        }
    }
}
