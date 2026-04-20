use thiserror::Error;

use crate::envelope::{DaemonEvent, DaemonRequest, DaemonResponse, IpcMessage};

pub fn encode_line<T: serde::Serialize>(msg: &T) -> Vec<u8> {
    let mut buf = serde_json::to_vec(msg).expect("ipc messages must serialize");
    buf.push(b'\n');
    buf
}

pub fn decode_line(line: &[u8]) -> Result<IpcMessage, ParseError> {
    if line.len() > crate::MAX_LINE_BYTES {
        return Err(ParseError::LineTooLong { size: line.len() });
    }
    let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
    let v: serde_json::Value = serde_json::from_slice(trimmed)?;
    if v.get("event").is_some() {
        let event: DaemonEvent = serde_json::from_value(v)?;
        Ok(IpcMessage::Event(event))
    } else if v.get("method").is_some() {
        Err(ParseError::MisroutedRequest)
    } else {
        let resp: DaemonResponse = serde_json::from_value(v)?;
        Ok(IpcMessage::Response(resp))
    }
}

pub fn decode_request_line(line: &[u8]) -> Result<DaemonRequest, ParseError> {
    if line.len() > crate::MAX_LINE_BYTES {
        return Err(ParseError::LineTooLong { size: line.len() });
    }
    let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
    Ok(serde_json::from_slice(trimmed)?)
}

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("line exceeds MAX_LINE_BYTES: size={size}")]
    LineTooLong { size: usize },
    #[error("decode failed: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("request received on client-inbound stream (wrong direction)")]
    MisroutedRequest,
}
