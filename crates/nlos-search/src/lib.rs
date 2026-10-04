//! Search minimal service: a read-only query face over the Semantic
//! authority's admitted facts (B-SEARCH-001, W33-E / X-1 second half).
//!
//! Stage-B decision point 2 (stage-b-progress §6.5.4) fixed the boundary:
//! Search is a *read-only query face* — zero Semantic-authority writes under
//! every path.  The Semantic authority (B-SEMANTIC-001..009) keeps owning
//! Assertions/Judgments/Verifications/Retractions and the `TrustView`; this
//! crate owns no semantic fact and no schema of its own:
//!
//! - structural queries read the authority's durable rows through a
//!   `SQLite` connection opened with `SQLITE_OPEN_READ_ONLY` — a write
//!   attempt through that connection is rejected by `SQLite` itself, not by
//!   discipline;
//! - trust-view-derived lookups route through the authority's own
//!   [`SemanticAuthority::inspect_trust_view`] via the bound handle, so the
//!   derivation logic (latest-outcome verification status, judgment facts,
//!   retraction facts) never has a second implementation here;
//! - [`SemanticIndex`] is an *optional* derived in-memory index built purely
//!   from authority reads: rebuildable at any time, never persisted, never
//!   canonical.  A stale index can only under-report retractions (they are
//!   append-only and never revived, `[SEM-RETRACT-004]`); a rebuild always
//!   converges to the authority state.
//!
//! The face fails closed on unknown authority schema versions: it reads the
//! durable `user_version` and refuses versions outside
//! [`SUPPORTED_AUTHORITY_SCHEMA_VERSIONS`], so a future authority migration
//! must extend that list before this face may read the new rows.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nlos_capability::CapabilityTarget;
use nlos_semantic::{
    AssertionMode, SemanticAuthority, SemanticAuthorityError, TrustViewSnapshot,
    TrustViewVerificationStatus, UnsignedAssertionEvent, decode_unsigned_assertion_event,
    semantic_event_id,
};
use nlos_types::{PrincipalId, SemanticEventId};
use rusqlite::{Connection, OpenFlags, ToSql};

/// Authority `user_version` values this face knows how to read.  Coupled to
/// `nlos-semantic`'s private schema version by review: an authority
/// migration that keeps the tables this face reads compatible must add the
/// new version here, otherwise every open fails closed.
/// v7/v8 are additive authority migrations (v8: the immutable typed-link
/// index table); v9 is additive too (v9: the durability-receipt signature
/// preimage shape marker on a table this face never reads); the read face's
/// tables are unchanged, so all stay readable (W43-INT seam for the W43-E2
/// semantic v8 bump; W61-B for the v9 canonical-preimage bump).
const SUPPORTED_AUTHORITY_SCHEMA_VERSIONS: &[i64] = &[6, 7, 8, 9];

const EVENT_TYPE_ASSERTION: i64 = 1;

#[derive(Debug)]
pub enum SearchError {
    Sqlite(rusqlite::Error),
    /// A typed read failure propagated from the Semantic authority without
    /// retry or translation (including trust-view lookups).
    Authority(SemanticAuthorityError),
    SchemaVersionUnsupported(i64),
    /// A zero result limit: the caller asked for nothing, which is treated
    /// as an input error instead of a silent empty page.
    InvalidLimit,
    /// The pure in-memory index cannot join authority-derived trust state;
    /// verification-filtered queries must go through [`SearchService`].
    VerificationJoinUnavailable,
    CorruptRecord(&'static str),
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite search face failure: {error}"),
            Self::Authority(error) => {
                write!(
                    formatter,
                    "semantic authority rejected search read: {error}"
                )
            }
            Self::SchemaVersionUnsupported(version) => {
                write!(
                    formatter,
                    "unsupported semantic authority schema version {version}"
                )
            }
            Self::InvalidLimit => formatter.write_str("result limit must be at least 1"),
            Self::VerificationJoinUnavailable => formatter.write_str(
                "the in-memory index cannot join trust views; query through the service",
            ),
            Self::CorruptRecord(reason) => {
                write!(formatter, "corrupt semantic authority record: {reason}")
            }
        }
    }
}

impl Error for SearchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Authority(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for SearchError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// One query hit: an admitted Assertion addressed by the fields a selector
/// can predicate on.  Everything else (canonical bytes, receipts, lineage)
/// stays behind the authority's point-read APIs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssertionHit {
    pub event_id: SemanticEventId,
    /// Admission order in the authority's single event log.
    pub log_seq: u64,
    pub scope: CapabilityTarget,
    pub issuer: PrincipalId,
    pub assertion_mode: AssertionMode,
    pub content_digest: [u8; 32],
    pub content_media_type: String,
    pub admitted_at_ms: u64,
    /// Whether the authority's durable `event_retractions` holds a row for
    /// this event **at read time** (live scan) or **at index build time**
    /// (derived snapshot).
    pub retracted: bool,
}

/// How the durable retraction fact filters hits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RetractionFilter {
    /// Keep every assertion regardless of retraction state.
    #[default]
    Any,
    ExcludeRetracted,
    OnlyRetracted,
}

/// How authority-derived verification state filters hits.  Evaluation routes
/// through [`SemanticAuthority::inspect_trust_view`] at query time — the
/// derivation is never reimplemented here.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VerificationFilter {
    #[default]
    Any,
    Status(TrustViewVerificationStatus),
}

/// The assertion query shape: every predicate is optional and predicates
/// compose with AND; hits return in admission (`log_seq`) order and `limit`
/// applies after all filters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AssertionSelector {
    pub scope: Option<CapabilityTarget>,
    pub issuer: Option<PrincipalId>,
    pub assertion_mode: Option<AssertionMode>,
    pub content_digest: Option<[u8; 32]>,
    pub retraction: RetractionFilter,
    pub verification: VerificationFilter,
    pub limit: usize,
}

impl AssertionSelector {
    /// A structural catch-all with the given result limit.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self {
            scope: None,
            issuer: None,
            assertion_mode: None,
            content_digest: None,
            retraction: RetractionFilter::Any,
            verification: VerificationFilter::Any,
            limit,
        }
    }
}

/// The read-only query face over one Semantic authority store.
///
/// The authority handle is kept for trust-view-derived lookups (its own
/// read APIs are the single source of the derivation); structural queries
/// go through the read-only connection, so the face has no write path at
/// all — not to the authority, and no database of its own either.
pub struct SearchService {
    authority: Arc<SemanticAuthority>,
    connection: Connection,
}

impl SearchService {
    /// Opens the face over `<root>/semantic-authority.db`, strictly
    /// read-only, bound to the live authority handle.
    ///
    /// The authority handle should own the same root (the face cannot
    /// observe the binding and trusts the host's wiring, like
    /// `nlos-notify`).  Opening a WAL-mode database whose `-wal`/`-shm`
    /// sidecars are missing after an unclean shutdown fails closed with the
    /// propagated `SQLite` error; re-opening the authority recovers the log
    /// first.
    ///
    /// # Errors
    ///
    /// Fails closed when the database cannot be opened read-only or its
    /// `user_version` is outside the supported authority schema versions.
    pub fn open(
        root: impl AsRef<Path>,
        authority: Arc<SemanticAuthority>,
    ) -> Result<Self, SearchError> {
        let database = root.as_ref().join("semantic-authority.db");
        let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if !SUPPORTED_AUTHORITY_SCHEMA_VERSIONS.contains(&version) {
            return Err(SearchError::SchemaVersionUnsupported(version));
        }
        Ok(Self {
            authority,
            connection,
        })
    }

    /// Queries admitted assertions by selector shape against the live
    /// authority rows.
    ///
    /// Structural predicates (scope, issuer, content digest) are evaluated
    /// in SQL over the read-only connection; the assertion-mode predicate
    /// is evaluated after decoding the canonical bytes with the
    /// authority's own public decoder; a verification predicate is
    /// evaluated by reading the authority's live
    /// [`SemanticAuthority::inspect_trust_view`] per surviving candidate.
    /// `limit` applies after all filters.
    ///
    /// The scan is limit-aware (audit #30): each round fetches one
    /// amplified SQL window — `LIMIT` pushed into the structural query
    /// over the deterministic `log_seq` order, keyset-paginated on the
    /// unique `log_seq` — and applies the Rust-side filters to it; when a
    /// window's survivors fall short of `limit`, the next window is
    /// fetched until `limit` survivors are collected or the event log is
    /// exhausted.  A small-limit query therefore never decodes or
    /// trust-view-joins the unfiltered tail.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SearchError::InvalidLimit`] for a zero limit, a
    /// propagated authority failure (for example `EventNotFound`-family or
    /// dangling-lineage during the trust-view join), or a storage/corruption
    /// failure.
    pub fn search_assertions(
        &self,
        selector: &AssertionSelector,
    ) -> Result<Vec<AssertionHit>, SearchError> {
        if selector.limit == 0 {
            return Err(SearchError::InvalidLimit);
        }
        let mut hits: Vec<AssertionHit> = Vec::new();
        // Keyset cursor over the unique, admission-monotone `log_seq`
        // (`event_log.log_seq` is `INTEGER PRIMARY KEY AUTOINCREMENT` on an
        // insert-only table): every non-empty page strictly advances the
        // cursor, so pages never skip or repeat rows.
        let mut after_log_seq: Option<u64> = None;
        loop {
            let page = fetch_assertion_rows(
                &self.connection,
                selector,
                after_log_seq,
                Some(sql_fetch_window(selector.limit)),
            )?;
            let Some(last) = page.last() else {
                // Empty page: the structural scan is exhausted.
                break;
            };
            after_log_seq = Some(last.log_seq);
            for hit in page {
                if selector
                    .assertion_mode
                    .is_some_and(|mode| hit.assertion_mode != mode)
                {
                    continue;
                }
                let retraction_keeps = match selector.retraction {
                    RetractionFilter::Any => true,
                    RetractionFilter::ExcludeRetracted => !hit.retracted,
                    RetractionFilter::OnlyRetracted => hit.retracted,
                };
                if !retraction_keeps {
                    continue;
                }
                // Verification join: one authority trust-view lookup per
                // surviving candidate.  Post-v8 each lookup is an indexed
                // point read on the authority side (admission receipt by
                // unique event id; verification/judgment facts resolve
                // through the `semantic_typed_links` primary-key prefix),
                // so the join costs O(survivors) small point queries and
                // the loop above bounds the candidate count.  A batch
                // (IN-list/JOIN) interface would widen the Semantic
                // authority's public read surface — a semantic-authority
                // change, not a search-face one (audit #30 keeps the
                // per-candidate form).
                if let VerificationFilter::Status(status) = selector.verification {
                    let view = self
                        .authority
                        .inspect_trust_view(hit.event_id)
                        .map_err(SearchError::Authority)?;
                    if view.verification_status != status {
                        continue;
                    }
                }
                hits.push(hit);
                if hits.len() == selector.limit {
                    // `limit` counts post-filter survivors, so the ordered
                    // prefix is complete the moment the limit-th survivor
                    // is collected.
                    return Ok(hits);
                }
            }
        }
        Ok(hits)
    }

    /// Reads the authority's `TrustView` snapshot for one committed event,
    /// verbatim.
    ///
    /// # Errors
    ///
    /// Propagates the authority's typed failures (`EventNotFound`,
    /// `DanglingLineage`, storage/corruption) without translation.
    pub fn trust_view(&self, event_id: SemanticEventId) -> Result<TrustViewSnapshot, SearchError> {
        self.authority
            .inspect_trust_view(event_id)
            .map_err(SearchError::Authority)
    }

    /// Builds the derived in-memory index from a full read pass over the
    /// authority's admitted assertions.
    ///
    /// The index is a point-in-time snapshot: never persisted, never
    /// canonical, and rebuildable at any moment. Each entry re-derives the
    /// `EventId` from the canonical bytes and fails closed on mismatch.
    /// The pass stays deliberately unbounded (audit #30's documented
    /// full-scan design): no cursor, no LIMIT window, one whole-log read.
    ///
    /// # Errors
    ///
    /// Fails closed for a storage, decode, or read-integrity failure.
    pub fn build_index(&self) -> Result<SemanticIndex, SearchError> {
        let entries = fetch_assertion_rows(
            &self.connection,
            &AssertionSelector::new(usize::MAX),
            None,
            None,
        )?;
        Ok(SemanticIndex { entries })
    }
}

/// The optional derived in-memory index: a rebuildable snapshot of every
/// admitted assertion, queryable without touching the authority again.
///
/// Never canonical: entries are frozen at build time, so an out-of-band
/// retraction (append-only, never revoked) leaves a stale index
/// under-reporting — the live [`SearchService`] read or a rebuild always
/// wins.  The index is a plain admission-ordered vector scanned linearly;
/// this minimal service deliberately owns no query engine.
pub struct SemanticIndex {
    entries: Vec<AssertionHit>,
}

impl SemanticIndex {
    /// Queries the snapshot with the structural predicates of `selector`.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SearchError::InvalidLimit`] for a zero limit and
    /// [`SearchError::VerificationJoinUnavailable`] when a verification
    /// predicate is requested — authority-derived trust state is only
    /// joined by [`SearchService::search_assertions`].
    pub fn query(&self, selector: &AssertionSelector) -> Result<Vec<AssertionHit>, SearchError> {
        if selector.limit == 0 {
            return Err(SearchError::InvalidLimit);
        }
        if selector.verification != VerificationFilter::Any {
            return Err(SearchError::VerificationJoinUnavailable);
        }
        let mut hits = self
            .entries
            .iter()
            .filter(|hit| {
                selector.scope.is_none_or(|scope| hit.scope == scope)
                    && selector.issuer.is_none_or(|issuer| hit.issuer == issuer)
                    && selector
                        .assertion_mode
                        .is_none_or(|mode| hit.assertion_mode == mode)
                    && selector
                        .content_digest
                        .is_none_or(|digest| hit.content_digest == digest)
                    && match selector.retraction {
                        RetractionFilter::Any => true,
                        RetractionFilter::ExcludeRetracted => !hit.retracted,
                        RetractionFilter::OnlyRetracted => hit.retracted,
                    }
            })
            .cloned()
            .collect::<Vec<_>>();
        hits.truncate(selector.limit);
        Ok(hits)
    }

    /// The snapshot entries in admission (`log_seq`) order.
    #[must_use]
    pub fn entries(&self) -> &[AssertionHit] {
        &self.entries
    }

    /// How many admitted assertions the snapshot holds.
    #[must_use]
    pub fn assertion_count(&self) -> usize {
        self.entries.len()
    }
}

type AssertionColumns = (
    Vec<u8>,
    Vec<u8>,
    i64,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
    String,
    i64,
);

fn assertion_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssertionColumns> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

/// Fetches the assertion rows matching the selector's SQL-side structural
/// predicates, in admission (`log_seq`) order.
///
/// `after_log_seq` keyset-pages the scan (`l.log_seq > ?n`; every page
/// strictly advances the unique, insert-only `log_seq`) and `fetch_limit`
/// pushes a `LIMIT ?n` window into the SQL so a limit-aware caller never
/// materializes the full hit set.  `None` for either means that narrowing
/// does not apply — the index build's single unbounded pass.  A
/// `fetch_limit` above the SQLite integer range is clamped to `i64::MAX`,
/// which SQLite evaluates as unbounded.
fn fetch_assertion_rows(
    connection: &Connection,
    selector: &AssertionSelector,
    after_log_seq: Option<u64>,
    fetch_limit: Option<usize>,
) -> Result<Vec<AssertionHit>, SearchError> {
    let mut conditions: Vec<String> = Vec::new();
    // Binding ?1 is the fixed event-type; selector predicates number from ?2.
    let mut parameters: Vec<Box<dyn ToSql>> = vec![Box::new(EVENT_TYPE_ASSERTION)];
    if let Some(scope) = selector.scope {
        let (kind, id) = encode_scope(scope);
        let next = parameters.len() + 1;
        conditions.push(format!("e.scope_kind=?{next} AND e.scope_id=?{}", next + 1));
        parameters.push(Box::new(kind));
        parameters.push(Box::new(id.to_vec()));
    }
    if let Some(issuer) = selector.issuer {
        let next = parameters.len() + 1;
        conditions.push(format!("e.issuer_principal_id=?{next}"));
        parameters.push(Box::new(issuer.as_bytes().to_vec()));
    }
    if let Some(digest) = selector.content_digest {
        let next = parameters.len() + 1;
        conditions.push(format!("e.content_digest=?{next}"));
        parameters.push(Box::new(digest.to_vec()));
    }
    if let Some(log_seq) = after_log_seq {
        let next = parameters.len() + 1;
        conditions.push(format!("l.log_seq>?{next}"));
        parameters.push(Box::new(i64::try_from(log_seq).map_err(|_| {
            SearchError::CorruptRecord("log seq exceeds sqlite integer")
        })?));
    }
    let mut sql = String::from(
        "SELECT e.event_id, e.canonical_unsigned_event, e.scope_kind, e.scope_id,
                e.issuer_principal_id, e.content_digest, l.log_seq, a.admitted_at_ms,
                c.media_type,
                EXISTS(SELECT 1 FROM event_retractions r
                       WHERE r.target_event_id = e.event_id) AS retracted
         FROM semantic_events e
         JOIN event_log l ON l.event_id = e.event_id
         JOIN admission_receipts a ON a.event_id = e.event_id
         JOIN content_objects c ON c.content_digest = e.content_digest
         WHERE e.event_type = ?1",
    );
    if !conditions.is_empty() {
        sql.push_str(" AND ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(" ORDER BY l.log_seq");
    if let Some(limit) = fetch_limit {
        let next = parameters.len() + 1;
        let limit_sql = format!(" LIMIT ?{next}");
        sql.push_str(&limit_sql);
        parameters.push(Box::new(i64::try_from(limit).unwrap_or(i64::MAX)));
    }
    let parameter_refs: Vec<&dyn ToSql> = parameters.iter().map(Box::as_ref).collect();
    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map(parameter_refs.as_slice(), assertion_columns)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(assertion_hit_from_columns).collect()
}

/// The SQL fetch window for one pagination round of
/// [`SearchService::search_assertions`] (audit #30).
///
/// The mode, retraction and verification filters run *after* the SQL
/// fetch, so the window oversamples the raw row count — factor 4 plus 64
/// rows of slack — which keeps small-limit queries to a single round
/// while any heavier filtering converges through keyset pagination until
/// the caller's `limit` post-filter survivors are collected or the event
/// log is exhausted.  Saturating on purpose: a `usize::MAX` limit
/// degenerates to one effectively unbounded pass.
fn sql_fetch_window(limit: usize) -> usize {
    limit.saturating_mul(4).saturating_add(64)
}

fn assertion_hit_from_columns(columns: AssertionColumns) -> Result<AssertionHit, SearchError> {
    let (
        event_id,
        canonical,
        scope_kind,
        scope_id,
        issuer,
        content_digest,
        log_seq,
        admitted,
        media_type,
        retracted,
    ) = columns;
    let event_id = decode_array::<32>(event_id, "event id length is not 32")?;
    let event: UnsignedAssertionEvent =
        decode_unsigned_assertion_event(&canonical).map_err(SearchError::Authority)?;
    if semantic_event_id(&canonical).into_bytes() != event_id {
        return Err(SearchError::CorruptRecord(
            "row event id does not derive from its canonical bytes",
        ));
    }
    let scope = decode_scope(scope_kind, decode_array::<16>(scope_id, "scope id")?)?;
    let issuer = PrincipalId::from_bytes(decode_array::<16>(issuer, "issuer")?);
    let hit = AssertionHit {
        event_id: SemanticEventId::from_bytes(event_id),
        log_seq: decode_u64(log_seq)?,
        scope,
        issuer,
        assertion_mode: event.assertion_mode,
        content_digest: decode_array::<32>(content_digest, "content digest")?,
        content_media_type: media_type,
        admitted_at_ms: decode_u64(admitted)?,
        retracted: retracted != 0,
    };
    Ok(hit)
}

fn encode_scope(scope: CapabilityTarget) -> (i64, [u8; 16]) {
    match scope {
        CapabilityTarget::Namespace(id) => (1, id.into_bytes()),
        CapabilityTarget::Task(id) => (2, id.into_bytes()),
    }
}

fn decode_scope(kind: i64, bytes: [u8; 16]) -> Result<CapabilityTarget, SearchError> {
    match kind {
        1 => Ok(CapabilityTarget::Namespace(
            nlos_types::NamespaceId::from_bytes(bytes),
        )),
        2 => Ok(CapabilityTarget::Task(nlos_types::TaskId::from_bytes(
            bytes,
        ))),
        _ => Err(SearchError::CorruptRecord("scope kind")),
    }
}

fn decode_u64(value: i64) -> Result<u64, SearchError> {
    u64::try_from(value).map_err(|_| SearchError::CorruptRecord("negative integer"))
}

fn decode_array<const N: usize>(
    bytes: Vec<u8>,
    field: &'static str,
) -> Result<[u8; N], SearchError> {
    bytes
        .try_into()
        .map_err(|_| SearchError::CorruptRecord(field))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::SearchService;
    use std::sync::Arc;

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            Self(std::env::temp_dir().join(format!(
                "nlos-search-unit-{label}-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn read_only_connection_rejects_writes() {
        let root = TempRoot::new("readonly");
        let authority =
            Arc::new(nlos_semantic::SemanticAuthority::open(root.path()).expect("open authority"));
        let service = SearchService::open(root.path(), authority).expect("open search face");
        // A valid write statement through the face's own connection must be
        // rejected by SQLite's read-only enforcement, not by policy.
        let result = service
            .connection
            .execute("UPDATE semantic_events SET key_id = key_id WHERE 0", []);
        let error = result.expect_err("write must fail");
        assert!(
            error.to_string().contains("readonly"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn open_fails_closed_without_an_authority_database() {
        let root = TempRoot::new("missing-db");
        let authority_root = TempRoot::new("missing-db-authority");
        let authority = Arc::new(
            nlos_semantic::SemanticAuthority::open(authority_root.path()).expect("open authority"),
        );
        std::fs::create_dir_all(root.path()).expect("create root");
        assert!(SearchService::open(root.path(), authority).is_err());
    }
}
