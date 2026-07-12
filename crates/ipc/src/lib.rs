//! Typed wire protocol between the loopr daemon and its clients.
//!
//! # Wire shape
//!
//! Per `docs/vision.md` §ipc: JSON-RPC-style envelope inherited verbatim
//! from v3/v4. Request is `{id, method, params}`; response is `{id,
//! result?, error?}`; event is `{event, data}`. See [`DaemonRequest`],
//! [`DaemonResponse`], [`DaemonEvent`]. Framing: NDJSON, one message per
//! line, max [`MAX_LINE_BYTES`] per line.
//!
//! # Rust-side typing
//!
//! The wire is deliberately loose (Value payloads); the Rust surface is
//! typed. [`Method`] is the exhaustive internal dispatch enum; [`RpcError`]
//! is the closed error enum that serializes through the `{code, message}`
//! wire shape. Handlers in `loopr` match on these, and compile errors
//! catch unhandled variants.
//!
//! # I/O discipline
//!
//! This crate is intentionally I/O-free: no tokio, no sockets. Async
//! transport lives in `loopr` per `crates/ipc/CLAUDE.md`.

mod envelope;
mod error;
mod frame;
mod method;
mod records;

/// Maximum bytes per NDJSON line, enforced by [`decode_line`] /
/// [`decode_request_line`]. Matches v3/v4 and the value wired to
/// `tokio_util::codec::LinesCodec::new_with_max_length` in `loopr`'s
/// transport layer. See `crates/ipc/docs/design/2026-04-19-protocol.md`.
pub const MAX_LINE_BYTES: usize = 1 << 20; // 1 MiB

/// Wire protocol version. Advertised via the `system.handshake` method;
/// negotiated by `loopr` at Stage 4. See `crates/ipc/docs/design/2026-04-19-protocol.md`.
pub const PROTOCOL_VERSION: u32 = 1;

pub use envelope::{DaemonEvent, DaemonRequest, DaemonResponse, IpcMessage};
pub use error::{RpcError, RpcErrorWire};
pub use frame::{ParseError, decode_line, decode_request_line, encode_line};
pub use method::{
    BudgetResetResult, DIRECTOR_CHAT_MESSAGE_BYTE_CAP, DirectorChatParams, DirectorChatResult, DirectorStatusParams,
    DirectorStatusResult, DirectorStatusSnapshot, HandshakeParams, HandshakeResult, Method, MethodName,
    PlanCreateParams, PlanCreateResult, PlanOverrideParams, PlanOverrideResult, StatusResult,
};
pub use records::{
    BundleSummary, PlanSummary, RecordGetParams, RecordKind, RecordListParams, RecordResult, RecordsResult,
    TickSummary, WorkSummary,
};

#[cfg(test)]
mod tests;
