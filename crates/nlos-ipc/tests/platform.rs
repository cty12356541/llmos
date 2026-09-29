use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_ipc::{
    IoOperation, IpcError, LocalRpcClient, OutboundResponse, PeerAuthorizer, PeerIdentity,
    TransportConfig, serve_one,
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

struct Allow;

impl PeerAuthorizer for Allow {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_round_trip_uses_owner_only_endpoint_and_peer_credentials() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use nlos_ipc::unix::{UnixListenerAdapter, connect};

    let path = std::env::temp_dir().join(format!(
        "nlos-ipc-{}-{}.sock",
        std::process::id(),
        unique_suffix()
    ));
    let listener = UnixListenerAdapter::bind(&path).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept(config()).await?;
        assert!(matches!(peer, PeerIdentity::Unix { .. }));
        serve_one(stream, config(), peer, &Allow, |validated| async move {
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(validated.envelope().clone()),
            }))
        })
        .await
    });

    let (stream, peer) = connect(&path, config()).await.unwrap();
    assert!(matches!(peer, PeerIdentity::Unix { .. }));
    let response = LocalRpcClient::new(stream, config())
        .exchange_validated(request())
        .await
        .unwrap();
    assert_eq!(response.envelope().request_id, vec![3; 16]);
    server.await.unwrap().unwrap();
    fs::remove_file(path).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn unavailable_unix_endpoint_is_an_explicit_connect_error() {
    use nlos_ipc::unix::connect;

    let path = std::env::temp_dir().join(format!(
        "nlos-ipc-missing-{}-{}.sock",
        std::process::id(),
        unique_suffix()
    ));
    let result = connect(path, config()).await;
    assert!(matches!(
        result,
        Err(IpcError::Io {
            operation: IoOperation::Connect,
            ..
        })
    ));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_named_pipe_round_trip_uses_the_same_schema_and_framing() {
    use nlos_ipc::windows::{NamedPipeListenerAdapter, connect};

    let name = format!(
        r"\\.\pipe\nlos-ipc-{}-{}",
        std::process::id(),
        unique_suffix()
    );
    let mut listener = NamedPipeListenerAdapter::bind(&name, 2, config()).unwrap();
    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept(config()).await?;
        assert_eq!(
            peer,
            PeerIdentity::WindowsNamedPipe {
                process_id: Some(std::process::id())
            }
        );
        serve_one(stream, config(), peer, &Allow, |validated| async move {
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(validated.envelope().clone()),
            }))
        })
        .await
    });

    let (stream, peer) = connect(&name, config()).await.unwrap();
    // The in-process server task is the pipe's server end, so the client-side
    // peer credential resolves to this process as well.
    assert_eq!(
        peer,
        PeerIdentity::WindowsNamedPipe {
            process_id: Some(std::process::id())
        }
    );
    let response = LocalRpcClient::new(stream, config())
        .exchange_validated(request())
        .await
        .unwrap();
    assert_eq!(response.envelope().request_id, vec![3; 16]);
    server.await.unwrap().unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn named_pipe_listener_survives_an_accept_timeout() {
    use nlos_ipc::windows::{NamedPipeListenerAdapter, connect};

    let name = format!(
        r"\\.\pipe\nlos-ipc-idle-{}-{}",
        std::process::id(),
        unique_suffix()
    );
    let mut listener = NamedPipeListenerAdapter::bind(&name, 2, config()).unwrap();
    // An idle accept window shorter than any client connect: the timeout
    // consumes the pre-created listening instance. The listener must rebuild
    // it and keep serving instead of failing every later accept.
    let idle = TransportConfig::new(
        4_096,
        Duration::from_millis(50),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .unwrap();
    assert!(matches!(
        listener.accept(idle).await,
        Err(IpcError::Timeout(IoOperation::Accept))
    ));

    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept(config()).await?;
        assert_eq!(
            peer,
            PeerIdentity::WindowsNamedPipe {
                process_id: Some(std::process::id())
            }
        );
        serve_one(stream, config(), peer, &Allow, |validated| async move {
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(validated.envelope().clone()),
            }))
        })
        .await
    });
    let (stream, _peer) = connect(&name, config()).await.unwrap();
    let response = LocalRpcClient::new(stream, config())
        .exchange_validated(request())
        .await
        .unwrap();
    assert_eq!(response.envelope().request_id, vec![3; 16]);
    server.await.unwrap().unwrap();
}

/// Child-process client role for
/// `named_pipe_accept_resolves_real_child_client_credentials`: when the
/// `NLOS_IPC_PIPE_CHILD` variable names a pipe, connect as a real separate
/// process and hold the connection long enough for the parent to capture
/// credentials. Without the variable this test is a no-op (it exists so the
/// test binary can re-exec itself in child mode).
#[cfg(windows)]
#[tokio::test]
async fn pipe_child_client_helper() {
    let Ok(name) = std::env::var("NLOS_IPC_PIPE_CHILD") else {
        return;
    };
    // Client role only: connect as a real separate process and hold the
    // connection so the parent can capture this process's credentials.
    // (The client-side peer identity names the parent's server pid, so there
    // is nothing pid-wise to assert here; the parent asserts both.)
    let (_stream, _peer) = nlos_ipc::windows::connect(name, config())
        .await
        .expect("child pipe connect");
    tokio::time::sleep(Duration::from_millis(750)).await;
}

#[cfg(windows)]
#[tokio::test]
async fn named_pipe_accept_resolves_real_child_client_credentials() {
    use nlos_ipc::windows::SameUserAuthorizer;

    let name = format!(
        r"\\.\pipe\nlos-ipc-child-{}-{}",
        std::process::id(),
        unique_suffix()
    );
    let mut listener =
        nlos_ipc::windows::NamedPipeListenerAdapter::bind(&name, 2, config()).unwrap();
    let gate = SameUserAuthorizer::new().unwrap();

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "pipe_child_client_helper"])
        .env("NLOS_IPC_PIPE_CHILD", &name)
        .spawn()
        .unwrap();

    let (server, peer) = listener.accept(config()).await.unwrap();
    let PeerIdentity::WindowsNamedPipe {
        process_id: Some(process_id),
    } = peer
    else {
        panic!("expected a resolved named-pipe client pid, got {peer:?}");
    };
    assert_eq!(process_id, child.id());
    gate.authorize(&peer).expect("same-user child process");

    // Fail-closed faces: missing pid, unopenable pid, and non-pipe identities.
    assert!(
        gate.authorize(&PeerIdentity::WindowsNamedPipe { process_id: None })
            .is_err()
    );
    assert!(
        gate.authorize(&PeerIdentity::WindowsNamedPipe {
            process_id: Some(0)
        })
        .is_err()
    );
    assert!(gate.authorize(&PeerIdentity::InMemory).is_err());

    drop(server);
    let status = child.wait().unwrap();
    assert!(status.success(), "child helper exited with {status}");
}

#[cfg(windows)]
#[tokio::test]
async fn unavailable_named_pipe_exhausts_the_bounded_connect_window() {
    use nlos_ipc::windows::connect;

    let name = format!(
        r"\\.\pipe\nlos-ipc-missing-{}-{}",
        std::process::id(),
        unique_suffix()
    );
    let bounded = TransportConfig::new(
        4_096,
        Duration::from_millis(100),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .unwrap();
    let result = connect(name, bounded).await;
    assert!(matches!(
        result,
        Err(IpcError::Timeout(IoOperation::Connect))
    ));
}
