#![cfg(all(windows, feature = "authenticated-server"))]

//! Minimal Windows named-pipe loopback for the ADR-0011 authenticated
//! `WaitControl` variant (mirrors the Unix `authenticated_wait_control`
//! roundtrip): `AuthenticatedWaitControlServer::serve_one` plus
//! `authenticated_connect` over a real local pipe with genuine
//! `IdentityAuthority` verification and an `AuthorityClock` durable wall
//! reading. One register MUTATION and one list QUERY cross the pipe, and the
//! durable row is asserted to carry the principal-bound idempotency key, not
//! the raw client key bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_channel::{ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest};
use nlos_clock::{AuthorityClock, NowRequest};
use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
};
use nlos_ipc::handshake::transport::{ServerHandshakeContext, authenticated_connect};
use nlos_ipc::windows::NamedPipeListenerAdapter;
use nlos_ipc::{LocalRpcClient, PeerAuthorizer, PeerIdentity, TransportConfig};
use nlos_schema::SABI_ENVELOPE_SCHEMA;
use nlos_schema::sabi::v1::{
    CallerIdentity, CapabilityHandle, Envelope, ExchangeRequest, SabiRequestContext,
    SchemaIdentity, envelope,
};
use nlos_types::{IdempotencyKey, PrincipalId};
use nlos_wait::{BindingId, WaitAuthority};
use nlos_wait_control::authenticated::{
    AuthenticatedWaitControlServer, principal_bound_idempotency_key,
};
use nlos_wait_control::{
    LIST_WAITS_METHOD, REGISTER_WAIT_METHOD, WAIT_CONTROL_SERVICE, WaitControlAuthorizer,
    decode_list_waits_result, decode_register_wait_result, encode_list_waits_request,
    encode_register_wait_request, payload, wait_control_schema_identity,
};

const NONCE: [u8; 32] = [0x5D; 32];

/// Epoch-ms of 2100-01-01: a "not expiring" key window that still fits the
/// identity authority's `SQLite` i64 encoding.
const KEY_VALID_UNTIL_MS: u64 = 4_102_444_800_000;

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn binding(seed: u8) -> BindingId {
    BindingId::from_bytes([seed; 16])
}

struct Root(PathBuf);

static NEXT_ROOT: AtomicU8 = AtomicU8::new(1);

impl Root {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-wait-control-auth-windows-{label}-{}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn pipe_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    PathBuf::from(format!(
        r"\\.\pipe\nlos-wc-auth-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn transport_config() -> TransportConfig {
    TransportConfig::new(
        64 * 1024,
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap()
}

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

struct AllowCapability;

impl WaitControlAuthorizer for AllowCapability {
    fn authorize_register_wait(
        &self,
        _: &SabiRequestContext,
        _: &payload::RegisterWaitRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_notify_commits(
        &self,
        _: &SabiRequestContext,
        _: &payload::NotifyCommitsRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_cancel_wait(
        &self,
        _: &SabiRequestContext,
        _: &payload::CancelWaitRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_list_waits(
        &self,
        _: &SabiRequestContext,
        _: &payload::ListWaitsRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_inspect_wait(
        &self,
        _: &SabiRequestContext,
        _: &payload::InspectWaitRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }
}

struct Principal {
    identity: IdentityAuthority,
    signing: SigningKey,
    binding: IdentityBinding,
}

/// Bootstraps one principal with an Ed25519 key valid over the given window
/// (epoch milliseconds). The identity and clock authorities live in their
/// own subdirectories so all four authorities coexist on one durable root.
fn bootstrap(root: &Root, seed: u8, valid_until_ms: u64) -> Principal {
    let identity = IdentityAuthority::open(root.path().join("identity")).unwrap();
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let BootstrapDecision::Created(binding) = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: signing.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: valid_until_ms,
            idempotency_key: key(seed.wrapping_add(3)),
            created_at_ms: 0,
        })
        .unwrap()
    else {
        unreachable!("fresh identity authority bootstraps a new principal");
    };
    Principal {
        identity,
        signing,
        binding,
    }
}

/// Opens the clock authority and advances its wall domain once, so the
/// verified-at reading is the real current epoch-millisecond high-water.
fn clock_with_advanced_wall(root: &Root) -> AuthorityClock {
    let clock = AuthorityClock::open(root.path().join("clock")).unwrap();
    clock
        .wall_now(NowRequest {
            idempotency_key: key(0xEE),
        })
        .unwrap();
    clock
}

fn open_waits(root: &Root) -> (Arc<ChannelAuthority>, Arc<WaitAuthority>) {
    let channel = Arc::new(ChannelAuthority::open(root.path()).unwrap());
    let wait = Arc::new(WaitAuthority::open(root.path(), Arc::clone(&channel)).unwrap());
    (channel, wait)
}

fn create_channel(channel: &ChannelAuthority, seed: u8) -> ChannelRecord {
    match channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4_096,
            policy_digest: [0x44; 32],
            idempotency_key: key(seed),
            created_at_ms: 900,
        })
        .unwrap()
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
    }
}

fn request_context(idempotency_key: Vec<u8>) -> SabiRequestContext {
    SabiRequestContext {
        caller: Some(CallerIdentity {
            principal_id: vec![0x31; 16],
            application_id: vec![0x32; 16],
            process_id: vec![0x33; 16],
            process_generation: 1,
        }),
        activity_context: Vec::new(),
        task_execution_binding: None,
        correlation_id: vec![0x34; 16],
        idempotency_key,
        deadline_monotonic_ns: 0,
        capability_handles: vec![CapabilityHandle {
            slot: 9,
            generation: 1,
        }],
        reservation_handle: None,
        proposal_or_input_digest_sha256: Vec::new(),
    }
}

fn envelope(
    method: &str,
    request_id: u8,
    context: SabiRequestContext,
    payload_bytes: Vec<u8>,
) -> ExchangeRequest {
    ExchangeRequest {
        envelope: Some(Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: 1,
                minor: 1,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: vec![request_id; 16],
            service: WAIT_CONTROL_SERVICE.to_owned(),
            method: method.to_owned(),
            common_context: Some(envelope::CommonContext::RequestContext(context)),
            payload: payload_bytes,
        }),
    }
}

fn register_envelope(channel: &ChannelRecord, key_seed: u8) -> ExchangeRequest {
    let payload_bytes = encode_register_wait_request(&payload::RegisterWaitRequest {
        schema: Some(wait_control_schema_identity()),
        binding: binding(1).as_bytes().to_vec(),
        channel_id: channel.channel_id.as_bytes().to_vec(),
        target_sequence: 5,
        idempotency_key: key(key_seed).as_bytes().to_vec(),
        registered_at_ms: 1_000,
    })
    .unwrap();
    envelope(
        REGISTER_WAIT_METHOD,
        0x35,
        request_context(key(key_seed).as_bytes().to_vec()),
        payload_bytes,
    )
}

fn list_envelope() -> ExchangeRequest {
    let payload_bytes = encode_list_waits_request(&payload::ListWaitsRequest {
        schema: Some(wait_control_schema_identity()),
        filter_channel_id: Vec::new(),
    })
    .unwrap();
    envelope(
        LIST_WAITS_METHOD,
        0x36,
        request_context(Vec::new()),
        payload_bytes,
    )
}

async fn connect_authenticated(
    path: &Path,
    principal_id: PrincipalId,
    signing: &SigningKey,
) -> LocalRpcClient<tokio::net::windows::named_pipe::NamedPipeClient> {
    let framed = authenticated_connect(
        path,
        transport_config(),
        principal_id,
        |digest: &[u8; 32]| Ok(signing.sign(digest).to_bytes()),
    )
    .await
    .unwrap();
    LocalRpcClient::new(framed.into_inner(), transport_config())
}

#[tokio::test]
async fn authenticated_register_and_list_cross_a_real_windows_named_pipe() {
    let root = Root::new("roundtrip");
    let path = pipe_path("roundtrip");
    let principal = bootstrap(&root, 0x41, KEY_VALID_UNTIL_MS);
    let clock = clock_with_advanced_wall(&root);
    let handshake = ServerHandshakeContext::new(&path, 8).unwrap();
    let (channel_authority, waits) = open_waits(&root);
    let channel = create_channel(&channel_authority, 0xA1);

    // Two sequential serve cycles (register MUTATION then list QUERY); each
    // accept creates the next pipe instance, so retain spare instances.
    let mut listener = NamedPipeListenerAdapter::bind(&path, 4, transport_config()).unwrap();
    let server = AuthenticatedWaitControlServer::new(
        Arc::clone(&waits),
        AllowCapability,
        principal.identity,
        clock,
        handshake,
    );
    let server_task = tokio::spawn(async move {
        let sequence = AtomicU8::new(0);
        let mut outcomes = Vec::new();
        for _ in 0..2 {
            let next_nonce = || {
                let mut issued = NONCE;
                issued[0] = sequence.fetch_add(1, Ordering::Relaxed) + 1;
                issued
            };
            outcomes.push(
                server
                    .serve_one(
                        &mut listener,
                        transport_config(),
                        &AllowPeer,
                        10,
                        next_nonce,
                    )
                    .await,
            );
        }
        outcomes
    });

    // Connection 1 (handshake + register MUTATION): the durable row carries
    // the principal-bound key, not the raw client key bytes.
    let response = connect_authenticated(&path, principal.binding.principal_id, &principal.signing)
        .await
        .exchange_validated(register_envelope(&channel, 1))
        .await
        .unwrap();
    let result = decode_register_wait_result(&response.envelope().payload).unwrap();
    assert!(!result.replayed);
    let record = result.record.expect("registered record");
    let raw_key = key(1).as_bytes().to_vec();
    assert_ne!(record.idempotency_key, raw_key);
    assert_eq!(
        record.idempotency_key,
        principal_bound_idempotency_key(principal.binding.principal_id, *key(1).as_bytes())
            .to_vec()
    );

    // Connection 2 (handshake + list QUERY): the authenticated enumeration
    // reports the same principal-bound durable row.
    let listed = connect_authenticated(&path, principal.binding.principal_id, &principal.signing)
        .await
        .exchange_validated(list_envelope())
        .await
        .unwrap();
    let list_result = decode_list_waits_result(&listed.envelope().payload).unwrap();
    assert_eq!(list_result.waits.len(), 1);
    assert_eq!(list_result.waits[0].idempotency_key, record.idempotency_key);

    // The durable registry itself carries the bound key.
    let durable = waits.list_waits(None).unwrap();
    assert_eq!(durable.len(), 1);
    assert_eq!(
        durable[0].idempotency_key.as_bytes().as_slice(),
        record.idempotency_key.as_slice()
    );

    let outcomes = server_task.await.unwrap();
    assert!(outcomes[0].is_ok(), "first connection: {:?}", outcomes[0]);
    assert!(outcomes[1].is_ok(), "second connection: {:?}", outcomes[1]);
    assert_eq!(
        outcomes[0].as_ref().unwrap().verified().principal_id(),
        principal.binding.principal_id
    );
}
