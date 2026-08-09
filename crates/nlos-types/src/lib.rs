//! Stable nominal types shared by NLOS contracts.
//!
//! This crate intentionally has no runtime, database, transport, or UI
//! dependencies. Stable identities must not inherit implementation-local
//! identities from Tokio, `SQLite`, Wasmtime, or an operating system.

use core::fmt;
use core::num::NonZeroU64;

macro_rules! nominal_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; 16]);

        impl $name {
            #[must_use]
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(bytes)
            }

            #[must_use]
            pub const fn into_bytes(self) -> [u8; 16] {
                self.0
            }

            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(stringify!($name))?;
                formatter.write_str("(")?;
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                formatter.write_str(")")
            }
        }
    };
}

nominal_id!(ApplicationId);
nominal_id!(PrincipalId);
nominal_id!(PackageId);
nominal_id!(InstallationId);
nominal_id!(ExecutableId);
nominal_id!(ProcessId);
nominal_id!(AgentRoleId);
nominal_id!(AgentInstanceId);
nominal_id!(ExecutionFiberId);
nominal_id!(ActivationId);
nominal_id!(TaskGroupId);
nominal_id!(TaskAttemptId);
nominal_id!(TaskId);
nominal_id!(IsolationDomainId);
nominal_id!(TaskPlanId);
nominal_id!(TaskNodeId);
nominal_id!(TaskSnapshotId);
nominal_id!(TaskCommitId);
nominal_id!(CommitPermitId);
nominal_id!(EffectSlotId);
nominal_id!(EffectPermitId);
nominal_id!(TaskAuthorityAssignmentId);
nominal_id!(IntentId);
nominal_id!(ActionId);
nominal_id!(ControlCommandId);
nominal_id!(OperationId);
nominal_id!(CallbackId);
nominal_id!(CancellationScopeId);
nominal_id!(ContextId);
nominal_id!(WorkspaceId);
nominal_id!(SessionId);
nominal_id!(NamespaceId);
nominal_id!(CapabilityId);
nominal_id!(ResourceAccountId);
nominal_id!(ResourceGroupId);
nominal_id!(ResourceAllocationId);
nominal_id!(ResourceControllerId);
nominal_id!(ControllerBindingId);
nominal_id!(AdmissionPlanId);
nominal_id!(AdmissionDecisionId);
nominal_id!(ReclaimRequestId);
nominal_id!(QuotaLeaseId);
nominal_id!(CapacityLeaseId);
nominal_id!(ExclusiveDeviceLeaseId);
nominal_id!(ReservationId);
nominal_id!(EscrowId);
nominal_id!(ExpireRequestId);
nominal_id!(ChannelId);
nominal_id!(DeviceId);
nominal_id!(DriverId);
nominal_id!(SemanticEventId);
nominal_id!(ControlDomainId);
nominal_id!(KeyId);
nominal_id!(SchedulerDomainId);
nominal_id!(ReceiptId);
nominal_id!(IdempotencyKey);
nominal_id!(ArtifactId);

/// A non-zero incarnation used to fence stale handles and callbacks.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Generation(NonZeroU64);

impl Generation {
    pub const INITIAL: Self = Self(NonZeroU64::MIN);

    #[must_use]
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Returns `None` instead of wrapping when the generation space is
    /// exhausted. Callers must fence the object rather than reuse an old value.
    #[must_use]
    pub fn checked_next(self) -> Option<Self> {
        self.0
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Self)
    }
}

/// A monotonic cancellation epoch. Epoch zero means no cancellation request
/// has yet fenced callbacks for the object.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CancelEpoch(u64);

impl CancelEpoch {
    pub const INITIAL: Self = Self(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CancelEpoch, ControlDomainId, DriverId, EffectPermitId, EffectSlotId, ExecutionFiberId,
        Generation, IsolationDomainId, ProcessId, ReservationId, SemanticEventId, TaskGroupId,
    };

    #[test]
    fn nominal_ids_preserve_bytes_and_type_name() {
        let bytes = [0xabu8; 16];
        let process = ProcessId::from_bytes(bytes);
        let fiber = ExecutionFiberId::from_bytes(bytes);

        assert_eq!(process.into_bytes(), bytes);
        assert_eq!(fiber.into_bytes(), bytes);
        assert!(format!("{process:?}").starts_with("ProcessId("));
        assert!(format!("{fiber:?}").starts_with("ExecutionFiberId("));
    }

    #[test]
    fn task_write_set_binding_ids_share_the_stable_nominal_contract() {
        let bytes = [0x5au8; 16];
        let values = [
            format!("{:?}", TaskGroupId::from_bytes(bytes)),
            format!("{:?}", IsolationDomainId::from_bytes(bytes)),
            format!("{:?}", EffectSlotId::from_bytes(bytes)),
            format!("{:?}", EffectPermitId::from_bytes(bytes)),
            format!("{:?}", SemanticEventId::from_bytes(bytes)),
            format!("{:?}", ControlDomainId::from_bytes(bytes)),
            format!("{:?}", DriverId::from_bytes(bytes)),
            format!("{:?}", ReservationId::from_bytes(bytes)),
        ];

        for value in values {
            assert!(value.ends_with("5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a)"));
        }
        assert_eq!(TaskGroupId::from_bytes(bytes).into_bytes(), bytes);
        assert_eq!(IsolationDomainId::from_bytes(bytes).into_bytes(), bytes);
    }

    #[test]
    fn generation_increments_without_wrapping() {
        assert_eq!(Generation::INITIAL.get(), 1);
        assert_eq!(
            Generation::INITIAL.checked_next().map(Generation::get),
            Some(2)
        );
    }

    #[test]
    fn cancel_epoch_starts_at_zero_and_increments() {
        assert_eq!(CancelEpoch::INITIAL.get(), 0);
        assert_eq!(
            CancelEpoch::INITIAL.checked_next().map(CancelEpoch::get),
            Some(1)
        );
    }
}
