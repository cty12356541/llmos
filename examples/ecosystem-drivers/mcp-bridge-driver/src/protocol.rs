//! MCP wire-message layer for `mcp-bridge-driver`: the `initialize`
//! handshake, `tools/list` discovery, and `tools/call` invocation shapes of
//! the Model Context Protocol stdio transport.

use ecosystem_driver_kit::{EffectClass, JsonValue};
use std::error::Error;
use std::fmt;

/// MCP `initialize` request parameters the bridge sends to the server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializeParams {
    /// Protocol version this bridge proposes.
    pub protocol_version: String,
    /// Human-readable client name.
    pub client_name: String,
    /// Client build version.
    pub client_version: String,
}

impl InitializeParams {
    /// Renders the `initialize` params object.
    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        JsonValue::object([
            (
                "protocolVersion",
                JsonValue::from(self.protocol_version.as_str()),
            ),
            (
                "clientInfo",
                JsonValue::object([
                    ("name", JsonValue::from(self.client_name.as_str())),
                    ("version", JsonValue::from(self.client_version.as_str())),
                ]),
            ),
            (
                "capabilities",
                JsonValue::object([("roots", JsonValue::object([("listChanged", false.into())]))]),
            ),
        ])
    }
}

/// MCP `initialize` response fields the bridge requires from the server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializeResult {
    /// Protocol version the server selected.
    pub protocol_version: String,
    /// Server name.
    pub server_name: String,
    /// Server build version.
    pub server_version: String,
    /// Whether the server exposes the `tools` capability.
    pub tools_capability: bool,
}

impl InitializeResult {
    /// Parses an `initialize` result payload.
    ///
    /// # Errors
    /// Returns [`McpProtocolError`] when mandatory members are missing.
    pub fn from_json(result: &JsonValue) -> Result<Self, McpProtocolError> {
        let protocol_version = result
            .get("protocolVersion")
            .and_then(JsonValue::as_str)
            .ok_or(McpProtocolError::MissingMember("protocolVersion"))?
            .to_owned();
        let Some(info) = result.get("serverInfo") else {
            return Err(McpProtocolError::MissingMember("serverInfo"));
        };
        let server_name = info
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or(McpProtocolError::MissingMember("serverInfo.name"))?
            .to_owned();
        let server_version = info
            .get("version")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let tools_capability = result
            .get("capabilities")
            .is_some_and(|caps| caps.get("tools").is_some());
        Ok(Self {
            protocol_version,
            server_name,
            server_version,
            tools_capability,
        })
    }

    /// Builds a server-side `initialize` result payload (used by the in-test
    /// server double).
    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        let capabilities = if self.tools_capability {
            JsonValue::object([("tools", JsonValue::object([("listChanged", false.into())]))])
        } else {
            JsonValue::object(Vec::<(&str, JsonValue)>::new())
        };
        JsonValue::object([
            (
                "protocolVersion",
                JsonValue::from(self.protocol_version.as_str()),
            ),
            (
                "serverInfo",
                JsonValue::object([
                    ("name", JsonValue::from(self.server_name.as_str())),
                    ("version", JsonValue::from(self.server_version.as_str())),
                ]),
            ),
            ("capabilities", capabilities),
        ])
    }
}

/// One discovered MCP tool.
#[derive(Clone, Debug, PartialEq)]
pub struct McpToolInfo {
    /// Tool name used in `tools/call`.
    pub name: String,
    /// Human description.
    pub description: String,
    /// Minimal input schema description.
    pub input_schema: JsonValue,
}

impl McpToolInfo {
    /// Renders one `tools` array entry.
    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        JsonValue::object([
            ("name", JsonValue::from(self.name.as_str())),
            ("description", JsonValue::from(self.description.as_str())),
            ("inputSchema", self.input_schema.clone()),
        ])
    }

    /// Parses one `tools` array entry.
    ///
    /// # Errors
    /// Returns [`McpProtocolError`] when `name` is missing.
    pub fn from_json(value: &JsonValue) -> Result<Self, McpProtocolError> {
        let name = value
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or(McpProtocolError::MissingMember("tools[].name"))?
            .to_owned();
        Ok(Self {
            description: value
                .get("description")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
                .to_owned(),
            input_schema: value.get("inputSchema").cloned().unwrap_or(JsonValue::Null),
            name,
        })
    }
}

/// Parses a `tools/list` result into tool entries.
///
/// # Errors
/// Returns [`McpProtocolError`] when the `tools` member is missing, is not an
/// array, or an entry lacks a name.
pub fn parse_tools_list_result(result: &JsonValue) -> Result<Vec<McpToolInfo>, McpProtocolError> {
    let Some(items) = result.get("tools").and_then(JsonValue::as_array) else {
        return Err(McpProtocolError::ShapeViolation("tools must be an array"));
    };
    items.iter().map(McpToolInfo::from_json).collect()
}

/// The outcome of one `tools/call` invocation.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCallOutcome {
    /// Joined text content blocks of the result.
    pub text: String,
    /// `isError` flag from the server (`false` when absent).
    pub is_error: bool,
    /// Structured content blob when present (`None` otherwise).
    pub structured: Option<JsonValue>,
}

impl ToolCallOutcome {
    /// Parses a `tools/call` result payload.
    ///
    /// # Errors
    /// Returns [`McpProtocolError`] when `content` is missing or malformed.
    pub fn from_json(result: &JsonValue) -> Result<Self, McpProtocolError> {
        let Some(content) = result.get("content").and_then(JsonValue::as_array) else {
            return Err(McpProtocolError::MissingMember("content"));
        };
        let mut text = String::new();
        for block in content {
            if block.get("type").and_then(JsonValue::as_str) == Some("text")
                && let Some(chunk) = block.get("text").and_then(JsonValue::as_str)
            {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(chunk);
            }
        }
        let is_error = result
            .get("isError")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        Ok(Self {
            text,
            is_error,
            structured: result.get("structuredContent").cloned(),
        })
    }

    /// Builds a `tools/call` result payload (used by the in-test server double).
    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        let content = JsonValue::Array(vec![JsonValue::object([
            ("type", JsonValue::from("text")),
            ("text", JsonValue::from(self.text.as_str())),
        ])]);
        let mut object = vec![
            ("content", content),
            ("isError", JsonValue::from(self.is_error)),
        ];
        if let Some(structured) = &self.structured {
            object.push(("structuredContent", structured.clone()));
        }
        JsonValue::object(object)
    }
}

/// Builds the `tools/call` params object.
#[must_use]
pub fn tool_call_params(tool_name: &str, arguments: &JsonValue) -> JsonValue {
    JsonValue::object([
        ("name", JsonValue::from(tool_name)),
        ("arguments", arguments.clone()),
    ])
}

/// The sample tool catalog exposed by the reference MCP server double: four
/// tools covering all four `EffectClass` categories.
#[must_use]
pub fn sample_tool_catalog() -> Vec<(McpToolInfo, EffectClass)> {
    let schema = |properties: &[(&str, &str)], required: &[&str]| {
        JsonValue::object([
            ("type", JsonValue::from("object")),
            (
                "properties",
                JsonValue::object(
                    properties
                        .iter()
                        .map(|(key, kind)| {
                            (*key, JsonValue::object([("type", JsonValue::from(*kind))]))
                        })
                        .collect::<Vec<(&str, JsonValue)>>(),
                ),
            ),
            (
                "required",
                JsonValue::Array(required.iter().map(|item| JsonValue::from(*item)).collect()),
            ),
        ])
    };
    vec![
        (
            McpToolInfo {
                name: "search-knowledge-base".to_owned(),
                description: "Query the connected knowledge base without mutation".to_owned(),
                input_schema: schema(&[("query", "string")], &["query"]),
            },
            EffectClass::ReadOnly,
        ),
        (
            McpToolInfo {
                name: "append-field-note".to_owned(),
                description: "Append one reversible note entry".to_owned(),
                input_schema: schema(&[("note", "string")], &["note"]),
            },
            EffectClass::Reversible,
        ),
        (
            McpToolInfo {
                name: "stage-batch-sync".to_owned(),
                description: "Stage a batch sync with a compensating rollback handler".to_owned(),
                input_schema: schema(&[("batch", "string")], &["batch"]),
            },
            EffectClass::Compensatable,
        ),
        (
            McpToolInfo {
                name: "purge-note-cache".to_owned(),
                description: "Irreversibly purge cached note artifacts".to_owned(),
                input_schema: schema(&[("scope", "string")], &["scope"]),
            },
            EffectClass::Irreversible,
        ),
    ]
}

/// Fail-closed MCP payload violations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpProtocolError {
    /// A mandatory member was absent.
    MissingMember(&'static str),
    /// A member had the wrong shape.
    ShapeViolation(&'static str),
}

impl fmt::Display for McpProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingMember(member) => write!(f, "MCP payload missing member {member}"),
            Self::ShapeViolation(reason) => write!(f, "MCP payload shape violation: {reason}"),
        }
    }
}

impl Error for McpProtocolError {}

#[cfg(test)]
mod tests {
    use super::{
        InitializeParams, InitializeResult, McpProtocolError, McpToolInfo, ToolCallOutcome,
        parse_tools_list_result, sample_tool_catalog, tool_call_params,
    };
    use ecosystem_driver_kit::{EffectClass, JsonValue};

    #[test]
    fn initialize_result_round_trips() {
        let result = InitializeResult {
            protocol_version: "2025-03-26".to_owned(),
            server_name: "sample-knowledge-server".to_owned(),
            server_version: "1.2.0".to_owned(),
            tools_capability: true,
        };
        let parsed = InitializeResult::from_json(&result.to_json()).expect("round trip");
        assert_eq!(parsed, result);
        assert!(
            InitializeResult::from_json(&JsonValue::object(Vec::<(&str, JsonValue)>::new()))
                .is_err()
        );
    }

    #[test]
    fn tools_list_and_call_shapes_parse_and_fail_closed() {
        let catalog = sample_tool_catalog();
        assert_eq!(catalog.len(), 4);
        for (expected, class) in [
            ("search-knowledge-base", EffectClass::ReadOnly),
            ("append-field-note", EffectClass::Reversible),
            ("stage-batch-sync", EffectClass::Compensatable),
            ("purge-note-cache", EffectClass::Irreversible),
        ] {
            assert!(
                catalog
                    .iter()
                    .any(|(tool, kind)| tool.name == expected && *kind == class)
            );
        }

        let tools_json = JsonValue::object([(
            "tools",
            JsonValue::Array(catalog.iter().map(|(tool, _)| tool.to_json()).collect()),
        )]);
        let parsed = parse_tools_list_result(&tools_json).expect("parse tools/list");
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].name, "search-knowledge-base");

        assert!(
            parse_tools_list_result(&JsonValue::object(Vec::<(&str, JsonValue)>::new())).is_err()
        );
        assert!(matches!(
            parse_tools_list_result(&JsonValue::object([("tools", JsonValue::from(3_u64))])),
            Err(McpProtocolError::ShapeViolation(_))
        ));

        let call = ToolCallOutcome {
            text: "note staged".to_owned(),
            is_error: false,
            structured: None,
        };
        let parsed_call = ToolCallOutcome::from_json(&call.to_json()).expect("parse call");
        assert_eq!(parsed_call, call);
        assert!(
            ToolCallOutcome::from_json(&JsonValue::object(Vec::<(&str, JsonValue)>::new()))
                .is_err()
        );

        let params = tool_call_params(
            "search-knowledge-base",
            &JsonValue::object([("query", "rust".into())]),
        );
        assert_eq!(
            params.get("name").and_then(JsonValue::as_str),
            Some("search-knowledge-base")
        );
        assert_eq!(
            params
                .get("arguments")
                .and_then(|a| a.get("query"))
                .and_then(JsonValue::as_str),
            Some("rust")
        );

        let _ = InitializeParams {
            protocol_version: "2025-03-26".to_owned(),
            client_name: "mcp-bridge-driver".to_owned(),
            client_version: "1.0.0".to_owned(),
        }
        .to_json();
        let _ =
            McpToolInfo::from_json(&JsonValue::object(Vec::<(&str, JsonValue)>::new())).is_err();
    }
}
