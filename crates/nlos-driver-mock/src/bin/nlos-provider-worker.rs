//! Standalone out-of-process NLOS provider worker binary (`#9 C-PROVIDER-REAL`).
//!
//! Opens durable `SqliteOperationStore`, `IdentityAuthority`, and
//! `AuthorityClock` roots, binds cross-platform authenticated IPC
//! (`UnixListenerAdapter` on Unix, `NamedPipeListenerAdapter` on Windows),
//! prints `READY endpoint=<...> pid=<os_pid>`, and serves frozen
//! `nlos.sabi.DriverProvider` v1.0 protobuf envelopes behind ADR-0011
//! principal challenge-response authentication.

use std::error::Error;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nlos_clock::{AuthorityClock, NowRequest};
use nlos_driver_mock::MockProvider;
use nlos_driver_mock::process_provider::{DriverProviderService, decode_hex_exact};
use nlos_driver_mock::proto_codec::ProviderIsolationProfile;
use nlos_identity::IdentityAuthority;
use nlos_ipc::handshake::transport::ServerHandshakeContext;
use nlos_ipc::handshake::{
    decode_attestation_wire, encode_challenge_wire, issue_challenge, verify_attestation,
};
#[cfg(unix)]
use nlos_ipc::unix::UnixListenerAdapter;
#[cfg(windows)]
use nlos_ipc::windows::NamedPipeListenerAdapter;
use nlos_ipc::{
    FramedIo, IoOperation, IpcError, OutboundResponse, PeerAuthorizer, PeerIdentity,
    TransportConfig, serve_one,
};
use nlos_schema::HANDSHAKE_NONCE_BYTES;
use nlos_schema::sabi::v1::ExchangeResponse;
use nlos_store::SqliteOperationStore;
use nlos_types::{IdempotencyKey, PrincipalId};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

const WORKER_CLOCK_BOOTSTRAP_KEY: [u8; 16] = *b"nlos-prov-wrk-w0";
const NONCE_DOMAIN: &[u8] = b"nlos/provider-worker/handshake-nonce/v1";

struct AllowLocalPeer;

impl PeerAuthorizer for AllowLocalPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

struct WorkerArgs {
    store_root: PathBuf,
    identity_root: PathBuf,
    clock_root: PathBuf,
    endpoint: PathBuf,
    isolation_profile: ProviderIsolationProfile,
    attestation_override: Option<[u8; 32]>,
    allowed_principal: Option<PrincipalId>,
    max_requests: Option<usize>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args(std::env::args().skip(1))?;
    let store = Arc::new(SqliteOperationStore::open(&args.store_root)?);
    let identity = IdentityAuthority::open(&args.identity_root)?;
    let clock = AuthorityClock::open(&args.clock_root)?;
    let startup_wall_ms = clock
        .wall_now(NowRequest {
            idempotency_key: IdempotencyKey::from_bytes(WORKER_CLOCK_BOOTSTRAP_KEY),
        })?
        .reading()
        .as_u64();

    let provider = Arc::new(MockProvider::new(store));
    let attestation = args
        .isolation_profile
        .to_attestation(args.attestation_override);
    let service = DriverProviderService::new(provider, attestation, args.allowed_principal);
    let handshake = ServerHandshakeContext::new(&args.endpoint, 64)?;
    let config = worker_transport_config()?;

    run_platform_loop(
        &args,
        &service,
        &identity,
        &clock,
        &handshake,
        config,
        startup_wall_ms,
    )
    .await
}

fn worker_transport_config() -> Result<TransportConfig, IpcError> {
    TransportConfig::new(
        64 * 1024,
        Duration::from_secs(60),
        Duration::from_secs(10),
        Duration::from_secs(10),
    )
}

fn announce_ready(endpoint: &std::path::Path) -> io::Result<()> {
    println!(
        "READY endpoint={} pid={}",
        endpoint.display(),
        std::process::id()
    );
    io::stdout().flush()
}

fn derive_nonce(
    endpoint: &std::path::Path,
    startup_wall_ms: u64,
    sequence: u64,
) -> [u8; HANDSHAKE_NONCE_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(NONCE_DOMAIN);
    hasher.update(std::process::id().to_be_bytes());
    hasher.update(startup_wall_ms.to_be_bytes());
    hasher.update(endpoint.to_string_lossy().as_bytes());
    hasher.update(sequence.to_be_bytes());
    hasher.finalize().into()
}

#[cfg(unix)]
async fn run_platform_loop(
    args: &WorkerArgs,
    service: &DriverProviderService,
    identity: &IdentityAuthority,
    clock: &AuthorityClock,
    handshake: &ServerHandshakeContext,
    config: TransportConfig,
    startup_wall_ms: u64,
) -> Result<(), Box<dyn Error>> {
    struct SocketGuard(PathBuf);
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // Clean up any stale socket left behind if a previous worker incarnation
    // was hard-killed with SIGKILL.
    let _ = std::fs::remove_file(&args.endpoint);
    let listener = UnixListenerAdapter::bind(&args.endpoint)?;
    let _guard = SocketGuard(args.endpoint.clone());
    announce_ready(&args.endpoint)?;

    let mut sequence = 0_u64;
    loop {
        if args
            .max_requests
            .is_some_and(|limit| usize::try_from(sequence).unwrap_or(usize::MAX) >= limit)
        {
            break;
        }
        let (stream, peer) = match listener.accept(config).await {
            Ok(accepted) => accepted,
            Err(IpcError::Timeout(IoOperation::Accept)) => continue,
            Err(error) => return Err(Box::new(error)),
        };
        sequence = sequence.saturating_add(1);
        let nonce = derive_nonce(&args.endpoint, startup_wall_ms, sequence);
        let _ = serve_authenticated_stream(
            stream, peer, config, service, identity, clock, handshake, nonce,
        )
        .await;
    }
    Ok(())
}

#[cfg(windows)]
async fn run_platform_loop(
    args: &WorkerArgs,
    service: &DriverProviderService,
    identity: &IdentityAuthority,
    clock: &AuthorityClock,
    handshake: &ServerHandshakeContext,
    config: TransportConfig,
    startup_wall_ms: u64,
) -> Result<(), Box<dyn Error>> {
    let mut listener = bind_named_pipe_with_retry(&args.endpoint, config).await?;
    announce_ready(&args.endpoint)?;

    let mut sequence = 0_u64;
    loop {
        if args
            .max_requests
            .is_some_and(|limit| usize::try_from(sequence).unwrap_or(usize::MAX) >= limit)
        {
            break;
        }
        let (stream, peer) = match listener.accept(config).await {
            Ok(accepted) => accepted,
            Err(IpcError::Timeout(IoOperation::Accept)) => {
                // Recreate the listening pipe instance if an idle accept window expired.
                listener = bind_named_pipe_with_retry(&args.endpoint, config).await?;
                continue;
            }
            Err(error) => return Err(Box::new(error)),
        };
        sequence = sequence.saturating_add(1);
        let nonce = derive_nonce(&args.endpoint, startup_wall_ms, sequence);
        let _ = serve_authenticated_stream(
            stream, peer, config, service, identity, clock, handshake, nonce,
        )
        .await;
    }
    Ok(())
}

#[cfg(windows)]
async fn bind_named_pipe_with_retry(
    endpoint: &std::path::Path,
    config: TransportConfig,
) -> Result<NamedPipeListenerAdapter, IpcError> {
    let mut attempts = 0_u32;
    loop {
        match NamedPipeListenerAdapter::bind(endpoint, 16, config) {
            Ok(listener) => return Ok(listener),
            Err(error) if attempts < 50 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
                let _ = error;
            }
            Err(error) => return Err(error),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_authenticated_stream<S>(
    stream: S,
    peer: PeerIdentity,
    config: TransportConfig,
    service: &DriverProviderService,
    identity: &IdentityAuthority,
    clock: &AuthorityClock,
    handshake: &ServerHandshakeContext,
    nonce: [u8; HANDSHAKE_NONCE_BYTES],
) -> Result<(), Box<dyn Error>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    AllowLocalPeer
        .authorize(&peer)
        .map_err(|err| io::Error::new(io::ErrorKind::PermissionDenied, err))?;

    let mut framed = FramedIo::new(stream, config);
    let challenge = issue_challenge(handshake.nonces(), nonce)?;
    framed.send(&encode_challenge_wire(&challenge)?).await?;
    let attestation_wire = framed.receive().await?;
    let attestation = decode_attestation_wire(&attestation_wire)?;
    let verified_at_ms = clock.inspect_wall()?.as_u64();
    let verified = verify_attestation(
        identity,
        handshake.nonces(),
        &attestation,
        handshake.binding(),
        verified_at_ms,
    )?;

    let principal = verified.principal_id();
    serve_one(
        framed.into_inner(),
        config,
        peer,
        &AllowLocalPeer,
        move |request| {
            let response = service.handle_for_ipc(request.envelope(), principal, 0);
            async move {
                Ok(OutboundResponse::Typed(ExchangeResponse {
                    envelope: Some(response),
                }))
            }
        },
    )
    .await?;
    Ok(())
}

fn parse_args<I>(mut args: I) -> Result<WorkerArgs, String>
where
    I: Iterator<Item = String>,
{
    let mut store_root: Option<PathBuf> = None;
    let mut identity_root: Option<PathBuf> = None;
    let mut clock_root: Option<PathBuf> = None;
    let mut endpoint: Option<PathBuf> = None;
    let mut isolation_profile: Option<ProviderIsolationProfile> = None;
    let mut attestation_override: Option<[u8; 32]> = None;
    let mut allowed_principal: Option<PrincipalId> = None;
    let mut max_requests: Option<usize> = None;

    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for argument {flag}"))?;
        match flag.as_str() {
            "--store-root" => store_root = Some(PathBuf::from(value)),
            "--identity-root" => identity_root = Some(PathBuf::from(value)),
            "--clock-root" => clock_root = Some(PathBuf::from(value)),
            "--endpoint" => endpoint = Some(PathBuf::from(value)),
            "--isolation-profile" => {
                isolation_profile = Some(
                    ProviderIsolationProfile::from_cli_str(&value)
                        .map_err(|err| err.to_string())?,
                );
            }
            "--attestation-hex" => {
                attestation_override = Some(decode_hex_exact::<32>(&value)?);
            }
            "--allowed-principal-hex" => {
                allowed_principal = Some(PrincipalId::from_bytes(decode_hex_exact::<16>(&value)?));
            }
            "--max-requests" => {
                max_requests = Some(
                    value
                        .parse::<usize>()
                        .map_err(|err| format!("invalid --max-requests {value:?}: {err}"))?,
                );
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    Ok(WorkerArgs {
        store_root: store_root.ok_or_else(|| "missing --store-root".to_owned())?,
        identity_root: identity_root.ok_or_else(|| "missing --identity-root".to_owned())?,
        clock_root: clock_root.ok_or_else(|| "missing --clock-root".to_owned())?,
        endpoint: endpoint.ok_or_else(|| "missing --endpoint".to_owned())?,
        isolation_profile: isolation_profile
            .ok_or_else(|| "missing --isolation-profile".to_owned())?,
        attestation_override,
        allowed_principal,
        max_requests,
    })
}
