//! Versioned, observed LLM accounting. Prices are injected by the server.

use std::collections::BTreeMap;

use nlos_resource::{LlmBudgetDimension, LlmTokenBudgetRecord, LlmTokenLimits};
use nlos_schema::sabi::v1::LlmUsageWire;

use crate::json::JsonValue;

use super::llm_gateway::GatewayError;

pub const MAX_MODEL_OUTPUT_TOKENS: u32 = 16_384;
pub(super) const MAX_MODEL_NAME_BYTES: usize = 128;
const MAX_PRICED_MODELS: usize = 1_024;
const PROMPT_PREAMBLE: u64 = 8;
const MESSAGE_FRAMING: u64 = 8;

/// Supported provider usage layouts, selected by trusted model configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelAccounting {
    /// Model configured to have no reasoning component.
    NoReasoning,
    /// The provider's output cap and completion counter both include reasoning.
    InclusiveReasoning,
    /// Separate provider reasoning counter with a server-defined quote allowance.
    SeparateReasoning { quote_reasoning_tokens: u32 },
}

/// Model price and usage layout. Change the version whenever the layout or rate changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelPrice {
    pub model: String,
    pub version: u64,
    pub max_output_tokens: u32,
    pub accounting: ModelAccounting,
    pub prompt_microcredits_per_kilotoken: u64,
    /// Applies to both visible and reasoning output.
    pub output_microcredits_per_kilotoken: u64,
}

/// Immutable server configuration. An empty registry denies all models.
#[derive(Clone, Debug, Default)]
pub struct PriceRegistry(BTreeMap<String, ModelPrice>);

impl PriceRegistry {
    /// Constructs a bounded unique mapping from exact model IDs to price versions.
    ///
    /// # Errors
    /// Rejects invalid or duplicate model configurations.
    pub fn new(prices: impl IntoIterator<Item = ModelPrice>) -> Result<Self, GatewayError> {
        let mut registry = BTreeMap::new();
        for price in prices {
            if registry.len() >= MAX_PRICED_MODELS
                || !valid_model(&price.model)
                || !price.valid()
                || registry.insert(price.model.clone(), price).is_some()
            {
                return Err(GatewayError::InvalidConfiguration);
            }
        }
        Ok(Self(registry))
    }

    pub(super) fn get(&self, model: &str) -> Result<&ModelPrice, GatewayError> {
        self.0.get(model).ok_or(GatewayError::UnsupportedModel)
    }
}

pub(super) fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= MAX_MODEL_NAME_BYTES
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:@".contains(&b))
}

impl ModelPrice {
    pub(super) fn valid(&self) -> bool {
        self.version != 0
            && (1..=MAX_MODEL_OUTPUT_TOKENS).contains(&self.max_output_tokens)
            && match self.accounting {
                ModelAccounting::SeparateReasoning {
                    quote_reasoning_tokens,
                } => (1..=MAX_MODEL_OUTPUT_TOKENS).contains(&quote_reasoning_tokens),
                _ => true,
            }
    }

    pub(super) fn quote(
        &self,
        utf8_turn_lengths: impl IntoIterator<Item = usize>,
        max_output_tokens: u32,
    ) -> Result<Five, GatewayError> {
        if !(1..=self.max_output_tokens).contains(&max_output_tokens) {
            return Err(GatewayError::InvalidRequest);
        }
        let prompt = utf8_turn_lengths
            .into_iter()
            .try_fold(PROMPT_PREAMBLE, |sum, length| {
                sum.checked_add(length as u64)
                    .and_then(|count| count.checked_add(MESSAGE_FRAMING))
                    .ok_or(GatewayError::Arithmetic)
            })?;
        let reasoning = match self.accounting {
            ModelAccounting::SeparateReasoning {
                quote_reasoning_tokens,
            } => u64::from(quote_reasoning_tokens),
            _ => 0,
        };
        let quote = self.usage(prompt, u64::from(max_output_tokens), reasoning)?;
        if !quote.authority_sized() {
            return Err(GatewayError::Arithmetic);
        }
        Ok(quote)
    }

    pub(super) fn actual(
        &self,
        root: &JsonValue,
        has_visible_reasoning: bool,
    ) -> Result<Actual, GatewayError> {
        let usage = root.get("usage").ok_or(GatewayError::UsageUnknown)?;
        let read = |name| {
            usage
                .get(name)
                .and_then(JsonValue::as_u64)
                .ok_or(GatewayError::UsageUnknown)
        };
        let prompt = read("prompt_tokens")?;
        let output = read("completion_tokens")?;
        let reasoning_report = usage
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"));
        let reasoning = match (self.accounting, reasoning_report) {
            (ModelAccounting::NoReasoning, None) if !has_visible_reasoning => 0,
            (_, Some(value)) => value.as_u64().ok_or(GatewayError::UsageUnknown)?,
            _ => return Err(GatewayError::UsageUnknown),
        };
        if (self.accounting == ModelAccounting::NoReasoning && reasoning != 0)
            || (has_visible_reasoning && reasoning == 0)
        {
            return Err(GatewayError::UsageUnknown);
        }
        let completion = if self.accounting == ModelAccounting::InclusiveReasoning {
            output
                .checked_sub(reasoning)
                .ok_or(GatewayError::UsageUnknown)?
        } else {
            output
        };
        let dimensions = self.usage(prompt, completion, reasoning)?;
        if let Some(reported) = usage.get("total_tokens")
            && reported.as_u64() != Some(dimensions.0[2])
        {
            return Err(GatewayError::UsageUnknown);
        }
        Ok(Actual {
            dimensions,
            reasoning,
        })
    }

    pub(super) fn covers_observation(&self, quote: Five, actual: Actual) -> bool {
        if quote.sub(actual.dimensions).is_err() {
            return false;
        }
        match self.accounting {
            ModelAccounting::NoReasoning => actual.reasoning == 0,
            ModelAccounting::InclusiveReasoning => actual.dimensions.0[1]
                .checked_add(actual.reasoning)
                .is_some_and(|output| output <= quote.0[1]),
            ModelAccounting::SeparateReasoning {
                quote_reasoning_tokens,
            } => actual.reasoning <= u64::from(quote_reasoning_tokens),
        }
    }

    fn usage(&self, prompt: u64, completion: u64, reasoning: u64) -> Result<Five, GatewayError> {
        let output = completion
            .checked_add(reasoning)
            .ok_or(GatewayError::Arithmetic)?;
        let total = prompt.checked_add(output).ok_or(GatewayError::Arithmetic)?;
        let prompt_price = u128::from(prompt) * u128::from(self.prompt_microcredits_per_kilotoken);
        let output_price = u128::from(output) * u128::from(self.output_microcredits_per_kilotoken);
        let weighted = prompt_price
            .checked_add(output_price)
            .ok_or(GatewayError::Arithmetic)?;
        let cost = u64::try_from(weighted.div_ceil(1_000)).map_err(|_| GatewayError::Arithmetic)?;
        Ok(Five([prompt, completion, total, 1, cost]))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Actual {
    pub dimensions: Five,
    pub reasoning: u64,
}

/// Dimension order is the authority's stable admission order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Five(pub [u64; 5]);

impl Five {
    pub fn add(self, rhs: Self) -> Result<Self, GatewayError> {
        self.combine(rhs, u64::checked_add)
    }

    pub fn sub(self, rhs: Self) -> Result<Self, GatewayError> {
        let mut result = [0; 5];
        for (index, dimension) in LlmBudgetDimension::ALL.into_iter().enumerate() {
            result[index] = self.0[index]
                .checked_sub(rhs.0[index])
                .ok_or(GatewayError::Budget(dimension))?;
        }
        Ok(Self(result))
    }

    fn combine(
        self,
        rhs: Self,
        op: impl Fn(u64, u64) -> Option<u64>,
    ) -> Result<Self, GatewayError> {
        let mut result = [0; 5];
        for (index, cell) in result.iter_mut().enumerate() {
            *cell = op(self.0[index], rhs.0[index]).ok_or(GatewayError::Arithmetic)?;
        }
        Ok(Self(result))
    }

    pub fn min(self, rhs: Self) -> Self {
        let mut result = [0; 5];
        for (index, cell) in result.iter_mut().enumerate() {
            *cell = self.0[index].min(rhs.0[index]);
        }
        Self(result)
    }

    pub fn authority_sized(self) -> bool {
        self.0.iter().all(|&amount| i64::try_from(amount).is_ok())
    }

    pub const fn limits(self) -> LlmTokenLimits {
        let [
            max_prompt_tokens,
            max_completion_tokens,
            max_total_tokens,
            max_api_calls,
            max_cost_microcredits,
        ] = self.0;
        LlmTokenLimits {
            max_prompt_tokens,
            max_completion_tokens,
            max_total_tokens,
            max_api_calls,
            max_cost_microcredits,
        }
    }

    pub const fn from_limits(limits: LlmTokenLimits) -> Self {
        Self([
            limits.max_prompt_tokens,
            limits.max_completion_tokens,
            limits.max_total_tokens,
            limits.max_api_calls,
            limits.max_cost_microcredits,
        ])
    }

    pub const fn consumed(record: &LlmTokenBudgetRecord) -> Self {
        Self([
            record.consumed_prompt_tokens,
            record.consumed_completion_tokens,
            record.consumed_total_tokens,
            record.consumed_api_calls,
            record.consumed_cost_microcredits,
        ])
    }

    pub const fn wire(self) -> LlmUsageWire {
        let [
            prompt_tokens,
            completion_tokens,
            total_tokens,
            api_calls,
            cost_microcredits,
        ] = self.0;
        LlmUsageWire {
            api_calls,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cost_microcredits,
        }
    }
}

impl From<LlmUsageWire> for Five {
    fn from(wire: LlmUsageWire) -> Self {
        Self([
            wire.prompt_tokens,
            wire.completion_tokens,
            wire.total_tokens,
            wire.api_calls,
            wire.cost_microcredits,
        ])
    }
}
