//! Real [`SearchInspector`] adapter backed by [`nlos_search::SearchService`]
//! and [`nlos_semantic::SemanticAuthority`] (`C-UNFREEZE-001`,
//! `[CONS-DECL-001]`, `[CONS-CTRL-001]`, `[PERF-CONTROL-001]`).
//!
//! Queries run strictly over [`SearchService::search_assertions_paginated`]
//! with a bounded page window ([`INSPECT_SEARCH_PAGE_LIMIT`]), verify the
//! non-authoritative `[CONS-CTRL-001]` restriction envelope and `[CONS-DECL-001]`
//! consistency declaration without downgrade, and cross-check every returned
//! event's admission receipt against the live [`SemanticAuthority`] handle.

use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_search::{
    AssertionSelector, CapabilityTarget, ControlConsumerKind, PaginatedAssertionSelector,
    RetractionFilter, SearchAchievedConsistency, SearchError, SearchRequestedConsistency,
    SearchService, SearchUseRestrictions,
};
use nlos_semantic::{SemanticAuthority, SemanticAuthorityError};
use nlos_types::{NamespaceId, TaskId};

use crate::control::{SCOPE_ID_BYTES, SearchInspection, SearchInspector};

/// Bounded per-inspection assertion page limit (`[PERF-CONTROL-001]`).
pub const INSPECT_SEARCH_PAGE_LIMIT: usize = 64;

/// Unscoped 32-byte search selector inspecting all admitted assertions up to
/// [`INSPECT_SEARCH_PAGE_LIMIT`].
pub const UNSCOPED_SEARCH_SCOPE_ID: [u8; SCOPE_ID_BYTES] = [0; SCOPE_ID_BYTES];

/// Scope-selector tag byte for a `CapabilityTarget::Namespace` query.
pub const SCOPE_TAG_NAMESPACE: u8 = 1;

/// Scope-selector tag byte for a `CapabilityTarget::Task` query.
pub const SCOPE_TAG_TASK: u8 = 2;

/// Canonical `[CONS-CTRL-001]` bitmask when all five control-plane authority
/// gates are prohibited (`SearchUseRestrictions::NON_AUTHORITATIVE`).
pub const NON_AUTHORITATIVE_RESTRICTIONS_ALL_BITS: u8 = 0x1F;

/// Encodes a [`NamespaceId`] into the canonical 32-byte `InspectSearch`
/// `scope_id` (`[SCOPE_TAG_NAMESPACE, 0..0 (15 bytes), namespace_id (16 bytes)]`).
#[must_use]
pub fn encode_namespace_scope_id(namespace_id: NamespaceId) -> [u8; SCOPE_ID_BYTES] {
    let mut out = [0u8; SCOPE_ID_BYTES];
    out[0] = SCOPE_TAG_NAMESPACE;
    out[16..32].copy_from_slice(&namespace_id.into_bytes());
    out
}

/// Encodes a [`TaskId`] into the canonical 32-byte `InspectSearch` `scope_id`
/// (`[SCOPE_TAG_TASK, 0..0 (15 bytes), task_id (16 bytes)]`).
#[must_use]
pub fn encode_task_scope_id(task_id: TaskId) -> [u8; SCOPE_ID_BYTES] {
    let mut out = [0u8; SCOPE_ID_BYTES];
    out[0] = SCOPE_TAG_TASK;
    out[16..32].copy_from_slice(task_id.as_bytes());
    out
}

/// Decodes a 32-byte `InspectSearch` `scope_id` fail-closed into an optional
/// [`CapabilityTarget`].
///
/// # Errors
///
/// Returns an `INVALID_ARGUMENT` [`SabiFailure`] when `scope_id` uses an
/// unknown tag byte or non-zero reserved padding bytes (`scope_id[1..16]`).
pub fn decode_inspect_scope_id(
    scope_id: [u8; SCOPE_ID_BYTES],
) -> Result<Option<CapabilityTarget>, SabiFailure> {
    if scope_id == UNSCOPED_SEARCH_SCOPE_ID {
        return Ok(None);
    }
    if scope_id[1..16] != [0u8; 15] {
        return Err(invalid_argument_failure(
            "search scope_id reserved prefix bytes must be zero",
        ));
    }
    let mut raw_id = [0u8; 16];
    raw_id.copy_from_slice(&scope_id[16..32]);
    match scope_id[0] {
        SCOPE_TAG_NAMESPACE => Ok(Some(CapabilityTarget::Namespace(NamespaceId::from_bytes(
            raw_id,
        )))),
        SCOPE_TAG_TASK => Ok(Some(CapabilityTarget::Task(TaskId::from_bytes(raw_id)))),
        _ => Err(invalid_argument_failure(
            "search scope_id carries an unsupported scope tag",
        )),
    }
}

/// Encodes [`SearchAchievedConsistency`] into the bounded `u8` projection code
/// carried by [`SearchInspection::consistency_class`].
#[must_use]
pub const fn encode_achieved_consistency(achieved: SearchAchievedConsistency) -> u8 {
    match achieved {
        SearchAchievedConsistency::EventualLiveScan => 1,
        SearchAchievedConsistency::DerivedInMemoryIndex => 2,
        SearchAchievedConsistency::CheckpointBoundedSnapshot => 3,
        SearchAchievedConsistency::FederatedPartialEventual => 4,
    }
}

/// Encodes [`SearchUseRestrictions`] into the 5-bit `[CONS-CTRL-001]` bitmask
/// carried by [`SearchInspection::non_authoritative_restrictions_bits`].
#[must_use]
pub const fn encode_restrictions_bits(restrictions: SearchUseRestrictions) -> u8 {
    let mut bits = 0u8;
    if restrictions.forbid_capability_decision {
        bits |= 1 << 0;
    }
    if restrictions.forbid_resource_ledger_commit {
        bits |= 1 << 1;
    }
    if restrictions.forbid_task_head_commit {
        bits |= 1 << 2;
    }
    if restrictions.forbid_package_update {
        bits |= 1 << 3;
    }
    if restrictions.forbid_irreversible_action {
        bits |= 1 << 4;
    }
    bits
}

/// Read-only [`SearchInspector`] over a live [`SearchService`] and its backing
/// [`SemanticAuthority`].
pub struct SearchServiceInspector<'a> {
    search: &'a SearchService,
    authority: &'a SemanticAuthority,
}

impl<'a> SearchServiceInspector<'a> {
    /// Binds one inspector to `search` and its authoritative `authority`.
    #[must_use]
    pub const fn new(search: &'a SearchService, authority: &'a SemanticAuthority) -> Self {
        Self { search, authority }
    }
}

impl SearchInspector for SearchServiceInspector<'_> {
    fn inspect_search(
        &self,
        scope_id: [u8; SCOPE_ID_BYTES],
    ) -> Result<SearchInspection, SabiFailure> {
        let scope = decode_inspect_scope_id(scope_id)?;
        let mut selector = AssertionSelector::new(INSPECT_SEARCH_PAGE_LIMIT);
        selector.scope = scope;
        selector.retraction = RetractionFilter::Any;

        let page = self
            .search
            .search_assertions_paginated(&PaginatedAssertionSelector::new(selector))
            .map_err(|error| map_search_error(&error))?;

        page.enforce_not_control_authoritative(ControlConsumerKind::CandidateDiscovery)
            .map_err(|error| map_search_error(&error))?;
        if page.use_restrictions != SearchUseRestrictions::NON_AUTHORITATIVE {
            return Err(driver_failure(
                "search page violated non-authoritative restriction envelope",
            ));
        }
        SearchRequestedConsistency::EventualOrDerived
            .verify_achieved(page.achieved_consistency, false, &[])
            .map_err(|error| map_search_error(&error))?;

        let mut retracted_count: u64 = 0;
        for hit in &page.items {
            self.authority
                .inspect_admission_receipt(hit.event_id)
                .map_err(|error| map_authority_error(&error))?;
            if hit.retracted {
                retracted_count = retracted_count.saturating_add(1);
            }
        }
        let admitted_event_count = u64::try_from(page.items.len()).unwrap_or(u64::MAX);
        let active_assertion_count = admitted_event_count.saturating_sub(retracted_count);

        Ok(SearchInspection {
            scope_id,
            consistency_class: encode_achieved_consistency(page.achieved_consistency),
            degradation_policy: 0,
            non_authoritative_restrictions_bits: encode_restrictions_bits(page.use_restrictions),
            admitted_event_count,
            active_assertion_count,
            retracted_event_count: retracted_count,
            truncated: !page.exhausted,
        })
    }
}

fn invalid_argument_failure(message: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::InvalidArgument.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    }
}

fn driver_failure(message: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::Driver.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    }
}

fn map_authority_error(error: &SemanticAuthorityError) -> SabiFailure {
    match error {
        SemanticAuthorityError::EventNotFound(_) => SabiFailure {
            code: SabiErrorCode::NotFound.into(),
            retry: RetryDirective::DoNotRetry.into(),
            safe_message: "search candidate event was not found in semantic authority".to_owned(),
        },
        _ => driver_failure("semantic authority rejected search inspection verification"),
    }
}

fn map_search_error(error: &SearchError) -> SabiFailure {
    match error {
        SearchError::InvalidLimit => invalid_argument_failure("search page limit must be non-zero"),
        SearchError::Authority(inner) => map_authority_error(inner),
        SearchError::ControlAuthoritativeUseForbidden(_) => SabiFailure {
            code: SabiErrorCode::Rights.into(),
            retry: RetryDirective::DoNotRetry.into(),
            safe_message: "non-authoritative search output is forbidden on control gates"
                .to_owned(),
        },
        SearchError::ConsistencyUnsatisfied { .. } => SabiFailure {
            code: SabiErrorCode::State.into(),
            retry: RetryDirective::DoNotRetry.into(),
            safe_message: "search consistency declaration was not satisfied".to_owned(),
        },
        SearchError::Sqlite(_) => SabiFailure {
            code: SabiErrorCode::Durability.into(),
            retry: RetryDirective::DoNotRetry.into(),
            safe_message: "search index storage failure".to_owned(),
        },
        SearchError::SchemaVersionUnsupported(_)
        | SearchError::CorruptRecord(_)
        | SearchError::VerificationJoinUnavailable
        | SearchError::InvalidCheckpoint(_)
        | SearchError::MissingDomainWatermark { .. }
        | SearchError::CheckpointWatermarkMismatch { .. }
        | SearchError::CandidateOutsideCheckpoint { .. }
        | SearchError::CandidateRetracted { .. }
        | SearchError::CandidateAuthorityMismatch(_)
        | SearchError::RemoteShardFailed(_) => {
            driver_failure("search service failed to answer bounded inspection")
        }
    }
}
