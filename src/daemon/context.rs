use std::sync::Arc;

use tokio::sync::{RwLock, broadcast};

use crate::config::Config;
use crate::ipc::protocol::DaemonEvent;

/// Shared state hub for the daemon.
/// All mutable state access goes through DaemonContext behind Arc<RwLock>.
pub struct DaemonContext {
    pub event_tx: broadcast::Sender<DaemonEvent>,
    pub config: Config,
}

impl DaemonContext {
    /// Create a new DaemonContext with the given config and event broadcast channel.
    pub fn new(config: Config, event_tx: broadcast::Sender<DaemonEvent>) -> Self {
        Self { config, event_tx }
    }

    /// Create a new DaemonContext wrapped in Arc<RwLock> for shared async access.
    pub fn shared(config: Config) -> (Arc<RwLock<Self>>, broadcast::Sender<DaemonEvent>) {
        let (event_tx, _) = broadcast::channel::<DaemonEvent>(256);
        let tx = event_tx.clone();
        let ctx = Self::new(config, event_tx);
        (Arc::new(RwLock::new(ctx)), tx)
    }

    /// Get a subscriber for daemon events.
    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_new() {
        let config = Config::default();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx);
        assert_eq!(ctx.config.name, "loopr");
    }

    #[test]
    fn test_context_shared() {
        let config = Config::default();
        let (ctx, tx) = DaemonContext::shared(config);
        // Can subscribe from the returned sender
        let _rx = tx.subscribe();
        // Can read from the context
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let c = ctx.read().await;
            assert_eq!(c.config.name, "loopr");
        });
    }

    #[test]
    fn test_context_subscribe() {
        let config = Config::default();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx);
        let mut rx = ctx.subscribe();
        // Send an event through the context's sender
        let event = DaemonEvent::record_created("plan", "p1");
        ctx.event_tx.send(event.clone()).unwrap();
        let received = rx.try_recv().unwrap();
        assert_eq!(received.event, "record.created");
    }

    #[test]
    fn test_context_shared_event_broadcast() {
        let config = Config::default();
        let (ctx, tx) = DaemonContext::shared(config);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let c = ctx.read().await;
            let mut rx = c.subscribe();
            drop(c);
            tx.send(DaemonEvent::record_created("spec", "s1")).unwrap();
            let received = rx.try_recv().unwrap();
            assert_eq!(received.data["collection"], "spec");
        });
    }
}
