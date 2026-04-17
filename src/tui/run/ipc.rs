use std::path::Path;

use tracing::warn;

use crate::agents::AgentSession;
use crate::domain::bundle::Bundle;
use crate::domain::learning::Learning;
use crate::domain::lock::Lock;
use crate::domain::phase::Phase;
use crate::domain::plan::Plan;
use crate::domain::spec::Spec;
use crate::domain::tick::Tick;
use crate::domain::work::Work;
use crate::ipc::client::IpcClient;
use crate::ipc::protocol::DaemonEvent;
use crate::tui::app::{AppState, IpcAction};

/// Try to connect and handshake with the daemon. Returns client + session_id.
pub async fn try_connect(socket_path: &Path) -> Option<(IpcClient, String)> {
    let mut client = IpcClient::connect(socket_path).await.ok()?;
    let resp = client.handshake(crate::version()).await.ok()?;
    // Reject version-mismatched daemons -- stale daemon must be restarted
    let version_match = resp
        .result
        .as_ref()
        .and_then(|r| r.get("version_match"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !version_match {
        tracing::warn!(
            "Daemon version mismatch, refusing connection (ours={}, theirs={:?})",
            crate::version(),
            resp.result.as_ref().and_then(|r| r.get("server_version")),
        );
        return None;
    }
    let session_id = resp
        .result
        .as_ref()
        .and_then(|r| r.get("session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some((client, session_id))
}

/// Send an IPC action to the daemon.
pub async fn dispatch_ipc_action(client: &mut IpcClient, action: IpcAction) {
    let (method, params) = match action {
        IpcAction::PauseAgent(session_id) => (
            "agent.pause".to_string(),
            serde_json::json!({ "session_id": session_id }),
        ),
        IpcAction::ResumeAgent(session_id) => (
            "agent.resume".to_string(),
            serde_json::json!({ "session_id": session_id }),
        ),
        IpcAction::StopAgent(session_id) => (
            "agent.stop".to_string(),
            serde_json::json!({ "session_id": session_id }),
        ),
        IpcAction::NewRecord { collection } => (
            format!("{collection}.create"),
            serde_json::json!({ "title": "New Record", "description": "" }),
        ),
        IpcAction::TransitionRecord { collection, id } => (
            format!("{collection}.transition"),
            serde_json::json!({ "id": id, "target_status": "Active" }),
        ),
        IpcAction::AcceptPlan(markdown) => ("doc.accept".to_string(), serde_json::json!({ "markdown": markdown })),
        IpcAction::StartPlanIntake { chat_session_id } => (
            "director.start_plan_intake".to_string(),
            serde_json::json!({ "chat_session_id": chat_session_id }),
        ),
    };
    if let Err(e) = client.request(&method, params).await {
        warn!("Failed to dispatch IPC action {method}: {e}");
    }
}

/// Refresh a collection in AppState by fetching the latest list from the daemon.
pub async fn refresh_collection(state: &mut AppState, client: &mut IpcClient, collection: &str) {
    let method = format!("{collection}.list");
    match client.request(&method, serde_json::json!({})).await {
        Ok((resp, _events)) => {
            if let Some(result) = resp.result {
                match collection {
                    "plan" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Plan>>(result) {
                            state.plans = items;
                        }
                    }
                    "spec" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Spec>>(result) {
                            state.specs = items;
                        }
                    }
                    "phase" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Phase>>(result) {
                            state.phases = items;
                        }
                    }
                    "work" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Work>>(result) {
                            state.works = items;
                        }
                    }
                    "bundle" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Bundle>>(result) {
                            state.bundles = items;
                        }
                    }
                    "tick" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Tick>>(result) {
                            state.ticks = items;
                        }
                    }
                    "learning" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Learning>>(result) {
                            state.learnings = items;
                        }
                    }
                    "lock" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Lock>>(result) {
                            state.locks = items;
                        }
                    }
                    "agent" => {
                        if let Ok(items) = serde_json::from_value::<Vec<AgentSession>>(result) {
                            state.agent_sessions = items;
                        }
                    }
                    _ => {}
                }
            }
        }
        Err(e) => {
            warn!("Failed to refresh collection {collection}: {e}");
        }
    }
}

/// Extract collection name from a daemon event, if applicable.
pub fn event_collection(event: &DaemonEvent) -> Option<&str> {
    match event.event.as_str() {
        "record.created" | "record.updated" | "transition.completed" => event.data["collection"].as_str(),
        "tick.published" | "tick.validation_failed" => Some("tick"),
        "bundle.rejected_stale" => Some("bundle"),
        e if e.starts_with("agent.") => Some("agent"),
        _ => None,
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::ipc::protocol::{DaemonRequest, DaemonResponse};
    use futures::future::BoxFuture;

    fn sync_h<F>(f: F) -> impl Fn(DaemonRequest) -> BoxFuture<'static, DaemonResponse> + Send + 'static
    where
        F: Fn(DaemonRequest) -> DaemonResponse + Send + 'static,
    {
        move |req| Box::pin(std::future::ready(f(req))) as BoxFuture<'static, DaemonResponse>
    }

    /// Build a mock handshake response that passes version checking.
    fn mock_handshake(req: &crate::ipc::protocol::DaemonRequest) -> crate::ipc::protocol::DaemonResponse {
        let client_version = req.params.get("client_version").and_then(|v| v.as_str()).unwrap_or("");
        crate::ipc::protocol::DaemonResponse::ok(
            req.id,
            serde_json::json!({
                "protocol": "ndjson/1",
                "server_version": client_version,
                "client_version": client_version,
                "version_match": true,
                "session_id": "test-session",
            }),
        )
    }

    /// Helper: start a mock daemon server that records the method+params of each request.
    async fn mock_ipc_server() -> (
        PathBuf,
        tokio::task::JoinHandle<()>,
        Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    ) {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("dispatch-{}.sock", crate::id::generate_id("xx")));

        let captured: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = captured.clone();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method != "system.handshake" {
                            captured_clone
                                .lock()
                                .unwrap()
                                .push((req.method.clone(), req.params.clone()));
                        }
                        DaemonResponse::ok(req.id, serde_json::json!({"ok": true}))
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        (path, handle, captured)
    }

    #[test]
    fn test_reconnect_interval_is_reasonable() {
        use super::super::events::RECONNECT_INTERVAL;
        assert!(RECONNECT_INTERVAL >= Duration::from_secs(1));
        assert!(RECONNECT_INTERVAL <= Duration::from_secs(10));
    }

    #[tokio::test]
    async fn test_try_connect_nonexistent_socket() {
        let result = try_connect(Path::new("/tmp/nonexistent-loopr-reconnect-test.sock")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_try_connect_succeeds_with_daemon() {
        use crate::ipc::protocol::DaemonEvent;
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("reconnect-{}.sock", crate::id::generate_id("xx")));

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(stream, sync_h(|req| mock_handshake(&req)), event_rx).await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = try_connect(&path).await;
        assert!(result.is_some());
        let (_, session_id) = result.unwrap();
        assert_eq!(session_id, "test-session");

        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_reconnect_after_disconnect() {
        use crate::ipc::protocol::DaemonEvent;
        use crate::ipc::server::{IpcServer, handle_client};
        use crate::tui::app::{App, ConnectionStatus};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("reconnect2-{}.sock", crate::id::generate_id("xx")));

        let mut app = App::new();
        app.connection = ConnectionStatus::Disconnected;
        assert_eq!(app.connection, ConnectionStatus::Disconnected);

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(stream, sync_h(|req| mock_handshake(&req)), event_rx).await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        if let Some((_, session_id)) = try_connect(&path).await {
            app.connection = ConnectionStatus::Connected;
            app.session_id = session_id;
        }
        assert_eq!(app.connection, ConnectionStatus::Connected);
        assert_eq!(app.session_id, "test-session");

        let _ = server_handle.await;
    }

    #[test]
    fn test_event_collection_record_created() {
        let event = DaemonEvent::record_created("plan", "p1");
        assert_eq!(event_collection(&event), Some("plan"));
    }

    #[test]
    fn test_event_collection_record_updated() {
        let event = DaemonEvent::record_updated("learning", "l1");
        assert_eq!(event_collection(&event), Some("learning"));
    }

    #[test]
    fn test_event_collection_transition_completed() {
        let event = DaemonEvent::transition_completed("work", "wi1", "Draft", "Ready", "Coordinator");
        assert_eq!(event_collection(&event), Some("work"));
    }

    #[test]
    fn test_event_collection_tick_published() {
        let event = DaemonEvent::tick_published("t1", "abc123");
        assert_eq!(event_collection(&event), Some("tick"));
    }

    #[test]
    fn test_event_collection_bundle_rejected_stale() {
        let event = DaemonEvent::bundle_rejected_stale("wi1", "t1", "t2");
        assert_eq!(event_collection(&event), Some("bundle"));
    }

    #[test]
    fn test_event_collection_unknown_event() {
        let event = DaemonEvent::new("some.unknown.event", serde_json::json!({}));
        assert_eq!(event_collection(&event), None);
    }

    #[test]
    fn test_event_collection_agent_status_changed() {
        let event = DaemonEvent::agent_status_changed("s1", crate::agents::AgentStatus::Running);
        assert_eq!(event_collection(&event), Some("agent"));
    }

    #[test]
    fn test_event_collection_agent_llm_output() {
        let event = DaemonEvent::new(
            "agent.llm_output",
            serde_json::json!({"session_id": "s1", "chunk": "hello", "is_final": false}),
        );
        assert_eq!(event_collection(&event), Some("agent"));
    }

    #[test]
    fn test_event_collection_tick_validation_failed() {
        let event = DaemonEvent::new(
            "tick.validation_failed",
            serde_json::json!({"tick_id": "t1", "reason": "test failed"}),
        );
        assert_eq!(event_collection(&event), Some("tick"));
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_plans() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-{}.sock", crate::id::generate_id("xx")));

        let mock_plan = Plan::new("Test Plan".into(), "Criteria".into());
        let plans_json = serde_json::to_value(vec![mock_plan.clone()]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let plans = plans_json.clone();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else if req.method == "plan.list" {
                            DaemonResponse::ok(req.id, plans.clone())
                        } else {
                            DaemonResponse::ok(req.id, serde_json::json!(null))
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        assert!(state.plans.is_empty());

        refresh_collection(&mut state, &mut client, "plan").await;
        assert_eq!(state.plans.len(), 1);
        assert_eq!(state.plans[0].title, "Test Plan");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_works() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-wi-{}.sock", crate::id::generate_id("xx")));

        let mock_wi = Work::new("ph1".into(), "Task 1".into());
        let wis_json = serde_json::to_value(vec![mock_wi.clone()]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let wis = wis_json.clone();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else if req.method == "work.list" {
                            DaemonResponse::ok(req.id, wis.clone())
                        } else {
                            DaemonResponse::ok(req.id, serde_json::json!(null))
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        assert!(state.works.is_empty());

        refresh_collection(&mut state, &mut client, "work").await;
        assert_eq!(state.works.len(), 1);
        assert_eq!(state.works[0].title, "Task 1");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_unknown_collection_is_noop() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-unk-{}.sock", crate::id::generate_id("xx")));

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, serde_json::json!([]))
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        refresh_collection(&mut state, &mut client, "unknown_collection").await;
        // State should be unchanged
        assert!(state.plans.is_empty());

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_bundles() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-bundle-{}.sock", crate::id::generate_id("xx")));

        let mock_bundle = Bundle::new("wi-1".into(), None, "feature/test".into(), vec!["Test bundle".into()]);
        let bundles_json = serde_json::to_value(vec![mock_bundle]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let bundles = bundles_json.clone();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else if req.method == "bundle.list" {
                            DaemonResponse::ok(req.id, bundles.clone())
                        } else {
                            DaemonResponse::ok(req.id, serde_json::json!(null))
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        assert!(state.bundles.is_empty());
        refresh_collection(&mut state, &mut client, "bundle").await;
        assert_eq!(state.bundles.len(), 1);

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_ticks() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-tick-{}.sock", crate::id::generate_id("xx")));

        let mock_tick = Tick::new(1);
        let ticks_json = serde_json::to_value(vec![mock_tick]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let ticks = ticks_json.clone();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else if req.method == "tick.list" {
                            DaemonResponse::ok(req.id, ticks.clone())
                        } else {
                            DaemonResponse::ok(req.id, serde_json::json!(null))
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        assert!(state.ticks.is_empty());
        refresh_collection(&mut state, &mut client, "tick").await;
        assert_eq!(state.ticks.len(), 1);

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_json_parse_error() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-parse-err-{}.sock", crate::id::generate_id("xx")));

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            // Return a string instead of an array -- will fail deserialization
                            DaemonResponse::ok(req.id, serde_json::json!("not an array"))
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();

        // Each collection should fail silently and leave state empty
        for collection in [
            "plan", "spec", "phase", "work", "bundle", "tick", "learning", "lock", "agent",
        ] {
            refresh_collection(&mut state, &mut client, collection).await;
        }
        assert!(state.plans.is_empty());
        assert!(state.specs.is_empty());
        assert!(state.phases.is_empty());
        assert!(state.works.is_empty());
        assert!(state.bundles.is_empty());
        assert!(state.ticks.is_empty());
        assert!(state.learnings.is_empty());
        assert!(state.locks.is_empty());
        assert!(state.agent_sessions.is_empty());

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_specs() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-spec-{}.sock", crate::id::generate_id("xx")));

        let mock_spec = Spec::new("plan-1".into(), "Test Spec".into());
        let specs_json = serde_json::to_value(vec![mock_spec]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let specs = specs_json.clone();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, specs.clone())
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        refresh_collection(&mut state, &mut client, "spec").await;
        assert_eq!(state.specs.len(), 1);
        assert_eq!(state.specs[0].title, "Test Spec");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_phases() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-phase-{}.sock", crate::id::generate_id("xx")));

        let mock_phase = Phase::new("spec-1".into(), "Phase 1".into());
        let phases_json = serde_json::to_value(vec![mock_phase]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let phases = phases_json.clone();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, phases.clone())
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        refresh_collection(&mut state, &mut client, "phase").await;
        assert_eq!(state.phases.len(), 1);
        assert_eq!(state.phases[0].title, "Phase 1");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_learnings() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-learning-{}.sock", crate::id::generate_id("xx")));

        let mock_learning = Learning::new(
            "wi-1".into(),
            crate::domain::learning::LearningScope::Global,
            "insight".into(),
        );
        let learnings_json = serde_json::to_value(vec![mock_learning]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let learnings = learnings_json.clone();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, learnings.clone())
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        refresh_collection(&mut state, &mut client, "learning").await;
        assert_eq!(state.learnings.len(), 1);

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_locks() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-lock-{}.sock", crate::id::generate_id("xx")));

        let mock_lock = Lock::new("src/main.rs".into(), "wi-1".into(), "coordinator".into());
        let locks_json = serde_json::to_value(vec![mock_lock]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let locks = locks_json.clone();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, locks.clone())
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        refresh_collection(&mut state, &mut client, "lock").await;
        assert_eq!(state.locks.len(), 1);
        assert_eq!(state.locks[0].resource, "src/main.rs");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_agent_sessions() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-agent-{}.sock", crate::id::generate_id("xx")));

        let mock_session = AgentSession::new(crate::agents::AgentKind::Implementer, "test-model".to_string());
        let sessions_json = serde_json::to_value(vec![mock_session]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let sessions = sessions_json.clone();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, sessions.clone())
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        refresh_collection(&mut state, &mut client, "agent").await;
        assert_eq!(state.agent_sessions.len(), 1);

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_invalid_json_no_panic() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-badjson-{}.sock", crate::id::generate_id("xx")));

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    sync_h(move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            // Return a non-array value that won't deserialize as Vec<T>
                            DaemonResponse::ok(req.id, serde_json::json!({"not": "an array"}))
                        }
                    }),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        // Should not panic -- deserialization failure is silently ignored
        for collection in [
            "plan", "spec", "phase", "work", "bundle", "tick", "learning", "lock", "agent",
        ] {
            refresh_collection(&mut state, &mut client, collection).await;
        }
        assert!(state.plans.is_empty());
        assert!(state.agent_sessions.is_empty());

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_dispatch_ipc_action_pause_agent() {
        let (path, server_handle, captured) = mock_ipc_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        dispatch_ipc_action(&mut client, IpcAction::PauseAgent("sess-1".to_string())).await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "agent.pause");
            assert_eq!(reqs[0].1["session_id"], "sess-1");
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_dispatch_ipc_action_resume_agent() {
        let (path, server_handle, captured) = mock_ipc_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        dispatch_ipc_action(&mut client, IpcAction::ResumeAgent("sess-2".to_string())).await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "agent.resume");
            assert_eq!(reqs[0].1["session_id"], "sess-2");
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_dispatch_ipc_action_stop_agent() {
        let (path, server_handle, captured) = mock_ipc_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        dispatch_ipc_action(&mut client, IpcAction::StopAgent("sess-3".to_string())).await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "agent.stop");
            assert_eq!(reqs[0].1["session_id"], "sess-3");
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_dispatch_ipc_action_new_record() {
        let (path, server_handle, captured) = mock_ipc_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        dispatch_ipc_action(
            &mut client,
            IpcAction::NewRecord {
                collection: "work".to_string(),
            },
        )
        .await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "work.create");
            assert_eq!(reqs[0].1["title"], "New Record");
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_dispatch_ipc_action_transition_record() {
        let (path, server_handle, captured) = mock_ipc_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        dispatch_ipc_action(
            &mut client,
            IpcAction::TransitionRecord {
                collection: "bundle".to_string(),
                id: "b-123".to_string(),
            },
        )
        .await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "bundle.transition");
            assert_eq!(reqs[0].1["id"], "b-123");
            assert_eq!(reqs[0].1["target_status"], "Active");
        }

        drop(client);
        let _ = server_handle.await;
    }
}
