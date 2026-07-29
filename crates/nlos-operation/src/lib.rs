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
pub struct AcceptedCallback {
    pub callback_id: CallbackId,
    pub outcome: CompletionOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IssuedCallback {
    pub callback_id: CallbackId,
    pub cancel_epoch: CancelEpoch,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationMachine {
    spec: OperationSpec,
    cancel_epoch: CancelEpoch,
    state: OperationState,
    issued_callback: Option<IssuedCallback>,
    accepted_callback: Option<AcceptedCallback>,
}

impl OperationMachine {
    #[must_use]
    pub const fn new(spec: OperationSpec) -> Self {
        Self {
            spec,
            cancel_epoch: CancelEpoch::INITIAL,
            state: OperationState::Registered,
            issued_callback: None,
            accepted_callback: None,
        }
    }

    /// Restores a machine from a trusted durable record after validating its
    /// cross-field invariants.
    ///
    /// # Errors
    ///
    /// Returns [`OperationError::InvalidState`] when terminal state and
    /// callback data disagree or when a non-terminal record carries a callback.
    pub fn restore(
        spec: OperationSpec,
        cancel_epoch: CancelEpoch,
        state: OperationState,
        issued_callback: Option<IssuedCallback>,
        accepted_callback: Option<AcceptedCallback>,
    ) -> Result<Self, OperationError> {
        let callback_is_terminal = accepted_callback
            .is_some_and(|callback| callback.outcome.state() == state && state.is_terminal());
        let issued_matches_accepted = match (issued_callback, accepted_callback) {
            (Some(issued), Some(accepted)) => issued.callback_id == accepted.callback_id,
            (_, None) => true,
            (None, Some(_)) => false,
        };
        let state_matches_issue = match state {
            OperationState::Registered => issued_callback.is_none(),
            OperationState::Dispatched | OperationState::CancelRequested => {
                issued_callback.is_some() && accepted_callback.is_none()
            }
            OperationState::CancelledBeforeEffect { .. } => {
                issued_callback.is_some() == accepted_callback.is_some()
            }
            OperationState::Completed { .. }
            | OperationState::Failed { .. }
            | OperationState::PartialEffect { .. }
            | OperationState::EffectUnknown { .. } => {
                issued_callback.is_some() && accepted_callback.is_some()
            }
        };
        let acceptance_is_valid = match accepted_callback {
            Some(_) => callback_is_terminal,
            None => !matches!(
                state,
                OperationState::Completed { .. }
                    | OperationState::Failed { .. }
                    | OperationState::PartialEffect { .. }
                    | OperationState::EffectUnknown { .. }
            ),
        };
        let epoch_matches_state = match issued_callback {
            None => match state {
                OperationState::Registered => cancel_epoch == CancelEpoch::INITIAL,
                OperationState::CancelledBeforeEffect { .. } => cancel_epoch == CancelEpoch::new(1),
                _ => false,
            },
            Some(issued) => {
                let same_epoch = cancel_epoch == issued.cancel_epoch;
                let cancelled_epoch = issued.cancel_epoch.checked_next() == Some(cancel_epoch);
                match state {
                    OperationState::Dispatched => same_epoch,
                    OperationState::CancelRequested => cancelled_epoch,
                    OperationState::Completed { .. }
                    | OperationState::Failed { .. }
                    | OperationState::CancelledBeforeEffect { .. }
                    | OperationState::PartialEffect { .. }
                    | OperationState::EffectUnknown { .. } => same_epoch || cancelled_epoch,
                    OperationState::Registered => false,
                }
            }
        };
        if !issued_matches_accepted
            || !state_matches_issue
            || !acceptance_is_valid
            || !epoch_matches_state
        {
            return Err(OperationError::InvalidState);
        }
        Ok(Self {
            spec,
            cancel_epoch,
            state,
            issued_callback,
            accepted_callback,
        })
    }

    #[must_use]
    pub const fn spec(&self) -> OperationSpec {
        self.spec
    }

    #[must_use]
    pub const fn accepted_callback(&self) -> Option<AcceptedCallback> {
        self.accepted_callback
    }

    #[must_use]
    pub const fn issued_callback(&self) -> Option<IssuedCallback> {
        self.issued_callback
    }

    #[must_use]
    pub const fn snapshot(&self) -> OperationSnapshot {
        OperationSnapshot {
            handle: OperationHandle {
                operation_id: self.spec.operation_id,
                generation: self.spec.generation,
            },
            owner_fiber: self.spec.owner_fiber,
            cancel_epoch: self.cancel_epoch,
            state: self.state,
        }
    }

    /// Applies the dispatch transition and returns its fenced callback ticket.
    ///
    /// # Errors
    ///
    /// Returns a generation or state error for stale or non-registered input.
    pub fn dispatch(
        &mut self,
        handle: OperationHandle,
        callback_id: CallbackId,
    ) -> Result<CallbackTicket, OperationError> {
        self.validate_handle(handle)?;
        if self.state != OperationState::Registered {
            return Err(OperationError::InvalidState);
        }
        self.state = OperationState::Dispatched;
        self.issued_callback = Some(IssuedCallback {
            callback_id,
            cancel_epoch: self.cancel_epoch,
        });
        Ok(CallbackTicket {
            callback_id,
            operation: handle,
            owner_fiber: self.spec.owner_fiber,
            cancel_epoch: self.cancel_epoch,
        })
    }

    /// Advances the cancellation fence and applies the matching state change.
    ///
    /// # Errors
    ///
    /// Returns a generation/state error or epoch exhaustion.
    pub fn request_cancel(
        &mut self,
        handle: OperationHandle,
        no_effect_receipt: ReceiptId,
    ) -> Result<OperationSnapshot, OperationError> {
        self.validate_handle(handle)?;
        if !matches!(
            self.state,
            OperationState::Registered | OperationState::Dispatched
        ) {
            return Err(OperationError::InvalidState);
        }
        self.cancel_epoch = self
            .cancel_epoch
            .checked_next()
            .ok_or(OperationError::CancelEpochExhausted)?;
        self.state = match self.state {
            OperationState::Registered => OperationState::CancelledBeforeEffect {
                receipt_id: no_effect_receipt,
            },
            OperationState::Dispatched => OperationState::CancelRequested,
            _ => unreachable!("state was validated before epoch mutation"),
        };
        Ok(self.snapshot())
    }

    /// Applies one terminal callback through the generation/cancel fence.
    ///
    /// # Errors
    ///
    /// Returns a generation/state error or callback identity conflict.
    pub fn complete(
        &mut self,
        ticket: CallbackTicket,
        outcome: CompletionOutcome,
    ) -> Result<CompletionDecision, OperationError> {
        self.validate_handle(ticket.operation)?;
        if ticket.owner_fiber != self.spec.owner_fiber {
            return Err(OperationError::InvalidGeneration);
        }
        let issued = self.issued_callback.ok_or(OperationError::InvalidState)?;
        if issued.callback_id != ticket.callback_id || issued.cancel_epoch != ticket.cancel_epoch {
            return Err(OperationError::InvalidGeneration);
        }

        if let Some(accepted) = self.accepted_callback {
            if accepted.callback_id == ticket.callback_id {
                if accepted.outcome != outcome {
                    return Err(OperationError::CallbackIdentityConflict);
                }
                return Ok(CompletionDecision::Duplicate { state: self.state });
            }
            return Err(OperationError::InvalidState);
        }

        if !matches!(
            self.state,
            OperationState::Dispatched | OperationState::CancelRequested
        ) {
            return Err(OperationError::InvalidState);
        }

        let wake_allowed =
            ticket.cancel_epoch == self.cancel_epoch && self.state == OperationState::Dispatched;
        self.state = outcome.state();
        self.accepted_callback = Some(AcceptedCallback {
            callback_id: ticket.callback_id,
            outcome,
        });

        Ok(if wake_allowed {
            CompletionDecision::CanonicalizedAndWake { state: self.state }
        } else {
            CompletionDecision::CanonicalizedForReconciliation { state: self.state }
        })
    }

    fn validate_handle(&self, handle: OperationHandle) -> Result<(), OperationError> {
        if self.spec.operation_id != handle.operation_id
            || self.spec.generation != handle.generation
        {
            return Err(OperationError::InvalidGeneration);
        }
        Ok(())
    }
}

/// Thread-safe in-memory `PoC` of the mechanical Operation callback fence.
#[derive(Default)]
pub struct OperationRegistry {
    operations: Mutex<HashMap<OperationId, OperationMachine>>,
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
        operations.insert(spec.operation_id, OperationMachine::new(spec));
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
        record.dispatch(handle, callback_id)
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
        record.request_cancel(handle, no_effect_receipt)
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
        record.complete(ticket, outcome)
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
        if record.spec().generation != handle.generation {
            return Err(OperationError::InvalidGeneration);
        }
        Ok(record.snapshot())
    }
}

fn record_mut(
    operations: &mut HashMap<OperationId, OperationMachine>,
    handle: OperationHandle,
) -> Result<&mut OperationMachine, OperationError> {
    let record = operations
        .get_mut(&handle.operation_id)
        .ok_or(OperationError::InvalidGeneration)?;
    if record.spec().generation != handle.generation {
        return Err(OperationError::InvalidGeneration);
    }
    Ok(record)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
