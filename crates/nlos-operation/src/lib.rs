//! Generation- and cancellation-fenced external operation registry.
//!
//! A late callback must never wake an obsolete fiber. It may still carry
//! authoritative evidence about an external effect, so terminal callbacks are
//! canonicalized for reconciliation even when their wake permission is fenced.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use nlos_runtime::FiberHandle;
use nlos_types::{
    CallbackId, CancelEpoch, CancellationScopeId, Generation, OperationId, ReceiptId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationSpec {
    pub operation_id: OperationId,
    pub generation: Generation,
    pub owner_fiber: FiberHandle,
    pub cancellation_scope_id: CancellationScopeId,
    pub cancellation_generation: Generation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationHandle {
    pub operation_id: OperationId,
    pub generation: Generation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallbackTicket {
    pub callback_id: CallbackId,
    pub operation: OperationHandle,
    pub owner_fiber: FiberHandle,
    pub cancel_epoch: CancelEpoch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationState {
    Registered,
    Dispatched,
    CancelRequested,
    Completed { receipt_id: ReceiptId },
    Failed { receipt_id: ReceiptId },
    CancelledBeforeEffect { receipt_id: ReceiptId },
    PartialEffect { receipt_id: ReceiptId },
    EffectUnknown { receipt_id: ReceiptId },
}

impl OperationState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed { .. }
                | Self::Failed { .. }
                | Self::CancelledBeforeEffect { .. }
                | Self::PartialEffect { .. }
                | Self::EffectUnknown { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionOutcome {
    Completed { receipt_id: ReceiptId },
    Failed { receipt_id: ReceiptId },
    CancelledBeforeEffect { receipt_id: ReceiptId },
    PartialEffect { receipt_id: ReceiptId },
    EffectUnknown { receipt_id: ReceiptId },
}

impl CompletionOutcome {
    const fn state(self) -> OperationState {
        match self {
            Self::Completed { receipt_id } => OperationState::Completed { receipt_id },
            Self::Failed { receipt_id } => OperationState::Failed { receipt_id },
            Self::CancelledBeforeEffect { receipt_id } => {
                OperationState::CancelledBeforeEffect { receipt_id }
            }
            Self::PartialEffect { receipt_id } => OperationState::PartialEffect { receipt_id },
            Self::EffectUnknown { receipt_id } => OperationState::EffectUnknown { receipt_id },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionDecision {
    CanonicalizedAndWake { state: OperationState },
    CanonicalizedForReconciliation { state: OperationState },
    Duplicate { state: OperationState },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationError {
    DuplicateOperation,
    InvalidGeneration,
    InvalidState,
    CancelEpochExhausted,
    CallbackIdentityConflict,
}

impl fmt::Display for OperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DuplicateOperation => "operation already exists",
            Self::InvalidGeneration => "operation generation is stale",
            Self::InvalidState => "operation state does not allow this transition",
            Self::CancelEpochExhausted => "operation cancellation epoch is exhausted",
            Self::CallbackIdentityConflict => {
                "callback identity was reused with different completion data"
            }
        })
    }
}

impl Error for OperationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationSnapshot {
    pub handle: OperationHandle,
    pub owner_fiber: FiberHandle,
    pub cancel_epoch: CancelEpoch,
    pub state: OperationState,
}

struct OperationRecord {
    spec: OperationSpec,
    cancel_epoch: CancelEpoch,
    state: OperationState,
    accepted_callback: Option<(CallbackId, CompletionOutcome)>,
}

/// Thread-safe in-memory `PoC` of the mechanical Operation callback fence.
#[derive(Default)]
pub struct OperationRegistry {
    operations: Mutex<HashMap<OperationId, OperationRecord>>,
}

impl OperationRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new operation generation.
    ///
    /// # Errors
    ///
    /// Returns [`OperationError::DuplicateOperation`] when the stable ID is
    /// already registered. Recovery requires a future explicit fenced takeover
    /// API rather than implicit replacement.
    pub fn register(&self, spec: OperationSpec) -> Result<OperationHandle, OperationError> {
        let mut operations = lock_unpoisoned(&self.operations);
        if operations.contains_key(&spec.operation_id) {
            return Err(OperationError::DuplicateOperation);
        }
        let handle = OperationHandle {
            operation_id: spec.operation_id,
            generation: spec.generation,
        };
        operations.insert(
            spec.operation_id,
            OperationRecord {
                spec,
                cancel_epoch: CancelEpoch::INITIAL,
                state: OperationState::Registered,
                accepted_callback: None,
            },
        );
        Ok(handle)
    }

    /// Marks an operation dispatched and returns the callback ticket that may
    /// wake its current owner fiber.
    ///
    /// # Errors
    ///
    /// Returns a generation or state error when the operation is stale,
    /// cancelled, terminal, or already dispatched.
    pub fn dispatch(
        &self,
        handle: OperationHandle,
        callback_id: CallbackId,
    ) -> Result<CallbackTicket, OperationError> {
        let mut operations = lock_unpoisoned(&self.operations);
        let record = record_mut(&mut operations, handle)?;
        if record.state != OperationState::Registered {
            return Err(OperationError::InvalidState);
        }
        record.state = OperationState::Dispatched;
        Ok(CallbackTicket {
            callback_id,
            operation: handle,
            owner_fiber: record.spec.owner_fiber,
            cancel_epoch: record.cancel_epoch,
        })
    }

    /// Fences the current callback epoch.
    ///
    /// A registered operation becomes terminal `CancelledBeforeEffect`. A
    /// dispatched operation becomes `CancelRequested`; its final callback is
    /// reconciliation-only.
    ///
    /// # Errors
    ///
    /// Returns a generation/state error or epoch exhaustion.
    pub fn request_cancel(
        &self,
        handle: OperationHandle,
        no_effect_receipt: ReceiptId,
    ) -> Result<OperationSnapshot, OperationError> {
        let mut operations = lock_unpoisoned(&self.operations);
        let record = record_mut(&mut operations, handle)?;
        if !matches!(
            record.state,
            OperationState::Registered | OperationState::Dispatched
        ) {
            return Err(OperationError::InvalidState);
        }
        record.cancel_epoch = record
            .cancel_epoch
            .checked_next()
            .ok_or(OperationError::CancelEpochExhausted)?;
        record.state = match record.state {
            OperationState::Registered => OperationState::CancelledBeforeEffect {
                receipt_id: no_effect_receipt,
            },
            OperationState::Dispatched => OperationState::CancelRequested,
            _ => unreachable!("state was validated before epoch mutation"),
        };
        Ok(snapshot(record))
    }

    /// Accepts a terminal callback exactly once.
    ///
    /// # Errors
    ///
    /// Returns a generation/state error for unknown or impossible operations.
    /// Reusing one callback ID with a different outcome is a protocol fault.
    pub fn complete(
        &self,
        ticket: CallbackTicket,
        outcome: CompletionOutcome,
    ) -> Result<CompletionDecision, OperationError> {
        let mut operations = lock_unpoisoned(&self.operations);
        let record = record_mut(&mut operations, ticket.operation)?;

        if ticket.owner_fiber != record.spec.owner_fiber {
            return Err(OperationError::InvalidGeneration);
        }

        if let Some((accepted_id, accepted_outcome)) = record.accepted_callback {
            if accepted_id == ticket.callback_id {
                if accepted_outcome != outcome {
                    return Err(OperationError::CallbackIdentityConflict);
                }
                return Ok(CompletionDecision::Duplicate {
                    state: record.state,
                });
            }
            return Err(OperationError::InvalidState);
        }

        if !matches!(
            record.state,
            OperationState::Dispatched | OperationState::CancelRequested
        ) {
            return Err(OperationError::InvalidState);
        }

        let wake_allowed = ticket.cancel_epoch == record.cancel_epoch
            && record.state == OperationState::Dispatched;
        record.state = outcome.state();
        record.accepted_callback = Some((ticket.callback_id, outcome));

        Ok(if wake_allowed {
            CompletionDecision::CanonicalizedAndWake {
                state: record.state,
            }
        } else {
            CompletionDecision::CanonicalizedForReconciliation {
                state: record.state,
            }
        })
    }

    /// Returns a mechanically consistent operation snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`OperationError::InvalidGeneration`] for unknown or stale
    /// handles.
    pub fn inspect(&self, handle: OperationHandle) -> Result<OperationSnapshot, OperationError> {
        let operations = lock_unpoisoned(&self.operations);
        let record = operations
            .get(&handle.operation_id)
            .ok_or(OperationError::InvalidGeneration)?;
        if record.spec.generation != handle.generation {
            return Err(OperationError::InvalidGeneration);
        }
        Ok(snapshot(record))
    }
}

fn record_mut(
    operations: &mut HashMap<OperationId, OperationRecord>,
    handle: OperationHandle,
) -> Result<&mut OperationRecord, OperationError> {
    let record = operations
        .get_mut(&handle.operation_id)
        .ok_or(OperationError::InvalidGeneration)?;
    if record.spec.generation != handle.generation {
        return Err(OperationError::InvalidGeneration);
    }
    Ok(record)
}

fn snapshot(record: &OperationRecord) -> OperationSnapshot {
    OperationSnapshot {
        handle: OperationHandle {
            operation_id: record.spec.operation_id,
            generation: record.spec.generation,
        },
        owner_fiber: record.spec.owner_fiber,
        cancel_epoch: record.cancel_epoch,
        state: record.state,
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
