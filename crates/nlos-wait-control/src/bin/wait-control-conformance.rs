//! Feature-gated cross-language `WaitControl` conformance server.
//!
//! The server mirrors the `takeover-control-conformance` transport shape: it
//! binds one platform adapter endpoint, announces a `READY` line followed by
//! a single `FIXTURE` key/value manifest, then serves a bounded number of
//! one-request IPC connections. TypeScript and Python clients rebuild the
//! protobuf envelopes from that manifest, drive the five `WaitControl`
//! methods plus their durable replays, and assert the bounded failure
//! envelope shape for the rejection classes.
//!
//! # Fixture scenes
//!
//! `NLOS_WAIT_CONTROL_SCENE` selects the preset durable state that is
//! prepared before the first connection is accepted (the Channel is always
//! created with the same idempotency key, so a restart on an existing
//! authority root replays it instead of forking):
//!
//! | Scene | Preset state |
//! |---|---|
//! | `fresh` (default) | one Channel, no waits |
//! | `registered` | one Channel + one `PENDING` wait (target 5) |
//! | `mixed` | one Channel + three waits covering every state |
//!
//! # Authorization posture
//!
//! The policy mirrors the Rust `wait_control` suite: exactly the capability
//! handle `{slot: 9, generation: 1}` authorizes; every other context is a
//! policy denial that must surface as a bounded `RIGHTS` failure envelope.
//!
//! # Canonical client script and exit contract
//!
//! Like the takeover server, the process exits non-zero unless the durable
//! registry matches the scene's canonical end state, so the postcondition
//! doubles as a server-side proof that the client script really committed:
//!
//! * `fresh`: clients register target 5 (woken by a notify up to 5) and
//!   target 7 (cancelled) — final states `[Woken, Cancelled]`;
//! * `registered`: clients must drive the preset wait to a terminal state;
//! * `mixed`: clients cancel the preset `PENDING` wait — final states
//!   `[Cancelled, Woken, Cancelled]` in enumeration order.
//!
//! # Environment
//!
//! * `NLOS_WAIT_CONTROL_SCENE`: fixture scene, `fresh` by default;
//! * `NLOS_WAIT_CONTROL_CONNECTIONS`: connections accepted per round
//!   (default 2, bounded 1..=32); every connection serves exactly one
//!   request, so this is also the client's per-round exchange budget;
//! * `NLOS_WAIT_CONTROL_ROUNDS`: accept rounds (default 1, bounded 1..=8).

use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use nlos_channel::{ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest};
use nlos_ipc::{
    IpcError, OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one,
};
use nlos_schema::sabi::v1::{CapabilityHandle, ExchangeResponse, SabiRequestContext};
use nlos_types::IdempotencyKey;
use nlos_wait::{
    BindingId, CancelWaitRequest, NotifyCommitsRequest, RegisterWaitRequest, WaitAuthority, WaitId,
    WaitState,
};
use nlos_wait_control::{WaitControlAuthorizer, WaitControlService, payload};

const CONNECTIONS_ENV: &str = "NLOS_WAIT_CONTROL_CONNECTIONS";
const ROUNDS_ENV: &str = "NLOS_WAIT_CONTROL_ROUNDS";
const SCENE_ENV: &str = "NLOS_WAIT_CONTROL_SCENE";
const CAPABILITY_SLOT: u64 = 9;
const CAPABILITY_GENERATION: u64 = 1;
const MONOTONIC_NOW_NS: u64 = 10;
const OBSERVED_AT_MS: i64 = 6_000;

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

static ALLOW_PEER: AllowPeer = AllowPeer;

/// The same capability posture the Rust `wait_control` suite uses: exactly
/// one known capability handle authorizes, anything else is a policy denial
/// that must surface as a bounded `RIGHTS` failure envelope.
struct ConformancePolicy;

impl ConformancePolicy {
    fn authorize(context: &SabiRequestContext) -> Result<(), &'static str> {
        if context.capability_handles.as_slice()
            == [CapabilityHandle {
                slot: CAPABILITY_SLOT,
                generation: CAPABILITY_GENERATION,
            }]
        {
            Ok(())
        } else {
            Err("missing wait control capability")
        }
    }
}

impl WaitControlAuthorizer for ConformancePolicy {
    fn authorize_register_wait(
        &self,
        context: &SabiRequestContext,
        _: &payload::RegisterWaitRequest,
    ) -> Result<(), &'static str> {
        Self::authorize(context)
    }

    fn authorize_notify_commits(
        &self,
        context: &SabiRequestContext,
        _: &payload::NotifyCommitsRequest,
    ) -> Result<(), &'static str> {
        Self::authorize(context)
    }

    fn authorize_cancel_wait(
        &self,
        context: &SabiRequestContext,
        _: &payload::CancelWaitRequest,
    ) -> Result<(), &'static str> {
        Self::authorize(context)
    }

    fn authorize_list_waits(
        &self,
        context: &SabiRequestContext,
        _: &payload::ListWaitsRequest,
    ) -> Result<(), &'static str> {
        Self::authorize(context)
    }

    fn authorize_inspect_wait(
        &self,
        context: &SabiRequestContext,
        _: &payload::InspectWaitRequest,
    ) -> Result<(), &'static str> {
        Self::authorize(context)
    }
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn binding(seed: u8) -> BindingId {
    BindingId::from_bytes([seed; 16])
}

#[derive(Clone, Copy)]
enum Scene {
    Fresh,
    Registered,
    Mixed,
}

impl Scene {
    fn parse(value: &str) -> Result<Self, Box<dyn Error>> {
        match value {
            "fresh" => Ok(Self::Fresh),
            "registered" => Ok(Self::Registered),
            "mixed" => Ok(Self::Mixed),
            other => Err(format!(
                "{SCENE_ENV} must be `fresh`, `registered` or `mixed`, got {other}"
            )
            .into()),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Registered => "registered",
            Self::Mixed => "mixed",
        }
    }
}

struct MixedWaits {
    cancelled: WaitId,
    woken: WaitId,
    pending: WaitId,
}

struct Fixture {
    scene: Scene,
    channel: ChannelRecord,
    registered_wait: Option<WaitId>,
    mixed_waits: Option<MixedWaits>,
}

fn register(
    waits: &WaitAuthority,
    channel: &ChannelRecord,
    binding_seed: u8,
    target: u64,
    key_seed: u8,
) -> Result<WaitId, Box<dyn Error>> {
    let decision = waits.register_wait(RegisterWaitRequest {
        binding: binding(binding_seed),
        channel_id: channel.channel_id,
        target_sequence: target,
        idempotency_key: key(key_seed),
        registered_at_ms: 1_000,
    })?;
    Ok(decision.record().wait_id)
}

fn prepare_fixture(
    channel_authority: &ChannelAuthority,
    waits: &WaitAuthority,
    scene: Scene,
) -> Result<Fixture, Box<dyn Error>> {
    let channel = match channel_authority.create_channel(CreateChannelRequest {
        capacity_bytes: 4_096,
        policy_digest: [0x44; 32],
        idempotency_key: key(0xB0),
        created_at_ms: 900,
    })? {
        ChannelDecision::Created(record) | ChannelDecision::Replayed(record) => record,
    };
    let registered_wait = match scene {
        Scene::Registered => Some(register(waits, &channel, 0xB1, 5, 0xD1)?),
        Scene::Fresh | Scene::Mixed => None,
    };
    let mixed_waits = match scene {
        Scene::Mixed => {
            let cancelled_wait_id = register(waits, &channel, 0xC1, 1, 0xE1)?;
            waits.cancel_wait(CancelWaitRequest {
                wait_id: cancelled_wait_id,
                cancelled_at_ms: 3_000,
                idempotency_key: key(0xE4),
            })?;
            let woken_wait_id = register(waits, &channel, 0xC2, 2, 0xE2)?;
            waits.notify_commits(NotifyCommitsRequest {
                channel_id: channel.channel_id,
                up_to_sequence: 2,
                notified_at_ms: 2_000,
                idempotency_key: key(0xE5),
            })?;
            let pending_wait_id = register(waits, &channel, 0xC3, 3, 0xE3)?;
            Some(MixedWaits {
                cancelled: cancelled_wait_id,
                woken: woken_wait_id,
                pending: pending_wait_id,
            })
        }
        Scene::Fresh | Scene::Registered => None,
    };
    Ok(Fixture {
        scene,
        channel,
        registered_wait,
        mixed_waits,
    })
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn announce_ready() -> io::Result<()> {
    println!("READY");
    io::stdout().flush()
}

fn announce_fixture(fixture: &Fixture) -> io::Result<()> {
    let mut line = format!(
        "FIXTURE scene={} channel_id={} channel_generation={} channel_fencing_token={}",
        fixture.scene.label(),
        hex(fixture.channel.channel_id.as_bytes()),
        fixture.channel.generation.get(),
        hex(&fixture.channel.fencing_token),
    );
    if let Some(wait_id) = fixture.registered_wait {
        let _ = write!(
            line,
            " wait_id={} wait_state=pending",
            hex(wait_id.as_bytes())
        );
    }
    if let Some(mixed) = fixture.mixed_waits.as_ref() {
        let _ = write!(
            line,
            " cancelled_wait_id={} woken_wait_id={} pending_wait_id={}",
            hex(mixed.cancelled.as_bytes()),
            hex(mixed.woken.as_bytes()),
            hex(mixed.pending.as_bytes()),
        );
    }
    println!("{line}");
    io::stdout().flush()
}

/// Asserts the scene's canonical durable end state server-side, proving the
/// client script actually committed (the takeover server's `LocallyCovered`
/// postcheck, adapted to the wait registry). `(state, target_sequence)`
/// pairs are asserted in the authority's durable enumeration order.
fn verify_scene_end_state(waits: &WaitAuthority, scene: Scene) -> Result<(), Box<dyn Error>> {
    let records = waits.list_waits(None)?;
    match scene {
        Scene::Fresh | Scene::Mixed => {
            let expected: &[(WaitState, u64)] = match scene {
                Scene::Fresh => &[(WaitState::Woken, 5), (WaitState::Cancelled, 7)],
                Scene::Mixed => &[
                    (WaitState::Cancelled, 1),
                    (WaitState::Woken, 2),
                    (WaitState::Cancelled, 3),
                ],
                Scene::Registered => unreachable!("handled by the match arm above"),
            };
            let actual: Vec<(WaitState, u64)> = records
                .iter()
                .map(|record| (record.state, record.target_sequence))
                .collect();
            if actual != expected {
                return Err(format!(
                    "scene {} ended in {actual:?}, expected {expected:?}",
                    scene.label()
                )
                .into());
            }
            Ok(())
        }
        Scene::Registered => {
            if records.len() != 1 {
                return Err(format!(
                    "registered scene expects exactly one durable wait, got {}",
                    records.len()
                )
                .into());
            }
            if records[0].state == WaitState::Pending {
                return Err(
                    "registered scene expects the preset wait driven to a terminal state".into(),
                );
            }
            Ok(())
        }
    }
}

async fn serve_exchange<S>(
    stream: S,
    peer: PeerIdentity,
    waits: Arc<WaitAuthority>,
) -> Result<(), IpcError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    serve_one(
        stream,
        TransportConfig::default(),
        peer,
        &ALLOW_PEER,
        move |validated| {
            let response = WaitControlService::new(Arc::clone(&waits), ConformancePolicy)
                .handle_for_ipc(validated.envelope(), MONOTONIC_NOW_NS, OBSERVED_AT_MS);
            async move {
                Ok(OutboundResponse::Typed(ExchangeResponse {
                    envelope: Some(response),
                }))
            }
        },
    )
    .await
}

fn connection_count() -> Result<usize, Box<dyn Error>> {
    let count = std::env::var(CONNECTIONS_ENV)
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(2);
    if !(1..=32).contains(&count) {
        return Err(format!("{CONNECTIONS_ENV} must be within 1..=32, got {count}").into());
    }
    Ok(count)
}

fn round_count() -> Result<usize, Box<dyn Error>> {
    let count = std::env::var(ROUNDS_ENV)
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(1);
    if !(1..=8).contains(&count) {
        return Err(format!("{ROUNDS_ENV} must be within 1..=8, got {count}").into());
    }
    Ok(count)
}

fn scene() -> Result<Scene, Box<dyn Error>> {
    let value = std::env::var(SCENE_ENV).unwrap_or_else(|_| "fresh".to_owned());
    Scene::parse(&value)
}

#[cfg(unix)]
async fn run(endpoint: OsString, authority_root: OsString) -> Result<(), Box<dyn Error>> {
    use nlos_ipc::unix::UnixListenerAdapter;
    use std::fs;

    struct EndpointGuard(PathBuf);

    impl Drop for EndpointGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    let endpoint = PathBuf::from(endpoint);
    let root = PathBuf::from(authority_root);
    let scene = scene()?;
    let channel_authority = Arc::new(ChannelAuthority::open(&root)?);
    let waits = Arc::new(WaitAuthority::open(&root, Arc::clone(&channel_authority))?);
    let fixture = prepare_fixture(channel_authority.as_ref(), waits.as_ref(), scene)?;
    let connections = connection_count()?;
    let rounds = round_count()?;
    let listener = UnixListenerAdapter::bind(&endpoint)?;
    let _guard = EndpointGuard(endpoint);
    announce_ready()?;
    announce_fixture(&fixture)?;
    for _ in 0..rounds {
        let mut handlers = tokio::task::JoinSet::new();
        for _ in 0..connections {
            let (stream, peer) = listener.accept(TransportConfig::default()).await?;
            handlers.spawn(serve_exchange(stream, peer, Arc::clone(&waits)));
        }
        while let Some(result) = handlers.join_next().await {
            result.map_err(|error| format!("WaitControl handler task panicked: {error}"))??;
        }
    }
    verify_scene_end_state(waits.as_ref(), scene)
}

#[cfg(windows)]
async fn run(endpoint: OsString, authority_root: OsString) -> Result<(), Box<dyn Error>> {
    use nlos_ipc::windows::NamedPipeListenerAdapter;

    let root = PathBuf::from(authority_root);
    let scene = scene()?;
    let channel_authority = Arc::new(ChannelAuthority::open(&root)?);
    let waits = Arc::new(WaitAuthority::open(&root, Arc::clone(&channel_authority))?);
    let fixture = prepare_fixture(channel_authority.as_ref(), waits.as_ref(), scene)?;
    let connections = connection_count()?;
    let rounds = round_count()?;
    // `accept` creates the next pipe instance before returning the current
    // one, so retain one spare instance while all requested handlers run.
    let mut listener =
        NamedPipeListenerAdapter::bind(endpoint, connections + 1, TransportConfig::default())?;
    announce_ready()?;
    announce_fixture(&fixture)?;
    for _ in 0..rounds {
        let mut handlers = tokio::task::JoinSet::new();
        for _ in 0..connections {
            let (stream, peer) = listener.accept(TransportConfig::default()).await?;
            handlers.spawn(serve_exchange(stream, peer, Arc::clone(&waits)));
        }
        while let Some(result) = handlers.join_next().await {
            result.map_err(|error| format!("WaitControl handler task panicked: {error}"))??;
        }
    }
    verify_scene_end_state(waits.as_ref(), scene)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let endpoint = arguments.next().ok_or("missing IPC endpoint")?;
    let authority_root = arguments.next().ok_or("missing authority root path")?;
    if arguments.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    run(endpoint, authority_root).await
}
