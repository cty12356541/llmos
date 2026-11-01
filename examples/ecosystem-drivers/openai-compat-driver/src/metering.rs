//! Five-dimension token metering for the OpenAI-compatible gateway sample,
//! backed by the real `nlos-resource` `ResourceAuthority` budget ledger
//! (`[BUD-INT-001]`, `[BUD-ARITH-001]`) — the E0 `nlos-driver-llm-http`
//! accounting pattern restated for a third-party adapter.

use nlos_resource::{
    ConsumeLlmTokensRequest, CreateAccountRequest, CreateLlmTokenBudgetRequest,
    LlmTokenConsumptionReceipt, LlmTokenLimits, ResourceAuthority, ResourceAuthorityError,
};
use nlos_types::{IdempotencyKey, OperationId, ResourceAccountId};
use std::error::Error;
use std::fmt;
use std::path::Path;

/// Domain separator for deterministic request-derived keys.
const KEY_DOMAIN: &[u8] = b"llmos/openai-compat-driver/key/v1";
/// Domain separator for deterministic operation ids.
const OPERATION_DOMAIN: &[u8] = b"llmos/openai-compat-driver/operation/v1";

/// The metered gateway adapter.
pub struct MeteredChatGateway {
    authority: ResourceAuthority,
    account: ResourceAccountId,
    cost_per_kilotoken_microcredits: u64,
}

/// Fail-closed metering failures.
#[derive(Debug)]
pub enum MeteringError {
    /// The underlying `ResourceAuthority` rejected the charge (including
    /// five-dimension budget exhaustion).
    Authority(ResourceAuthorityError),
    /// The caller supplied a negative or overflowing cost split.
    InvalidCostSplit,
}

impl fmt::Display for MeteringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authority(error) => write!(f, "resource authority rejection: {error}"),
            Self::InvalidCostSplit => f.write_str("usage cannot be split into the five dimensions"),
        }
    }
}

impl Error for MeteringError {}

impl From<ResourceAuthorityError> for MeteringError {
    fn from(error: ResourceAuthorityError) -> Self {
        Self::Authority(error)
    }
}

impl MeteredChatGateway {
    /// Opens (or reopens) the authority under `root` and provisions the
    /// sample account with the given five-dimension limits, replaying
    /// idempotently on re-open.
    ///
    /// # Errors
    /// Returns [`MeteringError::Authority`] on authority failures.
    pub fn open(
        root: &Path,
        limits: LlmTokenLimits,
        initial_credit: u64,
        provision_at_ms: u64,
    ) -> Result<Self, MeteringError> {
        let authority = ResourceAuthority::open(root).map_err(MeteringError::Authority)?;
        let account = authority
            .create_account(CreateAccountRequest {
                initial_credit,
                idempotency_key: IdempotencyKey::from_bytes([0xC1; 16]),
                created_at_ms: provision_at_ms,
            })?
            .account_id;
        authority.create_llm_token_budget(CreateLlmTokenBudgetRequest {
            account_id: account,
            limits,
            idempotency_key: IdempotencyKey::from_bytes([0xC2; 16]),
            created_at_ms: provision_at_ms,
        })?;
        Ok(Self {
            authority,
            account,
            cost_per_kilotoken_microcredits: 0,
        })
    }

    /// Sets the flat per-kilotoken price used to derive `cost_microcredits`.
    #[must_use]
    pub const fn with_price_per_kilotoken(mut self, microcredits: u64) -> Self {
        self.cost_per_kilotoken_microcredits = microcredits;
        self
    }

    /// The provisioned account id.
    #[must_use]
    pub const fn account_id(&self) -> ResourceAccountId {
        self.account
    }

    /// Deterministic idempotency key for one logical completion request,
    /// derived from the request digest so a retry after `UNCERTAIN` reuses it.
    #[must_use]
    pub fn idempotency_key_for(request_digest: &[u8; 32]) -> IdempotencyKey {
        IdempotencyKey::from_bytes(short16(KEY_DOMAIN, request_digest))
    }

    /// Deterministic operation id for one logical completion request.
    #[must_use]
    pub fn operation_id_for(request_digest: &[u8; 32]) -> OperationId {
        OperationId::from_bytes(short16(OPERATION_DOMAIN, request_digest))
    }

    /// Charges one completion against the budget. Retrying the same
    /// `request_digest` under the same account replays the durable receipt
    /// instead of double-charging (`KABI-UNCERTAIN-001`).
    ///
    /// # Errors
    /// Returns [`MeteringError::Authority`] on rejection — including
    /// `LlmTokenBudgetExhausted` when any dimension overflows — and
    /// [`MeteringError::InvalidCostSplit`] on overflow while pricing.
    pub fn charge_completion(
        &self,
        request_digest: &[u8; 32],
        prompt_tokens: u64,
        completion_tokens: u64,
        reasoning_tokens: u64,
        consumed_at_ms: u64,
    ) -> Result<LlmTokenConsumptionReceipt, MeteringError> {
        let total_tokens = prompt_tokens
            .checked_add(completion_tokens)
            .and_then(|sum| sum.checked_add(reasoning_tokens))
            .ok_or(MeteringError::InvalidCostSplit)?;
        let kilotokens = total_tokens.checked_add(999).map(|tokens| tokens / 1000);
        let cost_microcredits = kilotokens
            .and_then(|kt| kt.checked_mul(self.cost_per_kilotoken_microcredits))
            .ok_or(MeteringError::InvalidCostSplit)?;

        let decision = self.authority.consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: self.account,
            operation_id: Self::operation_id_for(request_digest),
            prompt_tokens,
            completion_tokens,
            reasoning_tokens,
            api_calls: 1,
            cost_microcredits,
            idempotency_key: Self::idempotency_key_for(request_digest),
            consumed_at_ms,
        })?;
        Ok(decision.receipt())
    }

    /// Inspects the current five-dimension budget state.
    ///
    /// # Errors
    /// Returns [`MeteringError::Authority`] on lookup failure.
    pub fn budget(&self) -> Result<nlos_resource::LlmTokenBudgetRecord, MeteringError> {
        self.authority
            .inspect_llm_token_budget(self.account)
            .map_err(MeteringError::Authority)
    }
}

fn short16(domain: &[u8], input: &[u8; 32]) -> [u8; 16] {
    let digest = ecosystem_driver_kit::domain_digest(domain, input);
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::MeteredChatGateway;
    use ecosystem_driver_kit::domain_digest;
    use nlos_resource::{LlmBudgetDimension, LlmTokenLimits, ResourceAuthorityError};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "openai-compat-driver-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn metered_completions_charge_fail_closed_and_replay_idempotently() {
        let root = temp_dir("metering");
        let gateway = MeteredChatGateway::open(
            &root,
            LlmTokenLimits {
                max_prompt_tokens: 1_000,
                max_completion_tokens: 500,
                max_total_tokens: 1_200,
                max_api_calls: 3,
                max_cost_microcredits: 10_000,
            },
            100_000,
            1_000,
        )
        .expect("open gateway")
        .with_price_per_kilotoken(500);

        let digest = domain_digest(b"llmos/test/request/v1", b"completion-1");
        let receipt = gateway
            .charge_completion(&digest, 600, 240, 60, 1_100)
            .expect("first charge");
        assert_eq!(receipt.total_tokens, 900);

        // Same digest replays the same durable receipt (no double charge).
        let replay = gateway
            .charge_completion(&digest, 600, 240, 60, 1_150)
            .expect("replay");
        assert_eq!(replay.receipt_id, receipt.receipt_id);
        let budget = gateway.budget().expect("budget");
        assert_eq!(budget.consumed_api_calls, 1);
        assert_eq!(budget.consumed_total_tokens, 900);

        // Exhausting the total-token dimension fails closed.
        let over = domain_digest(b"llmos/test/request/v1", b"completion-2");
        assert!(matches!(
            gateway.charge_completion(&over, 200, 200, 0, 1_200),
            Err(super::MeteringError::Authority(
                ResourceAuthorityError::LlmTokenBudgetExhausted {
                    dimension: LlmBudgetDimension::TotalTokens,
                    ..
                }
            ))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
}
