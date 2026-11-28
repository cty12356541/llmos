use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::*;
use crate::reservation_journal::JournalFault;

#[allow(dead_code)]
#[path = "../tests/gateway_support/mod.rs"]
mod gateway_support;
use gateway_support::{Policy, TestRoot, budget, call, chat_request, config, provider, root};

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn exact_replay_across_all_chat_durable_boundaries() {
    let (port, hits, server) = provider().await;
    for (index, point) in [
        FaultPoint::Prepared,
        FaultPoint::Fence,
        FaultPoint::Observed,
        FaultPoint::ReadyForConsume,
        FaultPoint::Consumed,
        FaultPoint::Settled,
    ]
    .into_iter()
    .enumerate()
    {
        let baseline_hits = hits.load(Ordering::SeqCst);
        let dir = TestRoot::new();
        let now = Arc::new(AtomicU64::new(10_000));
        let policy = Arc::new(Policy(AtomicBool::new(true)));
        let gateway =
            Gateway::initialize(&dir.0, config(port, Some(policy.clone()), now.clone())).unwrap();
        let binding = gateway.provision_root(root(), budget(1_000)).unwrap();
        let caller = call(13, 1);
        gateway.lock().unwrap().fault = Some(point);
        assert!(
            gateway
                .llm_chat(&caller, &binding, &chat_request())
                .await
                .is_err()
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            baseline_hits + usize::from(index >= 2)
        );
        assert!(matches!(
            gateway.query_budget(&caller, &binding, &query()),
            Err(GatewayError::Durability)
        ));
        drop(gateway);
        let gateway = Gateway::open(&dir.0, config(port, Some(policy), now.clone())).unwrap();
        let receipts = gateway
            .resource
            .list_llm_token_consumptions(binding.account_id())
            .unwrap();
        let replay = gateway
            .llm_chat(&call(13, 2), &binding, &chat_request())
            .await;
        match index {
            0 => assert_eq!(replay.unwrap_err(), GatewayError::ProviderFailure),
            1 => {
                let GatewayError::EffectUnknown(operation) = replay.unwrap_err() else {
                    panic!("in-doubt dispatch")
                };
                assert_eq!(
                    gateway.reconciliation(operation).unwrap().unwrap().status,
                    ReconciliationStatus::Pending
                );
                now.store(20_000, Ordering::SeqCst);
                let remaining = gateway
                    .query_budget(&caller, &binding, &query())
                    .unwrap()
                    .remaining
                    .unwrap();
                assert_eq!(remaining, budget(1_000));
                let reconciled = gateway.reconciliation(operation).unwrap().unwrap();
                assert_eq!(reconciled.status, ReconciliationStatus::EffectUnknown);
                assert_eq!(reconciled.provider_attempts, None);
                assert_eq!(reconciled.durable_attempt_fences, 1);
                assert_eq!(
                    gateway
                        .llm_chat(&call(13, 3), &binding, &chat_request())
                        .await
                        .unwrap_err(),
                    GatewayError::EffectUnknown(operation)
                );
            }
            _ => {
                let chat = replay.unwrap();
                assert_eq!(chat.result.content_utf8, "答");
                assert_eq!(chat.result.reasoning_utf8, "算");
                assert_eq!(chat.price_version, 42);
                assert_eq!(chat.receipt, receipts[0].receipt_id);
                assert_eq!(chat.provider_attempts, 1);
                let reconciled = gateway.reconciliation(chat.operation).unwrap().unwrap();
                assert_eq!(reconciled.reasoning_tokens, Some(3));
                assert_eq!(
                    reconciled.status,
                    ReconciliationStatus::Settled(chat.receipt)
                );
            }
        }
        assert_eq!(receipts.len(), usize::from(index >= 2));
        assert_eq!(
            gateway
                .resource
                .list_llm_token_consumptions(binding.account_id())
                .unwrap(),
            receipts
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            baseline_hits + usize::from(index >= 2)
        );
        let mut edited = chat_request();
        edited.session_hint = "changed".into();
        assert_eq!(
            gateway
                .llm_chat(&caller, &binding, &edited)
                .await
                .unwrap_err(),
            GatewayError::Conflict
        );
    }
    server.abort();
}

#[tokio::test]
async fn provisioning_replays_both_transactions_and_activates_only_after_sync() {
    let dir = TestRoot::new();
    let clock = Arc::new(AtomicU64::new(10_000));
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let mut gateway =
        Gateway::initialize(&dir.0, config(1, Some(policy.clone()), clock.clone())).unwrap();
    let parent = gateway.provision_root(root(), budget(1_000)).unwrap();
    for (index, point) in [
        FaultPoint::ChildIntent,
        FaultPoint::ChildAccount,
        FaultPoint::ChildBudget,
    ]
    .into_iter()
    .enumerate()
    {
        let raw = u8::try_from(index + 10).unwrap();
        let caller = call(raw, 2);
        let child_input = ChildSessionBinding {
            recipient: call(1, raw).caller,
            stable_identity: [raw; 16],
            delegated: false,
        };
        let request = AttenuateBudgetRequest {
            schema: Some(llm_gateway_schema_identity()),
            child_limit: Some(budget(100)),
            valid_until_ms: 12_000,
            child_process_id: Vec::new(),
            child_session_id: Vec::new(),
            child_principal_id: Vec::new(),
        };
        gateway.lock().unwrap().fault = Some(point);
        assert!(matches!(
            gateway.attenuate_budget(&caller, &parent, Some(&child_input), &request),
            Err(GatewayError::Durability)
        ));
        // The two authority transactions can exist, but the provision has no
        // active edge and no binding from which a call could be constructed.
        let store = gateway.store.lock().unwrap();
        assert_eq!(store.state.accounts.len(), index + 1);
        assert_eq!(store.state.pending_provisions.len(), 1);
        drop(store);
        drop(gateway);
        gateway = Gateway::open(&dir.0, config(1, Some(policy.clone()), clock.clone())).unwrap();
        let child = gateway
            .attenuate_budget(&call(raw, 3), &parent, Some(&child_input), &request)
            .unwrap()
            .child;
        let record = gateway
            .resource
            .inspect_llm_token_budget(child.account.account_id())
            .unwrap();
        assert_eq!(record.limits, Five::from(budget(100)).limits());
        assert_eq!(child.account.parent(), Some(parent.account_id()));
        assert_eq!(
            gateway
                .attenuate_budget(&caller, &parent, Some(&child_input), &request)
                .unwrap()
                .child,
            child
        );
        assert_eq!(gateway.lock().unwrap().state.accounts.len(), index + 2);
    }
}

#[tokio::test]
async fn failed_append_and_failed_sync_do_not_acknowledge_admission() {
    let (port, hits, server) = provider().await;
    for (index, fault) in [JournalFault::TornWrite, JournalFault::SyncFailure]
        .into_iter()
        .enumerate()
    {
        let dir = TestRoot::new();
        let clock = Arc::new(AtomicU64::new(10_000));
        let policy = Arc::new(Policy(AtomicBool::new(true)));
        let gateway =
            Gateway::initialize(&dir.0, config(port, Some(policy.clone()), clock.clone())).unwrap();
        let account = gateway.provision_root(root(), budget(1_000)).unwrap();
        let prior_hits = hits.load(Ordering::SeqCst);
        gateway.lock().unwrap().journal.fault = Some(fault);
        assert_eq!(
            gateway
                .llm_chat(&call(3, 3), &account, &chat_request())
                .await
                .unwrap_err(),
            GatewayError::Durability
        );
        assert_eq!(hits.load(Ordering::SeqCst), prior_hits);
        assert_eq!(
            gateway
                .query_budget(&call(3, 3), &account, &query())
                .unwrap_err(),
            GatewayError::Durability
        );
        drop(gateway);
        let gateway = Gateway::open(&dir.0, config(port, Some(policy), clock)).unwrap();
        let next = gateway
            .llm_chat(&call(3, 3), &account, &chat_request())
            .await;
        if index == 0 {
            // Torn prepare was never committed or dispatched. Same-key retry is safe.
            assert_eq!(next.unwrap().result.content_utf8, "答");
            assert_eq!(hits.load(Ordering::SeqCst), prior_hits + 1);
        } else {
            // A complete write with a failed sync may survive. It is recovered
            // conservatively as a prepared call with no dispatch fence.
            assert_eq!(next.unwrap_err(), GatewayError::ProviderFailure);
            assert_eq!(hits.load(Ordering::SeqCst), prior_hits);
        }
    }
    server.abort();
}

#[test]
fn journal_refuses_a_second_writer_and_invalid_or_unbounded_replay() {
    let dir = TestRoot::new();
    let clock = Arc::new(AtomicU64::new(10_000));
    let open = || Gateway::open(&dir.0, config(1, None, clock.clone()));
    let gateway = Gateway::initialize(&dir.0, config(1, None, clock.clone())).unwrap();
    gateway.provision_root(root(), budget(1_000)).unwrap();
    assert!(matches!(open(), Err(GatewayError::WriterBusy)));
    let mut other_root = root();
    other_root.authorization_domain[0] += 1;
    gateway.provision_root(other_root, budget(1_000)).unwrap();
    drop(gateway);

    let path = dir.0.join("llm-gateway.wal");
    let original = std::fs::read(&path).unwrap();
    let first_body = u32::from_be_bytes(original[16..20].try_into().unwrap()) as usize;
    let first_record = 84 + first_body;
    for bad in [
        // Complete but damaged frames at either end both fail closed.
        {
            let mut a = original.clone();
            a[53] ^= 1;
            a
        },
        {
            let mut a = original.clone();
            a[16] ^= 1;
            a
        },
        {
            let mut a = original.clone();
            a[15] ^= 2;
            a
        },
        {
            let mut a = original.clone();
            let last = a.len() - 1;
            a[last] ^= 1;
            a
        },
        // Dupe a valid complete existing record after the final sequence.
        {
            let mut a = original.clone();
            a.extend_from_slice(&original[..first_record]);
            a
        },
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(matches!(open(), Err(GatewayError::Durability)));
    }
    let mut torn = original.clone();
    torn.extend_from_slice(b"NLO");
    std::fs::write(&path, torn).unwrap();
    drop(open().unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), original);

    let mut cfg = config(1, None, clock.clone());
    cfg.limits.journal_admission_bytes = 90;
    cfg.limits.journal_max_bytes = 91;
    assert!(matches!(
        Gateway::open(&dir.0, cfg),
        Err(GatewayError::Durability)
    ));
    let mut cfg = config(1, None, clock);
    cfg.limits.journal_admission_records = 1;
    cfg.limits.journal_max_records = 3;
    assert!(matches!(
        Gateway::open(&dir.0, cfg),
        Err(GatewayError::Durability)
    ));
}

fn query() -> QueryBudgetRequest {
    QueryBudgetRequest {
        schema: Some(llm_gateway_schema_identity()),
    }
}
