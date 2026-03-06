use serde::{Deserialize, Serialize};

/// A structured message between agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: String,
    pub from: String,
    pub to: String,
    pub priority: MessagePriority,
    pub payload: MessagePayload,
    pub timestamp: i64,
    pub read: bool,
}

/// Priority levels for agent messages. Ordered by urgency (Shutdown highest).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum MessagePriority {
    Shutdown = 0,
    Idle = 1,
    Normal = 2,
}

/// Payload types for agent messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessagePayload {
    TaskCompleted {
        task_id: String,
        status: String,
    },
    ReviewRequested {
        task_id: String,
        files: Vec<String>,
    },
    ShutdownRequest {
        reason: String,
    },
    IdleNotification {
        summary: String,
    },
    Custom {
        data: serde_json::Value,
    },
}

/// In-memory mailbox for inter-agent messaging with priority ordering.
pub struct TaskMailbox {
    messages: Vec<AgentMessage>,
}

impl TaskMailbox {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    /// Send a message to the mailbox.
    pub fn send(&mut self, message: AgentMessage) {
        self.messages.push(message);
    }

    /// Read all unread messages for a recipient, ordered by priority (highest first).
    /// Marks returned messages as read.
    pub fn read_for(&mut self, recipient: &str) -> Vec<AgentMessage> {
        let mut result: Vec<AgentMessage> = Vec::new();

        for msg in self.messages.iter_mut() {
            if !msg.read && (msg.to == recipient || msg.to == "broadcast") {
                msg.read = true;
                result.push(msg.clone());
            }
        }

        // Sort by priority (Shutdown=0 < Idle=1 < Normal=2), so highest priority first
        result.sort_by_key(|m| m.priority);
        result
    }

    /// Count of unread messages for a recipient.
    pub fn unread_count(&self, recipient: &str) -> usize {
        self.messages
            .iter()
            .filter(|m| !m.read && (m.to == recipient || m.to == "broadcast"))
            .count()
    }

    /// Drain all read messages (cleanup).
    pub fn gc(&mut self) {
        self.messages.retain(|m| !m.read);
    }
}

impl Default for TaskMailbox {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(id: &str, from: &str, to: &str, priority: MessagePriority, payload: MessagePayload) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            priority,
            payload,
            timestamp: chrono::Utc::now().timestamp_millis(),
            read: false,
        }
    }

    #[test]
    fn test_agent_message_serde_roundtrip() {
        let msg = make_message(
            "m1",
            "coordinator",
            "implementer-1",
            MessagePriority::Normal,
            MessagePayload::TaskCompleted {
                task_id: "t1".to_string(),
                status: "done".to_string(),
            },
        );
        let json = serde_json::to_string(&msg).unwrap();
        let back: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "m1");
        assert_eq!(back.from, "coordinator");
        assert_eq!(back.to, "implementer-1");
        assert_eq!(back.priority, MessagePriority::Normal);
    }

    #[test]
    fn test_message_priority_ordering() {
        assert!(MessagePriority::Shutdown < MessagePriority::Idle);
        assert!(MessagePriority::Idle < MessagePriority::Normal);
    }

    #[test]
    fn test_message_payload_variants() {
        let payloads = vec![
            MessagePayload::TaskCompleted {
                task_id: "t1".into(),
                status: "done".into(),
            },
            MessagePayload::ReviewRequested {
                task_id: "t1".into(),
                files: vec!["src/main.rs".into()],
            },
            MessagePayload::ShutdownRequest {
                reason: "test".into(),
            },
            MessagePayload::IdleNotification {
                summary: "nothing to do".into(),
            },
            MessagePayload::Custom {
                data: serde_json::json!({"key": "value"}),
            },
        ];
        for payload in payloads {
            let json = serde_json::to_string(&payload).unwrap();
            let back: MessagePayload = serde_json::from_str(&json).unwrap();
            // Verify it round-trips by re-serializing
            let json2 = serde_json::to_string(&back).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn test_mailbox_send_and_read() {
        let mut mailbox = TaskMailbox::new();
        mailbox.send(make_message(
            "m1", "coord", "impl-1", MessagePriority::Normal,
            MessagePayload::TaskCompleted { task_id: "t1".into(), status: "done".into() },
        ));
        mailbox.send(make_message(
            "m2", "coord", "impl-2", MessagePriority::Normal,
            MessagePayload::TaskCompleted { task_id: "t2".into(), status: "done".into() },
        ));

        let msgs = mailbox.read_for("impl-1");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id, "m1");

        // Should be marked as read now
        let msgs2 = mailbox.read_for("impl-1");
        assert_eq!(msgs2.len(), 0);
    }

    #[test]
    fn test_mailbox_priority_ordering() {
        let mut mailbox = TaskMailbox::new();
        mailbox.send(make_message(
            "m1", "coord", "impl-1", MessagePriority::Normal,
            MessagePayload::Custom { data: serde_json::json!("normal") },
        ));
        mailbox.send(make_message(
            "m2", "coord", "impl-1", MessagePriority::Shutdown,
            MessagePayload::ShutdownRequest { reason: "test".into() },
        ));
        mailbox.send(make_message(
            "m3", "coord", "impl-1", MessagePriority::Idle,
            MessagePayload::IdleNotification { summary: "idle".into() },
        ));

        let msgs = mailbox.read_for("impl-1");
        assert_eq!(msgs.len(), 3);
        // Shutdown first, then Idle, then Normal
        assert_eq!(msgs[0].priority, MessagePriority::Shutdown);
        assert_eq!(msgs[1].priority, MessagePriority::Idle);
        assert_eq!(msgs[2].priority, MessagePriority::Normal);
    }

    #[test]
    fn test_mailbox_broadcast() {
        let mut mailbox = TaskMailbox::new();
        mailbox.send(make_message(
            "m1", "coord", "broadcast", MessagePriority::Shutdown,
            MessagePayload::ShutdownRequest { reason: "shutdown".into() },
        ));

        // All agents should see broadcast messages
        let msgs = mailbox.read_for("impl-1");
        assert_eq!(msgs.len(), 1);

        // But now it's read, so impl-2 won't see it
        let msgs2 = mailbox.read_for("impl-2");
        assert_eq!(msgs2.len(), 0);
    }

    #[test]
    fn test_mailbox_unread_count() {
        let mut mailbox = TaskMailbox::new();
        mailbox.send(make_message(
            "m1", "coord", "impl-1", MessagePriority::Normal,
            MessagePayload::Custom { data: serde_json::json!("a") },
        ));
        mailbox.send(make_message(
            "m2", "coord", "impl-1", MessagePriority::Normal,
            MessagePayload::Custom { data: serde_json::json!("b") },
        ));

        assert_eq!(mailbox.unread_count("impl-1"), 2);
        assert_eq!(mailbox.unread_count("impl-2"), 0);

        let _ = mailbox.read_for("impl-1");
        assert_eq!(mailbox.unread_count("impl-1"), 0);
    }

    #[test]
    fn test_mailbox_gc() {
        let mut mailbox = TaskMailbox::new();
        mailbox.send(make_message(
            "m1", "coord", "impl-1", MessagePriority::Normal,
            MessagePayload::Custom { data: serde_json::json!("a") },
        ));
        mailbox.send(make_message(
            "m2", "coord", "impl-1", MessagePriority::Normal,
            MessagePayload::Custom { data: serde_json::json!("b") },
        ));

        let _ = mailbox.read_for("impl-1");
        assert_eq!(mailbox.messages.len(), 2);

        mailbox.gc();
        assert_eq!(mailbox.messages.len(), 0);
    }

    #[test]
    fn test_priority_serde() {
        let p = MessagePriority::Shutdown;
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"shutdown\"");
        let back: MessagePriority = serde_json::from_str(&json).unwrap();
        assert_eq!(back, MessagePriority::Shutdown);
    }
}
