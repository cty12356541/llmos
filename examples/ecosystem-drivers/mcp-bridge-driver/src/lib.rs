//! `mcp-bridge-driver` — W80-E6B third-party userland MCP-compatible Driver
//! reference adapter (`examples/ecosystem-drivers`, Stage E `E6`).
//!
//! Wraps an external Model Context Protocol server as a *versioned* NLOS
//! driver extension (`DRV-PROTO-001`):
//! - [`jsonrpc`]: JSON-RPC 2.0 envelope codec;
//! - [`framing`]: newline-delimited stdio frame codec with hard bounds;
//! - [`protocol`]: MCP `initialize` / `tools/list` / `tools/call` shapes and
//!   the four-class sample tool catalog;
//! - [`adapter`]: the bridge state machine — fail-closed protocol-version
//!   negotiation (`DRV-NEGO-001`), `EffectDescriptor`-bound tool discovery,
//!   crash-fenced `tools/call` with same-`IdempotencyKey` `UNCERTAIN`
//!   reconciliation (`DRV-LIFE-001` / `KABI-UNCERTAIN-001`).

#![forbid(unsafe_code)]

pub mod adapter;
pub mod framing;
pub mod jsonrpc;
pub mod protocol;

pub use adapter::{
    BridgeState, CallOutcome, DeclaredTool, McpBridgeAdapter, McpBridgeError, McpTransport,
    SUPPORTED_MCP_VERSIONS, extension_descriptor,
};
pub use framing::{FrameDecoder, FrameError, MAX_BUFFERED_BYTES, MAX_FRAME_BYTES, encode_frame};
pub use jsonrpc::{JsonRpcError, JsonRpcMessage, JsonRpcResponse, RequestId};
pub use protocol::{
    InitializeParams, InitializeResult, McpProtocolError, McpToolInfo, ToolCallOutcome,
    parse_tools_list_result, sample_tool_catalog, tool_call_params,
};

/// Canonical Cargo package identifier for this sample crate.
pub const CRATE_NAME: &str = "mcp-bridge-driver";

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }
}
