//! JSON-RPC 2.0 wire envelope for the MCP bridge sample (`mcp-bridge-driver`).

use ecosystem_driver_kit::{JsonError, JsonValue};
use std::error::Error;
use std::fmt;

/// JSON-RPC request/notification id (number or string per JSON-RPC 2.0 §4).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum RequestId {
    /// Numeric id.
    Number(u64),
    /// String id.
    Text(String),
}

impl RequestId {
    /// Renders the id as a `JsonValue`.
    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        match self {
            Self::Number(n) => JsonValue::from(*n),
            Self::Text(s) => JsonValue::from(s.as_str()),
        }
    }

    /// Parses a `JsonValue` into a `RequestId`.
    #[must_use]
    pub fn from_json(value: &JsonValue) -> Option<Self> {
        match value {
            JsonValue::Number(n) => n.as_u64().map(Self::Number),
            JsonValue::String(s) => Some(Self::Text(s.clone())),
            _ => None,
        }
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::Text(s) => write!(f, "{s:?}"),
        }
    }
}

/// A JSON-RPC 2.0 request (has `id`) or notification (no `id`).
#[derive(Clone, Debug, PartialEq)]
pub struct JsonRpcMessage {
    /// `jsonrpc` member: always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Correlation id; `None` marks a notification.
    pub id: Option<RequestId>,
    /// Method name being invoked.
    pub method: String,
    /// Positional or named parameters (`None` omits the member).
    pub params: Option<JsonValue>,
}

impl JsonRpcMessage {
    /// Builds a request with an id.
    #[must_use]
    pub fn request(id: RequestId, method: &str, params: JsonValue) -> Self {
        Self {
            jsonrpc: "2.0",
            id: Some(id),
            method: method.to_owned(),
            params: Some(params),
        }
    }

    /// Builds a notification (no id).
    #[must_use]
    pub fn notification(method: &str, params: JsonValue) -> Self {
        Self {
            jsonrpc: "2.0",
            id: None,
            method: method.to_owned(),
            params: Some(params),
        }
    }

    /// Serializes the message into compact JSON.
    #[must_use]
    pub fn to_json_string(&self) -> String {
        let mut object = vec![("jsonrpc".to_owned(), JsonValue::from("2.0"))];
        if let Some(id) = &self.id {
            object.push(("id".to_owned(), id.to_json()));
        }
        object.push(("method".to_owned(), JsonValue::from(self.method.as_str())));
        if let Some(params) = &self.params {
            object.push(("params".to_owned(), params.clone()));
        }
        JsonValue::object(object).to_json_string()
    }

    /// Parses one JSON-RPC request/notification frame.
    ///
    /// # Errors
    /// Returns [`JsonRpcError`] on JSON syntax errors, missing `method`, a
    /// non-`"2.0"` jsonrpc member, or `params` that is not an object/array.
    pub fn parse(input: &str) -> Result<Self, JsonRpcError> {
        let value = JsonValue::parse(input).map_err(JsonRpcError::Syntax)?;
        let Some(object) = value.as_object() else {
            return Err(JsonRpcError::Shape("message must be a JSON object"));
        };
        match object.get("jsonrpc") {
            Some(JsonValue::String(v)) if v == "2.0" => {}
            _ => return Err(JsonRpcError::Shape("jsonrpc member must be \"2.0\"")),
        }
        let id = match object.get("id") {
            None => None,
            Some(raw) => Some(
                RequestId::from_json(raw)
                    .ok_or(JsonRpcError::Shape("id must be a number or string"))?,
            ),
        };
        let Some(JsonValue::String(method)) = object.get("method") else {
            return Err(JsonRpcError::Shape("method member must be a string"));
        };
        let params = match object.get("params") {
            None => None,
            Some(value @ (JsonValue::Object(_) | JsonValue::Array(_))) => Some(value.clone()),
            Some(_) => return Err(JsonRpcError::Shape("params must be an object or array")),
        };
        Ok(Self {
            jsonrpc: "2.0",
            id,
            method: method.clone(),
            params,
        })
    }
}

/// A JSON-RPC 2.0 success response.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonRpcSuccess {
    /// Correlated request id.
    pub id: RequestId,
    /// Result payload.
    pub result: JsonValue,
}

/// A JSON-RPC 2.0 error response (`code` / `message` per §5.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonRpcFailure {
    /// Correlated request id.
    pub id: RequestId,
    /// Numeric application error code.
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
}

/// A JSON-RPC 2.0 response envelope.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonRpcResponse {
    /// `result`-bearing success.
    Success(JsonRpcSuccess),
    /// `error`-bearing failure.
    Failure(JsonRpcFailure),
}

impl JsonRpcResponse {
    /// Builds a success response.
    #[must_use]
    pub fn success(id: RequestId, result: JsonValue) -> Self {
        Self::Success(JsonRpcSuccess { id, result })
    }

    /// Builds a failure response.
    #[must_use]
    pub fn failure(id: RequestId, code: i64, message: &str) -> Self {
        Self::Failure(JsonRpcFailure {
            id,
            code,
            message: message.to_owned(),
        })
    }

    /// The correlated request id.
    #[must_use]
    pub fn id(&self) -> &RequestId {
        match self {
            Self::Success(inner) => &inner.id,
            Self::Failure(inner) => &inner.id,
        }
    }

    /// Serializes the response into compact JSON.
    #[must_use]
    pub fn to_json_string(&self) -> String {
        let id = self.id().to_json();
        match self {
            Self::Success(inner) => JsonValue::object([
                ("jsonrpc", JsonValue::from("2.0")),
                ("id", id),
                ("result", inner.result.clone()),
            ])
            .to_json_string(),
            Self::Failure(inner) => JsonValue::object([
                ("jsonrpc", JsonValue::from("2.0")),
                ("id", id),
                (
                    "error",
                    JsonValue::object([
                        ("code", JsonValue::from(inner.code)),
                        ("message", JsonValue::from(inner.message.as_str())),
                    ]),
                ),
            ])
            .to_json_string(),
        }
    }

    /// Parses one JSON-RPC response frame.
    ///
    /// # Errors
    /// Returns [`JsonRpcError`] on syntax errors, missing id, or an envelope
    /// carrying both `result` and `error` (or neither).
    pub fn parse(input: &str) -> Result<Self, JsonRpcError> {
        let value = JsonValue::parse(input).map_err(JsonRpcError::Syntax)?;
        let Some(object) = value.as_object() else {
            return Err(JsonRpcError::Shape("response must be a JSON object"));
        };
        let Some(raw_id) = object.get("id") else {
            return Err(JsonRpcError::Shape("response requires an id member"));
        };
        let id = RequestId::from_json(raw_id)
            .ok_or(JsonRpcError::Shape("id must be a number or string"))?;
        match (object.get("result"), object.get("error")) {
            (Some(result), None) => Ok(Self::Success(JsonRpcSuccess {
                id,
                result: result.clone(),
            })),
            (None, Some(JsonValue::Object(error))) => {
                let code = error
                    .get("code")
                    .and_then(JsonValue::as_i64)
                    .ok_or(JsonRpcError::Shape("error.code must be an integer"))?;
                let message = error
                    .get("message")
                    .and_then(JsonValue::as_str)
                    .ok_or(JsonRpcError::Shape("error.message must be a string"))?
                    .to_owned();
                Ok(Self::Failure(JsonRpcFailure { id, code, message }))
            }
            (Some(_), Some(_)) => Err(JsonRpcError::Shape(
                "response carries both result and error",
            )),
            (None, None) => Err(JsonRpcError::Shape(
                "response carries neither result nor error",
            )),
            (None, Some(_)) => Err(JsonRpcError::Shape("error must be an object")),
        }
    }
}

/// JSON-RPC envelope failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JsonRpcError {
    /// Underlying JSON syntax error.
    Syntax(JsonError),
    /// Structural violation of the JSON-RPC 2.0 envelope.
    Shape(&'static str),
}

impl fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(error) => write!(f, "JSON-RPC payload syntax error: {error}"),
            Self::Shape(reason) => write!(f, "JSON-RPC 2.0 shape violation: {reason}"),
        }
    }
}

impl Error for JsonRpcError {}

#[cfg(test)]
mod tests {
    use super::{JsonRpcMessage, JsonRpcResponse, RequestId};
    use ecosystem_driver_kit::JsonValue;
    use std::collections::BTreeMap;

    #[test]
    fn requests_and_notifications_round_trip() {
        let request = JsonRpcMessage::request(
            RequestId::Number(7),
            "tools/call",
            ecosystem_driver_kit::JsonValue::object([("name", "search".into())]),
        );
        let encoded = request.to_json_string();
        assert!(encoded.contains("\"jsonrpc\":\"2.0\""));
        assert!(encoded.contains("\"id\":7"));
        let parsed = JsonRpcMessage::parse(&encoded).expect("parse request");
        assert_eq!(parsed, request);

        let notification = JsonRpcMessage::notification(
            "notifications/initialized",
            JsonValue::Object(BTreeMap::default()),
        );
        let parsed_notification =
            JsonRpcMessage::parse(&notification.to_json_string()).expect("parse notification");
        assert!(parsed_notification.id.is_none());
        assert_eq!(parsed_notification.method, "notifications/initialized");
    }

    #[test]
    fn responses_round_trip_and_reject_malformed_envelopes() {
        let success = JsonRpcResponse::success(
            RequestId::Text("req-1".to_owned()),
            JsonValue::object([("ok", true.into())]),
        );
        let parsed = JsonRpcResponse::parse(&success.to_json_string()).expect("parse success");
        assert_eq!(parsed, success);

        let failure = JsonRpcResponse::failure(RequestId::Number(2), -32601, "method not found");
        let parsed_failure =
            JsonRpcResponse::parse(&failure.to_json_string()).expect("parse failure");
        assert_eq!(parsed_failure, failure);

        assert!(JsonRpcResponse::parse(r#"{"jsonrpc":"2.0","id":1}"#).is_err());
        assert!(
            JsonRpcResponse::parse(
                r#"{"jsonrpc":"2.0","id":1,"result":1,"error":{"code":1,"message":"m"}}"#
            )
            .is_err()
        );
        assert!(JsonRpcMessage::parse(r#"{"jsonrpc":"1.0","method":"x"}"#).is_err());
        assert!(JsonRpcMessage::parse(r#"{"jsonrpc":"2.0","method":"x","params":5}"#).is_err());
    }
}
