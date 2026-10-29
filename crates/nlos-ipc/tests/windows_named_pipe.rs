#![cfg(windows)]

//! Windows named-pipe OS-credential tests: the kernel-observed client pid
//! from `GetNamedPipeClientProcessId` must match the pid the client
//! self-reports in its first frame, and [`ExactPeerAuthorizer`] must
//! enforce real-pid bindings — a mismatched binding is denied instead of
//! the historical vacuous `None`-matches-all behavior.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_ipc::windows::{NamedPipeListenerAdapter, connect};
use nlos_ipc::{
    ExactPeerAuthorizer, FramedIo, IoOperation, IpcError, LocalRpcClient, OutboundResponse,
    PeerAuthorizer, PeerCredentialBinding, PeerIdentity, TransportConfig, serve_one,
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

#[tokio::test]
async fn accept_timeout_and_cancellation_preserve_listener_and_anti_squat_ownership() {
    let name = pipe_name("idle-cancel");
    let short_cfg = TransportConfig::new(
        4_096,
        Duration::from_millis(40),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .unwrap();
    let mut listener = NamedPipeListenerAdapter::bind(&name, 2, short_cfg).unwrap();

    // Two consecutive idle timeouts without a client must both return
    // `Timeout(Accept)` rather than losing `self.next` into `InvalidConfig`.
    for round in 1..=2 {
        let err = listener
            .accept(short_cfg)
            .await
            .expect_err("idle accept without client must time out");
        assert!(
            matches!(err, IpcError::Timeout(IoOperation::Accept)),
            "idle round {round} must return Timeout(Accept), got {err:?}",
        );
    }

    // Mid-flight cancellation of the `accept` future must also leave the
    // underlying listening pipe instance intact.
    let cancelled =
        tokio::time::timeout(Duration::from_millis(15), listener.accept(config())).await;
    assert!(
        cancelled.is_err(),
        "outer timeout must cancel the in-flight accept future"
    );

    // Kernel pipe-namespace anti-squat ownership (`first_pipe_instance(true)`)
    // must still hold after both idle timeouts and future cancellation.
    let Err(squat_err) = NamedPipeListenerAdapter::bind(&name, 2, config()) else {
        panic!("second first-instance bind on the live pipe name must be rejected");
    };
    assert!(
        matches!(
            squat_err,
            IpcError::Io {
                operation: IoOperation::Accept,
                ..
            }
        ),
        "expected OS first-instance rejection, got {squat_err:?}",
    );

    // The same listener must subsequently serve two real serial clients
    // end-to-end without re-binding.
    let server = tokio::spawn(async move {
        let mut received = Vec::with_capacity(2);
        for seq in 1_u32..=2 {
            let (stream, peer) = listener.accept(config()).await?;
            let PeerIdentity::WindowsNamedPipe {
                process_id: Some(observed_pid),
            } = peer
            else {
                panic!("accept #{seq} must report an OS-observed client pid");
            };
            assert_eq!(observed_pid, std::process::id());
            let mut framed = FramedIo::new(stream, config());
            let payload = framed.receive().await?;
            framed.send(&payload).await?;
            received.push(payload);
        }
        Ok::<_, IpcError>(received)
    });

    for seq in 1_u32..=2 {
        let (stream, _peer) = connect(&name, config()).await.unwrap();
        let mut framed = FramedIo::new(stream, config());
        let msg = seq.to_be_bytes();
        framed.send(&msg).await.unwrap();
        let echoed = framed.receive().await.unwrap();
        assert_eq!(echoed, msg);
    }

    let server_received = server.await.unwrap().unwrap();
    assert_eq!(
        server_received,
        vec![1_u32.to_be_bytes().to_vec(), 2_u32.to_be_bytes().to_vec()]
    );
}
