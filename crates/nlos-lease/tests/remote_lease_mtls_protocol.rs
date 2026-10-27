//! Loopback multi-socket TLS 1.3 mTLS integration suite for `[LEASE-PROTO-001]`
//! (`crates/nlos-lease/src/remote_proto.rs`, `ADR-0026` §2.3 & §3–§6).
//!
//! Exercises:
//! 1. End-to-end three-family remote lease lifecycle (`GrantQuotaLease`,
//!    `ReportQuotaHighWater`, `RenewQuotaLease` `[LEASE-RENEW-001]` without
//!    `face_value` increase, `CloseQuotaLease` via `SignedLeaseCloseReceipt`,
//!    `GrantCapacityLease` -> `AttachCapacityHost` -> `ReturnCapacityLease`,
//!    `GrantExclusiveDeviceLease` -> `SubmitDeviceResetReceipt`, and
//!    quarantined multi-endpoint `SubmitFenceBarrierAck` +
//!    `ReconcileQuarantinedLease`) over real loopback TLS 1.3 mTLS + 5-axis
//!    Exporter `CellFence` (`MtlsEndpointListener` + `MtlsCellTransport`).
//! 2. Cross-Cell impersonation & spoofed caller rejection (`[TCB-ENDPOINT-001]`):
//!    authenticated Cell B attempting to spoof `caller_cell = CellA` or mutate
//!    Cell A's `QuotaLease` / `CapacityLease` / `ExclusiveDeviceLease` is 100%
//!    rejected fail-closed with Cell A's leases untouched.
//! 3. Mid-call post-commit TCP drop (`AfterFirstPayloadByte`) ->
//!    `RemoteCallOutcome::DeadlineExceededUncertain { retry_directive:
//!    ReconcileBeforeRetry }` -> blind retry refused (`BlindRetryBlocked`) ->
//!    durable `SqliteLeaseStore` (`WAL+FULL`) reopen +
//!    `reconcile_and_replay_uncertain` (`QueryLeaseReconciliationState` +
//!    `mark_reconciled` + exact replay) converges with **zero** double-deduction,
//!    while conflicting payload reuse on the same `IdempotencyKey` fails closed
//!    with `RemoteLeaseReject::IdempotencyConflict` (`[DIST-FAIL-001]`).
//! 4. `ceil(RTT / 2) > max_clock_skew` fail-closed quarantine over mTLS +
//!    `ClusterProfile::SingleHostDesktop` (`N = 1`) zero-socket guarantee.

#![allow(clippy::similar_names, clippy::too_many_lines)]

use std::fs;
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nlos_capability::custody::{
    BootstrapCustodyPrincipalRequest, CustodyTier, SigningKeyCustodyVault,
};
use nlos_cell::{
    CellAuthority, CellEpoch, CellFence, CellFencingToken, CellIdentity, RemoteCallOutcome,
    RemoteRetryDirective, RemoteRetryRefusal,
};
use nlos_clock::AuthorityClock;
use nlos_identity::{IdentityAuthority, IdentityBinding, KeyPurpose};
use nlos_lease::{
    AuditWallTimeHint, CapacityLeaseState, CapacityReconciliationReceipt, CleanupAttestation,
    ClockSkewPolicy, CrossHostLeaseFamily, DeviceResetReceipt, ExclusiveDeviceLeaseState,
    FenceBarrierAck, GrantorClockBoundedCoordinator, HolderLeaseGrantKind, HostAttachReceipt,
    MonotonicInstantMs, PhysicalCleanupProofKind, QuarantineDisposition, QuotaLeaseState,
    RemoteLeaseClientError, RemoteLeaseCommand, RemoteLeaseProtocolClient,
    RemoteLeaseProtocolServer, RemoteLeaseReject, RemoteLeaseRequestEnvelope, RemoteLeaseSuccess,
    RemoteReconcilePayload, SignedFenceBarrierAck, SqliteLeaseStore,
};
use nlos_net::{
    AuthorityCustodyPeerVerifier, ClusterProfile, CustodyPeerSigner, EndpointHintRecord,
    EndpointResolver, EndpointRoutingVersion, FenceNonceRegistry, IncomingRpcRequest,
    MonotonicEndpointResolver, MonotonicPeerFenceGate, MtlsCellTransport, MtlsEndpointListener,
    NetInitError, NetTransportConfig, NodeSecurityDeps, NodeTlsCredentials, RpcHandlerReply,
    RpcRequestHandler, SocketActivityAccounting, TransportDispatchRefusal,
};
use nlos_types::{
    CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, IdempotencyKey, QuotaLeaseId,
    ReceiptId, SchedulerDomainId,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

static ASYNC_AUTHORITY_LOCK: tokio::sync::Mutex<Option<Arc<CellAuthority>>> =
    tokio::sync::Mutex::const_new(None);
static NEXT_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

const GRANTOR_TAG: u8 = 0x77;
const NOW_MS: u64 = 1_780_000_000_000;
const VALID_FROM_MS: u64 = NOW_MS - 60_000;
const VALID_UNTIL_MS: u64 = NOW_MS + 86_400_000;

const GW_1: [u8; 16] = [0xE1; 16];
const GW_2: [u8; 16] = [0xE2; 16];

async fn acquire_grantor_authority() -> tokio::sync::MutexGuard<'static, Option<Arc<CellAuthority>>>
{
    let mut guard = ASYNC_AUTHORITY_LOCK.lock().await;
    if guard.is_none() {
        let claimed = CellAuthority::claim(SchedulerDomainId::from_bytes([GRANTOR_TAG; 16]))
            .expect("claim process-scoped grantor CellAuthority")
            .into_shared();
        *guard = Some(claimed);
    }
    guard
}

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn new(label: &str) -> Self {
        let seq = NEXT_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-lease-mtls-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn sub(&self, child: &str) -> PathBuf {
        let p = self.path.join(child);
        fs::create_dir_all(&p).expect("create sub dir");
        p
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

const fn cell_id(tag: u8) -> CellIdentity {
    CellIdentity::from_domain(SchedulerDomainId::from_bytes([tag; 16]))
}

const fn nz(v: u64) -> NonZeroU64 {
    NonZeroU64::new(v).expect("non-zero u64")
}

const fn gen_u64(v: u64) -> Generation {
    Generation::new(nz(v))
}

const fn epoch_u64(v: u64) -> CellEpoch {
    CellEpoch::from_u64(v).expect("non-zero epoch")
}

const fn token_u64(v: u64) -> CellFencingToken {
    CellFencingToken::from_u64(v).expect("non-zero token")
}

const fn skew_policy(max_skew_ms: u64, safety_margin_ms: u64) -> ClockSkewPolicy {
    ClockSkewPolicy::new(nz(max_skew_ms), nz(safety_margin_ms))
}

struct ProvisionedNode {
    cell: CellIdentity,
    boot_gen: Generation,
    binding: IdentityBinding,
    vault: Arc<SigningKeyCustodyVault>,
    signer: Arc<CustodyPeerSigner>,
    tls_creds: NodeTlsCredentials,
    fence_fn: Arc<dyn Fn() -> CellFence + Send + Sync>,
}

impl ProvisionedNode {
    fn bootstrap_holder(
        identity: &Arc<IdentityAuthority>,
        vault_dir: &Path,
        node_tag: u8,
        boot_gen: u64,
        epoch: u64,
        token: u64,
    ) -> Self {
        let cell = cell_id(node_tag);
        let g = gen_u64(boot_gen);
        let fence = CellFence::present(cell, g, epoch_u64(epoch), token_u64(token));
        let slot = Arc::new(Mutex::new(fence));
        Self::bootstrap_with_fence_fn(
            identity,
            vault_dir,
            node_tag,
            g,
            Arc::new(move || *slot.lock().unwrap_or_else(PoisonError::into_inner)),
        )
    }

    fn bootstrap_grantor(
        identity: &Arc<IdentityAuthority>,
        vault_dir: &Path,
        authority: &Arc<CellAuthority>,
    ) -> Self {
        let auth = Arc::clone(authority);
        let boot_gen = authority.node_boot_generation();
        Self::bootstrap_with_fence_fn(
            identity,
            vault_dir,
            GRANTOR_TAG,
            boot_gen,
            Arc::new(move || auth.fence()),
        )
    }

    fn bootstrap_with_fence_fn(
        identity: &Arc<IdentityAuthority>,
        vault_dir: &Path,
        node_tag: u8,
        boot_gen: Generation,
        fence_fn: Arc<dyn Fn() -> CellFence + Send + Sync>,
    ) -> Self {
        let kek = [node_tag.wrapping_add(0x40); 32];
        let vault = Arc::new(
            SigningKeyCustodyVault::open_durable(vault_dir, kek).expect("open custody vault"),
        );
        let (_, binding) = vault
            .bootstrap_principal_under_custody(
                identity,
                BootstrapCustodyPrincipalRequest {
                    authority_id: [node_tag; 16],
                    principal_profile_digest: [node_tag.wrapping_add(1); 32],
                    control_domain_policy_digest: [node_tag.wrapping_add(2); 32],
                    key_purpose: KeyPurpose::SemanticSigning,
                    custody_tier: CustodyTier::OperatorFileSealed,
                    valid_from_ms: VALID_FROM_MS,
                    valid_until_ms: VALID_UNTIL_MS,
                    seed: [node_tag.wrapping_add(0x10); 32],
                    bootstrap_idempotency_key: IdempotencyKey::from_bytes(
                        [node_tag.wrapping_add(0x20); 16],
                    ),
                    custody_binding_idempotency_key: IdempotencyKey::from_bytes(
                        [node_tag.wrapping_add(0x30); 16],
                    ),
                    provisioned_at_ms: VALID_FROM_MS,
                },
            )
            .expect("bootstrap principal");
        let signer = Arc::new(CustodyPeerSigner::new(
            Arc::clone(&vault),
            binding.principal_id,
            binding.key_id,
            binding.key_generation,
        ));
        let tls_creds = NodeTlsCredentials::issue(
            signer.as_ref(),
            [node_tag.wrapping_add(0x70); 32],
            VALID_FROM_MS,
            VALID_UNTIL_MS,
        )
        .expect("issue X.509v3 TLS credentials");

        Self {
            cell: cell_id(node_tag),
            boot_gen,
            binding,
            vault,
            signer,
            tls_creds,
            fence_fn,
        }
    }

    fn security_deps(&self, peer_verifier: Arc<AuthorityCustodyPeerVerifier>) -> NodeSecurityDeps {
        NodeSecurityDeps {
            local_fence: Arc::clone(&self.fence_fn),
            local_signer: self.signer.clone(),
            peer_verifier,
            fence_policy: Arc::new(MonotonicPeerFenceGate::default()),
            nonce_registry: Arc::new(FenceNonceRegistry::default()),
            clock_ms: Arc::new(|| NOW_MS),
            tls_credentials: self.tls_creds.clone(),
        }
    }
}

fn build_client_transport(
    holder: &ProvisionedNode,
    grantor: &ProvisionedNode,
    grantor_addr: SocketAddr,
    grantor_fence: CellFence,
    verifier: &Arc<AuthorityCustodyPeerVerifier>,
    cfg: NetTransportConfig,
) -> (Arc<MonotonicEndpointResolver>, Arc<MtlsCellTransport>) {
    let resolver = Arc::new(MonotonicEndpointResolver::new());
    resolver
        .upsert_hint(
            EndpointHintRecord::new(
                grantor.cell,
                grantor_addr,
                EndpointRoutingVersion::new(
                    grantor_fence.node_boot_generation(),
                    grantor_fence.epoch(),
                    1,
                ),
                1_000,
            )
            .with_expected_principal(grantor.binding.principal_id),
        )
        .expect("upsert grantor endpoint hint");

    let transport = Arc::new(
        MtlsCellTransport::new_multi_host(
            ClusterProfile::MultiHostCluster {
                local_cell: holder.cell,
                bind_hint: "127.0.0.1:0".parse().expect("loopback"),
            },
            resolver.clone(),
            holder.security_deps(Arc::clone(verifier)),
            cfg,
            SocketActivityAccounting::new(),
            None,
            None,
        )
        .expect("build holder MtlsCellTransport"),
    );
    (resolver, transport)
}

// ============================================================================
// Scenario 1: End-to-End Three-Family Lifecycle, Renewal (`[LEASE-RENEW-001]`),
//             Signed Close, & Multi-Endpoint Barrier Reconciliation Over mTLS
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mtls_01_end_to_end_three_family_remote_lease_lifecycle_renewal_and_barrier_reconcile() {
    let auth_guard = acquire_grantor_authority().await;
    let authority = auth_guard.as_ref().expect("authority").clone();

    let dir = TempTestDir::new("s1-e2e-three-family");
    let identity =
        Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));

    let grantor_node =
        ProvisionedNode::bootstrap_grantor(&identity, &dir.sub("vault-grantor"), &authority);
    let holder_a =
        ProvisionedNode::bootstrap_holder(&identity, &dir.sub("vault-holder-a"), 0xA1, 2, 1, 10);
    let gw_node =
        ProvisionedNode::bootstrap_holder(&identity, &dir.sub("vault-gw"), 0xC1, 1, 1, 10);

    let verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![
            Arc::clone(&grantor_node.vault),
            Arc::clone(&holder_a.vault),
            Arc::clone(&gw_node.vault),
        ],
    ));

    let dev_id = DeviceId::from_bytes([0xD1; 16]);
    let db_path = dir.sub("lease-db").join("leases.sqlite");
    let coordinator = GrantorClockBoundedCoordinator::open_durable(
        &db_path,
        Arc::clone(&authority),
        1_000,
        800,
        &[dev_id],
    )
    .expect("open durable coordinator");

    let auth_clock = Arc::new(AuthorityClock::open(dir.sub("clock")).expect("open AuthorityClock"));
    let policy = skew_policy(25, 10); // floor = 60 ms
    let grantor_mono_ms = Arc::new(AtomicU64::new(10_000));
    let holder_mono_ms = Arc::new(AtomicU64::new(10_000));

    let g_clock = Arc::clone(&grantor_mono_ms);
    let server = Arc::new(
        RemoteLeaseProtocolServer::new(
            coordinator,
            Arc::clone(&auth_clock),
            policy,
            Arc::new(move || MonotonicInstantMs::new(g_clock.load(Ordering::Acquire))),
            Arc::new(|| NOW_MS),
            grantor_node.signer.clone(),
            verifier.clone(),
        )
        .expect("create RemoteLeaseProtocolServer"),
    );

    let listener = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: grantor_node.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        grantor_node.security_deps(Arc::clone(&verifier)),
        NetTransportConfig::default(),
        SocketActivityAccounting::new(),
        Some(server.clone()),
        None,
    )
    .await
    .expect("bind grantor MtlsEndpointListener");
    let grantor_addr = listener.local_addr();
    let running_server = listener.spawn_accept_loop();

    let initial_fence = authority.fence();
    let (resolver_a, transport_a) = build_client_transport(
        &holder_a,
        &grantor_node,
        grantor_addr,
        initial_fence,
        &verifier,
        NetTransportConfig::default(),
    );

    let h_clock = Arc::clone(&holder_mono_ms);
    let client_a = RemoteLeaseProtocolClient::new(
        holder_a.cell,
        holder_a.boot_gen,
        grantor_node.cell,
        transport_a.clone(),
        policy,
        Arc::new(move || MonotonicInstantMs::new(h_clock.load(Ordering::Acquire))),
        Arc::new(|| NOW_MS),
        holder_a.signer.clone(),
        verifier.clone(),
    );

    let epoch_1 = initial_fence.epoch();

    // --- 1. QuotaLease: Grant -> Local Spend/Permits -> Report -> Renew -> Close ---
    let q_id = QuotaLeaseId::from_bytes([0x11; 16]);
    let (mut holder_quota, q_grant_resp) = client_a
        .grant_quota_lease(
            [0x01; 16],
            IdempotencyKey::from_bytes([0x01; 16]),
            epoch_1,
            1_000,
            500,
            q_id,
            400,
            200, // ttl_holder = 200 - 50 - 10 = 140 ms
            vec![GW_1, GW_2],
        )
        .await
        .expect("grant remote QuotaLease over mTLS");
    assert_eq!(holder_quota.ttl_holder().get(), 140);
    assert!(matches!(
        q_grant_resp.result,
        Ok(RemoteLeaseSuccess::Quota {
            grantor_available: 600,
            renewal_seq: 0,
            ..
        })
    ));

    // Spend 150 locally and mint CommitPermit + EffectPermit within shrunk budget:
    holder_mono_ms.store(10_040, Ordering::Release);
    assert_eq!(
        holder_quota
            .admit_local_quota_reserve(MonotonicInstantMs::new(10_040), 150)
            .expect("local reserve"),
        150
    );
    let commit_permit = holder_quota
        .admit_commit_permit(MonotonicInstantMs::new(10_040), 1)
        .expect("commit permit");
    assert_eq!(commit_permit.remaining_shrunk_budget_ms, 100);
    let _effect_permit = holder_quota
        .admit_irreversible_effect_permit(MonotonicInstantMs::new(10_040), [0xAB; 32])
        .expect("effect permit");

    // Report high-water 150 to grantor:
    grantor_mono_ms.store(10_045, Ordering::Release);
    let (_, _, report_resp) = client_a
        .send_signed_command(
            [0x02; 16],
            IdempotencyKey::from_bytes([0x02; 16]),
            epoch_1,
            1_000,
            Some(10),
            500,
            RemoteLeaseCommand::ReportQuotaHighWater {
                lease_id: q_id,
                high_water_spent: 150,
            },
        )
        .await
        .expect("report quota high-water");
    assert!(matches!(
        report_resp.result,
        Ok(RemoteLeaseSuccess::Quota {
            grant,
            grantor_available: 600,
            ..
        }) if grant.spent() == 150 && grant.remaining() == 250
    ));

    // Renew QuotaLease (`[LEASE-RENEW-001]`): `new_ttl_grant_ms = 300`, `face_value` unchanged!
    holder_mono_ms.store(10_080, Ordering::Release);
    grantor_mono_ms.store(10_082, Ordering::Release);
    let renew_resp = client_a
        .renew_quota_lease(
            &mut holder_quota,
            [0x03; 16],
            IdempotencyKey::from_bytes([0x03; 16]),
            epoch_1,
            1_000,
            500,
            q_id,
            300, // new ttl_holder = 300 - 60 = 240 ms
        )
        .await
        .expect("renew QuotaLease");
    assert_eq!(holder_quota.ttl_holder().get(), 240);
    assert_eq!(holder_quota.renewal_seq(), 1);
    assert!(matches!(
        renew_resp.result,
        Ok(RemoteLeaseSuccess::Quota {
            grant,
            ttl_grant_ms: 300,
            renewal_seq: 1,
            grantor_available: 600,
            ..
        }) if grant.face_value() == 400 && grant.spent() == 150
    ));

    // Close QuotaLease via Ed25519 `SignedLeaseCloseReceipt` (`150` spent, `250` refunded -> `850`):
    holder_mono_ms.store(10_120, Ordering::Release);
    grantor_mono_ms.store(10_125, Ordering::Release);
    let (close_receipt, close_resp) = client_a
        .close_quota_lease(
            &mut holder_quota,
            ReceiptId::from_bytes([0xC1; 16]),
            [0x04; 16],
            IdempotencyKey::from_bytes([0x04; 16]),
            epoch_1,
            1_000,
            500,
        )
        .await
        .expect("close QuotaLease");
    assert_eq!(close_receipt.final_high_water_spent, 150);
    assert_eq!(close_receipt.returned_unspent, 250);
    assert!(matches!(
        close_resp.result,
        Ok(RemoteLeaseSuccess::Quota {
            grant,
            grantor_available: 850,
            ..
        }) if grant.state() == QuotaLeaseState::Closed
    ));

    // --- 2. CapacityLease: Grant -> Attach -> Return ---
    let cap_id = CapacityLeaseId::from_bytes([0x21; 16]);
    grantor_mono_ms.store(10_200, Ordering::Release);
    let (holder_cap, cap_grant_resp) = client_a
        .grant_capacity_lease(
            [0x05; 16],
            IdempotencyKey::from_bytes([0x05; 16]),
            epoch_1,
            1_000,
            500,
            cap_id,
            250,
            200,
            vec![GW_1],
        )
        .await
        .expect("grant remote CapacityLease");
    let HolderLeaseGrantKind::Capacity { grant: cap_grant } = holder_cap.kind() else {
        panic!("expected Capacity grant");
    };
    assert_eq!(cap_grant.state(), CapacityLeaseState::TargetPrepared);
    assert!(matches!(
        cap_grant_resp.result,
        Ok(RemoteLeaseSuccess::Capacity {
            grantor_pool_remaining: 550,
            ..
        })
    ));

    let attach_receipt = HostAttachReceipt::new(
        ReceiptId::from_bytes([0x22; 16]),
        cap_id,
        holder_a.cell,
        holder_a.boot_gen,
        cap_grant.capacity_epoch(),
        cap_grant.fencing_token(),
        250,
    );
    let (_, _, attach_resp) = client_a
        .send_signed_command(
            [0x06; 16],
            IdempotencyKey::from_bytes([0x06; 16]),
            epoch_1,
            1_000,
            None,
            500,
            RemoteLeaseCommand::AttachCapacityHost { attach_receipt },
        )
        .await
        .expect("attach CapacityLease");
    assert!(matches!(
        attach_resp.result,
        Ok(RemoteLeaseSuccess::Capacity { grant, .. }) if grant.state() == CapacityLeaseState::Active
    ));

    let (_, _, return_cap_resp) = client_a
        .send_signed_command(
            [0x07; 16],
            IdempotencyKey::from_bytes([0x07; 16]),
            epoch_1,
            1_000,
            None,
            500,
            RemoteLeaseCommand::ReturnCapacityLease {
                capacity_lease_id: cap_id,
            },
        )
        .await
        .expect("return CapacityLease");
    assert!(matches!(
        return_cap_resp.result,
        Ok(RemoteLeaseSuccess::Capacity {
            grant,
            grantor_pool_remaining: 800,
            ..
        }) if grant.state() == CapacityLeaseState::Returned
    ));

    // --- 3. ExclusiveDeviceLease: Grant -> SubmitDeviceResetReceipt ---
    let dev_lease_id = ExclusiveDeviceLeaseId::from_bytes([0x31; 16]);
    grantor_mono_ms.store(10_300, Ordering::Release);
    let (holder_dev, dev_grant_resp) = client_a
        .grant_exclusive_device_lease(
            [0x08; 16],
            IdempotencyKey::from_bytes([0x08; 16]),
            epoch_1,
            1_000,
            500,
            dev_id,
            dev_lease_id,
            200,
            vec![GW_1],
        )
        .await
        .expect("grant remote ExclusiveDeviceLease");
    let HolderLeaseGrantKind::ExclusiveDevice { grant: dev_grant } = holder_dev.kind() else {
        panic!("expected ExclusiveDevice grant");
    };
    assert!(matches!(
        dev_grant_resp.result,
        Ok(RemoteLeaseSuccess::ExclusiveDevice {
            head_is_free: false,
            ..
        })
    ));

    let reset_receipt = DeviceResetReceipt::new(
        ReceiptId::from_bytes([0x32; 16]),
        dev_lease_id,
        holder_a.cell,
        holder_a.boot_gen,
        dev_grant.exclusivity_epoch(),
        dev_grant.fencing_token(),
        dev_grant.reset_generation(),
    );
    let (_, _, reset_resp) = client_a
        .send_signed_command(
            [0x09; 16],
            IdempotencyKey::from_bytes([0x09; 16]),
            epoch_1,
            1_000,
            None,
            500,
            RemoteLeaseCommand::SubmitDeviceResetReceipt {
                device_id: dev_id,
                reset_receipt,
            },
        )
        .await
        .expect("submit DeviceResetReceipt");
    assert!(matches!(
        reset_resp.result,
        Ok(RemoteLeaseSuccess::ExclusiveDevice {
            grant,
            head_is_free: true,
            head_reset_generation,
            ..
        }) if grant.state() == ExclusiveDeviceLeaseState::Returned
            && head_reset_generation > dev_grant.reset_generation()
    ));

    // --- 4. Quarantined Multi-Endpoint Barrier ACK + Reconcile Over mTLS ---
    let cap_q_id = CapacityLeaseId::from_bytes([0x41; 16]);
    grantor_mono_ms.store(10_400, Ordering::Release);
    let (holder_cap_q, _) = client_a
        .grant_capacity_lease(
            [0x0A; 16],
            IdempotencyKey::from_bytes([0x0A; 16]),
            epoch_1,
            1_000,
            500,
            cap_q_id,
            300,
            100, // grantor wait floor = 125 ms -> expires at > 10_525 ms
            vec![GW_1, GW_2],
        )
        .await
        .expect("grant cap_q");
    let HolderLeaseGrantKind::Capacity { grant: cap_q_grant } = holder_cap_q.kind() else {
        panic!("expected Capacity");
    };

    // Sweep on grantor at `10_526` (`elapsed = 126 > 125`) AND advance Cell epoch!
    let fence_e2 = server.with_coordinator(|coord| {
        let f1 = coord.current_fence();
        let swept = coord
            .sweep_unreachable_horizons(&f1, MonotonicInstantMs::new(10_526))
            .expect("sweep cap_q");
        assert_eq!(
            swept,
            vec![(CrossHostLeaseFamily::Capacity, *cap_q_id.as_bytes())]
        );
        coord
            .advance_epoch_and_quarantine_all()
            .expect("advance grantor epoch")
    });
    grantor_mono_ms.store(10_600, Ordering::Release);

    // Gateway node connects over its own mTLS session at `fence_e2` and submits
    // `GW_1` + `GW_2` `SignedFenceBarrierAck`s + `ReconcileQuarantinedLease`:
    let (_, transport_gw) = build_client_transport(
        &gw_node,
        &grantor_node,
        grantor_addr,
        fence_e2,
        &verifier,
        NetTransportConfig::default(),
    );
    let client_gw = RemoteLeaseProtocolClient::new(
        gw_node.cell,
        gw_node.boot_gen,
        grantor_node.cell,
        transport_gw,
        policy,
        Arc::new(|| MonotonicInstantMs::new(10_600)),
        Arc::new(|| NOW_MS),
        gw_node.signer.clone(),
        verifier.clone(),
    );

    for (idx, gw) in [(0x11_u8, GW_1), (0x12_u8, GW_2)] {
        let signed_ack = SignedFenceBarrierAck::sign(
            CrossHostLeaseFamily::Capacity,
            FenceBarrierAck::for_capacity(&cap_q_grant, gw),
            gw_node.signer.as_ref(),
            AuditWallTimeHint::new(NOW_MS),
        )
        .expect("sign barrier ack");
        let (_, _, ack_resp) = client_gw
            .send_signed_command(
                [idx; 16],
                IdempotencyKey::from_bytes([idx; 16]),
                fence_e2.epoch(),
                1_000,
                None,
                500,
                RemoteLeaseCommand::SubmitFenceBarrierAck { signed_ack },
            )
            .await
            .expect("submit barrier ack over mTLS");
        if gw == GW_1 {
            assert!(matches!(
                ack_resp.result,
                Ok(RemoteLeaseSuccess::BarrierAckRecorded { ref coverage })
                    if coverage.acked_count == 1 && !coverage.is_complete()
            ));
        } else {
            assert!(matches!(
                ack_resp.result,
                Ok(RemoteLeaseSuccess::BarrierAckRecorded { ref coverage })
                    if coverage.acked_count == 2 && coverage.is_complete()
            ));
        }
    }

    // Advance `resolver_a` routing version for `fence_e2` and reconcile `cap_q_id`:
    resolver_a
        .upsert_hint(
            EndpointHintRecord::new(
                grantor_node.cell,
                grantor_addr,
                EndpointRoutingVersion::new(fence_e2.node_boot_generation(), fence_e2.epoch(), 2),
                2_000,
            )
            .with_expected_principal(grantor_node.binding.principal_id),
        )
        .expect("advance resolver_a hint");

    let cap_recon = CapacityReconciliationReceipt::for_lease(
        ReceiptId::from_bytes([0x49; 16]),
        &cap_q_grant,
        CleanupAttestation::for_lease(&cap_q_grant, 300, true),
        FenceBarrierAck::for_capacity(&cap_q_grant, GW_1),
    );
    let (_, _, recon_resp) = client_gw
        .send_signed_command(
            [0x13; 16],
            IdempotencyKey::from_bytes([0x13; 16]),
            fence_e2.epoch(),
            1_000,
            None,
            500,
            RemoteLeaseCommand::ReconcileQuarantinedLease {
                payload: RemoteReconcilePayload::CapacityViaBarrier { receipt: cap_recon },
            },
        )
        .await
        .expect("reconcile quarantined capacity over mTLS");
    assert!(matches!(
        recon_resp.result,
        Ok(RemoteLeaseSuccess::Capacity {
            grant,
            grantor_pool_remaining: 800,
            disposition: QuarantineDisposition::PhysicallyCleanedVerified {
                proof_kind: PhysicalCleanupProofKind::CapacityCleanupAttestation,
                ..
            },
            ..
        }) if grant.state() == CapacityLeaseState::Returned
    ));

    running_server.shutdown().await;
}

// ============================================================================
// Scenario 2: Cross-Cell Impersonation & Spoofed Envelope Rejection
//             (`[TCB-ENDPOINT-001]`)
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mtls_02_cross_cell_impersonation_and_spoofed_envelope_rejected_fail_closed() {
    let auth_guard = acquire_grantor_authority().await;
    let authority = auth_guard.as_ref().expect("authority").clone();

    let dir = TempTestDir::new("s2-cross-cell-impersonation");
    let identity =
        Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));

    let grantor_node =
        ProvisionedNode::bootstrap_grantor(&identity, &dir.sub("vault-grantor"), &authority);
    let cell_a = ProvisionedNode::bootstrap_holder(&identity, &dir.sub("vault-a"), 0xA2, 1, 1, 10);
    let cell_b = ProvisionedNode::bootstrap_holder(&identity, &dir.sub("vault-b"), 0xB2, 1, 1, 10);

    let verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![
            Arc::clone(&grantor_node.vault),
            Arc::clone(&cell_a.vault),
            Arc::clone(&cell_b.vault),
        ],
    ));

    let dev_id = DeviceId::from_bytes([0xD2; 16]);
    let coordinator =
        GrantorClockBoundedCoordinator::open(Arc::clone(&authority), 1_000, 1_000, &[dev_id]);
    let auth_clock = Arc::new(AuthorityClock::open(dir.sub("clock")).expect("open AuthorityClock"));
    let policy = skew_policy(25, 10);

    let server = Arc::new(
        RemoteLeaseProtocolServer::new(
            coordinator,
            auth_clock,
            policy,
            Arc::new(|| MonotonicInstantMs::new(20_000)),
            Arc::new(|| NOW_MS),
            grantor_node.signer.clone(),
            verifier.clone(),
        )
        .expect("server"),
    );

    let listener = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: grantor_node.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        grantor_node.security_deps(Arc::clone(&verifier)),
        NetTransportConfig::default(),
        SocketActivityAccounting::new(),
        Some(server.clone()),
        None,
    )
    .await
    .expect("bind");
    let grantor_addr = listener.local_addr();
    let running_server = listener.spawn_accept_loop();

    let cur_fence = authority.fence();
    let cur_epoch = cur_fence.epoch();

    let (_, transport_a) = build_client_transport(
        &cell_a,
        &grantor_node,
        grantor_addr,
        cur_fence,
        &verifier,
        NetTransportConfig::default(),
    );
    let client_a = RemoteLeaseProtocolClient::new(
        cell_a.cell,
        cell_a.boot_gen,
        grantor_node.cell,
        transport_a,
        policy,
        Arc::new(|| MonotonicInstantMs::new(20_000)),
        Arc::new(|| NOW_MS),
        cell_a.signer.clone(),
        verifier.clone(),
    );

    // Cell A legitimately obtains Quota, Capacity, and ExclusiveDevice leases:
    let q_a = QuotaLeaseId::from_bytes([0xA1; 16]);
    let c_a = CapacityLeaseId::from_bytes([0xA2; 16]);
    let d_a = ExclusiveDeviceLeaseId::from_bytes([0xA3; 16]);

    let (holder_q_a, _) = client_a
        .grant_quota_lease(
            [0x21; 16],
            IdempotencyKey::from_bytes([0x21; 16]),
            cur_epoch,
            1_000,
            500,
            q_a,
            300,
            200,
            vec![GW_1],
        )
        .await
        .expect("grant q_a");
    let _ = client_a
        .grant_capacity_lease(
            [0x22; 16],
            IdempotencyKey::from_bytes([0x22; 16]),
            cur_epoch,
            1_000,
            500,
            c_a,
            200,
            200,
            vec![GW_1],
        )
        .await
        .expect("grant c_a");
    let (holder_d_a, _) = client_a
        .grant_exclusive_device_lease(
            [0x23; 16],
            IdempotencyKey::from_bytes([0x23; 16]),
            cur_epoch,
            1_000,
            500,
            dev_id,
            d_a,
            200,
            vec![GW_1],
        )
        .await
        .expect("grant d_a");

    // Authenticated attacker Cell B connects over its own valid TLS 1.3 mTLS session:
    let (_, transport_b) = build_client_transport(
        &cell_b,
        &grantor_node,
        grantor_addr,
        cur_fence,
        &verifier,
        NetTransportConfig::default(),
    );
    let client_b = RemoteLeaseProtocolClient::new(
        cell_b.cell,
        cell_b.boot_gen,
        grantor_node.cell,
        transport_b.clone(),
        policy,
        Arc::new(|| MonotonicInstantMs::new(20_005)),
        Arc::new(|| NOW_MS),
        cell_b.signer.clone(),
        verifier.clone(),
    );

    // --- Attack 2a: Cell B spoofs `envelope.caller_cell = cell_a.cell` over Cell B's mTLS session ---
    let spoofed_env = RemoteLeaseRequestEnvelope::sign(
        IdempotencyKey::from_bytes([0xB0; 16]),
        cell_a.cell, // SPOOFED caller_cell!
        cell_a.boot_gen,
        cur_epoch,
        1_000,
        None,
        AuditWallTimeHint::new(NOW_MS),
        RemoteLeaseCommand::ReportQuotaHighWater {
            lease_id: q_a,
            high_water_spent: 250,
        },
        cell_b.signer.as_ref(),
    )
    .expect("sign spoofed envelope");
    let spoof_err = client_b
        .dispatch_raw_envelope([0xB0; 16], &spoofed_env, 500, true)
        .await
        .expect_err("spoofed caller_cell over Cell B mTLS session must be rejected");
    assert!(matches!(
        spoof_err,
        RemoteLeaseClientError::ServerRejected(RemoteLeaseReject::PeerSessionIdentityMismatch(_))
    ));

    // --- Attack 2b: Authenticated Cell B (`caller_cell = cell_b`) targets Cell A's leases ---
    // (i) ReportQuotaHighWater on `q_a`:
    let err_report = client_b
        .send_signed_command(
            [0xB1; 16],
            IdempotencyKey::from_bytes([0xB1; 16]),
            cur_epoch,
            1_000,
            None,
            500,
            RemoteLeaseCommand::ReportQuotaHighWater {
                lease_id: q_a,
                high_water_spent: 290,
            },
        )
        .await
        .expect_err("cross-cell report must fail closed");
    assert_eq!(
        err_report.to_string(),
        RemoteLeaseClientError::ServerRejected(RemoteLeaseReject::CrossCellImpersonation {
            caller_cell: cell_b.cell,
            expected_holder_cell: cell_a.cell,
        })
        .to_string()
    );

    // (ii) RenewQuotaLease on `q_a`:
    let err_renew = client_b
        .send_signed_command(
            [0xB2; 16],
            IdempotencyKey::from_bytes([0xB2; 16]),
            cur_epoch,
            1_000,
            None,
            500,
            RemoteLeaseCommand::RenewQuotaLease {
                lease_id: q_a,
                new_ttl_grant_ms: 500,
            },
        )
        .await
        .expect_err("cross-cell renew must fail closed");
    assert!(matches!(
        err_renew,
        RemoteLeaseClientError::ServerRejected(RemoteLeaseReject::CrossCellImpersonation {
            caller_cell,
            expected_holder_cell,
        }) if caller_cell == cell_b.cell && expected_holder_cell == cell_a.cell
    ));

    // (iii) CloseQuotaLease forging close receipt on `q_a`:
    let HolderLeaseGrantKind::Quota {
        grant: q_a_grant, ..
    } = holder_q_a.kind()
    else {
        panic!("expected Quota");
    };
    let forged_close = nlos_lease::SignedLeaseCloseReceipt::sign(
        &q_a_grant,
        ReceiptId::from_bytes([0xBF; 16]),
        0,
        300,
        cell_b.signer.as_ref(),
        AuditWallTimeHint::new(NOW_MS),
    )
    .expect("sign forged close");
    let err_close = client_b
        .send_signed_command(
            [0xB3; 16],
            IdempotencyKey::from_bytes([0xB3; 16]),
            cur_epoch,
            1_000,
            None,
            500,
            RemoteLeaseCommand::CloseQuotaLease {
                close_receipt: forged_close,
            },
        )
        .await
        .expect_err("cross-cell close must fail closed");
    assert!(matches!(
        err_close,
        RemoteLeaseClientError::ServerRejected(RemoteLeaseReject::CrossCellImpersonation { .. })
    ));

    // (iv) ReturnCapacityLease on `c_a`:
    let err_cap = client_b
        .send_signed_command(
            [0xB4; 16],
            IdempotencyKey::from_bytes([0xB4; 16]),
            cur_epoch,
            1_000,
            None,
            500,
            RemoteLeaseCommand::ReturnCapacityLease {
                capacity_lease_id: c_a,
            },
        )
        .await
        .expect_err("cross-cell capacity return must fail closed");
    assert!(matches!(
        err_cap,
        RemoteLeaseClientError::ServerRejected(RemoteLeaseReject::CrossCellImpersonation { .. })
    ));

    // (v) SubmitDeviceResetReceipt on `d_a`:
    let HolderLeaseGrantKind::ExclusiveDevice { grant: d_a_grant } = holder_d_a.kind() else {
        panic!("expected ExclusiveDevice");
    };
    let forged_reset = DeviceResetReceipt::new(
        ReceiptId::from_bytes([0xBE; 16]),
        d_a,
        cell_a.cell,
        cell_a.boot_gen,
        d_a_grant.exclusivity_epoch(),
        d_a_grant.fencing_token(),
        d_a_grant.reset_generation(),
    );
    let err_dev = client_b
        .send_signed_command(
            [0xB5; 16],
            IdempotencyKey::from_bytes([0xB5; 16]),
            cur_epoch,
            1_000,
            None,
            500,
            RemoteLeaseCommand::SubmitDeviceResetReceipt {
                device_id: dev_id,
                reset_receipt: forged_reset,
            },
        )
        .await
        .expect_err("cross-cell device reset must fail closed");
    assert!(matches!(
        err_dev,
        RemoteLeaseClientError::ServerRejected(RemoteLeaseReject::CrossCellImpersonation { .. })
    ));

    // Confirm Cell A's leases on Grantor are 100% untouched!
    server.with_coordinator(|coord| {
        let q = coord.quota().query(q_a).expect("q_a");
        assert_eq!(q.state(), QuotaLeaseState::Active);
        assert_eq!(q.spent(), 0);
        assert_eq!(coord.quota().available(), 700);
        assert_eq!(
            coord.capacity().query(c_a).expect("c_a").state(),
            CapacityLeaseState::TargetPrepared
        );
        assert!(!coord.device(dev_id).expect("dev").is_free());
    });

    running_server.shutdown().await;
}

// ============================================================================
// Scenario 3: Post-Commit TCP Drop (`AfterFirstPayloadByte`) ->
//             `DeadlineExceededUncertain` (`ReconcileBeforeRetry`) ->
//             Blind Retry Blocked -> Durable Reopen + `reconcile_and_replay_uncertain`
// ============================================================================

struct CommitThenDropBarrierHandler {
    inner: Arc<RemoteLeaseProtocolServer>,
    drop_key: IdempotencyKey,
    dropped_once: AtomicBool,
    committed_notify: Arc<Notify>,
    hold_response: Arc<Notify>,
}

impl RpcRequestHandler for CommitThenDropBarrierHandler {
    fn handle_rpc(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>> {
        let reply = self.inner.process_incoming_sync(&request);
        if request.idempotency_key == self.drop_key
            && !self.dropped_once.swap(true, Ordering::AcqRel)
        {
            let committed = Arc::clone(&self.committed_notify);
            let hold = Arc::clone(&self.hold_response);
            return Box::pin(async move {
                committed.notify_waiters();
                hold.notified().await;
                reply
            });
        }
        Box::pin(async move { reply })
    }
}

async fn spawn_one_shot_cut_tcp_proxy(
    upstream_addr: SocketAddr,
    cut_signal: Arc<Notify>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let proxy_addr = listener.local_addr().expect("proxy addr");
    let handle = tokio::spawn(async move {
        let Ok((mut client_sock, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut server_sock) = TcpStream::connect(upstream_addr).await else {
            return;
        };
        let (mut c_r, mut c_w) = client_sock.split();
        let (mut s_r, mut s_w) = server_sock.split();

        let c_to_s = async {
            let mut buf = [0u8; 8192];
            while let Ok(n) = c_r.read(&mut buf).await {
                if n == 0 || s_w.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        };
        let s_to_c = async {
            let mut buf = [0u8; 8192];
            while let Ok(n) = s_r.read(&mut buf).await {
                if n == 0 || c_w.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        };

        tokio::select! {
            () = c_to_s => {}
            () = s_to_c => {}
            () = cut_signal.notified() => {
                // Server has durably committed to SQLite! Abruptly close the TCP
                // connection before any RPC response frame reaches the client!
            }
        }
    });
    (proxy_addr, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mtls_03_post_commit_socket_drop_uncertain_blocks_blind_retry_and_reconciles_once() {
    let auth_guard = acquire_grantor_authority().await;
    let authority = auth_guard.as_ref().expect("authority").clone();

    let dir = TempTestDir::new("s3-uncertain-reconcile");
    let identity =
        Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));

    let grantor_node =
        ProvisionedNode::bootstrap_grantor(&identity, &dir.sub("vault-grantor"), &authority);
    let holder_a =
        ProvisionedNode::bootstrap_holder(&identity, &dir.sub("vault-a"), 0xA3, 1, 1, 10);

    let verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![Arc::clone(&grantor_node.vault), Arc::clone(&holder_a.vault)],
    ));

    let db_path = dir.sub("lease-db").join("leases.sqlite");
    let clock_dir = dir.sub("clock");
    let policy = skew_policy(25, 10);
    let key_uncertain = IdempotencyKey::from_bytes([0x7A; 16]);
    let lease_uncertain = QuotaLeaseId::from_bytes([0x7B; 16]);

    let committed_notify = Arc::new(Notify::new());
    let hold_response = Arc::new(Notify::new());

    // --- Phase 1: Grantor commits durably to `SQLite WAL+FULL`, then TCP proxy cuts socket! ---
    let cur_fence = authority.fence();
    let cur_epoch = cur_fence.epoch();

    let fast_cfg = NetTransportConfig {
        rpc_call_timeout: Duration::from_millis(500),
        timeout_elapsed_ticks: 15,
        ..NetTransportConfig::default()
    };

    let (resolver, transport_a) = {
        let coordinator = GrantorClockBoundedCoordinator::open_durable(
            &db_path,
            Arc::clone(&authority),
            1_000,
            500,
            &[],
        )
        .expect("open durable coordinator");
        let auth_clock = Arc::new(AuthorityClock::open(&clock_dir).expect("open clock"));
        let server_1 = Arc::new(
            RemoteLeaseProtocolServer::new(
                coordinator,
                auth_clock,
                policy,
                Arc::new(|| MonotonicInstantMs::new(30_000)),
                Arc::new(|| NOW_MS),
                grantor_node.signer.clone(),
                verifier.clone(),
            )
            .expect("server_1"),
        );

        let fault_handler = Arc::new(CommitThenDropBarrierHandler {
            inner: Arc::clone(&server_1),
            drop_key: key_uncertain,
            dropped_once: AtomicBool::new(false),
            committed_notify: Arc::clone(&committed_notify),
            hold_response: Arc::clone(&hold_response),
        });

        let listener_1 = MtlsEndpointListener::bind(
            ClusterProfile::MultiHostCluster {
                local_cell: grantor_node.cell,
                bind_hint: "127.0.0.1:0".parse().expect("loopback"),
            },
            grantor_node.security_deps(Arc::clone(&verifier)),
            fast_cfg,
            SocketActivityAccounting::new(),
            Some(fault_handler),
            None,
        )
        .await
        .expect("bind listener_1");
        let upstream_addr = listener_1.local_addr();
        let running_1 = listener_1.spawn_accept_loop();

        let (proxy_addr, proxy_task) =
            spawn_one_shot_cut_tcp_proxy(upstream_addr, Arc::clone(&committed_notify)).await;

        let (resolver, transport_a) = build_client_transport(
            &holder_a,
            &grantor_node,
            proxy_addr,
            cur_fence,
            &verifier,
            fast_cfg,
        );
        let client_a = RemoteLeaseProtocolClient::new(
            holder_a.cell,
            holder_a.boot_gen,
            grantor_node.cell,
            transport_a.clone(),
            policy,
            Arc::new(|| MonotonicInstantMs::new(30_000)),
            Arc::new(|| NOW_MS),
            holder_a.signer.clone(),
            verifier.clone(),
        );

        let req_env = RemoteLeaseRequestEnvelope::sign(
            key_uncertain,
            holder_a.cell,
            holder_a.boot_gen,
            cur_epoch,
            1_000,
            None,
            AuditWallTimeHint::new(NOW_MS),
            RemoteLeaseCommand::GrantQuotaLease {
                lease_id: lease_uncertain,
                face_value: 350,
                ttl_grant_ms: 200,
                multi_endpoint_gateways: vec![GW_1],
            },
            holder_a.signer.as_ref(),
        )
        .expect("sign uncertain request");

        let uncertain_err = client_a
            .dispatch_raw_envelope([0x71; 16], &req_env, 200, true)
            .await
            .expect_err("mid-call TCP cut after request write MUST yield uncertain outcome");

        proxy_task.await.expect("proxy task");
        hold_response.notify_waiters();
        running_1.shutdown().await;

        match uncertain_err {
            RemoteLeaseClientError::TransportOutcome(
                RemoteCallOutcome::DeadlineExceededUncertain {
                    side_effecting: true,
                    retry_directive: RemoteRetryDirective::ReconcileBeforeRetry,
                    idempotency_key,
                    ..
                },
            ) => assert_eq!(idempotency_key, key_uncertain),
            other => {
                panic!("expected DeadlineExceededUncertain + ReconcileBeforeRetry, got {other:?}")
            }
        }

        // Blind retry BEFORE reconciliation MUST be refused by `PendingReconciliationTracker`!
        let blind_err = client_a
            .dispatch_raw_envelope([0x72; 16], &req_env, 200, true)
            .await
            .expect_err("blind retry before reconciliation MUST be blocked");
        assert!(matches!(
            blind_err,
            RemoteLeaseClientError::DispatchRefused(TransportDispatchRefusal::BlindRetryBlocked(
                RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation { idempotency_key }
            )) if idempotency_key == key_uncertain
        ));

        (resolver, transport_a)
    };

    // --- Phase 2: Reopen `RemoteLeaseProtocolServer` from `SQLite WAL+FULL` & Reconcile! ---
    let store_reopened = Arc::new(SqliteLeaseStore::open(&db_path).expect("reopen sqlite store"));
    let coordinator_reopened = GrantorClockBoundedCoordinator::open_with_store(
        store_reopened,
        Arc::clone(&authority),
        999_999,
        999_999,
        &[],
    )
    .expect("rehydrate coordinator");
    // Confirm the 350 deduction committed durably before the crash/cut (`1000 - 350 = 650`):
    assert_eq!(coordinator_reopened.quota().available(), 650);

    let auth_clock_2 = Arc::new(AuthorityClock::open(&clock_dir).expect("reopen clock"));
    let server_2 = Arc::new(
        RemoteLeaseProtocolServer::new(
            coordinator_reopened,
            auth_clock_2,
            policy,
            Arc::new(|| MonotonicInstantMs::new(30_050)),
            Arc::new(|| NOW_MS),
            grantor_node.signer.clone(),
            verifier.clone(),
        )
        .expect("rehydrate server_2"),
    );

    let listener_2 = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: grantor_node.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        grantor_node.security_deps(Arc::clone(&verifier)),
        fast_cfg,
        SocketActivityAccounting::new(),
        Some(server_2.clone()),
        None,
    )
    .await
    .expect("bind listener_2");
    let healthy_addr = listener_2.local_addr();
    let running_2 = listener_2.spawn_accept_loop();

    resolver
        .upsert_hint(
            EndpointHintRecord::new(
                grantor_node.cell,
                healthy_addr,
                EndpointRoutingVersion::new(cur_fence.node_boot_generation(), cur_fence.epoch(), 2),
                2_000,
            )
            .with_expected_principal(grantor_node.binding.principal_id),
        )
        .expect("advance hint to healthy listener_2");

    let client_a = RemoteLeaseProtocolClient::new(
        holder_a.cell,
        holder_a.boot_gen,
        grantor_node.cell,
        transport_a.clone(),
        policy,
        Arc::new(|| MonotonicInstantMs::new(30_050)),
        Arc::new(|| NOW_MS),
        holder_a.signer.clone(),
        verifier.clone(),
    );

    let original_env = RemoteLeaseRequestEnvelope::sign(
        key_uncertain,
        holder_a.cell,
        holder_a.boot_gen,
        cur_epoch,
        1_000,
        None,
        AuditWallTimeHint::new(NOW_MS),
        RemoteLeaseCommand::GrantQuotaLease {
            lease_id: lease_uncertain,
            face_value: 350,
            ttl_grant_ms: 200,
            multi_endpoint_gateways: vec![GW_1],
        },
        holder_a.signer.as_ref(),
    )
    .expect("re-create identical signed envelope");

    let (snapshot, replayed_resp) = client_a
        .reconcile_and_replay_uncertain(
            &transport_a,
            CrossHostLeaseFamily::Quota,
            *lease_uncertain.as_bytes(),
            [0x73; 16],
            IdempotencyKey::from_bytes([0x73; 16]),
            [0x74; 16],
            &original_env,
            500,
        )
        .await
        .expect("reconcile_and_replay_uncertain must succeed");

    assert!(snapshot.lease_known);
    assert_eq!(snapshot.quota_grant.expect("quota_grant").face_value(), 350);
    assert_eq!(snapshot.probed_idempotency_committed_seq, Some(1));
    assert!(
        replayed_resp.idempotency_replayed,
        "durable idempotency cache MUST mark replayed response idempotency_replayed = true"
    );
    assert!(matches!(
        replayed_resp.result,
        Ok(RemoteLeaseSuccess::Quota {
            grant,
            grantor_available: 650,
            ..
        }) if grant.face_value() == 350
    ));

    // Reusing `key_uncertain` with a different command payload (`face_value = 999`)
    // MUST fail closed with `RemoteLeaseReject::IdempotencyConflict`!
    let conflicting_env = RemoteLeaseRequestEnvelope::sign(
        key_uncertain,
        holder_a.cell,
        holder_a.boot_gen,
        cur_epoch,
        1_000,
        None,
        AuditWallTimeHint::new(NOW_MS),
        RemoteLeaseCommand::GrantQuotaLease {
            lease_id: lease_uncertain,
            face_value: 999,
            ttl_grant_ms: 200,
            multi_endpoint_gateways: vec![GW_1],
        },
        holder_a.signer.as_ref(),
    )
    .expect("sign conflicting reuse");
    let conflict_err = client_a
        .dispatch_raw_envelope([0x75; 16], &conflicting_env, 500, true)
        .await
        .expect_err("conflicting IdempotencyKey reuse must fail closed");
    assert!(matches!(
        conflict_err,
        RemoteLeaseClientError::ServerRejected(RemoteLeaseReject::IdempotencyConflict)
    ));

    running_2.shutdown().await;
}

// ============================================================================
// Scenario 4: `ceil(RTT / 2) > max_clock_skew` Fail-Closed Quarantine Over mTLS
//             + `ClusterProfile::SingleHostDesktop` (`N = 1`) Zero-Socket Proof
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mtls_04_rtt_skew_violation_quarantines_over_mtls_and_single_host_opens_zero_sockets() {
    let auth_guard = acquire_grantor_authority().await;
    let authority = auth_guard.as_ref().expect("authority").clone();

    let dir = TempTestDir::new("s4-rtt-and-single-host");
    let identity =
        Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));

    let grantor_node =
        ProvisionedNode::bootstrap_grantor(&identity, &dir.sub("vault-grantor"), &authority);
    let holder_a =
        ProvisionedNode::bootstrap_holder(&identity, &dir.sub("vault-a"), 0xA4, 1, 1, 10);

    let verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![Arc::clone(&grantor_node.vault), Arc::clone(&holder_a.vault)],
    ));

    // --- Part A: `ClusterProfile::SingleHostDesktop` (`N = 1`) Zero-Socket Regression Proof ---
    let desktop_accounting = SocketActivityAccounting::new();
    let bind_res = MtlsEndpointListener::bind(
        ClusterProfile::SingleHostDesktop,
        grantor_node.security_deps(Arc::clone(&verifier)),
        NetTransportConfig::default(),
        desktop_accounting.clone(),
        None,
        None,
    )
    .await;
    assert!(matches!(bind_res, Err(NetInitError::SingleHostProfile(_))));

    let resolver_dyn: Arc<dyn EndpointResolver> = Arc::new(MonotonicEndpointResolver::new());
    let client_res = MtlsCellTransport::new_multi_host(
        ClusterProfile::SingleHostDesktop,
        resolver_dyn,
        holder_a.security_deps(Arc::clone(&verifier)),
        NetTransportConfig::default(),
        desktop_accounting.clone(),
        None,
        None,
    );
    assert!(matches!(
        client_res,
        Err(NetInitError::SingleHostProfile(_))
    ));
    assert_eq!(desktop_accounting.peak_bound_listener_count(), 0);
    assert_eq!(desktop_accounting.peak_active_connection_count(), 0);

    // --- Part B: `ceil(RTT / 2) > max_clock_skew` Over Real mTLS Quarantines Lease ---
    let policy = skew_policy(25, 10); // max_clock_skew_ms = 25 ms
    let coordinator = GrantorClockBoundedCoordinator::open(Arc::clone(&authority), 1_000, 500, &[]);
    let auth_clock = Arc::new(AuthorityClock::open(dir.sub("clock")).expect("open clock"));
    let server = Arc::new(
        RemoteLeaseProtocolServer::new(
            coordinator,
            auth_clock,
            policy,
            Arc::new(|| MonotonicInstantMs::new(40_000)),
            Arc::new(|| NOW_MS),
            grantor_node.signer.clone(),
            verifier.clone(),
        )
        .expect("server"),
    );

    let listener = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: grantor_node.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        grantor_node.security_deps(Arc::clone(&verifier)),
        NetTransportConfig::default(),
        SocketActivityAccounting::new(),
        Some(server.clone()),
        None,
    )
    .await
    .expect("bind");
    let grantor_addr = listener.local_addr();
    let running_server = listener.spawn_accept_loop();

    let cur_fence = authority.fence();
    let cur_epoch = cur_fence.epoch();
    let (_, transport_a) = build_client_transport(
        &holder_a,
        &grantor_node,
        grantor_addr,
        cur_fence,
        &verifier,
        NetTransportConfig::default(),
    );
    let client_a = RemoteLeaseProtocolClient::new(
        holder_a.cell,
        holder_a.boot_gen,
        grantor_node.cell,
        transport_a,
        policy,
        Arc::new(|| MonotonicInstantMs::new(40_000)),
        Arc::new(|| NOW_MS),
        holder_a.signer.clone(),
        verifier.clone(),
    );

    let q_rtt = QuotaLeaseId::from_bytes([0x91; 16]);
    let _ = client_a
        .grant_quota_lease(
            [0x91; 16],
            IdempotencyKey::from_bytes([0x91; 16]),
            cur_epoch,
            1_000,
            500,
            q_rtt,
            200,
            200,
            vec![GW_1],
        )
        .await
        .expect("grant q_rtt");

    // Report usage with `measured_rtt_hint_ms = Some(51)` (`ceil(51/2) = 26 > 25`):
    let rtt_reject = client_a
        .send_signed_command(
            [0x92; 16],
            IdempotencyKey::from_bytes([0x92; 16]),
            cur_epoch,
            1_000,
            Some(51),
            500,
            RemoteLeaseCommand::ReportQuotaHighWater {
                lease_id: q_rtt,
                high_water_spent: 50,
            },
        )
        .await
        .expect_err("RTT/2 > max_clock_skew over mTLS MUST fail closed");
    assert!(matches!(
        rtt_reject,
        RemoteLeaseClientError::ServerRejected(RemoteLeaseReject::ClockSkewQuarantined(_))
    ));

    // Confirm grantor immediately quarantined `q_rtt` with `LedgerQuarantinedFenceOnly`
    // and did NOT refund the 200 face value (`AVAILABLE` remains `800`)!
    server.with_coordinator(|coord| {
        assert_eq!(
            coord.quota().query(q_rtt).expect("q_rtt").state(),
            QuotaLeaseState::Quarantined
        );
        assert_eq!(coord.quota().available(), 800);
        assert_eq!(
            coord
                .quarantine_disposition(CrossHostLeaseFamily::Quota, *q_rtt.as_bytes())
                .expect("disp"),
            QuarantineDisposition::LedgerQuarantinedFenceOnly
        );
    });

    running_server.shutdown().await;
}
