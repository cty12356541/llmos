//! The deterministic fake provider core.
//!
//! [`MockProvider`] wires the three provider operations onto the durable
//! `SqliteOperationStore` authority and derives every terminal outcome from
//! caller-supplied seeds through domain-separated SHA-256:
//!
//! * `register` — idempotent `OperationSpec` registration; the response
//!   receipt is the authority-derived endpoint admission receipt
//!   (`inspect_endpoint_proof`), never a caller-supplied tuple.
//! * `dispatch` — the durable prepare→activate boundary
//!   (`prepare_dispatch` → `activate_dispatch`); the one-shot callback
//!   ticket, preparation receipt and activation receipt all come from the
//!   authority. An exact replay after a restart returns the original ticket
//!   (`replayed = true`) and never issues a second dispatch.
//! * `complete` — reconstructs the owner-bound callback ticket from the
//!   durable row (never from caller claims), derives the terminal outcome
//!   deterministically from `(operation, callback, seed)`, and commits it
//!   through the callback identity fence. A different seed replaying a
//!   terminal callback is a `CallbackIdentityConflict`, not a new outcome.
//!
//! The core takes no time and no randomness inputs; determinism is total for
//! a fixed durable database and request bytes.

use std::sync::Arc;

use nlos_operation::{
    CallbackTicket, CompletionDecision, CompletionOutcome, OperationHandle, OperationSpec,
    OperationState,
};
use nlos_store::{
    OperationActivationDecision, OperationPrepareDecision, RegistrationDecision,
    SqliteOperationStore, StoreError,
};
use nlos_types::{CallbackId, ReceiptId};
use sha2::{Digest, Sha256};

/// Domain separator for seed-derived terminal outcomes.
pub const OUTCOME_DOMAIN: &[u8] = b"nlos/driver-mock/outcome/v1";

/// Outcome of one idempotent provider registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterProviderOutcome {
    /// `false` for the first registration, `true` for an exact replay.
    pub replayed: bool,
    pub handle: OperationHandle,
    /// Authority-derived endpoint admission receipt (`inspect_endpoint_proof`),
    /// stable across restarts because it derives from the immutable
    /// registration row.
    pub admission_receipt_id: ReceiptId,
}

/// One provider dispatch request: the operation handle plus the callback
/// identity the provider must bind durably before activating.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchProviderOperation {
    pub handle: OperationHandle,
    pub callback_id: CallbackId,
}

/// Outcome of one provider dispatch through the durable prepare→activate
/// boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchProviderOutcome {
    /// `true` when the activation replayed the original one-shot ticket
    /// instead of opening the dispatch boundary for the first time.
    pub replayed: bool,
    pub preparation_receipt_id: ReceiptId,
    pub activation_receipt_id: ReceiptId,
    pub ticket: CallbackTicket,
}

/// One provider completion request. The terminal outcome is derived
/// deterministically from `(handle, callback_id, seed)`; replaying the same
/// request replays the same terminal state, while a different seed under the
/// same callback identity is rejected by the durable fence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteProviderOperation {
    pub handle: OperationHandle,
    pub callback_id: CallbackId,
    pub seed: [u8; 32],
}

/// Outcome of one provider completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteProviderOutcome {
    /// `true` when the durable callback fence recognized a duplicate
    /// terminal callback; the terminal state is returned unchanged.
    pub replayed: bool,
    pub outcome: CompletionOutcome,
    pub state: OperationState,
}

/// Derives the deterministic terminal outcome for one completion: a
/// domain-separated SHA-256 over `(domain, operation id, generation,
/// callback id, seed)`. The first 16 digest bytes are the terminal receipt;
/// byte 16 selects the outcome class, so a caller controls the provider's
/// behavior entirely through seed bytes.
#[must_use]
pub fn derive_provider_outcome(
    handle: OperationHandle,
    callback_id: CallbackId,
    seed: &[u8; 32],
) -> CompletionOutcome {
    let mut hasher = Sha256::new();
    hasher.update(OUTCOME_DOMAIN);
    hasher.update(handle.operation_id.as_bytes());
    hasher.update(handle.generation.get().to_be_bytes());
    hasher.update(callback_id.as_bytes());
    hasher.update(seed);
    let digest: [u8; 32] = hasher.finalize().into();
    let mut receipt = [0_u8; 16];
    receipt.copy_from_slice(&digest[..16]);
    let receipt_id = ReceiptId::from_bytes(receipt);
    match digest[16] % 4 {
        0 => CompletionOutcome::Completed { receipt_id },
        1 => CompletionOutcome::Failed { receipt_id },
        2 => CompletionOutcome::PartialEffect { receipt_id },
        _ => CompletionOutcome::EffectUnknown { receipt_id },
    }
}

/// The deterministic fake provider. Both the in-process handle and the typed
/// IPC face share one instance; the provider owns no canonical state beyond
/// the durable `SqliteOperationStore` authority it is bound to.
pub struct MockProvider {
    store: Arc<SqliteOperationStore>,
}

impl MockProvider {
    /// Binds the provider to one durable operation authority. Reopening the
    /// same database path with a new `SqliteOperationStore` and a new
    /// provider reproduces the replay semantics of a provider process
    /// restart.
    #[must_use]
    pub const fn new(store: Arc<SqliteOperationStore>) -> Self {
        Self { store }
    }

    /// The durable authority this provider is bound to.
    #[must_use]
    pub fn store(&self) -> &SqliteOperationStore {
        &self.store
    }

    /// Registers one provider operation idempotently. Exact spec replays
    /// return the original handle and admission receipt; conflicting spec
    /// reuse of the same operation id is rejected by the authority.
    ///
    /// # Errors
    ///
    /// Returns the authority's typed storage or duplicate-operation error.
    pub fn register(&self, spec: OperationSpec) -> Result<RegisterProviderOutcome, StoreError> {
        let decision = self.store.register(spec)?;
        let replayed = matches!(decision, RegistrationDecision::Existing(_));
        let handle = decision.handle();
        let admission_receipt_id = self
            .store
            .inspect_endpoint_proof(handle)?
            .admission_receipt_id;
        Ok(RegisterProviderOutcome {
            replayed,
            handle,
            admission_receipt_id,
        })
    }

    /// Opens (or exactly replays) the durable dispatch boundary for one
    /// operation: `prepare_dispatch` records the owner-bound callback
    /// identity, `activate_dispatch` consumes it and issues the fenced
    /// one-shot callback ticket.
    ///
    /// # Errors
    ///
    /// Returns the authority's typed stale-generation, state, conflict, or
    /// storage errors.
    pub fn dispatch(
        &self,
        request: DispatchProviderOperation,
    ) -> Result<DispatchProviderOutcome, StoreError> {
        let preparation = match self
            .store
            .prepare_dispatch(request.handle, request.callback_id)?
        {
            OperationPrepareDecision::Prepared(preparation)
            | OperationPrepareDecision::Replayed(preparation) => preparation,
        };
        let activation = self.store.activate_dispatch(preparation)?;
        let (replayed, activation) = match activation {
            OperationActivationDecision::Activated(activation) => (false, activation),
            OperationActivationDecision::Replayed(activation) => (true, activation),
        };
        Ok(DispatchProviderOutcome {
            replayed,
            preparation_receipt_id: preparation.preparation_receipt_id,
            activation_receipt_id: activation.activation_receipt_id,
            ticket: activation.ticket,
        })
    }

    /// Completes one dispatched provider operation. The callback ticket is
    /// reconstructed from the durable owner row, the terminal outcome is
    /// derived deterministically from the request, and the callback identity
    /// fence commits it. Replaying the exact request reports the original
    /// terminal state with `replayed = true`.
    ///
    /// # Errors
    ///
    /// Returns the authority's typed state, generation, callback-conflict,
    /// or storage errors.
    pub fn complete(
        &self,
        request: CompleteProviderOperation,
    ) -> Result<CompleteProviderOutcome, StoreError> {
        let snapshot = self.store.inspect(request.handle)?;
        let ticket = CallbackTicket {
            callback_id: request.callback_id,
            operation: request.handle,
            owner_fiber: snapshot.owner_fiber,
            cancel_epoch: snapshot.cancel_epoch,
        };
        let outcome = derive_provider_outcome(request.handle, request.callback_id, &request.seed);
        let decision = self.store.complete(ticket, outcome)?;
        let replayed = matches!(decision, CompletionDecision::Duplicate { .. });
        let state = match decision {
            CompletionDecision::CanonicalizedAndWake { state }
            | CompletionDecision::CanonicalizedForReconciliation { state }
            | CompletionDecision::Duplicate { state } => state,
        };
        Ok(CompleteProviderOutcome {
            replayed,
            outcome,
            state,
        })
    }
}
