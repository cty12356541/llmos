//! OpenAI-compatible `/v1/chat/completions` request/response codec for the
//! `openai-compat-driver` sample (`examples/ecosystem-drivers`).

use ecosystem_driver_kit::{JsonError, JsonValue};
use std::error::Error;
use std::fmt;

/// One chat message (`role` / `content`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatMessage {
    /// `system`, `user`, or `assistant`.
    pub role: ChatRole,
    /// Message body.
    pub content: String,
}

/// Chat speaker role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatRole {
    /// System prompt.
    System,
    /// User turn.
    User,
    /// Assistant turn.
    Assistant,
}

impl ChatRole {
    /// Wire token for the role.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    /// Parses a role token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "system" => Some(Self::System),
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }
}

/// A non-streaming chat completion request.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatCompletionRequest {
    /// Target model id on the gateway.
    pub model: String,
    /// Conversation turns.
    pub messages: Vec<ChatMessage>,
    /// Optional `max_tokens` cap.
    pub max_tokens: Option<u64>,
    /// Optional sampling temperature.
    pub temperature: Option<f64>,
    /// Whether to request SSE streaming.
    pub stream: bool,
    /// Whether to ask the gateway to include usage in the final stream chunk.
    pub stream_options_include_usage: bool,
}

impl ChatCompletionRequest {
    /// Renders the JSON request body.
    ///
    /// # Errors
    /// Returns [`ChatCodecError`] when `temperature` is non-finite.
    pub fn to_json(&self) -> Result<JsonValue, ChatCodecError> {
        let mut fields: Vec<(String, JsonValue)> = vec![
            ("model".to_owned(), JsonValue::from(self.model.as_str())),
            (
                "messages".to_owned(),
                JsonValue::Array(
                    self.messages
                        .iter()
                        .map(|message| {
                            JsonValue::object([
                                ("role", JsonValue::from(message.role.as_token())),
                                ("content", JsonValue::from(message.content.as_str())),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("stream".to_owned(), JsonValue::from(self.stream)),
        ];
        if let Some(max_tokens) = self.max_tokens {
            fields.push(("max_tokens".to_owned(), JsonValue::from(max_tokens)));
        }
        if let Some(temperature) = self.temperature {
            let Some(number) = ecosystem_driver_kit::JsonNumber::from_f64(temperature) else {
                return Err(ChatCodecError::InvalidTemperature(temperature));
            };
            fields.push(("temperature".to_owned(), JsonValue::Number(number)));
        }
        if self.stream && self.stream_options_include_usage {
            fields.push((
                "stream_options".to_owned(),
                JsonValue::object([("include_usage", JsonValue::from(true))]),
            ));
        }
        Ok(JsonValue::object(fields))
    }

    /// The absolute gateway path this request targets.
    #[must_use]
    pub const fn path() -> &'static str {
        "/v1/chat/completions"
    }
}

/// Token usage block of one completion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChatUsage {
    /// Billed prompt tokens.
    pub prompt_tokens: u64,
    /// Billed completion tokens.
    pub completion_tokens: u64,
    /// Total tokens as reported by the gateway.
    pub total_tokens: u64,
}

impl ChatUsage {
    /// Parses a `usage` object; missing members default to `0` (some
    /// gateways omit usage on streamed chunks).
    #[must_use]
    pub fn from_json(value: &JsonValue) -> Self {
        Self {
            prompt_tokens: value
                .get("prompt_tokens")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0),
            completion_tokens: value
                .get("completion_tokens")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0),
            total_tokens: value
                .get("total_tokens")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0),
        }
    }
}

/// One choice of a non-streaming completion.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatChoice {
    /// Choice index.
    pub index: u64,
    /// The assistant message.
    pub message: ChatMessage,
    /// `stop`, `length`, or gateway-specific stop reason.
    pub finish_reason: String,
}

/// A non-streaming chat completion response.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatCompletionResponse {
    /// Gateway completion id.
    pub id: String,
    /// Model that served the request.
    pub model: String,
    /// Choices (single for chat).
    pub choices: Vec<ChatChoice>,
    /// Usage accounting.
    pub usage: Option<ChatUsage>,
}

impl ChatCompletionResponse {
    /// Parses a non-streaming completion payload.
    ///
    /// # Errors
    /// Fails closed when `choices` is missing/not an array, or a choice lacks
    /// a `message.role`/`message.content` pair.
    pub fn from_json(value: &JsonValue) -> Result<Self, ChatCodecError> {
        let id = value
            .get("id")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_owned();
        let model = value
            .get("model")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(raw_choices) = value.get("choices").and_then(JsonValue::as_array) else {
            return Err(ChatCodecError::MissingMember("choices"));
        };
        let mut choices = Vec::with_capacity(raw_choices.len());
        for raw in raw_choices {
            let Some(message) = raw.get("message") else {
                return Err(ChatCodecError::MissingMember("choices[].message"));
            };
            let Some(role_token) = message.get("role").and_then(JsonValue::as_str) else {
                return Err(ChatCodecError::MissingMember("choices[].message.role"));
            };
            let Some(role) = ChatRole::parse(role_token) else {
                return Err(ChatCodecError::UnknownRole(role_token.to_owned()));
            };
            let content = message
                .get("content")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_owned();
            choices.push(ChatChoice {
                index: raw.get("index").and_then(JsonValue::as_u64).unwrap_or(0),
                message: ChatMessage { role, content },
                finish_reason: raw
                    .get("finish_reason")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("stop")
                    .to_owned(),
            });
        }
        Ok(Self {
            id,
            model,
            choices,
            usage: value.get("usage").map(ChatUsage::from_json),
        })
    }

    /// Builds a gateway-side response payload (test double).
    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        let choices = JsonValue::Array(
            self.choices
                .iter()
                .map(|choice| {
                    JsonValue::object([
                        ("index", JsonValue::from(choice.index)),
                        (
                            "message",
                            JsonValue::object([
                                ("role", JsonValue::from(choice.message.role.as_token())),
                                ("content", JsonValue::from(choice.message.content.as_str())),
                            ]),
                        ),
                        (
                            "finish_reason",
                            JsonValue::from(choice.finish_reason.as_str()),
                        ),
                    ])
                })
                .collect(),
        );
        let mut fields = vec![
            ("id", JsonValue::from(self.id.as_str())),
            ("object", JsonValue::from("chat.completion")),
            ("model", JsonValue::from(self.model.as_str())),
            ("choices", choices),
        ];
        if let Some(usage) = self.usage {
            fields.push((
                "usage",
                JsonValue::object([
                    ("prompt_tokens", JsonValue::from(usage.prompt_tokens)),
                    (
                        "completion_tokens",
                        JsonValue::from(usage.completion_tokens),
                    ),
                    ("total_tokens", JsonValue::from(usage.total_tokens)),
                ]),
            ));
        }
        JsonValue::object(fields)
    }
}

/// Fail-closed codec errors.
#[derive(Clone, Debug, PartialEq)]
pub enum ChatCodecError {
    /// Underlying JSON syntax error.
    Syntax(JsonError),
    /// A mandatory member was missing.
    MissingMember(&'static str),
    /// `message.role` held an unknown token.
    UnknownRole(String),
    /// `temperature` was NaN or infinite.
    InvalidTemperature(f64),
}

impl fmt::Display for ChatCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(error) => write!(f, "chat payload syntax error: {error}"),
            Self::MissingMember(member) => write!(f, "chat payload missing member {member}"),
            Self::UnknownRole(role) => write!(f, "unknown chat role {role:?}"),
            Self::InvalidTemperature(value) => {
                write!(f, "temperature must be finite, got {value}")
            }
        }
    }
}

impl Error for ChatCodecError {}

#[cfg(test)]
mod tests {
    use super::{
        ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ChatRole, ChatUsage,
    };
    use ecosystem_driver_kit::JsonValue;

    #[test]
    fn requests_render_canonical_bodies() {
        let request = ChatCompletionRequest {
            model: "glm-4.6".to_owned(),
            messages: vec![
                ChatMessage {
                    role: ChatRole::System,
                    content: "You summarize field notes.".to_owned(),
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: "Summarize: rusty hinge".to_owned(),
                },
            ],
            max_tokens: Some(512),
            temperature: Some(0.2),
            stream: false,
            stream_options_include_usage: false,
        };
        let body = request.to_json().expect("render body");
        assert_eq!(
            body.get("model").and_then(JsonValue::as_str),
            Some("glm-4.6")
        );
        assert_eq!(body.get("stream").and_then(JsonValue::as_bool), Some(false));
        assert_eq!(
            body.get("max_tokens").and_then(JsonValue::as_u64),
            Some(512)
        );
        assert_eq!(ChatCompletionRequest::path(), "/v1/chat/completions");

        let bad = ChatCompletionRequest {
            temperature: Some(f64::NAN),
            ..request
        };
        assert!(bad.to_json().is_err());
    }

    #[test]
    fn responses_round_trip_and_fail_closed() {
        let response = ChatCompletionResponse {
            id: "chatcmpl-sample-1".to_owned(),
            model: "glm-4.6".to_owned(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: ChatRole::Assistant,
                    content: "Two-line summary.".to_owned(),
                },
                finish_reason: "stop".to_owned(),
            }],
            usage: Some(ChatUsage {
                prompt_tokens: 120,
                completion_tokens: 36,
                total_tokens: 156,
            }),
        };
        let parsed = ChatCompletionResponse::from_json(&response.to_json()).expect("round trip");
        assert_eq!(parsed, response);

        assert!(
            ChatCompletionResponse::from_json(&JsonValue::object(Vec::<(&str, JsonValue)>::new()))
                .is_err()
        );
        let bad_role = JsonValue::object([(
            "choices",
            JsonValue::Array(vec![JsonValue::object([
                ("index", JsonValue::from(0_u64)),
                (
                    "message",
                    JsonValue::object([("role", "robot".into()), ("content", "hi".into())]),
                ),
            ])]),
        )]);
        assert!(ChatCompletionResponse::from_json(&bad_role).is_err());
    }
}
