//! IPC Layer - Unix socket server for TUI-daemon communication
//!
//! This module provides:
//! - Message types for requests and events
//! - Unix socket server for daemon
//! - Client for TUI connection
//! - Length-prefixed JSON codec

pub mod client;
pub mod codec;
pub mod messages;
pub mod server;

pub use client::IpcClient;
pub use codec::LooprCodec;
pub use messages::{IpcEvent, IpcRequest, IpcResponse};
pub use server::IpcServer;
