//! Step C (`ADR-0027` section 2.4 Step C) — In-Place Host `N` (`host_epoch = 2`)
//! Upgrade, `[KABI-SVC-001]` Internal KABI Refactor Insulation, Dynamic
//! `nlos-compat-shim` Mount Planning, and `[COMPAT-VER-002]` Wire Semantics.
//!
//! Performs the simulated in-place OS upgrade from Host `N-1` (`epoch = 1`) to
//! Host `N` (`epoch = 2`) over the **exact same durable SQLite databases and
//! `UserData` directories** created by Step B (`0` bytes of any `.nlos-pkg`
//! modified, recompiled, or re-signed), proving:
//! 1. `[KABI-SVC-001]`: completely refactoring the host's private internal KABI
//!    handle representation, slot table layout, and opcode dispatch numbering
//!    between Host `N-1` and Host `N` produces **byte-identical SABI wire
//!    responses** for frozen `N-1` packages;
//! 2. `[COMPAT-VER-001]` & `[COMPAT-DEPRECATE-001]`: Host `N` (`current_epoch = 2`,
//!    `min_compatibility_window_epochs = 2`) evolves `DeprecationManifest` with
//!    additive SABI minor extensions, transitions an `N-1` contract symbol into
//!    `DEPRECATED` (`deprecated_in_epoch = 2`, `earliest_sunset_epoch = 4`),
//!    forbids premature sunset (`epoch = 3` rejected), and enforces permanent
//!    `RESERVED` / `SUNSET` tombstones (`E_ABI_VERSION` + `DO_NOT_RETRY`);
//! 3. `[SABI-DISC-001]` & `[DRV-NEGO-001]`: `CompatMountPlanner` mounts
//!    `OutOfCoreCompatShim` (`1` or `2` hops) for all 4 installed `N-1`
//!    (`target_sabi_epoch = 1`) packages while keeping native `N`
//!    (`target_sabi_epoch = 2`) traffic on `0`-hop `NativePassthrough`;
//! 4. `[COMPAT-VER-002]`: unknown `major` (`0`, `3`, `99`, or expired `1` on a
//!    hypothetical `host_epoch = 4` host) and unknown `critical_extension_ids`
//!    fail closed with `SabiErrorCode::AbiVersion` + `RetryDirective::DoNotRetry`,
//!    whereas unknown `non_critical_extension_ids` and unknown raw Protobuf wire
//!    fields survive `N-1 -> Host N -> N-1` round-trips bit-for-bit.

use std::collections::BTreeMap;

use nlos_compat_shim::{
    CompatMountDecision, CompatMountPlanner, DriverWireEpoch, EpochTranslationRules,
    ExtractedEnvelopeUnknowns, PackageCompatibilitySpec, RawUnknownField, SabiEpochAdapterCell,
    encode_envelope_with_unknown_fields, extract_envelope_unknown_fields,
};
use nlos_schema::sabi::v1::{
    Envelope, RetryDirective, SabiErrorCode, SabiRequestContext, SabiResponseContext,
    SchemaIdentity, envelope,
};
use nlos_schema::{
    CANONICAL_DEPRECATION_MANIFEST_YAML, DeprecationManifest, MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
    SABI_DRIVER_PROVIDER_SCHEMA, SABI_ENVELOPE_SCHEMA, SABI_SERVICE_DIRECTORY_SCHEMA,
    VersionCompatibilityPolicy, VersionRoute, parse_deprecation_manifest,
};
use nlos_types::IdempotencyKey;
use prost::Message;

use crate::step_b_n1_host::StepBN1HostStateSnapshot;
use crate::{HOST_EPOCH_N, HOST_EPOCH_N_MINUS_1, SampleKind, TempHostWorkspace, derive_key16};

/// Known critical extension ID supported on Host `N` (`2001`).
pub const HOST_N_KNOWN_CRITICAL_EXT_ID: u32 = 2001;

/// Deprecated `N-1` service contract symbol on `nlos.sabi.ServiceDirectory`
/// (`introduced_in_epoch: 1`, `deprecated_in_epoch: 2`, `earliest_sunset_epoch: 4`).
pub const DEPRECATED_N1_SYMBOL: &str = "LegacyObservatorySyncV1";

/// Replacement contract symbol advertised in Host `N`'s `DeprecationWarningReceipt`.
pub const DEPRECATED_N1_REPLACEMENT: &str = "ObservatorySyncV2";

/// Permanently reserved tombstone symbol rejected fail-closed on Host `N`.
pub const RESERVED_TOMBSTONE_SYMBOL: &str = "RetiredExperimentalPreV1Slot";

/// Permanently reserved field slot number on `nlos.sabi.Envelope` rejected fail-closed on Host `N`.
pub const RESERVED_TOMBSTONE_FIELD_NUMBER: u32 = 901;

const STEP_C_KEY_DOMAIN: &[u8] = b"llmos/w81-road-e001/step-c/key/v1";

/// Receipt produced by Step C (`ADR-0027` section 2.4 Step C) capturing the
/// upgraded Host `N` (`host_epoch = 2`) compatibility configuration and all
/// `[KABI-SVC-001]` / `[COMPAT-VER-002]` verification outcomes.
#[derive(Clone, Debug)]
pub struct StepCInPlaceUpgradeReceipt {
    /// Upgraded Host major epoch (`2`).
    pub host_epoch: u32,
    /// Minimum backward compatibility window enforced (`>= 2` major epochs).
    pub backward_window_epochs: u32,
    /// Validated Host `N` (`current_epoch = 2`) machine-readable `DeprecationManifest`.
    pub host_n_deprecation_manifest: DeprecationManifest,
    /// Per-package mounted shim hop count for the 4 installed `N-1` packages (`>= 1`).
    pub mounted_n1_shim_hops: BTreeMap<String, usize>,
    /// Native `N` (`target_sabi_epoch = 2`) shim hop count (`0`).
    pub native_epoch_n_shim_hops: usize,
    /// Proof flag confirming `[KABI-SVC-001]` internal KABI refactor produced
    /// byte-identical SABI wire responses.
    pub kabi_refactor_wire_identical: bool,
    /// Count of `[COMPAT-VER-002]` fail-closed rejections verified (`E_ABI_VERSION` + `DO_NOT_RETRY`).
    pub fail_closed_rejection_count: usize,
    /// Preserved unknown `non_critical_extension_ids` across `N-1 -> Host N -> N-1`.
    pub preserved_non_critical_extensions: Vec<u32>,
    /// Total byte length of preserved unknown raw Protobuf wire fields across round-trip.
    pub preserved_unknown_protobuf_bytes: usize,
}

// ============================================================================
// [KABI-SVC-001] Private Internal KABI Refactoring Insulation Model
// ============================================================================

/// Host `N-1` (`epoch = 1`) private kernel-internal KABI layout:
/// flat 32-bit slot array + packed `(slot: u16, gen: u16)` internal handle +
/// legacy internal opcode numbering (`0x10`, `0x20`).
struct HostN1PrivateKabiTable {
    flat_slots: Vec<(u16, String, [u8; 32])>,
}

impl HostN1PrivateKabiTable {
    fn new_seeded() -> Self {
        Self {
            flat_slots: vec![
                (
                    1,
                    "nlos.authority.ArtifactCatalog".to_owned(),
                    derive_key_digest32(b"artifact-catalog-v1"),
                ),
                (
                    1,
                    "nlos.authority.PlanScheduler".to_owned(),
                    derive_key_digest32(b"plan-scheduler-v1"),
                ),
            ],
        }
    }

    /// Private `N-1` KABI lookup using packed `u32` `(slot << 16 | gen)` internal handle.
    fn kabi_v1_lookup_by_slot(&self, service_name: &str) -> Option<(u32, [u8; 32])> {
        for (idx, (gen_u16, name, digest)) in self.flat_slots.iter().enumerate() {
            if name == service_name {
                let slot_u16 = u16::try_from(idx).unwrap_or(0);
                let packed_handle = (u32::from(slot_u16) << 16) | u32::from(*gen_u16);
                return Some((packed_handle, *digest));
            }
        }
        None
    }
}

/// Host `N` (`epoch = 2`) **completely refactored** private kernel-internal KABI layout:
/// flat `Vec` replaced by sharded `BTreeMap` keyed by 128-bit internal capability salt,
/// packed 32-bit handle replaced by 128-bit `(shard_id: u32, arena_gen: u64, flags: u32)`
/// internal struct, and internal dispatch opcodes renumbered (`0xA100`, `0xB200`).
struct HostN2RefactoredPrivateKabiArena {
    sharded_buckets: BTreeMap<[u8; 16], (u32, u64, String, [u8; 32])>,
}

impl HostN2RefactoredPrivateKabiArena {
    fn new_refactored_from_durable_truth() -> Self {
        let mut sharded_buckets = BTreeMap::new();
        for (ord, (svc, tag)) in [
            (
                "nlos.authority.ArtifactCatalog",
                &b"artifact-catalog-v1"[..],
            ),
            ("nlos.authority.PlanScheduler", &b"plan-scheduler-v1"[..]),
        ]
        .into_iter()
        .enumerate()
        {
            let internal_arena_key = derive_key16(
                STEP_C_KEY_DOMAIN,
                svc.as_bytes(),
                u32::try_from(ord).unwrap_or(0),
            );
            sharded_buckets.insert(
                internal_arena_key,
                (
                    0xA100 + u32::try_from(ord).unwrap_or(0),
                    0x0002_0000_0001_u64,
                    svc.to_owned(),
                    derive_key_digest32(tag),
                ),
            );
        }
        Self { sharded_buckets }
    }

    /// Private `N` refactored KABI lookup — totally incompatible internal signature
    /// and memory representation compared to `HostN1PrivateKabiTable`, yet projects
    /// the exact same public SABI contract payload!
    fn kabi_v2_sharded_arena_query(&self, service_name: &str) -> Option<([u8; 16], u64, [u8; 32])> {
        for (arena_key, (_opcode, arena_gen, name, digest)) in &self.sharded_buckets {
            if name == service_name {
                return Some((*arena_key, *arena_gen, *digest));
            }
        }
        None
    }
}

fn derive_key_digest32(label: &[u8]) -> [u8; 32] {
    crate::derive_digest32(STEP_C_KEY_DOMAIN, label, 1)
}

/// Public SABI service facade insulating third-party applications from private
/// internal KABI data-structure and handle refactors (`[KABI-SVC-001]`).
fn project_public_sabi_service_payload(service_name: &str, state_digest: [u8; 32]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(service_name.len() + 40);
    payload.extend_from_slice(b"nlos.sabi.service-projection/v1:");
    payload.extend_from_slice(service_name.as_bytes());
    payload.push(b':');
    payload.extend_from_slice(&state_digest);
    payload
}

/// Mechanically proves `[KABI-SVC-001]`: frozen `N-1` (`major = 1`) SABI callers
/// observe **byte-for-byte identical SABI response frames** after the host
/// completely refactors its private internal KABI handle format and table layout
/// on Host `N` (`major = 2`).
///
/// # Errors
///
/// Fails closed if any SABI response byte differs between Host `N-1` and
/// refactored Host `N` (`via SabiEpochAdapterCell`).
#[allow(clippy::too_many_lines)]
pub fn verify_kabi_internal_refactor_insulation() -> Result<(), String> {
    let n1_kabi = HostN1PrivateKabiTable::new_seeded();
    let n2_refactored_kabi = HostN2RefactoredPrivateKabiArena::new_refactored_from_durable_truth();

    let mut adapter =
        SabiEpochAdapterCell::with_canonical_manifest(HOST_EPOCH_N_MINUS_1, HOST_EPOCH_N)
            .map_err(|err| format!("init SabiEpochAdapterCell for KABI proof: {err}"))?;

    for (idx, service_target) in [
        "nlos.authority.ArtifactCatalog",
        "nlos.authority.PlanScheduler",
    ]
    .into_iter()
    .enumerate()
    {
        let req_id = derive_key16(
            STEP_C_KEY_DOMAIN,
            b"kabi-insulation-req",
            u32::try_from(idx + 1).unwrap_or(1),
        );
        let idemp_key = IdempotencyKey::from_bytes(derive_key16(
            STEP_C_KEY_DOMAIN,
            b"kabi-insulation-idem",
            u32::try_from(idx + 1).unwrap_or(1),
        ));

        let frozen_n1_client_req = Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: HOST_EPOCH_N_MINUS_1,
                minor: 0,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: req_id.to_vec(),
            service: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            method: "QueryAuthorityProjection".to_owned(),
            common_context: Some(envelope::CommonContext::RequestContext(
                SabiRequestContext {
                    caller: None,
                    activity_context: b"frozen-n1-caller".to_vec(),
                    task_execution_binding: None,
                    correlation_id: req_id.to_vec(),
                    idempotency_key: idemp_key.as_bytes().to_vec(),
                    deadline_monotonic_ns: 90_000_000_000,
                    capability_handles: Vec::new(),
                    reservation_handle: None,
                    proposal_or_input_digest_sha256: Vec::new(),
                },
            )),
            payload: service_target.as_bytes().to_vec(),
        };
        let frozen_n1_req_wire = frozen_n1_client_req.encode_to_vec();

        // Path 1: Execute on Host `N-1` using `HostN1PrivateKabiTable`.
        let (_packed_u32_handle, n1_digest) = n1_kabi
            .kabi_v1_lookup_by_slot(service_target)
            .ok_or_else(|| format!("N-1 KABI missing {service_target}"))?;
        let n1_direct_resp = Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: HOST_EPOCH_N_MINUS_1,
                minor: 0,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: req_id.to_vec(),
            service: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            method: "QueryAuthorityProjection".to_owned(),
            common_context: Some(envelope::CommonContext::ResponseContext(
                SabiResponseContext {
                    correlation_id: req_id.to_vec(),
                    operation: None,
                    receipts: Vec::new(),
                    failure: None,
                },
            )),
            payload: project_public_sabi_service_payload(service_target, n1_digest),
        };
        let n1_resp_wire = n1_direct_resp.encode_to_vec();

        // Path 2: Translate frozen `N-1` wire -> Host `N` (`major = 2`), execute against
        // completely refactored `HostN2RefactoredPrivateKabiArena`, and translate back to `N-1`.
        let upgraded_req = adapter
            .translate_client_to_host_wire(&frozen_n1_req_wire)
            .map_err(|err| format!("translate_client_to_host_wire: {err}"))?;
        if upgraded_req.envelope.schema.as_ref().map(|s| s.major) != Some(HOST_EPOCH_N) {
            return Err("expected upgraded request major == HOST_EPOCH_N (2)".to_owned());
        }
        let queried_svc = String::from_utf8(upgraded_req.envelope.payload.clone())
            .map_err(|err| format!("service UTF-8: {err}"))?;
        let (_arena_128_key, _arena_gen64, n2_digest) = n2_refactored_kabi
            .kabi_v2_sharded_arena_query(&queried_svc)
            .ok_or_else(|| format!("N refactored KABI missing {queried_svc}"))?;

        let host_n_resp = Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: HOST_EPOCH_N,
                minor: 2,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: upgraded_req.preserved_request_id.to_vec(),
            service: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            method: "QueryAuthorityProjection".to_owned(),
            common_context: Some(envelope::CommonContext::ResponseContext(
                SabiResponseContext {
                    correlation_id: upgraded_req.preserved_request_id.to_vec(),
                    operation: None,
                    receipts: Vec::new(),
                    failure: None,
                },
            )),
            payload: project_public_sabi_service_payload(&queried_svc, n2_digest),
        };
        let downgraded_resp = adapter
            .translate_host_to_client_wire(&host_n_resp.encode_to_vec())
            .map_err(|err| format!("translate_host_to_client_wire: {err}"))?;

        if downgraded_resp.wire != n1_resp_wire {
            return Err(format!(
                "[KABI-SVC-001] wire drift detected for {service_target} across internal KABI refactor"
            ));
        }
    }

    Ok(())
}

/// Builds Host `N`'s (`current_epoch = 2`) machine-readable `DeprecationManifest`
/// by evolving the canonical SABI v1 manifest with:
/// - `current_epoch: 2` (`min_compatibility_window_epochs: 2`, `9` frozen channels);
/// - additive minor metadata on `nlos.sabi.Envelope` (`minor: 2`) and
///   `nlos.sabi.DriverProvider` (`minor: 1`);
/// - permanent tombstone slot `901` and symbol `RetiredExperimentalPreV1Slot` on
///   `nlos.sabi.Envelope`;
/// - `DEPRECATED` entry `LegacyObservatorySyncV1` on `nlos.sabi.ServiceDirectory`
///   (`introduced_in_epoch: 1`, `deprecated_in_epoch: 2`, `earliest_sunset_epoch: 4`,
///   `replacement: "ObservatorySyncV2"`).
///
/// # Errors
///
/// Fails closed if the evolved YAML violates any `ADR-0027` §3.2 invariant.
pub fn build_host_n_evolved_deprecation_manifest() -> Result<DeprecationManifest, String> {
    let mut evolved = CANONICAL_DEPRECATION_MANIFEST_YAML
        .replace("\r\n", "\n")
        .replace("current_epoch: 1", "current_epoch: 2");
    evolved = evolved.replace(
        "  - name: \"nlos.sabi.Envelope\"\n    major: 1\n    minor: 1\n    frozen: true\n    reserved_field_numbers: []\n    reserved_symbols: []",
        "  - name: \"nlos.sabi.Envelope\"\n    major: 2\n    minor: 2\n    frozen: true\n    reserved_field_numbers:\n      - 901\n    reserved_symbols:\n      - \"RetiredExperimentalPreV1Slot\"",
    );
    evolved = evolved.replace(
        "  - name: \"nlos.sabi.DriverProvider\"\n    major: 1\n    minor: 0\n    frozen: true",
        "  - name: \"nlos.sabi.DriverProvider\"\n    major: 2\n    minor: 1\n    frozen: true",
    );
    evolved.push_str(concat!(
        "\n  - channel: \"nlos.sabi.ServiceDirectory\"\n",
        "    symbol: \"LegacyObservatorySyncV1\"\n",
        "    kind: \"METHOD\"\n",
        "    introduced_in_epoch: 1\n",
        "    state: \"DEPRECATED\"\n",
        "    deprecated_in_epoch: 2\n",
        "    earliest_sunset_epoch: 4\n",
        "    replacement: \"ObservatorySyncV2\"\n",
        "    migration_rule_id: \"ADR-0027-E7-OBS-SYNC-V2\"\n",
        "    reason: \"Superseded by additive batch checkpoint sync in Host Epoch 2\"\n",
    ));

    let parsed = parse_deprecation_manifest(&evolved)
        .map_err(|err| format!("parse_deprecation_manifest(Host N epoch=2): {err}"))?;
    if parsed.current_epoch != HOST_EPOCH_N
        || parsed.min_compatibility_window_epochs < MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS
        || parsed.frozen_channels_total != 9
    {
        return Err(format!(
            "unexpected Host N DeprecationManifest header: {parsed:?}"
        ));
    }

    // Also prove `[COMPAT-DEPRECATE-001]` premature sunset (`earliest_sunset_epoch: 3`
    // for `deprecated_in_epoch: 2`) fails closed!
    let premature_sunset_yaml =
        evolved.replace("earliest_sunset_epoch: 4", "earliest_sunset_epoch: 3");
    if parse_deprecation_manifest(&premature_sunset_yaml).is_ok() {
        return Err(
            "premature earliest_sunset_epoch=3 (< deprecated_in_epoch + 2) must fail closed"
                .to_owned(),
        );
    }

    Ok(parsed)
}

/// Executes the `[COMPAT-VER-002]` positive and `H7` negative wire-compatibility
/// matrix on Host `N` (`host_epoch = 2`).
///
/// # Errors
///
/// Fails closed if any unknown `major`, expired `major`, unknown `critical_extension_ids`,
/// or `SUNSET`/`RESERVED` tombstone fails to map to `E_ABI_VERSION` + `DO_NOT_RETRY`,
/// or if any unknown `non_critical_extension_ids` / unknown Protobuf tag bytes
/// are dropped during round-trip translation.
#[allow(clippy::too_many_lines)]
pub fn verify_compat_ver_002_matrix_on_host_n(
    manifest: &DeprecationManifest,
) -> Result<(usize, Vec<u32>, usize), String> {
    let policy_host_n = VersionCompatibilityPolicy::with_backward_window(
        HOST_EPOCH_N,
        MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
        &[HOST_N_KNOWN_CRITICAL_EXT_ID],
    );
    let planner = CompatMountPlanner::new(manifest.clone());
    let mut fail_closed_count = 0_usize;

    // 1. Negative: `major = 0`, future `major = 3`, future `major = 99` on Host `N` (`epoch = 2`).
    for bad_major in [0_u32, 3, 99] {
        let spec = PackageCompatibilitySpec::new("bad-epoch-pkg", bad_major, 0);
        let err = planner
            .plan_mount(&spec, &policy_host_n)
            .expect_err("invalid major epoch must fail closed");
        let failure = err.to_sabi_failure();
        if failure.code != i32::from(SabiErrorCode::AbiVersion)
            || failure.retry != i32::from(RetryDirective::DoNotRetry)
        {
            return Err(format!(
                "bad major {bad_major} expected E_ABI_VERSION + DO_NOT_RETRY, got {failure:?}"
            ));
        }
        fail_closed_count += 1;
    }

    // 2. Negative: expired epoch outside `[N, N-1, N-2]` (e.g., `client_major = 1` on `host_epoch = 4`).
    let policy_future_host_4 = VersionCompatibilityPolicy::with_backward_window(
        4,
        MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
        &[HOST_N_KNOWN_CRITICAL_EXT_ID],
    );
    let sunset_spec = PackageCompatibilitySpec::new("expired-n-minus-3-pkg", 1, 0);
    let sunset_err = CompatMountPlanner::with_canonical_manifest()
        .plan_mount(&sunset_spec, &policy_future_host_4)
        .expect_err("N-3 expired package must fail closed");
    let sunset_failure = sunset_err.to_sabi_failure();
    if sunset_failure.code != i32::from(SabiErrorCode::AbiVersion)
        || sunset_failure.retry != i32::from(RetryDirective::DoNotRetry)
    {
        return Err(format!(
            "expired N-3 package expected E_ABI_VERSION + DO_NOT_RETRY, got {sunset_failure:?}"
        ));
    }
    fail_closed_count += 1;

    // 3. Negative: unknown `critical_extension_ids` (`9999`) rejected fail-closed both at mount
    //    time AND on the live `SabiEpochAdapterCell` wire!
    let mut crit_spec =
        PackageCompatibilitySpec::new("unknown-crit-ext-pkg", HOST_EPOCH_N_MINUS_1, 0);
    crit_spec.critical_extensions = vec![9999];
    let crit_mount_err = planner
        .plan_mount(&crit_spec, &policy_host_n)
        .expect_err("unknown critical extension at mount must fail closed");
    let crit_mount_failure = crit_mount_err.to_sabi_failure();
    if crit_mount_failure.code != i32::from(SabiErrorCode::AbiVersion)
        || crit_mount_failure.retry != i32::from(RetryDirective::DoNotRetry)
    {
        return Err("unknown critical extension mount failure code mismatch".to_owned());
    }
    fail_closed_count += 1;

    let mut adapter = SabiEpochAdapterCell::new(
        HOST_EPOCH_N_MINUS_1,
        HOST_EPOCH_N,
        MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
        vec![HOST_N_KNOWN_CRITICAL_EXT_ID],
        manifest.clone(),
        EpochTranslationRules::default(),
    )
    .map_err(|err| format!("SabiEpochAdapterCell::new: {err}"))?;

    let req_id = derive_key16(STEP_C_KEY_DOMAIN, b"compat-ver-002-wire", 1);
    let idemp =
        IdempotencyKey::from_bytes(derive_key16(STEP_C_KEY_DOMAIN, b"compat-ver-002-idem", 1));
    let bad_crit_env = Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: HOST_EPOCH_N_MINUS_1,
            minor: 0,
            critical_extension_ids: vec![9999],
            non_critical_extension_ids: vec![7701],
        }),
        request_id: req_id.to_vec(),
        service: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
        method: "ResolveService".to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(
            SabiRequestContext {
                caller: None,
                activity_context: b"crit-test".to_vec(),
                task_execution_binding: None,
                correlation_id: req_id.to_vec(),
                idempotency_key: idemp.as_bytes().to_vec(),
                deadline_monotonic_ns: 50_000_000,
                capability_handles: Vec::new(),
                reservation_handle: None,
                proposal_or_input_digest_sha256: Vec::new(),
            },
        )),
        payload: b"ping".to_vec(),
    };
    let wire_crit_err = adapter
        .translate_client_to_host_wire(&bad_crit_env.encode_to_vec())
        .expect_err("unknown critical extension on live wire must fail closed");
    let wire_crit_failure = wire_crit_err.to_sabi_failure();
    if wire_crit_failure.code != i32::from(SabiErrorCode::AbiVersion)
        || wire_crit_failure.retry != i32::from(RetryDirective::DoNotRetry)
    {
        return Err("unknown critical extension live wire failure code mismatch".to_owned());
    }
    fail_closed_count += 1;

    // 4. Negative: `RESERVED` tombstone symbol and `RESERVED` field slot (`901`) fail closed.
    let mut tombstone_spec =
        PackageCompatibilitySpec::new("tombstone-symbol-pkg", HOST_EPOCH_N_MINUS_1, 0);
    tombstone_spec.referenced_symbols = vec![(
        SABI_ENVELOPE_SCHEMA.to_owned(),
        RESERVED_TOMBSTONE_SYMBOL.to_owned(),
    )];
    let tomb_err = planner
        .plan_mount(&tombstone_spec, &policy_host_n)
        .expect_err("reserved tombstone symbol must fail closed");
    let tomb_failure = tomb_err.to_sabi_failure();
    if tomb_failure.code != i32::from(SabiErrorCode::AbiVersion)
        || tomb_failure.retry != i32::from(RetryDirective::DoNotRetry)
    {
        return Err("reserved tombstone symbol failure code mismatch".to_owned());
    }
    fail_closed_count += 1;

    let mut tombstone_field_spec =
        PackageCompatibilitySpec::new("tombstone-field-pkg", HOST_EPOCH_N_MINUS_1, 0);
    tombstone_field_spec.referenced_symbols = vec![(
        SABI_ENVELOPE_SCHEMA.to_owned(),
        format!("field:{RESERVED_TOMBSTONE_FIELD_NUMBER}"),
    )];
    let tomb_field_err = planner
        .plan_mount(&tombstone_field_spec, &policy_host_n)
        .expect_err("reserved tombstone field number must fail closed");
    if tomb_field_err.to_sabi_failure().code != i32::from(SabiErrorCode::AbiVersion) {
        return Err("reserved tombstone field failure code mismatch".to_owned());
    }
    fail_closed_count += 1;

    // 5. Positive `[COMPAT-VER-002]`: Known critical extension (`2001`) + unknown
    //    `non_critical_extension_ids` (`[7701, 7702]`) + unknown raw Protobuf wire
    //    fields (`tag 204` varint = `42`, `tag 205` length-delimited = `b"future-sabi-hint"`)
    //    are preserved losslessly across `N-1 -> Host N -> N-1`!
    let base_n1_env = Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: HOST_EPOCH_N_MINUS_1,
            minor: 1,
            critical_extension_ids: vec![HOST_N_KNOWN_CRITICAL_EXT_ID],
            non_critical_extension_ids: vec![7701, 7702],
        }),
        request_id: req_id.to_vec(),
        service: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
        method: DEPRECATED_N1_SYMBOL.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(
            SabiRequestContext {
                caller: None,
                activity_context: b"n1-unknown-preserve".to_vec(),
                task_execution_binding: None,
                correlation_id: Vec::new(), // Will be deterministically backfilled from `request_id`!
                idempotency_key: idemp.as_bytes().to_vec(),
                deadline_monotonic_ns: 95_000_000,
                capability_handles: Vec::new(),
                reservation_handle: None,
                proposal_or_input_digest_sha256: Vec::new(),
            },
        )),
        payload: b"observatory-sync-v1-req".to_vec(),
    };

    // Tag 204 varint: key = (204 << 3) | 0 = 1632 -> varint bytes `[0xE0, 0x0C]`, value `42` -> `[0x2A]`.
    // Tag 205 bytes:  key = (205 << 3) | 2 = 1642 -> varint bytes `[0xEA, 0x0C]`, len `16` -> `[0x10]`, `b"future-sabi-hint"`.
    let injected_unknowns = ExtractedEnvelopeUnknowns {
        envelope_fields: vec![
            RawUnknownField {
                field_number: 204,
                wire_type: 0,
                raw_bytes: vec![0xE0, 0x0C, 0x2A],
            },
            RawUnknownField {
                field_number: 205,
                wire_type: 2,
                raw_bytes: {
                    let mut v = vec![0xEA, 0x0C, 0x10];
                    v.extend_from_slice(b"future-sabi-hint");
                    v
                },
            },
        ],
        schema_fields: Vec::new(),
        request_context_fields: Vec::new(),
        response_context_fields: Vec::new(),
    };
    let expected_unknown_bytes = injected_unknowns.total_raw_bytes();
    let client_wire_with_unknowns =
        encode_envelope_with_unknown_fields(&base_n1_env, &injected_unknowns)
            .map_err(|err| format!("encode_envelope_with_unknown_fields: {err}"))?;

    let to_host = adapter
        .translate_client_to_host_wire(&client_wire_with_unknowns)
        .map_err(|err| format!("translate_client_to_host_wire(positive): {err}"))?;
    if to_host.preserved_request_id != req_id || to_host.preserved_idempotency_key != Some(idemp) {
        return Err(
            "128-bit request_id or IdempotencyKey drifted during N-1 -> N translation".to_owned(),
        );
    }
    if to_host.preserved_non_critical_extensions != vec![7701, 7702] {
        return Err(format!(
            "non_critical_extension_ids drifted: {:?}",
            to_host.preserved_non_critical_extensions
        ));
    }
    if to_host.preserved_unknown_fields != injected_unknowns {
        return Err("unknown Protobuf wire fields lost on N-1 -> Host N ingress".to_owned());
    }
    if to_host.emitted_warnings.len() != 1
        || to_host.emitted_warnings[0].symbol != DEPRECATED_N1_SYMBOL
        || to_host.emitted_warnings[0].earliest_sunset_epoch != 4
    {
        return Err(format!(
            "expected 1 DeprecationWarningReceipt for {DEPRECATED_N1_SYMBOL}, got {:?}",
            to_host.emitted_warnings
        ));
    }

    // Round-trip back from Host `N` (`major = 2`) -> Client `N-1` (`major = 1`) preserving unknown tags!
    let back_to_client = adapter
        .translate_host_to_client_wire(&to_host.wire)
        .map_err(|err| format!("translate_host_to_client_wire(roundtrip): {err}"))?;
    let extracted_after_roundtrip = extract_envelope_unknown_fields(&back_to_client.wire)
        .map_err(|err| format!("extract_envelope_unknown_fields(roundtrip): {err}"))?;
    if extracted_after_roundtrip != injected_unknowns {
        return Err("unknown Protobuf wire fields lost on Host N -> N-1 egress".to_owned());
    }
    if back_to_client
        .envelope
        .schema
        .as_ref()
        .map(|s| (s.major, s.non_critical_extension_ids.clone()))
        != Some((HOST_EPOCH_N_MINUS_1, vec![7701, 7702]))
    {
        return Err(
            "downgraded N-1 schema major or non_critical_extension_ids mismatch".to_owned(),
        );
    }

    Ok((
        fail_closed_count,
        to_host.preserved_non_critical_extensions,
        expected_unknown_bytes,
    ))
}

/// Executes Step C (`ADR-0027` section 2.4 Step C): upgrades the Host in place
/// from `N-1` (`epoch = 1`) to `N` (`epoch = 2`) over `host_ws` without touching
/// any `.nlos-pkg` byte, proves `[KABI-SVC-001]` internal KABI refactor insulation,
/// plans dynamic `nlos-compat-shim` mounts for all 4 installed `N-1` packages
/// vs `0`-hop `NativePassthrough` for native `N` packages, and verifies the
/// `[COMPAT-VER-002]` positive/negative compatibility matrix.
///
/// # Errors
///
/// Fails closed if any compatibility window, KABI insulation, mount decision,
/// deprecation grace window, or `[COMPAT-VER-002]` check fails.
#[allow(clippy::too_many_lines)]
pub fn run_step_c_upgrade_host_to_epoch_n(
    host_ws: &TempHostWorkspace,
    step_b: &StepBN1HostStateSnapshot,
) -> Result<StepCInPlaceUpgradeReceipt, String> {
    if step_b.host_epoch != HOST_EPOCH_N_MINUS_1 {
        return Err(format!(
            "Step C expected Step B baseline at epoch {}, got {}",
            HOST_EPOCH_N_MINUS_1, step_b.host_epoch
        ));
    }
    if !host_ws.authority_root().exists() {
        return Err("Step C requires Step B's durable authority_root to exist in place".to_owned());
    }

    // 1. Prove `[KABI-SVC-001]` internal KABI refactor insulation.
    verify_kabi_internal_refactor_insulation()?;

    // 2. Evolve Host `N` (`current_epoch = 2`) SABI `DeprecationManifest` and
    //    `VersionCompatibilityPolicy` (`window >= 2` major epochs).
    let host_n_manifest = build_host_n_evolved_deprecation_manifest()?;
    let policy_host_n = VersionCompatibilityPolicy::with_backward_window(
        HOST_EPOCH_N,
        MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
        &[HOST_N_KNOWN_CRITICAL_EXT_ID],
    );
    let planner = CompatMountPlanner::new(host_n_manifest.clone());

    // 3. Plan mounts for all 4 already-installed `N-1` (`target_sabi_epoch = 1`) packages:
    //    - 2 Multi-Component Applications -> `MountOutOfCoreShim` (`1` SABI adapter hop)
    //      + `DeprecationWarningReceipt` when referencing `DEPRECATED_N1_SYMBOL`;
    //    - 2 Userland Drivers -> `MountOutOfCoreShim` (`2` hops: `SabiEpochAdapterCell` +
    //      `DriverProtocolShim` bridging `N-1` / Stage B driver wire to Host `N`).
    let mut mounted_n1_shim_hops = BTreeMap::new();
    for (label, rec) in &step_b.installed_samples {
        let mut spec = PackageCompatibilitySpec::new(label.clone(), HOST_EPOCH_N_MINUS_1, 0);
        spec.noncritical_extensions = vec![7701];
        match rec.kind {
            SampleKind::MultiComponentApplication => {
                spec.referenced_symbols = vec![(
                    SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
                    DEPRECATED_N1_SYMBOL.to_owned(),
                )];
            }
            SampleKind::UserlandDriver => {
                spec.driver_abi_epoch = Some(DriverWireEpoch::LegacyStageBMinicborV1);
                spec.referenced_symbols = vec![(
                    SABI_DRIVER_PROVIDER_SCHEMA.to_owned(),
                    "DriverProviderRequest".to_owned(),
                )];
            }
        }

        let decision = planner
            .plan_mount(&spec, &policy_host_n)
            .map_err(|err| format!("CompatMountPlanner::plan_mount({label}): {err}"))?;
        let hops = decision.shim_hop_count();
        match (&decision, rec.kind) {
            (
                CompatMountDecision::MountOutOfCoreShim(stack),
                SampleKind::MultiComponentApplication,
            ) => {
                if !matches!(
                    stack.sabi_negotiation.route,
                    VersionRoute::OutOfCoreCompatShim {
                        client_epoch: HOST_EPOCH_N_MINUS_1,
                        host_epoch: HOST_EPOCH_N,
                        epoch_distance: 1,
                    }
                ) || hops != 1
                    || stack.warnings.len() != 1
                {
                    return Err(format!(
                        "unexpected mount stack for N-1 application {label}: hops={hops}, warnings={:?}",
                        stack.warnings
                    ));
                }
            }
            (CompatMountDecision::MountOutOfCoreShim(stack), SampleKind::UserlandDriver) => {
                if !matches!(
                    stack.sabi_negotiation.route,
                    VersionRoute::OutOfCoreCompatShim {
                        client_epoch: HOST_EPOCH_N_MINUS_1,
                        host_epoch: HOST_EPOCH_N,
                        epoch_distance: 1,
                    }
                ) || hops != 2
                    || stack.sabi_adapter.is_none()
                    || stack.driver_shim.is_none()
                {
                    return Err(format!(
                        "expected 2-hop (SABI + DriverProtocolShim) stack for N-1 driver {label}, got hops={hops}"
                    ));
                }
            }
            (CompatMountDecision::NativePassthrough { .. }, _) => {
                return Err(format!(
                    "N-1 package {label} must mount OutOfCoreCompatShim on Host N, got NativePassthrough"
                ));
            }
        }
        mounted_n1_shim_hops.insert(label.clone(), hops);
    }

    // 4. Verify native Host `N` (`target_sabi_epoch = 2`) package receives `0`-hop `NativePassthrough`.
    let native_n_spec = PackageCompatibilitySpec::new("native-epoch-2-package", HOST_EPOCH_N, 0);
    let native_decision = planner
        .plan_mount(&native_n_spec, &policy_host_n)
        .map_err(|err| format!("plan_mount(native-epoch-2): {err}"))?;
    let native_hops = native_decision.shim_hop_count();
    if !matches!(
        native_decision,
        CompatMountDecision::NativePassthrough { .. }
    ) || native_hops != 0
    {
        return Err(format!(
            "expected 0-hop NativePassthrough for native epoch N package, got hops={native_hops}"
        ));
    }

    // 5. Run the `[COMPAT-VER-002]` positive + `H7` negative matrix on Host `N`.
    let (
        fail_closed_rejection_count,
        preserved_non_critical_extensions,
        preserved_unknown_protobuf_bytes,
    ) = verify_compat_ver_002_matrix_on_host_n(&host_n_manifest)?;

    Ok(StepCInPlaceUpgradeReceipt {
        host_epoch: HOST_EPOCH_N,
        backward_window_epochs: MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
        host_n_deprecation_manifest: host_n_manifest,
        mounted_n1_shim_hops,
        native_epoch_n_shim_hops: native_hops,
        kabi_refactor_wire_identical: true,
        fail_closed_rejection_count,
        preserved_non_critical_extensions,
        preserved_unknown_protobuf_bytes,
    })
}
