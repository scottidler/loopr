use tokio_util::codec::LinesCodec;

/// Create the standard NDJSON codec used for IPC framing.
/// Each line is one JSON message. Max line length is 1 MiB.
/// Large limit is needed because tick validation logs from cargo
/// can be substantial (especially clippy/test output).
pub fn ndjson_codec() -> LinesCodec {
    LinesCodec::new_with_max_length(1024 * 1024)
}

// Test-only codec helpers (thin serde_json wrappers for readability in tests)
#[cfg(test)]
use super::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, IpcMessage};

#[cfg(test)]
pub fn encode_request(req: &DaemonRequest) -> Result<String, serde_json::Error> {
    serde_json::to_string(req)
}

#[cfg(test)]
pub fn encode_response(resp: &DaemonResponse) -> Result<String, serde_json::Error> {
    serde_json::to_string(resp)
}

#[cfg(test)]
pub fn encode_event(event: &DaemonEvent) -> Result<String, serde_json::Error> {
    serde_json::to_string(event)
}

#[cfg(test)]
pub fn decode_request(line: &str) -> Result<DaemonRequest, serde_json::Error> {
    serde_json::from_str(line)
}

#[cfg(test)]
pub fn decode_client_message(line: &str) -> Result<IpcMessage, serde_json::Error> {
    IpcMessage::from_json(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_ndjson_codec_max_length() {
        let codec = ndjson_codec();
        let _ = codec;
    }

    #[test]
    fn test_encode_request() {
        let req = DaemonRequest::new(1, "plan.create", json!({"title": "test"}));
        let line = encode_request(&req).unwrap();
        assert!(line.contains("\"method\":\"plan.create\""));
        assert!(line.contains("\"id\":1"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn test_encode_response_ok() {
        let resp = DaemonResponse::ok(1, json!({"status": "Draft"}));
        let line = encode_response(&resp).unwrap();
        assert!(line.contains("\"id\":1"));
        assert!(line.contains("\"result\""));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn test_encode_response_err() {
        use super::super::protocol::RpcError;
        let resp = DaemonResponse::err(2, RpcError::method_not_found("bad"));
        let line = encode_response(&resp).unwrap();
        assert!(line.contains("\"id\":2"));
        assert!(line.contains("\"error\""));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn test_encode_event() {
        let event = DaemonEvent::record_created("plan", "p1");
        let line = encode_event(&event).unwrap();
        assert!(line.contains("\"event\":\"record.created\""));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn test_decode_request() {
        let json = r#"{"id":1,"method":"work.get","params":{"id":"wi1"}}"#;
        let req = decode_request(json).unwrap();
        assert_eq!(req.id, 1);
        assert_eq!(req.method, "work.get");
        assert_eq!(req.params["id"], "wi1");
    }

    #[test]
    fn test_decode_request_no_params() {
        let json = r#"{"id":5,"method":"system.status"}"#;
        let req = decode_request(json).unwrap();
        assert_eq!(req.id, 5);
        assert_eq!(req.method, "system.status");
        assert_eq!(req.params, serde_json::Value::Null);
    }

    #[test]
    fn test_decode_client_message_response() {
        let json = r#"{"id":1,"result":{"status":"ok"}}"#;
        let msg = decode_client_message(json).unwrap();
        match msg {
            IpcMessage::Response(r) => {
                assert_eq!(r.id, 1);
                assert!(!r.is_error());
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn test_decode_client_message_event() {
        let json = r#"{"event":"tick.published","data":{"sha":"abc"}}"#;
        let msg = decode_client_message(json).unwrap();
        match msg {
            IpcMessage::Event(e) => {
                assert_eq!(e.event, "tick.published");
            }
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn test_encode_decode_request_roundtrip() {
        let req = DaemonRequest::new(42, "bundle.propose", json!({"work_id": "wi1"}));
        let line = encode_request(&req).unwrap();
        let decoded = decode_request(&line).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_encode_decode_response_roundtrip() {
        let resp = DaemonResponse::ok(7, json!({"id": "p1", "status": "Active"}));
        let line = encode_response(&resp).unwrap();
        let msg = decode_client_message(&line).unwrap();
        match msg {
            IpcMessage::Response(r) => assert_eq!(r, resp),
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn test_encode_decode_event_roundtrip() {
        let event = DaemonEvent::transition_completed("work", "wi1", "Draft", "Ready", "Coordinator");
        let line = encode_event(&event).unwrap();
        let msg = decode_client_message(&line).unwrap();
        match msg {
            IpcMessage::Event(e) => assert_eq!(e, event),
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn test_decode_request_invalid_json() {
        let result = decode_request("not valid json");
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_client_message_invalid_json() {
        let result = decode_client_message("{broken");
        assert!(result.is_err());
    }
}
