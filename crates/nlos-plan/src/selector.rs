//! Ecosystem selector resolution half of the Dependency Resolver
//! (W36-P7, ADR-0016 决定 5 second half; `[PLAN-DEPENDENCY-001]`:
//! "Package、Skill、Tool、Model、Artifact、Topic 和外部服务依赖 MUST
//! 在计划中以 typed selector 声明，并在执行前解析为带版本/generation
//! 的 handle。Dependency Resolver 不得把搜索结果或 `latest` 直接当成
//! 已授权依赖").
//!
//! Where the W29-B half resolves *plan revisions*, this half resolves
//! *ecosystem entities* (applications installed from packages, artifact
//! heads) through a pluggable [`EcosystemSelectorSource`] boundary, so
//! this crate stays decoupled from the entity authorities — the mirror
//! of the W31-F [`crate::AdmissionConsult`] posture: associated error
//! type, plain-data outcome, static dispatch, no `dyn`.
//!
//! Semantics are the G4 pinned-handle rules carried to this half:
//!
//! - a typed [`EcosystemSelector`] resolves **once**, inside one
//!   `BEGIN IMMEDIATE` transaction, into a durable immutable
//!   [`EcosystemResolutionHandle`] pinning `(generation,
//!   content_digest)`;
//! - `Current` pins the generation observed when the resolution commits;
//!   a replay of the same key after the entity advanced is answered from
//!   the original receipt (crash-retry semantics) — the receipt never
//!   floats, and a replay never re-consults the source;
//! - `At(generation)` demands one exact generation; any mismatch is the
//!   typed stale fence [`PlanStoreError::StaleEcosystemGeneration`]
//!   (fail-closed in either direction);
//! - a selector whose kind the source does not declare in
//!   [`EcosystemSelectorSource::kinds`] fails typed
//!   [`PlanStoreError::EcosystemSourceUnavailable`] — never a panic;
//! - a source failure leaves **no durable row** (the source is consulted
//!   inside the write transaction, mirroring the W31-A
//!   consult-before-commit posture).
//!
//! Readback faces fail closed: a stored row whose id does not re-derive
//! from its fields, or whose kind is outside this build's closed enum,
//! is refused typed, never silently reinterpreted. The declaration side
//! still binds ecosystem dependencies by `input_selectors_digest`; the
//! structured binding of resolution handles into node declarations is
//! the materialization-gate follow-up lane (see the lane evidence).

use nlos_types::{IdempotencyKey, ReceiptId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::PlanStoreError;
use crate::model::{
    ECOSYSTEM_RESOLUTION_ID_DOMAIN, EcosystemEntityKind, EcosystemResolutionDecision,
    EcosystemResolutionHandle, EcosystemSelector, EcosystemSourceLookup, GenerationExpectation,
    ResolveEcosystemRequest,
};
use crate::store::{SqlitePlanAuthority, decode_u64, digest16, encode_u64, fixed16, fixed32};

/// The ecosystem-entity readback boundary: one source answers generation
/// lookups for the entity kinds it declares in [`Self::kinds`]. Sources
/// live with their authorities (or as thin adapters beside them); this
/// crate defines only the contract, mirroring the W31-F
/// [`crate::AdmissionConsult`] posture.
pub trait EcosystemSelectorSource {
    /// The source's own failure type (diagnostics stay with the
    /// implementation; the resolver records only the typed fact and
    /// writes nothing durable).
    type Error;

    /// The entity kinds this source is registered to answer. A selector
    /// of any other kind fails typed
    /// [`PlanStoreError::EcosystemSourceUnavailable`] before the source
    /// is consulted.
    fn kinds(&self) -> &'static [EcosystemEntityKind];

    /// Looks up one entity's current generation-carrying state. Only
    /// invoked for a kind listed in [`Self::kinds`]; `Ok(NotFound)` is a
    /// typed miss (unknown entity id), `Err` a failed lookup posture.
    ///
    /// # Errors
    ///
    /// `Err` denotes a failed lookup (storage/transport posture), not a
    /// miss — a miss is the `Ok(EcosystemSourceLookup::NotFound)` answer.
    fn lookup(&self, selector: &EcosystemSelector) -> Result<EcosystemSourceLookup, Self::Error>;
}

/// Errors of one ecosystem resolution: plan-side typed failures
/// ([`PlanStoreError`]) or the source's own failed lookup, propagated
/// unchanged (nothing durable was written).
#[derive(Debug)]
pub enum EcosystemResolutionError<E> {
    Plan(PlanStoreError),
    Source(E),
}

impl<E> From<PlanStoreError> for EcosystemResolutionError<E> {
    fn from(error: PlanStoreError) -> Self {
        Self::Plan(error)
    }
}

impl<E: std::fmt::Debug> std::fmt::Display for EcosystemResolutionError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plan(error) => write!(formatter, "plan authority refusal: {error}"),
            Self::Source(error) => {
                write!(formatter, "ecosystem source lookup failed: {error:?}")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for EcosystemResolutionError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Plan(error) => Some(error),
            Self::Source(error) => Some(error),
        }
    }
}

impl SqlitePlanAuthority {
    /// Resolves one ecosystem selector into a durable immutable
    /// resolution receipt (the generation-carrying handle of ADR-0016
    /// 决定 5's ecosystem half). `Current` pins exactly once, at the
    /// generation the source observed when the resolution commits; a
    /// replay of the same key is answered from the original receipt, it
    /// never floats. `At(generation)` resolves only against that exact
    /// generation — any mismatch is the typed stale fence.
    ///
    /// The source is consulted inside the write transaction after the
    /// replay check and the `kinds()` gate: a failed lookup leaves no
    /// durable row and surfaces as [`EcosystemResolutionError::Source`].
    ///
    /// # Errors
    ///
    /// Fails typed on unregistered kinds
    /// ([`PlanStoreError::EcosystemSourceUnavailable`]), unknown entities
    /// ([`PlanStoreError::EcosystemEntityNotFound`]), stale generation
    /// expectations ([`PlanStoreError::StaleEcosystemGeneration`]),
    /// idempotency rebinding, zero `At` expectations, or storage failure;
    /// a failed source lookup propagates as
    /// [`EcosystemResolutionError::Source`].
    pub fn resolve_ecosystem_selector<S: EcosystemSelectorSource>(
        &self,
        source: &S,
        request: ResolveEcosystemRequest,
    ) -> Result<EcosystemResolutionDecision, EcosystemResolutionError<S::Error>> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(PlanStoreError::from)?;

        // Idempotent replay first: the durable receipt is the authority.
        // A `Current` retry after the entity advanced is the same
        // operation and is answered from the original receipt without
        // consulting the source again; rebinding the entity, the
        // observation time, or an `At` expectation to another generation
        // is a typed conflict.
        if let Some(existing) =
            load_ecosystem_resolution_by_key(&transaction, request.idempotency_key)?
        {
            if existing.kind != request.selector.kind()
                || existing.entity_id != request.selector.entity_id()
                || request.resolved_at_ms != existing.resolved_at_ms
                || matches!(
                    request.selector.expectation(),
                    GenerationExpectation::At(generation) if generation != existing.generation
                )
            {
                return Err(PlanStoreError::IdempotencyConflict.into());
            }
            transaction.commit().map_err(PlanStoreError::from)?;
            return Ok(EcosystemResolutionDecision::Replayed(existing));
        }

        if let GenerationExpectation::At(0) = request.selector.expectation() {
            return Err(PlanStoreError::InvalidRequest {
                reason: "generation expectations are dense generations starting at one",
            }
            .into());
        }
        if !source.kinds().contains(&request.selector.kind()) {
            return Err(PlanStoreError::EcosystemSourceUnavailable {
                kind: request.selector.kind(),
            }
            .into());
        }
        let state = match source
            .lookup(&request.selector)
            .map_err(EcosystemResolutionError::Source)?
        {
            EcosystemSourceLookup::Found(state) => state,
            EcosystemSourceLookup::NotFound => {
                return Err(PlanStoreError::EcosystemEntityNotFound {
                    kind: request.selector.kind(),
                    entity_id: request.selector.entity_id(),
                }
                .into());
            }
        };
        if state.generation == 0 {
            return Err(PlanStoreError::InvalidRequest {
                reason: "ecosystem source returned a zero generation",
            }
            .into());
        }
        let generation = match request.selector.expectation() {
            GenerationExpectation::Current => state.generation,
            GenerationExpectation::At(expected) if expected == state.generation => state.generation,
            GenerationExpectation::At(expected) => {
                return Err(PlanStoreError::StaleEcosystemGeneration {
                    kind: request.selector.kind(),
                    entity_id: request.selector.entity_id(),
                    expected,
                    current: state.generation,
                }
                .into());
            }
        };
        let handle = EcosystemResolutionHandle {
            resolution_id: derive_ecosystem_resolution_id(
                request.idempotency_key,
                request.selector.kind(),
                request.selector.entity_id(),
                generation,
            ),
            kind: request.selector.kind(),
            entity_id: request.selector.entity_id(),
            generation,
            content_digest: state.content_digest,
            idempotency_key: request.idempotency_key,
            resolved_at_ms: request.resolved_at_ms,
        };
        insert_ecosystem_resolution(&transaction, &handle)?;
        transaction.commit().map_err(PlanStoreError::from)?;
        Ok(EcosystemResolutionDecision::Resolved(handle))
    }

    /// Reads one durable ecosystem resolution receipt, `None` when
    /// absent.
    ///
    /// # Errors
    ///
    /// Fails on storage failure, a corrupt row, or a stored kind outside
    /// this build's closed enum
    /// ([`PlanStoreError::EcosystemKindUnknown`]).
    pub fn inspect_ecosystem_resolution(
        &self,
        resolution_id: ReceiptId,
    ) -> Result<Option<EcosystemResolutionHandle>, PlanStoreError> {
        let connection = self.lock()?;
        load_ecosystem_resolution_by_id(&connection, resolution_id)
    }

    /// Re-checks one durable receipt against the source's *current*
    /// generation — the explicit freshness fence of this half (the G4
    /// "typed stale/fence error" face; there is no downstream CAS to
    /// piggyback on here, unlike the plan-revision half's
    /// `StaleNodeRevision`). Returns the still-pinned handle when the
    /// entity is still at the receipt's generation; a mismatch (either
    /// direction) or a since-disappeared entity fails typed.
    ///
    /// # Errors
    ///
    /// Fails typed when the receipt does not exist
    /// ([`PlanStoreError::EcosystemResolutionNotFound`]), the kind is not
    /// registered with this source
    /// ([`PlanStoreError::EcosystemSourceUnavailable`]), the entity no
    /// longer exists ([`PlanStoreError::EcosystemEntityNotFound`]), or
    /// the generation moved
    /// ([`PlanStoreError::StaleEcosystemGeneration`]); a failed source
    /// lookup propagates as [`EcosystemResolutionError::Source`].
    pub fn verify_ecosystem_resolution_current<S: EcosystemSelectorSource>(
        &self,
        source: &S,
        resolution_id: ReceiptId,
    ) -> Result<EcosystemResolutionHandle, EcosystemResolutionError<S::Error>> {
        let connection = self.lock()?;
        let handle = load_ecosystem_resolution_by_id(&connection, resolution_id)?
            .ok_or(PlanStoreError::EcosystemResolutionNotFound(resolution_id))?;
        drop(connection);
        let selector = current_selector_of(&handle);
        if !source.kinds().contains(&handle.kind) {
            return Err(PlanStoreError::EcosystemSourceUnavailable { kind: handle.kind }.into());
        }
        let state = match source
            .lookup(&selector)
            .map_err(EcosystemResolutionError::Source)?
        {
            EcosystemSourceLookup::Found(state) => state,
            EcosystemSourceLookup::NotFound => {
                return Err(PlanStoreError::EcosystemEntityNotFound {
                    kind: handle.kind,
                    entity_id: handle.entity_id,
                }
                .into());
            }
        };
        if state.generation != handle.generation {
            return Err(PlanStoreError::StaleEcosystemGeneration {
                kind: handle.kind,
                entity_id: handle.entity_id,
                expected: handle.generation,
                current: state.generation,
            }
            .into());
        }
        Ok(handle)
    }
}

/// Rebuilds the `Current` selector a pinned handle addresses (the verify
/// face consults the entity's live generation, never a stored one).
fn current_selector_of(handle: &EcosystemResolutionHandle) -> EcosystemSelector {
    match handle.kind {
        EcosystemEntityKind::Application => EcosystemSelector::Application {
            package_id: nlos_types::PackageId::from_bytes(handle.entity_id),
            expectation: GenerationExpectation::Current,
        },
        EcosystemEntityKind::Artifact => EcosystemSelector::Artifact {
            artifact_id: nlos_types::ArtifactId::from_bytes(handle.entity_id),
            expectation: GenerationExpectation::Current,
        },
    }
}

fn derive_ecosystem_resolution_id(
    key: IdempotencyKey,
    kind: EcosystemEntityKind,
    entity_id: [u8; 16],
    generation: u64,
) -> ReceiptId {
    ReceiptId::from_bytes(digest16(
        ECOSYSTEM_RESOLUTION_ID_DOMAIN,
        &[
            key.as_bytes(),
            &[kind.discriminant()],
            &entity_id,
            &generation.to_be_bytes(),
        ],
    ))
}

// ---------------------------------------------------------------------------
// durable receipt rows
// ---------------------------------------------------------------------------

const ECOSYSTEM_RESOLUTION_COLUMNS: &str = "resolution_id, idempotency_key, entity_kind,
        entity_id, generation, content_digest, resolved_at_ms";

type EcosystemResolutionRow = (Vec<u8>, Vec<u8>, i64, Vec<u8>, i64, Vec<u8>, i64);

fn raw_ecosystem_resolution_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<EcosystemResolutionRow> {
    Ok((
        row.get::<_, Vec<u8>>(0)?,
        row.get::<_, Vec<u8>>(1)?,
        row.get::<_, i64>(2)?,
        row.get::<_, Vec<u8>>(3)?,
        row.get::<_, i64>(4)?,
        row.get::<_, Vec<u8>>(5)?,
        row.get::<_, i64>(6)?,
    ))
}

fn decode_ecosystem_resolution_row(
    row: EcosystemResolutionRow,
) -> Result<EcosystemResolutionHandle, PlanStoreError> {
    let handle = EcosystemResolutionHandle {
        resolution_id: ReceiptId::from_bytes(fixed16(row.0, "ecosystem resolution id")?),
        idempotency_key: IdempotencyKey::from_bytes(fixed16(row.1, "ecosystem resolution key")?),
        kind: EcosystemEntityKind::decode(row.2)?,
        entity_id: fixed16(row.3, "ecosystem resolution entity id")?,
        generation: decode_u64(row.4)?,
        content_digest: fixed32(row.5, "ecosystem resolution content digest")?,
        resolved_at_ms: decode_u64(row.6)?,
    };
    if derive_ecosystem_resolution_id(
        handle.idempotency_key,
        handle.kind,
        handle.entity_id,
        handle.generation,
    ) != handle.resolution_id
    {
        return Err(PlanStoreError::CorruptRecord(
            "ecosystem resolution id does not re-derive from its row",
        ));
    }
    Ok(handle)
}

fn load_ecosystem_resolution_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<EcosystemResolutionHandle>, PlanStoreError> {
    connection
        .query_row(
            &format!(
                "SELECT {ECOSYSTEM_RESOLUTION_COLUMNS} FROM ecosystem_resolution_receipts
                 WHERE idempotency_key = ?1"
            ),
            [key.as_bytes().as_slice()],
            raw_ecosystem_resolution_row,
        )
        .optional()?
        .map(decode_ecosystem_resolution_row)
        .transpose()
}

fn load_ecosystem_resolution_by_id(
    connection: &Connection,
    resolution_id: ReceiptId,
) -> Result<Option<EcosystemResolutionHandle>, PlanStoreError> {
    connection
        .query_row(
            &format!(
                "SELECT {ECOSYSTEM_RESOLUTION_COLUMNS} FROM ecosystem_resolution_receipts
                 WHERE resolution_id = ?1"
            ),
            [resolution_id.as_bytes().as_slice()],
            raw_ecosystem_resolution_row,
        )
        .optional()?
        .map(decode_ecosystem_resolution_row)
        .transpose()
}

fn insert_ecosystem_resolution(
    transaction: &rusqlite::Transaction<'_>,
    handle: &EcosystemResolutionHandle,
) -> Result<(), PlanStoreError> {
    transaction.execute(
        "INSERT INTO ecosystem_resolution_receipts (
            resolution_id, idempotency_key, entity_kind, entity_id,
            generation, content_digest, resolved_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            handle.resolution_id.as_bytes().as_slice(),
            handle.idempotency_key.as_bytes().as_slice(),
            handle.kind.encode(),
            handle.entity_id.as_slice(),
            encode_u64(handle.generation)?,
            handle.content_digest.as_slice(),
            encode_u64(handle.resolved_at_ms)?,
        ],
    )?;
    Ok(())
}
