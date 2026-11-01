//! The MCP bridge adapter state machine: versioned-extension negotiation
//! (`initialize` handshake), tool discovery with `EffectDescriptor` mapping,
//! fenced `tools/call` dispatch, crash-to-`UNCERTAIN`, and same-key
//! reconciliation over an injected stdio transport.

use ecosystem_driver_kit::{
    CallAdmission, DriverExtensionDescriptor, EffectDescriptor, JsonValue, PendingCallFence,
    ProviderProbe, SettledOutcome,
};
use std::error::Error;
use std::fmt;

use crate::framing::{self, FrameDecoder};
use crate::jsonrpc::{JsonRpcMessage, JsonRpcResponse, RequestId};
use crate::protocol::{
    InitializeParams, InitializeResult, McpToolInfo, ToolCallOutcome, parse_tools_list_result,
    tool_call_params,
};

/// Preferred MCP protocol versions, newest first (negotiation proposals).
pub const SUPPORTED_MCP_VERSIONS: [&str; 2] = ["2025-03-26", "2024-11-05"];

/// Builds the real (owned) extension descriptor for this adapter build.
#[must_use]
pub fn extension_descriptor() -> DriverExtensionDescriptor {
    DriverExtensionDescriptor::new(
        "llmos.ext.driver.mcp-bridge",
        (1, 0),
        "mcp",
        &SUPPORTED_MCP_VERSIONS,
        "nlos.sabi.DriverProvider/v1.0",
    )
}

/// Byte transport to an external MCP server process (stdio in production; a
/// scripted double in tests).
pub trait McpTransport {
    /// Sends one raw frame (with trailing newline) to the server.
    ///
    /// # Errors
    /// Implementation-defined transport failure.
    fn send_frame(&mut self, frame: &[u8]) -> Result<(), McpBridgeError>;

    /// Blocks until one full frame is available from the server.
    ///
    /// # Errors
    /// Implementation-defined transport failure.
    fn recv_frame(&mut self) -> Result<Vec<u8>, McpBridgeError>;
}

/// Adapter session lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeState {
    /// Created; no `initialize` sent yet.
    Created,
    /// `initialize` sent; awaiting the server handshake response.
    Initializing,
    /// Handshake complete and `notifications/initialized` delivered.
    Ready,
    /// A crash was injected; uncertain calls await reconcile.
    Recovering,
    /// Terminal after a fail-closed negotiation refusal.
    Closed,
}

/// One discovered tool paired with its declared effect descriptor.
#[derive(Clone, Debug, PartialEq)]
pub struct DeclaredTool {
    /// The MCP tool metadata.
    pub tool: McpToolInfo,
    /// The `DRV-LIFE-001` effect declaration bound to this tool.
    pub effect: EffectDescriptor,
}

/// Fail-closed adapter errors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpBridgeError {
    /// Wire/transport framing failure.
    Transport(String),
    /// JSON-RPC envelope failure.
    JsonRpc(String),
    /// MCP payload violation.
    Protocol(String),
    /// Protocol-version negotiation failed (`DRV-NEGO-001`).
    Negotiation(String),
    /// The call was refused by the crash fence.
    Fence(String),
    /// The tool is unknown or carries no effect declaration.
    UnknownTool(String),
    /// A state-machine ordering violation.
    WrongState(&'static str),
}

impl fmt::Display for McpBridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(reason) => write!(f, "mcp transport failure: {reason}"),
            Self::JsonRpc(reason) => write!(f, "json-rpc failure: {reason}"),
            Self::Protocol(reason) => write!(f, "mcp protocol failure: {reason}"),
            Self::Negotiation(reason) => write!(f, "extension negotiation failure: {reason}"),
            Self::Fence(reason) => write!(f, "pending-call fence refusal: {reason}"),
            Self::UnknownTool(name) => write!(f, "unknown or undeclared tool {name:?}"),
            Self::WrongState(reason) => write!(f, "adapter state violation: {reason}"),
        }
    }
}

impl Error for McpBridgeError {}

/// The MCP bridge adapter session.
pub struct McpBridgeAdapter<'a> {
    transport: &'a mut dyn McpTransport,
    decoder: FrameDecoder,
    state: BridgeState,
    negotiated_version: Option<String>,
    server_info: Option<InitializeResult>,
    fence: PendingCallFence,
    declared_tools: Vec<DeclaredTool>,
    next_request_id: u64,
}

impl<'a> McpBridgeAdapter<'a> {
    /// Creates an adapter over `transport` in the `Created` state.
    #[must_use]
    pub fn new(transport: &'a mut dyn McpTransport) -> Self {
        Self {
            transport,
            decoder: FrameDecoder::new(),
            state: BridgeState::Created,
            negotiated_version: None,
            server_info: None,
            fence: PendingCallFence::new(),
            declared_tools: Vec::new(),
            next_request_id: 1,
        }
    }

    /// Current lifecycle state.
    #[must_use]
    pub fn state(&self) -> BridgeState {
        self.state
    }

    /// The negotiated MCP protocol version after a successful handshake.
    #[must_use]
    pub fn negotiated_version(&self) -> Option<&str> {
        self.negotiated_version.as_deref()
    }

    /// The negotiated extension binding after a successful handshake.
    #[must_use]
    pub fn negotiated_extension(&self) -> Option<ecosystem_driver_kit::NegotiatedExtension> {
        let version = self.negotiated_version.as_ref()?;
        ecosystem_driver_kit::negotiate_protocol_version(
            &extension_descriptor(),
            &[version.as_str()],
        )
        .ok()
    }

    /// The server info observed during the handshake.
    #[must_use]
    pub fn server_info(&self) -> Option<&InitializeResult> {
        self.server_info.as_ref()
    }

    /// Read-only view of the pending-call fence.
    #[must_use]
    pub fn fence(&self) -> &PendingCallFence {
        &self.fence
    }

    /// Runs the `initialize` handshake: proposes the preferred version, then
    /// fail-closes unless the server-selected version is one this build
    /// supports (`DRV-NEGO-001`), and finally delivers the
    /// `notifications/initialized` notification.
    ///
    /// # Errors
    /// Returns [`McpBridgeError::Negotiation`] on version mismatch and other
    /// typed errors on transport/protocol failures.
    pub fn initialize(&mut self) -> Result<(), McpBridgeError> {
        if self.state != BridgeState::Created {
            return Err(McpBridgeError::WrongState(
                "initialize requires Created state",
            ));
        }
        self.state = BridgeState::Initializing;
        let request_id = self.take_request_id();
        let params = InitializeParams {
            protocol_version: SUPPORTED_MCP_VERSIONS[0].to_owned(),
            client_name: "mcp-bridge-driver".to_owned(),
            client_version: "1.0.0".to_owned(),
        };
        let message = JsonRpcMessage::request(
            RequestId::Number(request_id),
            "initialize",
            params.to_json(),
        );
        let response = self.round_trip(&message)?;

        let result = match response {
            JsonRpcResponse::Success(inner) => inner.result,
            JsonRpcResponse::Failure(failure) => {
                self.state = BridgeState::Closed;
                return Err(McpBridgeError::Protocol(format!(
                    "initialize rejected: {} {}",
                    failure.code, failure.message
                )));
            }
        };
        let handshake = InitializeResult::from_json(&result)
            .map_err(|error| McpBridgeError::Protocol(error.to_string()))?;
        if !handshake.tools_capability {
            self.state = BridgeState::Closed;
            return Err(McpBridgeError::Protocol(
                "server does not expose the tools capability".to_owned(),
            ));
        }
        let negotiated = ecosystem_driver_kit::negotiate_protocol_version(
            &extension_descriptor(),
            &[handshake.protocol_version.as_str()],
        )
        .map_err(|error| {
            self.state = BridgeState::Closed;
            McpBridgeError::Negotiation(error.to_string())
        })?;
        self.negotiated_version = Some(negotiated.chosen_protocol_version);
        self.server_info = Some(handshake);

        let initialized = JsonRpcMessage::notification(
            "notifications/initialized",
            JsonValue::object(Vec::<(&str, JsonValue)>::new()),
        );
        let frame = framing::encode_frame(&initialized.to_json_string())
            .map_err(|error| McpBridgeError::Transport(error.to_string()))?;
        self.transport
            .send_frame(&frame)
            .map_err(|error| McpBridgeError::Transport(error.to_string()))?;
        self.state = BridgeState::Ready;
        Ok(())
    }

    /// Discovers the server tool catalog and binds every tool to its declared
    /// `EffectDescriptor` from `effect_declarations` (`DRV-LIFE-001`).
    ///
    /// # Errors
    /// Returns [`McpBridgeError::UnknownTool`] when a discovered tool carries
    /// no matching declaration, plus typed transport/protocol errors.
    pub fn discover_tools(
        &mut self,
        effect_declarations: &[EffectDescriptor],
    ) -> Result<Vec<DeclaredTool>, McpBridgeError> {
        if self.state != BridgeState::Ready {
            return Err(McpBridgeError::WrongState(
                "tools/list requires Ready state",
            ));
        }
        let request_id = self.take_request_id();
        let message = JsonRpcMessage::request(
            RequestId::Number(request_id),
            "tools/list",
            JsonValue::object(Vec::<(&str, JsonValue)>::new()),
        );
        let response = self.round_trip(&message)?;
        let result = match response {
            JsonRpcResponse::Success(inner) => inner.result,
            JsonRpcResponse::Failure(failure) => {
                return Err(McpBridgeError::Protocol(format!(
                    "tools/list failed: {} {}",
                    failure.code, failure.message
                )));
            }
        };
        let tools = parse_tools_list_result(&result)
            .map_err(|error| McpBridgeError::Protocol(error.to_string()))?;
        let mut declared = Vec::with_capacity(tools.len());
        for tool in tools {
            let Some(effect) = effect_declarations
                .iter()
                .find(|effect| effect.effect_id == tool.name)
                .cloned()
            else {
                return Err(McpBridgeError::UnknownTool(tool.name));
            };
            declared.push(DeclaredTool { tool, effect });
        }
        self.declared_tools.clone_from(&declared);
        Ok(declared)
    }

    /// The tool catalog discovered in the last `discover_tools` call.
    #[must_use]
    pub fn declared_tools(&self) -> &[DeclaredTool] {
        &self.declared_tools
    }

    /// Invokes one tool under the crash fence. A same-key retry of a settled
    /// call replays the recorded outcome without re-dispatching.
    ///
    /// # Errors
    /// Returns typed fence/protocol/transport errors; in particular
    /// [`McpBridgeError::Fence`] when the key is `UNCERTAIN` (reconcile
    /// first) or conflicts with a different operation identity.
    pub fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: &JsonValue,
        operation_id: &[u8; 16],
        idempotency_key: &[u8; 16],
    ) -> Result<CallOutcome, McpBridgeError> {
        if !matches!(self.state, BridgeState::Ready | BridgeState::Recovering) {
            return Err(McpBridgeError::WrongState(
                "tools/call requires Ready or Recovering state",
            ));
        }
        let Some(declared) = self
            .declared_tools
            .iter()
            .find(|entry| entry.tool.name == tool_name)
        else {
            return Err(McpBridgeError::UnknownTool(tool_name.to_owned()));
        };
        let descriptor = declared.effect.clone();
        match self
            .fence
            .open(idempotency_key, operation_id, &descriptor)
            .map_err(|error| McpBridgeError::Fence(error.to_string()))?
        {
            CallAdmission::Replay(settled) => {
                return Ok(CallOutcome::Replayed(settled));
            }
            CallAdmission::Fresh => {}
        }

        let request_id = self.take_request_id();
        let message = JsonRpcMessage::request(
            RequestId::Number(request_id),
            "tools/call",
            tool_call_params(tool_name, arguments),
        );
        let response = self.round_trip(&message)?;
        match response {
            JsonRpcResponse::Success(inner) => {
                let outcome = ToolCallOutcome::from_json(&inner.result)
                    .map_err(|error| McpBridgeError::Protocol(error.to_string()))?;
                let probe = if outcome.is_error {
                    ProviderProbe::EffectAbsent
                } else {
                    ProviderProbe::EffectLanded {
                        receipt: request_id,
                    }
                };
                let settled = self
                    .fence
                    .settle(idempotency_key, operation_id, &descriptor, probe)
                    .map_err(|error| McpBridgeError::Fence(error.to_string()))?;
                Ok(CallOutcome::Executed {
                    tool_result: outcome,
                    settled,
                })
            }
            JsonRpcResponse::Failure(failure) => {
                let settled = self
                    .fence
                    .settle(
                        idempotency_key,
                        operation_id,
                        &descriptor,
                        ProviderProbe::EffectAbsent,
                    )
                    .map_err(|error| McpBridgeError::Fence(error.to_string()))?;
                Ok(CallOutcome::Failed {
                    code: failure.code,
                    message: failure.message,
                    settled,
                })
            }
        }
    }

    /// Simulates a driver crash mid-flight: every dispatched call is fenced to
    /// `UNCERTAIN` and the session enters [`BridgeState::Recovering`]
    /// (`DRV-LIFE-001`).
    pub fn simulate_crash(&mut self) {
        self.fence.crash();
        self.state = BridgeState::Recovering;
    }

    /// Reconciles one `UNCERTAIN` call against provider evidence under the
    /// *same* idempotency key; never re-dispatches the effect.
    ///
    /// # Errors
    /// Returns [`McpBridgeError::Fence`] for unknown or non-`UNCERTAIN` keys.
    pub fn reconcile(
        &mut self,
        tool_name: &str,
        operation_id: &[u8; 16],
        idempotency_key: &[u8; 16],
        probe: ProviderProbe,
    ) -> Result<SettledOutcome, McpBridgeError> {
        let Some(declared) = self
            .declared_tools
            .iter()
            .find(|entry| entry.tool.name == tool_name)
        else {
            return Err(McpBridgeError::UnknownTool(tool_name.to_owned()));
        };
        let outcome = self
            .fence
            .reconcile(idempotency_key, operation_id, &declared.effect, probe)
            .map_err(|error| McpBridgeError::Fence(error.to_string()))?;
        if self.fence.uncertain_count() == 0 && self.state == BridgeState::Recovering {
            self.state = BridgeState::Ready;
        }
        Ok(outcome)
    }

    fn take_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        id
    }

    fn round_trip(&mut self, message: &JsonRpcMessage) -> Result<JsonRpcResponse, McpBridgeError> {
        let frame = framing::encode_frame(&message.to_json_string())
            .map_err(|error| McpBridgeError::Transport(error.to_string()))?;
        self.transport
            .send_frame(&frame)
            .map_err(|error| McpBridgeError::Transport(error.to_string()))?;
        let expected_id = message.id.clone();
        loop {
            let raw = self
                .transport
                .recv_frame()
                .map_err(|error| McpBridgeError::Transport(error.to_string()))?;
            let frames = self
                .decoder
                .push(&raw)
                .map_err(|error| McpBridgeError::Transport(error.to_string()))?;
            for frame in frames {
                if let Ok(notification) = JsonRpcMessage::parse(&frame)
                    && notification.id.is_none()
                {
                    // Server notifications (e.g. progress) are skipped.
                    continue;
                }
                let response = JsonRpcResponse::parse(&frame)
                    .map_err(|error| McpBridgeError::JsonRpc(error.to_string()))?;
                if response.id() != expected_id.as_ref().expect("request has an id") {
                    continue;
                }
                return Ok(response);
            }
        }
    }
}

/// The outcome of one [`McpBridgeAdapter::call_tool`] invocation.
#[derive(Clone, Debug, PartialEq)]
pub enum CallOutcome {
    /// The tool executed; content plus the settled fence record.
    Executed {
        /// The MCP `tools/call` result content.
        tool_result: ToolCallOutcome,
        /// The settled reconcile record.
        settled: SettledOutcome,
    },
    /// The server returned a JSON-RPC error; the call settled as absent.
    Failed {
        /// JSON-RPC error code.
        code: i64,
        /// JSON-RPC error message.
        message: String,
        /// The settled reconcile record.
        settled: SettledOutcome,
    },
    /// A settled call with the same operation identity was replayed.
    Replayed(SettledOutcome),
}

#[cfg(test)]
mod tests {
    use super::{
        BridgeState, CallOutcome, McpBridgeAdapter, McpBridgeError, McpTransport,
        extension_descriptor,
    };
    use ecosystem_driver_kit::JsonValue;
    use ecosystem_driver_kit::{
        DriverExtensionDescriptor, EffectClass, EffectDescriptor, ProviderProbe,
        negotiate_protocol_version, parse_effect_declarations,
    };
    use std::collections::VecDeque;

    /// Shared handles so tests can observe and steer the server double while
    /// the adapter holds the transport borrow.
    #[derive(Clone, Default)]
    struct ServerControls {
        silent_calls: std::rc::Rc<std::cell::Cell<bool>>,
        calls_executed: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    /// Scripted MCP server double speaking newline-delimited JSON-RPC.
    struct ScriptedServer {
        outgoing: VecDeque<Vec<u8>>,
        server_protocol_version: &'static str,
        expose_tools: bool,
        controls: ServerControls,
    }

    impl ScriptedServer {
        fn new(
            server_protocol_version: &'static str,
            expose_tools: bool,
        ) -> (Self, ServerControls) {
            let controls = ServerControls::default();
            (
                Self {
                    outgoing: VecDeque::new(),
                    server_protocol_version,
                    expose_tools,
                    controls: controls.clone(),
                },
                controls,
            )
        }

        fn handle(&mut self, frame: &[u8]) {
            let text = String::from_utf8_lossy(frame);
            let text = text.trim_end_matches(['\n', '\r']);
            let Ok(value) = ecosystem_driver_kit::JsonValue::parse(text) else {
                return;
            };
            let method = value.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let Some(raw_id) = value.get("id").cloned() else {
                return;
            };
            let Some(id) = crate::jsonrpc::RequestId::from_json(&raw_id) else {
                return;
            };
            match method {
                "initialize" => {
                    let result = crate::protocol::InitializeResult {
                        protocol_version: self.server_protocol_version.to_owned(),
                        server_name: "sample-knowledge-server".to_owned(),
                        server_version: "1.2.0".to_owned(),
                        tools_capability: self.expose_tools,
                    }
                    .to_json();
                    let response = crate::jsonrpc::JsonRpcResponse::success(id, result);
                    self.reply(&response);
                }
                "tools/list" => {
                    let tools: Vec<_> = crate::protocol::sample_tool_catalog()
                        .into_iter()
                        .map(|(tool, _)| tool.to_json())
                        .collect();
                    let response = crate::jsonrpc::JsonRpcResponse::success(
                        id,
                        ecosystem_driver_kit::JsonValue::object([(
                            "tools",
                            ecosystem_driver_kit::JsonValue::Array(tools),
                        )]),
                    );
                    self.reply(&response);
                }
                "tools/call" if !self.controls.silent_calls.get() => {
                    let name = value
                        .get("params")
                        .and_then(|p| p.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_owned();
                    self.controls.calls_executed.borrow_mut().push(name.clone());
                    let result = crate::protocol::ToolCallOutcome {
                        text: format!("ok: {name}"),
                        is_error: false,
                        structured: None,
                    }
                    .to_json();
                    let response = crate::jsonrpc::JsonRpcResponse::success(id, result);
                    self.reply(&response);
                }
                _ => {}
            }
        }

        fn reply(&mut self, response: &crate::jsonrpc::JsonRpcResponse) {
            self.outgoing
                .push_back(format!("{}\n", response.to_json_string()).into_bytes());
        }
    }

    impl McpTransport for ScriptedServer {
        fn send_frame(&mut self, frame: &[u8]) -> Result<(), McpBridgeError> {
            self.handle(frame);
            Ok(())
        }

        fn recv_frame(&mut self) -> Result<Vec<u8>, McpBridgeError> {
            self.outgoing
                .pop_front()
                .ok_or_else(|| McpBridgeError::Transport("server closed".to_owned()))
        }
    }

    fn declarations() -> Vec<EffectDescriptor> {
        parse_effect_declarations(
            "# sample declarations\n\
             effect = search-knowledge-base class=READ_ONLY reconcile=same-idempotency-key summary=\"Query notes\"\n\
             effect = append-field-note class=REVERSIBLE reconcile=same-idempotency-key summary=\"Append note\"\n\
             effect = stage-batch-sync class=COMPENSATABLE reconcile=same-idempotency-key summary=\"Stage sync\" compensation=rollback-batch-sync\n\
             effect = purge-note-cache class=IRREVERSIBLE reconcile=same-idempotency-key summary=\"Purge cache\"\n",
        )
        .expect("declarations parse")
    }

    #[test]
    fn extension_descriptor_matches_kit_negotiation_rules() {
        let descriptor = extension_descriptor();
        assert_eq!(descriptor.extension_id, "llmos.ext.driver.mcp-bridge");
        assert_eq!(
            descriptor,
            DriverExtensionDescriptor::new(
                "llmos.ext.driver.mcp-bridge",
                (1, 0),
                "mcp",
                &["2025-03-26", "2024-11-05"],
                "nlos.sabi.DriverProvider/v1.0",
            )
        );
        assert!(negotiate_protocol_version(&descriptor, &["2024-11-05"]).is_ok());
    }

    #[test]
    fn full_handshake_discover_and_fenced_calls() {
        let (mut server, controls) = ScriptedServer::new("2025-03-26", true);
        let mut adapter = McpBridgeAdapter::new(&mut server);
        adapter.initialize().expect("initialize");
        assert_eq!(adapter.state(), BridgeState::Ready);
        assert_eq!(adapter.negotiated_version(), Some("2025-03-26"));
        assert!(adapter.server_info().is_some());
        assert!(adapter.negotiated_extension().is_some());

        let tools = adapter.discover_tools(&declarations()).expect("discover");
        assert_eq!(tools.len(), 4);
        assert_eq!(tools[3].effect.class, EffectClass::Irreversible);

        let key = [0x51; 16];
        let op = [0x61; 16];
        let outcome = adapter
            .call_tool(
                "purge-note-cache",
                &ecosystem_driver_kit::JsonValue::object([("scope", "stale".into())]),
                &op,
                &key,
            )
            .expect("call tool");
        assert!(matches!(outcome, CallOutcome::Executed { .. }));
        assert_eq!(controls.calls_executed.borrow().len(), 1);

        // Same-key retry replays instead of re-executing.
        let replay = adapter
            .call_tool(
                "purge-note-cache",
                &ecosystem_driver_kit::JsonValue::object(Vec::<(&str, JsonValue)>::new()),
                &op,
                &key,
            )
            .expect("replay");
        assert!(matches!(replay, CallOutcome::Replayed(_)));
        assert_eq!(controls.calls_executed.borrow().len(), 1);

        // Undeclared tool names fail closed.
        assert!(matches!(
            adapter.call_tool(
                "unknown-tool",
                &ecosystem_driver_kit::JsonValue::object(Vec::<(&str, JsonValue)>::new()),
                &[9; 16],
                &[8; 16]
            ),
            Err(McpBridgeError::UnknownTool(_))
        ));
    }

    #[test]
    fn version_mismatch_fails_closed_to_closed_state() {
        let (mut server, _controls) = ScriptedServer::new("1999-01-01", true);
        let mut adapter = McpBridgeAdapter::new(&mut server);
        assert!(matches!(
            adapter.initialize(),
            Err(McpBridgeError::Negotiation(_))
        ));
        assert_eq!(adapter.state(), BridgeState::Closed);
    }

    #[test]
    fn missing_tools_capability_fails_closed() {
        let (mut server, _controls) = ScriptedServer::new("2025-03-26", false);
        let mut adapter = McpBridgeAdapter::new(&mut server);
        assert!(matches!(
            adapter.initialize(),
            Err(McpBridgeError::Protocol(_))
        ));
    }

    #[test]
    fn crash_fences_to_uncertain_and_reconcile_restores_ready() {
        // `silent_calls` makes the server never answer `tools/call`, so the
        // adapter dies mid-flight with the fence entry left `Dispatched` —
        // the real DRV-LIFE-001 crash shape.
        let (mut server, controls) = ScriptedServer::new("2025-03-26", true);
        controls.silent_calls.set(true);
        let mut adapter = McpBridgeAdapter::new(&mut server);
        adapter.initialize().expect("initialize");
        adapter.discover_tools(&declarations()).expect("discover");

        let key = [0x71; 16];
        let op = [0x81; 16];
        let mid_flight = adapter.call_tool(
            "stage-batch-sync",
            &ecosystem_driver_kit::JsonValue::object([("batch", "b-17".into())]),
            &op,
            &key,
        );
        assert!(
            matches!(&mid_flight, Err(McpBridgeError::Transport(reason)) if reason.contains("closed")),
            "mid-flight transport death, got {mid_flight:?}"
        );
        // The fence holds the call as Dispatched (in flight, not yet uncertain).
        assert_eq!(adapter.fence().uncertain_count(), 0);

        adapter.simulate_crash();
        assert_eq!(adapter.fence().uncertain_count(), 1);
        assert_eq!(adapter.state(), BridgeState::Recovering);

        // Blind same-key retry while UNCERTAIN is refused.
        assert!(matches!(
            adapter.call_tool(
                "stage-batch-sync",
                &ecosystem_driver_kit::JsonValue::object(Vec::<(&str, JsonValue)>::new()),
                &op,
                &key
            ),
            Err(McpBridgeError::Fence(_))
        ));

        // Reconcile with provider evidence under the SAME key settles it.
        let settled = adapter
            .reconcile(
                "stage-batch-sync",
                &op,
                &key,
                ProviderProbe::EffectLanded { receipt: 42 },
            )
            .expect("reconcile");
        assert_eq!(settled.probe, ProviderProbe::EffectLanded { receipt: 42 });
        assert_eq!(adapter.fence().uncertain_count(), 0);
        assert_eq!(adapter.state(), BridgeState::Ready);

        // Post-reconcile retry replays the settled outcome (no new dispatch).
        controls.silent_calls.set(false);
        let replay = adapter
            .call_tool(
                "stage-batch-sync",
                &ecosystem_driver_kit::JsonValue::object(Vec::<(&str, JsonValue)>::new()),
                &op,
                &key,
            )
            .expect("replay after reconcile");
        assert!(matches!(replay, CallOutcome::Replayed(_)));
    }
}
