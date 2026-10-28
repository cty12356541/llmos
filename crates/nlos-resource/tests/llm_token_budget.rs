//! Integration coverage for `nlos-resource` v8 LLM token and model API call
//! budget/metering ledger (`[NLOS-RESOURCE-001]`, `[BUD-INT-001]`,
//! `[BUD-ALG-001]`, `[BUD-ARITH-001]`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_resource::{
    ActivateReservationRequest, ConsumeLlmTokensRequest, ConsumeReservationRequest,
    CreateAccountRequest, CreateLlmTokenBudgetRequest, CreateQuoteRequest, LlmBudgetDimension,
    LlmTokenBudgetDecision, LlmTokenBudgetUpdateDecision, LlmTokenConsumeDecision, LlmTokenLimits,
    RegisterDriverRequest, ReserveRequest, ResourceAuthority, ResourceAuthorityError,
    ResourceDemand, SCHEMA_VERSION, UpdateLlmTokenBudgetRequest,
};
use nlos_types::{CallId, Generation, IdempotencyKey, OperationId, ResourceAccountId};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);
impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-resource-llm-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn database(&self) -> PathBuf {
        self.0.join("resource-authority.db")
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn account_request(seed: u8, credit: u64) -> CreateAccountRequest {
    CreateAccountRequest {
        initial_credit: credit,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
        created_at_ms: 1_000,
    }
}

fn default_limits() -> LlmTokenLimits {
    LlmTokenLimits {
        max_prompt_tokens: 1_000,
        max_completion_tokens: 500,
        max_total_tokens: 1_800,
        max_api_calls: 5,
        max_cost_microcredits: 10_000,
    }
}

#[test]
fn create_llm_token_budget_is_durable_idempotent_and_detects_conflicts() {
    let root = Root::new("create-budget");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let account = authority.create_account(account_request(10, 500)).unwrap();
    let other_account = authority.create_account(account_request(11, 500)).unwrap();

    // Unknown account fails closed.
    assert!(matches!(
        authority.create_llm_token_budget(CreateLlmTokenBudgetRequest {
            account_id: ResourceAccountId::from_bytes([0xFF; 16]),
            limits: default_limits(),
            idempotency_key: IdempotencyKey::from_bytes([0x99; 16]),
            created_at_ms: 1_100,
        }),
        Err(ResourceAuthorityError::AccountNotFound)
    ));

    let create_req = CreateLlmTokenBudgetRequest {
        account_id: account.account_id,
        limits: default_limits(),
        idempotency_key: IdempotencyKey::from_bytes([0x20; 16]),
        created_at_ms: 1_200,
    };
    let created = authority.create_llm_token_budget(create_req).unwrap();
    assert!(matches!(created, LlmTokenBudgetDecision::Created(_)));
    let record = created.record();
    assert_eq!(record.account_id, account.account_id);
    assert_eq!(record.generation, Generation::INITIAL);
    assert_eq!(record.limits, default_limits());
    assert_eq!(record.remaining_prompt_tokens(), 1_000);
    assert_eq!(record.remaining_completion_tokens(), 500);
    assert_eq!(record.remaining_total_tokens(), 1_800);
    assert_eq!(record.remaining_api_calls(), 5);
    assert_eq!(record.remaining_cost_microcredits(), 10_000);

    // Exact replay (tolerating timestamp drift) returns Replayed.
    let mut replay_req = create_req;
    replay_req.created_at_ms = 9_999;
    let replayed = authority.create_llm_token_budget(replay_req).unwrap();
    assert!(matches!(replayed, LlmTokenBudgetDecision::Replayed(r) if r == record));

    // Rebinding the same idempotency key to different limits or account fails closed.
    let mut conflict_limits = create_req;
    conflict_limits.limits.max_total_tokens = 2_000;
    assert!(matches!(
        authority.create_llm_token_budget(conflict_limits),
        Err(ResourceAuthorityError::IdempotencyConflict)
    ));
    let mut conflict_account = create_req;
    conflict_account.account_id = other_account.account_id;
    assert!(matches!(
        authority.create_llm_token_budget(conflict_account),
        Err(ResourceAuthorityError::IdempotencyConflict)
    ));

    // Creating a second budget on the same account with a distinct key fails closed.
    assert!(matches!(
        authority.create_llm_token_budget(CreateLlmTokenBudgetRequest {
            account_id: account.account_id,
            limits: default_limits(),
            idempotency_key: IdempotencyKey::from_bytes([0x21; 16]),
            created_at_ms: 1_300,
        }),
        Err(ResourceAuthorityError::LlmTokenBudgetAlreadyExists)
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn consume_llm_tokens_charges_multi_step_usage_and_replays_across_reopen() {
    let root = Root::new("consume-replay");
    let (account_id, first_receipt, second_receipt, first_req) = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let account = authority.create_account(account_request(20, 500)).unwrap();
        authority
            .create_llm_token_budget(CreateLlmTokenBudgetRequest {
                account_id: account.account_id,
                limits: default_limits(),
                idempotency_key: IdempotencyKey::from_bytes([0x30; 16]),
                created_at_ms: 1_000,
            })
            .unwrap();

        // Zero API calls is rejected before any durable state change.
        assert!(matches!(
            authority.consume_llm_tokens(ConsumeLlmTokensRequest {
                account_id: account.account_id,
                operation_id: OperationId::from_bytes([0x01; 16]),
                prompt_tokens: 10,
                completion_tokens: 5,
                reasoning_tokens: 0,
                api_calls: 0,
                cost_microcredits: 100,
                idempotency_key: IdempotencyKey::from_bytes([0x31; 16]),
                consumed_at_ms: 1_100,
            }),
            Err(ResourceAuthorityError::InvalidLlmTokenUsage)
        ));

        let first_req = ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x01; 16]),
            prompt_tokens: 300,
            completion_tokens: 120,
            reasoning_tokens: 80,
            api_calls: 1,
            cost_microcredits: 2_500,
            idempotency_key: IdempotencyKey::from_bytes([0x32; 16]),
            consumed_at_ms: 1_500,
        };
        let first = authority.consume_llm_tokens(first_req).unwrap();
        assert!(matches!(first, LlmTokenConsumeDecision::Charged(_)));
        let first_receipt = first.receipt();
        assert_eq!(first_receipt.sequence, 1);
        assert_eq!(first_receipt.total_tokens, 500);
        assert_eq!(first_receipt.cumulative_total_tokens, 500);
        assert_eq!(first_receipt.cumulative_api_calls, 1);
        assert_eq!(first_receipt.cumulative_cost_microcredits, 2_500);

        let second_req = ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x02; 16]),
            prompt_tokens: 200,
            completion_tokens: 100,
            reasoning_tokens: 100,
            api_calls: 2,
            cost_microcredits: 3_000,
            idempotency_key: IdempotencyKey::from_bytes([0x33; 16]),
            consumed_at_ms: 2_000,
        };
        let second = authority.consume_llm_tokens(second_req).unwrap();
        assert!(matches!(second, LlmTokenConsumeDecision::Charged(_)));
        let second_receipt = second.receipt();
        assert_eq!(second_receipt.sequence, 2);
        assert_eq!(second_receipt.total_tokens, 400);
        assert_eq!(second_receipt.cumulative_total_tokens, 900);
        assert_eq!(second_receipt.cumulative_api_calls, 3);
        assert_eq!(second_receipt.cumulative_cost_microcredits, 5_500);

        // Parameter drift on an existing idempotency key fails closed.
        let mut drifted = first_req;
        drifted.prompt_tokens = 301;
        assert!(matches!(
            authority.consume_llm_tokens(drifted),
            Err(ResourceAuthorityError::IdempotencyConflict)
        ));

        (account.account_id, first_receipt, second_receipt, first_req)
    };

    let reopened = ResourceAuthority::open(root.path()).unwrap();
    let replay = reopened.consume_llm_tokens(first_req).unwrap();
    assert!(matches!(replay, LlmTokenConsumeDecision::Replayed(r) if r == first_receipt));

    let budget = reopened.inspect_llm_token_budget(account_id).unwrap();
    assert_eq!(budget.consumed_prompt_tokens, 500);
    assert_eq!(budget.consumed_completion_tokens, 220);
    assert_eq!(budget.consumed_reasoning_tokens, 180);
    assert_eq!(budget.consumed_total_tokens, 900);
    assert_eq!(budget.consumed_api_calls, 3);
    assert_eq!(budget.consumed_cost_microcredits, 5_500);
    assert_eq!(budget.remaining_prompt_tokens(), 500);
    assert_eq!(budget.remaining_completion_tokens(), 280);
    assert_eq!(budget.remaining_total_tokens(), 900);
    assert_eq!(budget.remaining_api_calls(), 2);
    assert_eq!(budget.remaining_cost_microcredits(), 4_500);

    let ledger = reopened.list_llm_token_consumptions(account_id).unwrap();
    assert_eq!(ledger, vec![first_receipt, second_receipt]);
}

#[test]
#[allow(clippy::too_many_lines)]
fn exhaustion_in_each_dimension_fails_closed_with_zero_durable_writes() {
    let root = Root::new("exhaustion-dimensions");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let account = authority.create_account(account_request(30, 500)).unwrap();
    authority
        .create_llm_token_budget(CreateLlmTokenBudgetRequest {
            account_id: account.account_id,
            limits: default_limits(),
            idempotency_key: IdempotencyKey::from_bytes([0x40; 16]),
            created_at_ms: 1_000,
        })
        .unwrap();

    // Baseline consumption leaving room for targeted overages.
    authority
        .consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x41; 16]),
            prompt_tokens: 900,
            completion_tokens: 400,
            reasoning_tokens: 300,
            api_calls: 4,
            cost_microcredits: 9_000,
            idempotency_key: IdempotencyKey::from_bytes([0x41; 16]),
            consumed_at_ms: 1_100,
        })
        .unwrap();
    let baseline = authority
        .inspect_llm_token_budget(account.account_id)
        .unwrap();

    // 1. PromptTokens exhaustion (900 + 101 > 1000)
    assert!(matches!(
        authority.consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x42; 16]),
            prompt_tokens: 101,
            completion_tokens: 0,
            reasoning_tokens: 0,
            api_calls: 1,
            cost_microcredits: 10,
            idempotency_key: IdempotencyKey::from_bytes([0x42; 16]),
            consumed_at_ms: 1_200,
        }),
        Err(ResourceAuthorityError::LlmTokenBudgetExhausted {
            dimension: LlmBudgetDimension::PromptTokens,
            requested_cumulative: 1_001,
            limit: 1_000,
        })
    ));

    // 2. CompletionTokens exhaustion (400 + 101 > 500)
    assert!(matches!(
        authority.consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x43; 16]),
            prompt_tokens: 10,
            completion_tokens: 101,
            reasoning_tokens: 0,
            api_calls: 1,
            cost_microcredits: 10,
            idempotency_key: IdempotencyKey::from_bytes([0x43; 16]),
            consumed_at_ms: 1_200,
        }),
        Err(ResourceAuthorityError::LlmTokenBudgetExhausted {
            dimension: LlmBudgetDimension::CompletionTokens,
            requested_cumulative: 501,
            limit: 500,
        })
    ));

    // 3. TotalTokens exhaustion via reasoning_tokens (1600 + 201 > 1800) while prompt/completion fit
    assert!(matches!(
        authority.consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x44; 16]),
            prompt_tokens: 50,
            completion_tokens: 50,
            reasoning_tokens: 101,
            api_calls: 1,
            cost_microcredits: 10,
            idempotency_key: IdempotencyKey::from_bytes([0x44; 16]),
            consumed_at_ms: 1_200,
        }),
        Err(ResourceAuthorityError::LlmTokenBudgetExhausted {
            dimension: LlmBudgetDimension::TotalTokens,
            requested_cumulative: 1_801,
            limit: 1_800,
        })
    ));

    // 4. ApiCalls exhaustion (4 + 2 > 5)
    assert!(matches!(
        authority.consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x45; 16]),
            prompt_tokens: 10,
            completion_tokens: 10,
            reasoning_tokens: 0,
            api_calls: 2,
            cost_microcredits: 10,
            idempotency_key: IdempotencyKey::from_bytes([0x45; 16]),
            consumed_at_ms: 1_200,
        }),
        Err(ResourceAuthorityError::LlmTokenBudgetExhausted {
            dimension: LlmBudgetDimension::ApiCalls,
            requested_cumulative: 6,
            limit: 5,
        })
    ));

    // 5. CostMicrocredits exhaustion (9000 + 1001 > 10000)
    assert!(matches!(
        authority.consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x46; 16]),
            prompt_tokens: 10,
            completion_tokens: 10,
            reasoning_tokens: 0,
            api_calls: 1,
            cost_microcredits: 1_001,
            idempotency_key: IdempotencyKey::from_bytes([0x46; 16]),
            consumed_at_ms: 1_200,
        }),
        Err(ResourceAuthorityError::LlmTokenBudgetExhausted {
            dimension: LlmBudgetDimension::CostMicrocredits,
            requested_cumulative: 10_001,
            limit: 10_000,
        })
    ));

    // Zero durable writes from any rejected attempt.
    assert_eq!(
        authority
            .inspect_llm_token_budget(account.account_id)
            .unwrap(),
        baseline
    );
    assert_eq!(
        authority
            .list_llm_token_consumptions(account.account_id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn checked_arithmetic_overflow_fails_closed() {
    let root = Root::new("arithmetic-overflow");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let account = authority.create_account(account_request(40, 500)).unwrap();
    authority
        .create_llm_token_budget(CreateLlmTokenBudgetRequest {
            account_id: account.account_id,
            limits: default_limits(),
            idempotency_key: IdempotencyKey::from_bytes([0x50; 16]),
            created_at_ms: 1_000,
        })
        .unwrap();

    // Per-call sum overflow (`prompt_tokens + completion_tokens > u64::MAX`).
    assert!(matches!(
        authority.consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x51; 16]),
            prompt_tokens: u64::MAX,
            completion_tokens: 1,
            reasoning_tokens: 0,
            api_calls: 1,
            cost_microcredits: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x51; 16]),
            consumed_at_ms: 1_100,
        }),
        Err(ResourceAuthorityError::LlmUsageArithmeticOverflow)
    ));

    // Cumulative counter overflow after initial usage.
    authority
        .consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x52; 16]),
            prompt_tokens: 10,
            completion_tokens: 5,
            reasoning_tokens: 0,
            api_calls: 1,
            cost_microcredits: 10,
            idempotency_key: IdempotencyKey::from_bytes([0x52; 16]),
            consumed_at_ms: 1_200,
        })
        .unwrap();

    assert!(matches!(
        authority.consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x53; 16]),
            prompt_tokens: 1,
            completion_tokens: 0,
            reasoning_tokens: 0,
            api_calls: 1,
            cost_microcredits: u64::MAX,
            idempotency_key: IdempotencyKey::from_bytes([0x53; 16]),
            consumed_at_ms: 1_300,
        }),
        Err(ResourceAuthorityError::LlmUsageArithmeticOverflow)
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn update_llm_token_budget_raises_exhausted_budget_and_enforces_consumed_floor() {
    let root = Root::new("update-budget");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let account = authority.create_account(account_request(50, 500)).unwrap();
    let initial = authority
        .create_llm_token_budget(CreateLlmTokenBudgetRequest {
            account_id: account.account_id,
            limits: LlmTokenLimits {
                max_prompt_tokens: 100,
                max_completion_tokens: 100,
                max_total_tokens: 200,
                max_api_calls: 1,
                max_cost_microcredits: 500,
            },
            idempotency_key: IdempotencyKey::from_bytes([0x60; 16]),
            created_at_ms: 1_000,
        })
        .unwrap()
        .record();

    authority
        .consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x61; 16]),
            prompt_tokens: 80,
            completion_tokens: 60,
            reasoning_tokens: 20,
            api_calls: 1,
            cost_microcredits: 400,
            idempotency_key: IdempotencyKey::from_bytes([0x61; 16]),
            consumed_at_ms: 1_200,
        })
        .unwrap();

    // Budget is now exhausted on ApiCalls (1/1).
    let second_consume = ConsumeLlmTokensRequest {
        account_id: account.account_id,
        operation_id: OperationId::from_bytes([0x62; 16]),
        prompt_tokens: 10,
        completion_tokens: 10,
        reasoning_tokens: 0,
        api_calls: 1,
        cost_microcredits: 50,
        idempotency_key: IdempotencyKey::from_bytes([0x62; 16]),
        consumed_at_ms: 1_300,
    };
    assert!(matches!(
        authority.consume_llm_tokens(second_consume),
        Err(ResourceAuthorityError::LlmTokenBudgetExhausted {
            dimension: LlmBudgetDimension::ApiCalls,
            requested_cumulative: 2,
            limit: 1,
        })
    ));

    // Shrinking below already consumed tokens (79 < 80 prompt tokens) is rejected.
    assert!(matches!(
        authority.update_llm_token_budget(UpdateLlmTokenBudgetRequest {
            account_id: account.account_id,
            expected_generation: initial.generation,
            limits: LlmTokenLimits {
                max_prompt_tokens: 79,
                max_completion_tokens: 100,
                max_total_tokens: 200,
                max_api_calls: 5,
                max_cost_microcredits: 500,
            },
            idempotency_key: IdempotencyKey::from_bytes([0x63; 16]),
            updated_at_ms: 1_400,
        }),
        Err(ResourceAuthorityError::LlmBudgetUpdateBelowConsumed {
            dimension: LlmBudgetDimension::PromptTokens,
            consumed: 80,
            requested_limit: 79,
        })
    ));

    // Raising the budget while shrinking prompt/completion/cost down to exact
    // consumed values succeeds and bumps generation to 2.
    let update_req = UpdateLlmTokenBudgetRequest {
        account_id: account.account_id,
        expected_generation: initial.generation,
        limits: LlmTokenLimits {
            max_prompt_tokens: 90,
            max_completion_tokens: 70,
            max_total_tokens: 180,
            max_api_calls: 2,
            max_cost_microcredits: 450,
        },
        idempotency_key: IdempotencyKey::from_bytes([0x64; 16]),
        updated_at_ms: 1_500,
    };
    let updated = authority.update_llm_token_budget(update_req).unwrap();
    assert!(matches!(updated, LlmTokenBudgetUpdateDecision::Updated(_)));
    assert_eq!(updated.record().generation.get(), 2);

    // Idempotent replay of the update returns the exact updated record.
    let replayed = authority.update_llm_token_budget(update_req).unwrap();
    assert!(matches!(replayed, LlmTokenBudgetUpdateDecision::Replayed(r) if r == updated.record()));

    // Stale generation is rejected.
    assert!(matches!(
        authority.update_llm_token_budget(UpdateLlmTokenBudgetRequest {
            idempotency_key: IdempotencyKey::from_bytes([0x65; 16]),
            ..update_req
        }),
        Err(ResourceAuthorityError::StaleLlmTokenBudget { .. })
    ));

    // The raised budget now admits `second_consume` and reaches exact limits.
    let charged = authority.consume_llm_tokens(second_consume).unwrap();
    assert!(matches!(charged, LlmTokenConsumeDecision::Charged(_)));
    let after = authority
        .inspect_llm_token_budget(account.account_id)
        .unwrap();
    assert_eq!(after.remaining_prompt_tokens(), 0);
    assert_eq!(after.remaining_completion_tokens(), 0);
    assert_eq!(after.remaining_total_tokens(), 0);
    assert_eq!(after.remaining_api_calls(), 0);
    assert_eq!(after.remaining_cost_microcredits(), 0);
}

#[test]
#[allow(clippy::too_many_lines)]
fn v7_database_with_data_upgrades_cleanly_to_v8_and_ddl_triggers_hold() {
    let root = Root::new("v7-upgrade");
    let (account, reservation, consumption) = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let driver = authority
            .register_driver(RegisterDriverRequest {
                profile_digest: [0x70; 32],
                idempotency_key: IdempotencyKey::from_bytes([0x71; 16]),
                created_at_ms: 1_000,
            })
            .unwrap()
            .record();
        let account = authority
            .create_account(account_request(70, 1_000))
            .unwrap();
        let quote = authority
            .create_quote(CreateQuoteRequest {
                driver_id: driver.driver_id,
                driver_generation: driver.generation,
                driver_fencing_token: driver.fencing_token,
                operation_proposal_digest: [0x72; 32],
                pricing_version: [0x73; 32],
                upper_bound: 100,
                demand_capacity: ResourceDemand::default(),
                valid_until_ms: 10_000,
                idempotency_key: IdempotencyKey::from_bytes([0x74; 16]),
                created_at_ms: 1_000,
            })
            .unwrap()
            .record();
        let reserved = authority
            .reserve(ReserveRequest {
                account_id: account.account_id,
                quote_id: quote.quote_id,
                call_id: CallId::from_bytes([0x75; 16]),
                operation_id: OperationId::from_bytes([0x76; 16]),
                idempotency_key: IdempotencyKey::from_bytes([0x77; 16]),
                demand: ResourceDemand::default(),
                reserved_at_ms: 1_500,
            })
            .unwrap()
            .record();
        let activation = authority
            .activate(ActivateReservationRequest {
                reservation_id: reserved.reservation_id,
                call_id: reserved.call_id,
                operation_id: reserved.operation_id,
                driver_id: reserved.driver_id,
                driver_generation: reserved.driver_generation,
                driver_fencing_token: reserved.driver_fencing_token,
                activation_token: reserved.activation_token,
                activated_at_ms: 2_000,
            })
            .unwrap()
            .receipt();
        let consumption = authority
            .consume(ConsumeReservationRequest {
                reservation_id: reserved.reservation_id,
                operation_id: reserved.operation_id,
                activation_receipt_id: activation.receipt_id,
                sequence: 1,
                cumulative_usage: 35,
                consumed_at_ms: 2_500,
            })
            .unwrap()
            .receipt();
        let live_reservation = authority
            .inspect_reservation(reserved.reservation_id)
            .unwrap();
        (account, live_reservation, consumption)
    };

    // Strip v8 tables and stamp user_version = 7 to simulate a legacy v7 database.
    {
        let raw = Connection::open(root.database()).unwrap();
        raw.execute_batch(
            "DROP TABLE llm_token_consumptions;
             DROP TABLE llm_token_budget_revisions;
             DROP TABLE llm_token_budgets;
             PRAGMA user_version = 7;",
        )
        .unwrap();
    }

    let authority = ResourceAuthority::open(root.path()).unwrap();
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        900
    );
    assert_eq!(
        authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap(),
        reservation
    );
    assert_eq!(
        authority
            .inspect_consumption_receipt(reservation.reservation_id, 1)
            .unwrap(),
        consumption
    );

    // Create and charge an LLM token budget on the migrated v7 account.
    authority
        .create_llm_token_budget(CreateLlmTokenBudgetRequest {
            account_id: account.account_id,
            limits: default_limits(),
            idempotency_key: IdempotencyKey::from_bytes([0x78; 16]),
            created_at_ms: 3_000,
        })
        .unwrap();
    let receipt = authority
        .consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: reservation.operation_id,
            prompt_tokens: 50,
            completion_tokens: 25,
            reasoning_tokens: 5,
            api_calls: 1,
            cost_microcredits: 200,
            idempotency_key: IdempotencyKey::from_bytes([0x79; 16]),
            consumed_at_ms: 3_100,
        })
        .unwrap()
        .receipt();
    drop(authority);

    // Verify user_version == SCHEMA_VERSION (8) and DDL triggers reject mutation/deletion.
    let raw = Connection::open(root.database()).unwrap();
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert!(
        raw.execute(
            "UPDATE llm_token_budgets SET consumed_prompt_tokens = 49 WHERE account_id = ?1",
            [account.account_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM llm_token_budgets WHERE account_id = ?1",
            [account.account_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE llm_token_consumptions SET prompt_tokens = 1 WHERE receipt_id = ?1",
            [receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM llm_token_consumptions WHERE receipt_id = ?1",
            [receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
}

#[test]
fn open_metrics_text_renders_expected_gauges_and_counters() {
    let root = Root::new("open-metrics");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let account = authority.create_account(account_request(80, 500)).unwrap();
    authority
        .create_llm_token_budget(CreateLlmTokenBudgetRequest {
            account_id: account.account_id,
            limits: default_limits(),
            idempotency_key: IdempotencyKey::from_bytes([0x80; 16]),
            created_at_ms: 1_000,
        })
        .unwrap();
    authority
        .consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: OperationId::from_bytes([0x81; 16]),
            prompt_tokens: 120,
            completion_tokens: 80,
            reasoning_tokens: 40,
            api_calls: 2,
            cost_microcredits: 950,
            idempotency_key: IdempotencyKey::from_bytes([0x81; 16]),
            consumed_at_ms: 1_500,
        })
        .unwrap();

    let record = authority
        .inspect_llm_token_budget(account.account_id)
        .unwrap();
    let text = record.to_open_metrics_text();
    assert!(text.contains("nlos_llm_budget_max_total_tokens 1800\n"));
    assert!(text.contains("nlos_llm_budget_consumed_total_tokens 240\n"));
    assert!(text.contains("nlos_llm_budget_consumed_prompt_tokens 120\n"));
    assert!(text.contains("nlos_llm_budget_consumed_completion_tokens 80\n"));
    assert!(text.contains("nlos_llm_budget_consumed_reasoning_tokens 40\n"));
    assert!(text.contains("nlos_llm_budget_consumed_api_calls_total 2\n"));
    assert!(text.contains("nlos_llm_budget_consumed_cost_microcredits_total 950\n"));
}
