//! `DRV-LIFE-001` four-class `EffectDescriptor` declarations and the
//! crash-fenced pending-call ledger with same-`IdempotencyKey` `UNCERTAIN`
//! reconciliation for the W80-E6B third-party driver samples.
//!
//! Mirrors the `nlos-certify` Gate 1 runtime contract as an executable
//! userland adapter surface:
//! - every mutating effect (`REVERSIBLE` / `COMPENSATABLE` / `IRREVERSIBLE`)
//!   must declare `reconcile = same-idempotency-key`;
//! - a driver crash fences every in-flight call to `EffectUnknown` (`UNCERTAIN`)
//!   until an explicit reconcile pass resolves it against provider evidence;
//! - a retry under the *same* 128-bit `IdempotencyKey` replays the settled
//!   outcome instead of dispatching a second side effect
//!   (`KABI-UNCERTAIN-001`), and a settled key can never be silently re-used
//!   for a different operation identity.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// Four-class effect category (`ADR-0027` Gate 1 `EffectDescriptor` taxonomy).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum EffectClass {
    /// Pure read; no observable external mutation.
    ReadOnly,
    /// Mutating but reversible by construction.
    Reversible,
    /// Mutating with a declared compensation handler.
    Compensatable,
    /// Mutating and irreversible.
    Irreversible,
}

impl EffectClass {
    /// Parses the manifest token form used by driver effect declaration files.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token.trim().to_ascii_uppercase().as_str() {
            "READ_ONLY" | "READ-ONLY" => Some(Self::ReadOnly),
            "REVERSIBLE" => Some(Self::Reversible),
            "COMPENSATABLE" => Some(Self::Compensatable),
            "IRREVERSIBLE" => Some(Self::Irreversible),
            _ => None,
        }
    }

    /// Canonical manifest token.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::ReadOnly => "READ_ONLY",
            Self::Reversible => "REVERSIBLE",
            Self::Compensatable => "COMPENSATABLE",
            Self::Irreversible => "IRREVERSIBLE",
        }
    }

    /// Whether this class mutates external state and therefore mandates
    /// same-idempotency-key reconciliation.
    #[must_use]
    pub const fn is_mutating(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }
}

impl fmt::Display for EffectClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_token())
    }
}

/// One declared `EffectDescriptor` for a driver-exposed operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectDescriptor {
    /// Stable package-local effect identifier (e.g. a tool or method name).
    pub effect_id: String,
    /// Human-readable purpose line surfaced in `Z4` transparency views.
    pub summary: String,
    /// Declared four-class category.
    pub class: EffectClass,
    /// Declared compensation handler id (mandatory when `Compensatable`).
    pub compensation_handler: Option<String>,
}

impl EffectDescriptor {
    /// Validates the Gate 1 invariants for this declaration.
    ///
    /// # Errors
    /// Returns [`EffectsError::InvalidDeclaration`] when a `Compensatable`
    /// effect lacks a compensation handler or the effect id is malformed.
    pub fn validate(&self) -> Result<(), EffectsError> {
        if self.effect_id.is_empty() || self.effect_id.len() > 128 {
            return Err(EffectsError::InvalidDeclaration {
                effect_id: self.effect_id.clone(),
                reason: "effect_id must be 1..=128 bytes".to_owned(),
            });
        }
        if self.class == EffectClass::Compensatable {
            let handler = self.compensation_handler.as_deref().unwrap_or("");
            if handler.is_empty() {
                return Err(EffectsError::InvalidDeclaration {
                    effect_id: self.effect_id.clone(),
                    reason: "COMPENSATABLE effect requires a compensation handler".to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// Line-based effect declaration manifest parser for package data payloads.
///
/// Grammar (one declaration per line, `#` comments and blank lines skipped):
/// `effect = <effect-id> class=<TOKEN> reconcile=same-idempotency-key summary="<text>" [compensation=<id>]`
///
/// Mutating classes (`REVERSIBLE` / `COMPENSATABLE` / `IRREVERSIBLE`) must
/// declare `reconcile=same-idempotency-key`; any other reconcile directive is
/// rejected, mirroring `nlos-certify` Gate 1 (`KABI-UNCERTAIN-001`).
///
/// # Errors
/// Returns [`EffectsError::ManifestSyntax`] on malformed lines and
/// [`EffectsError::InvalidDeclaration`] /
/// [`EffectsError::MutatingEffectWithoutReconcile`] when a parsed declaration
/// violates the Gate 1 invariants.
pub fn parse_effect_declarations(text: &str) -> Result<Vec<EffectDescriptor>, EffectsError> {
    let mut out = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("effect = ") else {
            return Err(EffectsError::ManifestSyntax {
                line: idx + 1,
                reason: "expected 'effect = <id> class=<TOKEN> ...'".to_owned(),
            });
        };
        let effect_id = rest.split_whitespace().next().unwrap_or_default();
        let class = extract_attr(rest, "class")
            .and_then(|token| EffectClass::parse(&token))
            .ok_or_else(|| EffectsError::ManifestSyntax {
                line: idx + 1,
                reason: format!("unknown or missing class token in {line:?}"),
            })?;
        match extract_attr(rest, "reconcile").as_deref() {
            Some("same-idempotency-key") => {}
            Some(other) => {
                return Err(EffectsError::ManifestSyntax {
                    line: idx + 1,
                    reason: format!("forbidden reconcile directive {other:?}"),
                });
            }
            None if class.is_mutating() => {
                return Err(EffectsError::MutatingEffectWithoutReconcile {
                    effect_id: effect_id.to_owned(),
                });
            }
            None => {}
        }
        let summary = extract_attr(rest, "summary").unwrap_or_default();
        let compensation_handler = extract_attr(rest, "compensation");
        let descriptor = EffectDescriptor {
            effect_id: effect_id.to_owned(),
            summary,
            class,
            compensation_handler,
        };
        descriptor
            .validate()
            .map_err(|error| EffectsError::ManifestSyntax {
                line: idx + 1,
                reason: error.to_string(),
            })?;
        out.push(descriptor);
    }
    Ok(out)
}

fn extract_attr(rest: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    let start = rest.find(&prefix)?;
    let tail = &rest[start + prefix.len()..];
    let trimmed = tail.trim_start();
    if let Some(stripped) = trimmed.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_owned())
    } else {
        Some(
            trimmed
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned(),
        )
    }
}

/// Provider evidence outcome fed into a reconcile pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderProbe {
    /// The provider confirms the effect landed; `receipt` is its opaque proof.
    EffectLanded { receipt: u64 },
    /// The provider confirms the effect never started.
    EffectAbsent,
}

/// Terminal outcome recorded for one fenced call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettledOutcome {
    /// Operation identity the caller dispatched under.
    pub operation_id: [u8; 16],
    /// Effect descriptor the call was bound to.
    pub descriptor: EffectDescriptor,
    /// Provider evidence the settlement was based on.
    pub probe: ProviderProbe,
}

/// Lifecycle state of one fenced call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PendingCallState {
    /// Dispatched to the provider; not yet observable.
    Dispatched,
    /// Driver crashed mid-flight; outcome unknown until reconciled
    /// (`DRV-LIFE-001` default-`UNCERTAIN` rule).
    EffectUnknown,
    /// Terminally settled (by reply or by reconcile).
    Settled(SettledOutcome),
}

/// One entry in the pending-call fence ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FencedCall {
    /// 128-bit idempotency key chosen by the caller.
    pub idempotency_key: [u8; 16],
    /// Fenced lifecycle state.
    pub state: PendingCallState,
}

/// Fail-closed fence errors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectsError {
    /// A declaration violated the Gate 1 invariants.
    InvalidDeclaration {
        /// Offending effect id.
        effect_id: String,
        /// Human-readable reason.
        reason: String,
    },
    /// A declaration file line was malformed.
    ManifestSyntax {
        /// 1-based line number.
        line: usize,
        /// Human-readable reason.
        reason: String,
    },
    /// An idempotency key was reused for a different operation identity.
    IdempotencyKeyConflict {
        /// The reused key.
        idempotency_key: [u8; 16],
    },
    /// A mutating effect lacks the mandatory same-key reconcile policy.
    MutatingEffectWithoutReconcile {
        /// Offending effect id.
        effect_id: String,
    },
    /// A call is still `UNCERTAIN` and must be reconciled first.
    UncertainPendingReconcile {
        /// The fenced key.
        idempotency_key: [u8; 16],
    },
    /// Reconcile was invoked on a call that is not `UNCERTAIN`.
    ReconcileNotUncertain {
        /// The fenced key.
        idempotency_key: [u8; 16],
    },
    /// The referenced key is unknown to the fence.
    UnknownCall {
        /// The missing key.
        idempotency_key: [u8; 16],
    },
}

impl fmt::Display for EffectsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDeclaration { effect_id, reason } => {
                write!(f, "invalid effect declaration for {effect_id:?}: {reason}")
            }
            Self::ManifestSyntax { line, reason } => {
                write!(f, "effect manifest line {line}: {reason}")
            }
            Self::IdempotencyKeyConflict { idempotency_key } => write!(
                f,
                "idempotency key {} reused for a different operation",
                hex16(idempotency_key)
            ),
            Self::MutatingEffectWithoutReconcile { effect_id } => write!(
                f,
                "mutating effect {effect_id:?} must reconcile with the same idempotency key"
            ),
            Self::UncertainPendingReconcile { idempotency_key } => write!(
                f,
                "call {} is UNCERTAIN and must be reconciled before retry",
                hex16(idempotency_key)
            ),
            Self::ReconcileNotUncertain { idempotency_key } => write!(
                f,
                "call {} is not UNCERTAIN; nothing to reconcile",
                hex16(idempotency_key)
            ),
            Self::UnknownCall { idempotency_key } => {
                write!(f, "unknown fenced call {}", hex16(idempotency_key))
            }
        }
    }
}

impl Error for EffectsError {}

/// Admission answer for [`PendingCallFence::open`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallAdmission {
    /// No prior call under this key: dispatch the provider effect now.
    Fresh,
    /// A settled call with the identical operation identity exists: replay
    /// the recorded outcome instead of dispatching again.
    Replay(SettledOutcome),
}

/// Crash-fenced pending-call ledger: dispatch, crash-to-`UNCERTAIN`, and
/// same-key reconcile/replay for one driver adapter session.
#[derive(Clone, Debug, Default)]
pub struct PendingCallFence {
    calls: BTreeMap<[u8; 16], FencedCall>,
}

impl PendingCallFence {
    /// Creates an empty fence.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            calls: BTreeMap::new(),
        }
    }

    /// Number of calls currently fenced in any state.
    #[must_use]
    pub fn len(&self) -> usize {
        self.calls.len()
    }

    /// Whether the fence holds no calls.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Inspects one fenced call by key.
    #[must_use]
    pub fn inspect(&self, idempotency_key: &[u8; 16]) -> Option<&FencedCall> {
        self.calls.get(idempotency_key)
    }

    /// Lists every fenced call in key order.
    #[must_use]
    pub fn list(&self) -> Vec<FencedCall> {
        self.calls.values().cloned().collect()
    }

    /// Counts calls currently in the `EffectUnknown` (`UNCERTAIN`) state.
    #[must_use]
    pub fn uncertain_count(&self) -> usize {
        self.calls
            .values()
            .filter(|call| matches!(call.state, PendingCallState::EffectUnknown))
            .count()
    }

    /// Opens one fenced call under `idempotency_key`.
    ///
    /// # Errors
    /// Fails closed with [`EffectsError::IdempotencyKeyConflict`] when the key
    /// was already used for a different operation identity, and with
    /// [`EffectsError::UncertainPendingReconcile`] when the previous call under
    /// this key is still in flight or `UNCERTAIN` (a blind same-key retry is
    /// forbidden until it is reconciled).
    pub fn open(
        &mut self,
        idempotency_key: &[u8; 16],
        operation_id: &[u8; 16],
        descriptor: &EffectDescriptor,
    ) -> Result<CallAdmission, EffectsError> {
        descriptor.validate()?;
        if let Some(existing) = self.calls.get(idempotency_key) {
            return match &existing.state {
                PendingCallState::Settled(settled) => {
                    if &settled.operation_id == operation_id {
                        Ok(CallAdmission::Replay(settled.clone()))
                    } else {
                        Err(EffectsError::IdempotencyKeyConflict {
                            idempotency_key: *idempotency_key,
                        })
                    }
                }
                PendingCallState::EffectUnknown | PendingCallState::Dispatched => {
                    Err(EffectsError::UncertainPendingReconcile {
                        idempotency_key: *idempotency_key,
                    })
                }
            };
        }
        self.calls.insert(
            *idempotency_key,
            FencedCall {
                idempotency_key: *idempotency_key,
                state: PendingCallState::Dispatched,
            },
        );
        Ok(CallAdmission::Fresh)
    }

    /// Marks a dispatched call as completed with provider evidence.
    ///
    /// # Errors
    /// Fails with [`EffectsError::UnknownCall`] for an unfenced key and with
    /// [`EffectsError::ReconcileNotUncertain`] semantics (settlement of a
    /// settled call is an idempotent no-op when the operation identity
    /// matches, otherwise a conflict).
    pub fn settle(
        &mut self,
        idempotency_key: &[u8; 16],
        operation_id: &[u8; 16],
        descriptor: &EffectDescriptor,
        probe: ProviderProbe,
    ) -> Result<SettledOutcome, EffectsError> {
        let Some(call) = self.calls.get_mut(idempotency_key) else {
            return Err(EffectsError::UnknownCall {
                idempotency_key: *idempotency_key,
            });
        };
        match &call.state {
            PendingCallState::Settled(settled) => {
                if &settled.operation_id == operation_id {
                    Ok(settled.clone())
                } else {
                    Err(EffectsError::IdempotencyKeyConflict {
                        idempotency_key: *idempotency_key,
                    })
                }
            }
            PendingCallState::Dispatched | PendingCallState::EffectUnknown => {
                let outcome = SettledOutcome {
                    operation_id: *operation_id,
                    descriptor: descriptor.clone(),
                    probe,
                };
                call.state = PendingCallState::Settled(outcome.clone());
                Ok(outcome)
            }
        }
    }

    /// Simulates a driver crash: every in-flight (`Dispatched`) call is fenced
    /// to `EffectUnknown` (`UNCERTAIN`) until an explicit reconcile pass.
    pub fn crash(&mut self) {
        for call in self.calls.values_mut() {
            if matches!(call.state, PendingCallState::Dispatched) {
                call.state = PendingCallState::EffectUnknown;
            }
        }
    }

    /// Reconciles one `UNCERTAIN` call against provider evidence under the
    /// *same* 128-bit idempotency key. This is the only path out of
    /// `EffectUnknown`; it never re-dispatches the effect.
    ///
    /// # Errors
    /// Fails with [`EffectsError::UnknownCall`] or
    /// [`EffectsError::ReconcileNotUncertain`] for non-`UNCERTAIN` targets.
    pub fn reconcile(
        &mut self,
        idempotency_key: &[u8; 16],
        operation_id: &[u8; 16],
        descriptor: &EffectDescriptor,
        probe: ProviderProbe,
    ) -> Result<SettledOutcome, EffectsError> {
        let Some(call) = self.calls.get_mut(idempotency_key) else {
            return Err(EffectsError::UnknownCall {
                idempotency_key: *idempotency_key,
            });
        };
        if !matches!(call.state, PendingCallState::EffectUnknown) {
            return Err(EffectsError::ReconcileNotUncertain {
                idempotency_key: *idempotency_key,
            });
        }
        let outcome = SettledOutcome {
            operation_id: *operation_id,
            descriptor: descriptor.clone(),
            probe,
        };
        call.state = PendingCallState::Settled(outcome.clone());
        Ok(outcome)
    }

    /// Resolves the replay answer for a settled key: the recorded outcome is
    /// handed back instead of a second dispatch.
    #[must_use]
    pub fn replay_of(&self, idempotency_key: &[u8; 16]) -> Option<&SettledOutcome> {
        match self.inspect(idempotency_key).map(|call| &call.state) {
            Some(PendingCallState::Settled(outcome)) => Some(outcome),
            _ => None,
        }
    }
}

/// Lowercase hex rendering of a 128-bit key for diagnostics.
#[must_use]
pub fn hex16(bytes: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        EffectClass, EffectDescriptor, EffectsError, PendingCallFence, PendingCallState,
        ProviderProbe, parse_effect_declarations,
    };

    const KEY_A: [u8; 16] = [0x11; 16];
    const KEY_B: [u8; 16] = [0x22; 16];
    const OP_A: [u8; 16] = [0xA1; 16];
    const OP_A2: [u8; 16] = [0xA2; 16];

    fn descriptor(class: EffectClass) -> EffectDescriptor {
        EffectDescriptor {
            effect_id: "append-field-note".to_owned(),
            summary: "append one note".to_owned(),
            class,
            compensation_handler: (class == EffectClass::Compensatable)
                .then(|| "rollback-batch-sync".to_owned()),
        }
    }

    #[test]
    fn effect_classes_parse_and_flag_mutation() {
        assert_eq!(EffectClass::parse("read_only"), Some(EffectClass::ReadOnly));
        assert_eq!(
            EffectClass::parse("Reversible"),
            Some(EffectClass::Reversible)
        );
        assert_eq!(
            EffectClass::parse("COMPENSATABLE"),
            Some(EffectClass::Compensatable)
        );
        assert_eq!(
            EffectClass::parse("irreversible"),
            Some(EffectClass::Irreversible)
        );
        assert_eq!(EffectClass::parse("unknown"), None);
        assert!(!EffectClass::ReadOnly.is_mutating());
        assert!(EffectClass::Irreversible.is_mutating());
    }

    #[test]
    fn compensatable_without_handler_is_rejected() {
        let bad = EffectDescriptor {
            effect_id: "stage-batch-sync".to_owned(),
            summary: String::new(),
            class: EffectClass::Compensatable,
            compensation_handler: None,
        };
        assert!(bad.validate().is_err());
        assert!(descriptor(EffectClass::Compensatable).validate().is_ok());
    }

    #[test]
    fn manifest_declarations_parse_and_fail_closed() {
        let text = "# comment\n\
             effect = search-knowledge-base class=READ_ONLY reconcile=same-idempotency-key summary=\"Query notes\"\n\
             effect = stage-batch-sync class=COMPENSATABLE reconcile=same-idempotency-key summary=\"Stage sync\" compensation=rollback-batch-sync\n\
             effect = purge-note-cache class=IRREVERSIBLE reconcile=same-idempotency-key summary=\"Purge cache\"\n";
        let parsed = parse_effect_declarations(text).expect("parse declarations");
        assert_eq!(parsed.len(), 3);
        assert_eq!(
            parsed[1].compensation_handler.as_deref(),
            Some("rollback-batch-sync")
        );

        assert!(parse_effect_declarations("effect = x class=MYSTERY summary=\"?\"").is_err());
        // COMPENSATABLE without compensation fails Gate 1 validation.
        assert!(
            parse_effect_declarations(
                "effect = x class=COMPENSATABLE reconcile=same-idempotency-key summary=\"?\""
            )
            .is_err()
        );
        // Mutating class without a reconcile directive fails closed.
        assert!(matches!(
            parse_effect_declarations("effect = x class=IRREVERSIBLE summary=\"?\""),
            Err(EffectsError::MutatingEffectWithoutReconcile { .. })
        ));
        // Forbidden reconcile directives fail closed.
        assert!(matches!(
            parse_effect_declarations(
                "effect = x class=REVERSIBLE reconcile=fresh-key-blind-retry summary=\"?\""
            ),
            Err(EffectsError::ManifestSyntax { .. })
        ));
        assert!(parse_effect_declarations("nonsense line").is_err());
    }

    #[test]
    fn crash_fences_dispatched_calls_to_uncertain_until_reconciled() {
        let mut fence = PendingCallFence::new();
        fence
            .open(&KEY_A, &OP_A, &descriptor(EffectClass::Reversible))
            .expect("open");
        assert_eq!(fence.uncertain_count(), 0);

        // Same-key retry while in flight is refused (no blind double dispatch).
        assert!(matches!(
            fence.open(&KEY_A, &OP_A, &descriptor(EffectClass::Reversible)),
            Err(EffectsError::UncertainPendingReconcile { .. })
        ));

        fence.crash();
        assert_eq!(fence.uncertain_count(), 1);
        assert!(matches!(
            fence.inspect(&KEY_A).map(|call| &call.state),
            Some(PendingCallState::EffectUnknown)
        ));

        // Reconcile with provider evidence under the SAME key settles it.
        let settled = fence
            .reconcile(
                &KEY_A,
                &OP_A,
                &descriptor(EffectClass::Reversible),
                ProviderProbe::EffectLanded { receipt: 7 },
            )
            .expect("reconcile");
        assert_eq!(settled.probe, ProviderProbe::EffectLanded { receipt: 7 });
        assert_eq!(fence.uncertain_count(), 0);

        // Reconciling an already-settled call is refused.
        assert!(matches!(
            fence.reconcile(
                &KEY_A,
                &OP_A,
                &descriptor(EffectClass::Reversible),
                ProviderProbe::EffectAbsent
            ),
            Err(EffectsError::ReconcileNotUncertain { .. })
        ));

        // Retry under the same key replays the settled outcome.
        fence
            .open(&KEY_A, &OP_A, &descriptor(EffectClass::Reversible))
            .expect("same-identity reopen is a replay slot");
        assert!(fence.replay_of(&KEY_A).is_some());
    }

    #[test]
    fn settled_key_conflicts_with_different_operation() {
        let mut fence = PendingCallFence::new();
        fence
            .open(&KEY_B, &OP_A, &descriptor(EffectClass::ReadOnly))
            .expect("open");
        fence
            .settle(
                &KEY_B,
                &OP_A,
                &descriptor(EffectClass::ReadOnly),
                ProviderProbe::EffectAbsent,
            )
            .expect("settle");
        assert!(matches!(
            fence.open(&KEY_B, &OP_A2, &descriptor(EffectClass::ReadOnly)),
            Err(EffectsError::IdempotencyKeyConflict { .. })
        ));
        assert!(matches!(
            fence.reconcile(
                &KEY_B,
                &OP_A,
                &descriptor(EffectClass::ReadOnly),
                ProviderProbe::EffectAbsent
            ),
            Err(EffectsError::ReconcileNotUncertain { .. })
        ));
        assert!(matches!(
            fence.reconcile(
                &[0x99; 16],
                &OP_A,
                &descriptor(EffectClass::ReadOnly),
                ProviderProbe::EffectAbsent
            ),
            Err(EffectsError::UnknownCall { .. })
        ));
    }
}
