//! IPC Layer - Unix socket server for TUI-daemon communication
//!
//! This module provides:
//! - Message types for requests and events
//! - Unix socket server for daemon
//! - Client for TUI connection
//! - Length-prefixed JSON codec

pub mod codec;
pub mod messages;

pub use codec::{decode_message, encode_message, JsonCodec, NdJsonCodec};
pub use messages::{
    DaemonError, DaemonEvent, DaemonRequest, DaemonResponse, ErrorCode, Events, IpcMessage,
    Methods,
};
