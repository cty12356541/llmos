#![cfg(windows)]

//! Windows named-pipe OS-credential tests: the kernel-observed client pid
//! from `GetNamedPipeClientProcessId` must match the pid the client
//! self-reports in its first frame, and [`ExactPeerAuthorizer`] must
//! enforce real-pid bindings — a mismatched binding is denied instead of
//! the historical vacuous `None`-matches-all behavior.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_ipc::windows::{NamedPipeListenerAdapter, connect};
use nlos_ipc::{
    ExactPeerAuthorizer, FramedIo, IpcError, LocalRpcClient, OutboundResponse, PeerAuthorizer,
    PeerCredentialBinding, PeerIdentity, TransportConfig, serve_one,
};
use nlos_schema::SABI_ENVELOPE_SCHEMA;
use nlos_schema::sabi::v1::{Envelope, ExchangeRequest, ExchangeResponse, SchemaIdentity};

fn config() -> TransportConfig {
    TransportConfig::new(
        4_096,
        Duration::from_secs(2),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .unwrap()
}

fn pipe_name(label: &str) -> String {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\nlos-ipc-pid-{label}-{}-{suffix}",
        std::process::id()
    )
}

fn request() -> ExchangeRequest {
    ExchangeRequest {
        envelope: Some(Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: 1,
                minor: 0,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: vec![3; 16],
            service: "operation".to_owned(),
            method: "get".to_owned(),
            common_context: None,
            payload: Vec::new(),
        }),
    }
}

#[tokio::test]
async fn server_peer_pid_matches_the_pid_self_reported_in_the_first_frame() {
    let name = pipe_name("frame");
    let mut listener = NamedPipeListenerAdapter::bind(&name, 2, config()).unwrap();
    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept(config()).await?;
        let PeerIdentity::WindowsNamedPipe { process_id } = peer else {
            panic!("expected a WindowsNamedPipe peer identity");
        };
        let mut framed = FramedIo::new(stream, config());
        let first_frame = framed.receive().await?;
        Ok::<_, IpcError>((process_id, first_frame))
    });

    let (stream, peer) = connect(&name, config()).await.unwrap();
    assert_eq!(
        peer,
        PeerIdentity::WindowsNamedPipe {
            process_id: Some(std::process::id())
        }
    );
    let mut framed = FramedIo::new(stream, config());
    framed
        .send(&std::process::id().to_be_bytes())
        .await
        .unwrap();

    let (server_pid, first_frame) = server.await.unwrap().unwrap();
    let observed = server_pid.expect("accept must report an OS-observed client pid");
    assert_eq!(observed, std::process::id());
    assert_eq!(first_frame, std::process::id().to_be_bytes());
}

#[tokio::test]
async fn exact_peer_authorizer_enforces_the_os_observed_pid() {
    let name = pipe_name("authz");
    let mut listener = NamedPipeListenerAdapter::bind(&name, 2, config()).unwrap();
    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept(config()).await?;
        let PeerIdentity::WindowsNamedPipe {
            process_id: Some(observed_pid),
        } = peer
        else {
            panic!("accept must report an OS-observed client pid");
        };
        // A real-but-wrong pid binding must be denied by the exact-match
        // pre-gate; the historical None binding would have matched anyone.
        let wrong_pid = observed_pid.wrapping_add(1);
        let mismatched = ExactPeerAuthorizer::new(PeerCredentialBinding::from_peer(
            PeerIdentity::WindowsNamedPipe {
                process_id: Some(wrong_pid),
            },
        ));
        assert!(
            mismatched
                .authorize(&PeerIdentity::WindowsNamedPipe {
                    process_id: Some(observed_pid)
                })
                .is_err()
        );
        serve_one(
            stream,
            config(),
            peer,
            &mismatched,
            |validated| async move {
                Ok(OutboundResponse::Typed(ExchangeResponse {
                    envelope: Some(validated.envelope().clone()),
                }))
            },
        )
        .await
    });

    let (stream, peer) = connect(&name, config()).await.unwrap();
    assert_eq!(
        peer,
        PeerIdentity::WindowsNamedPipe {
            process_id: Some(std::process::id())
        }
    );
    // The pre-gate denies before any request byte is dispatched; depending
    // on which side of the drop race the client lands on, it observes a
    // typed write or read transport failure.
    let client = LocalRpcClient::new(stream, config());
    let exchange = client.exchange_validated(request()).await;
    assert!(matches!(exchange, Err(IpcError::Io { .. })));
    let serve_failure = server.await.unwrap().unwrap_err();
    assert!(matches!(
        serve_failure,
        IpcError::AuthorizationDenied(reason) if reason.contains("exact binding")
    ));
}
