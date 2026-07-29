use nlos_operation::{
    AcceptedCallback, CallbackTicket, CompletionDecision, CompletionOutcome, IssuedCallback,
    OperationError, OperationMachine, OperationRegistry, OperationSpec, OperationState,
};
use nlos_runtime::FiberHandle;
use nlos_types::{
    CallbackId, CancelEpoch, CancellationScopeId, ExecutionFiberId, Generation, OperationId,
    ReceiptId,
};
use std::sync::Barrier;

fn bytes(value: u8) -> [u8; 16] {
    [value; 16]
}

fn spec() -> OperationSpec {
    OperationSpec {
        operation_id: OperationId::from_bytes(bytes(1)),
        generation: Generation::INITIAL,
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes(bytes(2)),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(3)),
        cancellation_generation: Generation::INITIAL,
    }
}

#[test]
fn current_callback_becomes_canonical_and_may_wake() {
    let registry = OperationRegistry::new();
    let handle = registry.register(spec()).expect("register");
    let ticket = registry
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");
    let outcome = CompletionOutcome::Completed {
        receipt_id: ReceiptId::from_bytes(bytes(5)),
    };

    assert_eq!(
        registry.complete(ticket, outcome),
        Ok(CompletionDecision::CanonicalizedAndWake {
            state: OperationState::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5))
            }
        })
    );
    assert_eq!(
        registry.complete(ticket, outcome),
        Ok(CompletionDecision::Duplicate {
            state: OperationState::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5))
            }
        })
    );
}

#[test]
fn late_callback_is_reconciliation_only_after_cancel_epoch_advances() {
    let registry = OperationRegistry::new();
    let handle = registry.register(spec()).expect("register");
    let ticket = registry
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");
    let cancelled = registry
        .request_cancel(handle, ReceiptId::from_bytes(bytes(6)))
        .expect("cancel");
    assert_eq!(cancelled.state, OperationState::CancelRequested);
    assert_eq!(cancelled.cancel_epoch.get(), 1);

    let outcome = CompletionOutcome::PartialEffect {
        receipt_id: ReceiptId::from_bytes(bytes(7)),
    };
    assert_eq!(
        registry.complete(ticket, outcome),
        Ok(CompletionDecision::CanonicalizedForReconciliation {
            state: OperationState::PartialEffect {
                receipt_id: ReceiptId::from_bytes(bytes(7))
            }
        })
    );
}

#[test]
fn cancel_before_dispatch_prevents_dispatch() {
    let registry = OperationRegistry::new();
    let handle = registry.register(spec()).expect("register");
    let receipt = ReceiptId::from_bytes(bytes(8));
    let snapshot = registry
        .request_cancel(handle, receipt)
        .expect("cancel before dispatch");

    assert_eq!(
        snapshot.state,
        OperationState::CancelledBeforeEffect {
            receipt_id: receipt
        }
    );
    assert_eq!(
        registry.dispatch(handle, CallbackId::from_bytes(bytes(9))),
        Err(OperationError::InvalidState)
    );
}

#[test]
fn stale_operation_and_fiber_generations_are_rejected() {
    let registry = OperationRegistry::new();
    let handle = registry.register(spec()).expect("register");
    let ticket = registry
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");
    let next = Generation::INITIAL.checked_next().expect("next");

    let stale_operation = nlos_operation::OperationHandle {
        operation_id: handle.operation_id,
        generation: next,
    };
    assert_eq!(
        registry.inspect(stale_operation),
        Err(OperationError::InvalidGeneration)
    );

    let stale_fiber_ticket = CallbackTicket {
        owner_fiber: FiberHandle {
            fiber_id: ticket.owner_fiber.fiber_id,
            generation: next,
        },
        ..ticket
    };
    assert_eq!(
        registry.complete(
            stale_fiber_ticket,
            CompletionOutcome::EffectUnknown {
                receipt_id: ReceiptId::from_bytes(bytes(10))
            }
        ),
        Err(OperationError::InvalidGeneration)
    );
}

#[test]
fn callback_ticket_cannot_be_substituted_after_dispatch() {
    let registry = OperationRegistry::new();
    let handle = registry.register(spec()).expect("register");
    let mut ticket = registry
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");
    ticket.callback_id = CallbackId::from_bytes(bytes(8));

    assert_eq!(
        registry.complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5)),
            },
        ),
        Err(OperationError::InvalidGeneration)
    );
    assert_eq!(
        registry.inspect(handle).expect("inspect").state,
        OperationState::Dispatched
    );
}

#[test]
fn restore_rejects_impossible_callback_and_cancel_epoch_combinations() {
    let callback_id = CallbackId::from_bytes(bytes(4));
    let receipt_id = ReceiptId::from_bytes(bytes(5));
    assert_eq!(
        OperationMachine::restore(
            spec(),
            CancelEpoch::INITIAL,
            OperationState::Completed { receipt_id },
            Some(IssuedCallback {
                callback_id,
                cancel_epoch: CancelEpoch::INITIAL,
            }),
            None,
        ),
        Err(OperationError::InvalidState)
    );
    assert_eq!(
        OperationMachine::restore(
            spec(),
            CancelEpoch::new(2),
            OperationState::Completed { receipt_id },
            Some(IssuedCallback {
                callback_id,
                cancel_epoch: CancelEpoch::INITIAL,
            }),
            Some(AcceptedCallback {
                callback_id,
                outcome: CompletionOutcome::Completed { receipt_id },
            }),
        ),
        Err(OperationError::InvalidState)
    );
}

#[test]
fn callback_id_cannot_be_reused_with_different_outcome() {
    let registry = OperationRegistry::new();
    let handle = registry.register(spec()).expect("register");
    let ticket = registry
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");
    registry
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5)),
            },
        )
        .expect("complete");

    assert_eq!(
        registry.complete(
            ticket,
            CompletionOutcome::Failed {
                receipt_id: ReceiptId::from_bytes(bytes(11))
            }
        ),
        Err(OperationError::CallbackIdentityConflict)
    );
}

#[test]
fn cancel_and_completion_race_has_only_two_linearizable_results() {
    for _ in 0..256 {
        let registry = OperationRegistry::new();
        let handle = registry.register(spec()).expect("register");
        let ticket = registry
            .dispatch(handle, CallbackId::from_bytes(bytes(4)))
            .expect("dispatch");
        let barrier = Barrier::new(3);
        let cancel_receipt = ReceiptId::from_bytes(bytes(12));
        let complete_receipt = ReceiptId::from_bytes(bytes(13));

        std::thread::scope(|scope| {
            let cancel = scope.spawn(|| {
                barrier.wait();
                registry.request_cancel(handle, cancel_receipt)
            });
            let complete = scope.spawn(|| {
                barrier.wait();
                registry.complete(
                    ticket,
                    CompletionOutcome::Completed {
                        receipt_id: complete_receipt,
                    },
                )
            });
            barrier.wait();

            let cancel_result = cancel.join().expect("cancel thread");
            let complete_result = complete.join().expect("complete thread");
            match (cancel_result, complete_result) {
                (
                    Ok(snapshot),
                    Ok(CompletionDecision::CanonicalizedForReconciliation { state }),
                ) => {
                    assert_eq!(snapshot.state, OperationState::CancelRequested);
                    assert_eq!(
                        state,
                        OperationState::Completed {
                            receipt_id: complete_receipt
                        }
                    );
                }
                (
                    Err(OperationError::InvalidState),
                    Ok(CompletionDecision::CanonicalizedAndWake { state }),
                ) => {
                    assert_eq!(
                        state,
                        OperationState::Completed {
                            receipt_id: complete_receipt
                        }
                    );
                }
                unexpected => panic!("non-linearizable race result: {unexpected:?}"),
            }
        });
    }
}
