//! Inter-process communication between llamastash frontends (TUI, CLI) and
//! the daemon. JSON-RPC 2.0 over a 4-byte length-prefixed framing layer
//! on a Unix-domain socket.
//!
//! Module layout:
//! - `framing` — length-prefix codec with a 1 MiB cap.
//! - `protocol` — JSON-RPC types (`Request`, `Response`, `ErrorObject`).
//! - `methods` — server-side method dispatch (Unit 2 ships ping / version /
//!   shutdown; later units bolt on model methods).
//! - `client` — thin async client used by the TUI and CLI.

pub mod client;
pub mod framing;
pub mod methods;
pub mod protocol;

#[allow(unused_imports)]
pub use client::{Client, ClientError, DEFAULT_CALL_TIMEOUT};
#[allow(unused_imports)]
pub use framing::{read_frame, write_frame, FrameError, MAX_FRAME_SIZE};
#[allow(unused_imports)]
pub use methods::{dispatch_request, MethodContext};
#[allow(unused_imports)]
pub use protocol::{ErrorCode, ErrorObject, Request, Response, JSONRPC_VERSION};
