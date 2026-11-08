//! W82-A session-mux tests: the bounded multi-round `serve_connection`
//! primitive and its authenticated sibling. A resident external client keeps
//! ONE connection open and issues many sequential exchanges; the server loops
//! receive→decode→handler→send until the client closes the connection or the
//! connection-level idle timeout expires (fail-closed — the per-frame
//! `read_timeout` budget alone would kill any inter-frame pause longer than
//! five seconds, so the idle window is a separate, explicit budget on the
//! FIRST byte of the next frame).

use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nlos_ipc::{
    ConnectionTermination, ExactPeerAuthorizer, FramedIo, IoOperation, IpcError, LocalRpcClient,
    OutboundResponse, PeerAuthorizer, PeerCredentialBinding, PeerIdentity, ServedConnection,
    TransportConfig, serve_connection,
};
use nlos_schema::SABI_ENVELOPE_SCHEMA;
use nlos_schema::sabi::v1::{Envelope, ExchangeRequest, ExchangeResponse, SchemaIdentity};
use tokio::io::duplex;
use tokio::time::{sleep, timeout};

fn envelope(request_id: u8) -> Envelope {
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 0,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![request_id; 16],
        service: "operation".to_owned(),
        method: "get".to_owned(),
        common_context: None,
        payload: b"payload".to_vec(),
    }
}

fn request(request_id: u8) -> ExchangeRequest {
    ExchangeRequest {
        envelope: Some(envelope(request_id)),
    }
}

struct Allow;

impl PeerAuthorizer for Allow {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

fn fast_config(maximum_frame_bytes: usize) -> TransportConfig {
    TransportConfig::new(
        maximum_frame_bytes,
        Duration::from_millis(100),
        Duration::from_millis(100),
        Duration::from_millis(100),
    )
    .unwrap()
}

/// Multi-round echo handler: returns the request envelope stamped with the
/// marker byte so each round is individually observable.
fn echo_handler() -> impl FnMut(
    nlos_schema::ValidatedExchangeRequest,
) -> std::future::Ready<Result<OutboundResponse, IpcError>> {
    |validated| {
        std::future::ready(Ok(OutboundResponse::Typed(ExchangeResponse {
            envelope: Some(validated.envelope().clone()),
        })))
    }
}

#[tokio::test]
async fn serve_connection_serves_rounds_until_client_closes() {
    let (client_stream, server_stream) = duplex(4_096);
    let config = fast_config(4_096);
    let server = tokio::spawn(serve_connection(
        server_stream,
        config,
        PeerIdentity::InMemory,
        &Allow,
        Duration::from_secs(30),
        echo_handler(),
    ));

    let client = LocalRpcClient::new(client_stream, config);
    for round in 1_u8..=3 {
        let response = client.exchange_validated(request(round)).await.unwrap();
        assert_eq!(response.envelope().request_id, vec![round; 16]);
    }
    drop(client);

    let summary = timeout(Duration::from_secs(5), server)
        .await
        .expect("server settles after client close")
        .unwrap()
        .unwrap();
    assert_eq!(
        summary,
        ServedConnection {
            rounds: 3,
            termination: ConnectionTermination::ClientClosed,
        }
    );
}

#[tokio::test]
async fn serve_connection_closes_after_idle_timeout_and_fail_closed_is_observable() {
    let (client_stream, server_stream) = duplex(4_096);
    let config = fast_config(4_096);
    let server = tokio::spawn(serve_connection(
        server_stream,
        config,
        PeerIdentity::InMemory,
        &Allow,
        Duration::from_millis(150),
        echo_handler(),
    ));

    let mut client = FramedIo::new(client_stream, config);
    // One full round, then the client goes silent well past the idle window.
    let wire = nlos_schema::encode_exchange_request(&request(1)).unwrap();
    client.send(&wire).await.unwrap();
    let response_wire = client.receive().await.unwrap();
    let response = nlos_schema::decode_exchange_response(&response_wire).unwrap();
    assert_eq!(response.envelope().request_id, vec![1; 16]);

    sleep(Duration::from_millis(400)).await;

    let summary = timeout(Duration::from_secs(5), server)
        .await
        .expect("idle timeout retires the server")
        .unwrap()
        .unwrap();
    assert_eq!(
        summary,
        ServedConnection {
            rounds: 1,
            termination: ConnectionTermination::IdleExpired,
        }
    );
    // Fail-closed: the server closed the connection, so the client's next
    // exchange surfaces the closed stream quickly instead of hanging on the
    // per-frame read budget.
    let second_wire = nlos_schema::encode_exchange_request(&request(2)).unwrap();
    let started = std::time::Instant::now();
    let outcome = client.send(&second_wire).await;
    match outcome {
        Ok(()) => {
            let read = client.receive().await;
            assert!(
                matches!(
                    &read,
                    Err(IpcError::Io {
                        operation: IoOperation::Read,
                        source,
                    }) if source.kind() == ErrorKind::UnexpectedEof
                ),
                "closed connection read fails with EOF, got {read:?}"
            );
        }
        Err(error) => {
            assert!(
                !matches!(error, IpcError::Timeout(_)),
                "failure must be the closed stream, not a timeout: {error}"
            );
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "server-initiated close observed promptly"
    );
}

#[tokio::test]
async fn serve_connection_rejects_zero_idle_timeout() {
    let (_client_stream, server_stream) = duplex(4_096);
    let config = fast_config(4_096);
    let outcome = serve_connection(
        server_stream,
        config,
        PeerIdentity::InMemory,
        &Allow,
        Duration::ZERO,
        echo_handler(),
    )
    .await;
    assert!(
        matches!(outcome, Err(IpcError::InvalidConfig(_))),
        "zero idle timeout is invalid config, got {outcome:?}"
    );
}

#[tokio::test]
async fn serve_connection_denies_unauthorized_peer_before_any_frame() {
    let (client_stream, server_stream) = duplex(4_096);
    let config = fast_config(4_096);
    let binding = PeerCredentialBinding::from_peer(PeerIdentity::InMemory);
    let authorizer = ExactPeerAuthorizer::new(binding);
    let outcome = serve_connection(
        server_stream,
        config,
        // Unix identity cannot match the InMemory binding: denied pre-gate.
        PeerIdentity::Unix {
            process_id: None,
            user_id: 1,
            group_id: 1,
        },
        &authorizer,
        Duration::from_secs(30),
        echo_handler(),
    )
    .await;
    assert!(
        matches!(outcome, Err(IpcError::AuthorizationDenied(_))),
        "peer gate runs before any frame, got {outcome:?}"
    );
    drop(client_stream);
}

#[tokio::test]
async fn serve_connection_handler_runs_once_per_round() {
    let (client_stream, server_stream) = duplex(4_096);
    let config = fast_config(4_096);
    let rounds = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&rounds);
    let handler = move |validated: nlos_schema::ValidatedExchangeRequest| {
        let observed = Arc::clone(&observed);
        async move {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(validated.envelope().clone()),
            }))
        }
    };
    let server = tokio::spawn(serve_connection(
        server_stream,
        config,
        PeerIdentity::InMemory,
        &Allow,
        Duration::from_secs(30),
        handler,
    ));

    let client = LocalRpcClient::new(client_stream, config);
    for round in 1_u8..=4 {
        client.exchange_validated(request(round)).await.unwrap();
    }
    drop(client);
    let summary = server.await.unwrap().unwrap();
    assert_eq!(summary.rounds, 4);
    assert_eq!(rounds.load(Ordering::SeqCst), 4, "handler once per round");
}

#[tokio::test]
async fn serve_connection_forwarded_mismatch_fails_closed_mid_connection() {
    let (client_stream, server_stream) = duplex(4_096);
    let config = fast_config(4_096);
    // A Forwarded response whose request_id cannot match round 2, built via
    // the validated decode path exactly like the framing tests.
    let mut forwarded_wire = nlos_schema::encode_exchange_response(&ExchangeResponse {
        envelope: Some(envelope(9)),
    })
    .unwrap();
    forwarded_wire.extend_from_slice(&[0xa0, 0x06, 0x07]);
    let forwarded = nlos_schema::decode_exchange_response(&forwarded_wire).unwrap();
    let mut first = true;
    let mut forwarded = Some(forwarded);
    let handler = move |_validated: nlos_schema::ValidatedExchangeRequest| {
        let use_forwarded = !first;
        first = false;
        let forwarded_now = if use_forwarded {
            forwarded.take()
        } else {
            None
        };
        async move {
            if let Some(forwarded) = forwarded_now {
                Ok(OutboundResponse::Forwarded(forwarded))
            } else {
                Ok(OutboundResponse::Typed(ExchangeResponse {
                    envelope: Some(envelope(1)),
                }))
            }
        }
    };
    let server = tokio::spawn(serve_connection(
        server_stream,
        config,
        PeerIdentity::InMemory,
        &Allow,
        Duration::from_secs(30),
        handler,
    ));

    let client = LocalRpcClient::new(client_stream, config);
    client.exchange_validated(request(1)).await.unwrap();
    // Round 2: the server refuses to send the mismatched forwarded response
    // and drops the connection, so the client surfaces an error.
    let outcome = client.exchange_validated(request(2)).await;
    assert!(
        outcome.is_err(),
        "mismatched forwarded response breaks the connection"
    );
    let summary = server.await.unwrap();
    assert!(
        matches!(summary, Err(IpcError::RequestIdMismatch)),
        "typed mismatch, got {summary:?}"
    );
}

// ---------------------------------------------------------------------------
// Authenticated multi-round serving over a real Unix socket (mirrors the
// handshake_transport.rs harness: a genuine IdentityAuthority, deterministic
// nonce source, and the platform authenticated_connect client).
// ---------------------------------------------------------------------------
#[cfg(unix)]
mod authenticated_session {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use nlos_identity::{
        BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose,
    };
    use nlos_ipc::handshake::HandshakeNonceRegistry;
    use nlos_ipc::handshake::transport::{
        authenticated_connect, authenticated_serve_connection, endpoint_channel_binding,
    };
    use nlos_ipc::unix::UnixListenerAdapter;
    use nlos_types::IdempotencyKey;
    use std::path::PathBuf;

    const NONCE: [u8; 32] = [0x5D; 32];

    struct TempRoot(PathBuf);

    static NEXT_ROOT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            Self(std::env::temp_dir().join(format!(
                "nlos-ipc-session-mux-{label}-{}-{nanos}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )))
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn short_socket(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("nlos-sm-{label}-{}-{nanos}", std::process::id()))
    }

    fn bootstrap(
        seed: u8,
    ) -> (
        TempRoot,
        IdentityAuthority,
        SigningKey,
        nlos_types::PrincipalId,
    ) {
        let root = TempRoot::new("auth");
        let identity = IdentityAuthority::open(&root.0).unwrap();
        let key = SigningKey::from_bytes(&[seed; 32]);
        let BootstrapDecision::Created(binding) = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [seed.wrapping_add(1); 32],
                control_domain_policy_digest: [seed.wrapping_add(2); 32],
                public_key: key.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 0,
                key_valid_until_ms: 10_000,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
                created_at_ms: 0,
            })
            .unwrap()
        else {
            unreachable!("fresh authority bootstraps a new principal");
        };
        (root, identity, key, binding.principal_id)
    }

    async fn accept(
        listener: &UnixListenerAdapter,
        config: TransportConfig,
    ) -> (tokio::net::UnixStream, PeerIdentity) {
        listener.accept(config).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_serve_connection_serves_multiple_rounds_and_threads_principal() {
        let path = short_socket("chain");
        let (_root, identity, key, principal) = bootstrap(0x41);
        let nonces = HandshakeNonceRegistry::new(8).unwrap();
        let binding_bytes = endpoint_channel_binding(&path).to_vec();
        let listener = UnixListenerAdapter::bind(&path).unwrap();
        let config = fast_config(4_096);

        let server = tokio::spawn(async move {
            let (stream, peer) = accept(&listener, config).await;
            let mut issued = 0_u8;
            authenticated_serve_connection(
                stream,
                peer,
                config,
                &identity,
                &nonces,
                &binding_bytes,
                &super::Allow,
                Duration::from_secs(30),
                |verified| {
                    // The factory observes the connection's verified
                    // principal and binds it into the per-round handler.
                    assert_eq!(verified.principal_id(), principal);
                    move |validated: nlos_schema::ValidatedExchangeRequest| {
                        std::future::ready(Ok(OutboundResponse::Typed(ExchangeResponse {
                            envelope: Some(validated.envelope().clone()),
                        })))
                    }
                },
                || {
                    issued = issued.wrapping_add(1);
                    let mut nonce = NONCE;
                    nonce[0] = issued;
                    nonce
                },
                5_000,
            )
            .await
        });

        let framed = authenticated_connect(&path, config, principal, |digest: &[u8; 32]| {
            Ok(key.sign(digest).to_bytes())
        })
        .await
        .unwrap();
        let client = LocalRpcClient::new(framed.into_inner(), config);
        for round in 1_u8..=3 {
            let response = client.exchange_validated(request(round)).await.unwrap();
            assert_eq!(response.envelope().request_id, vec![round; 16]);
        }
        drop(client);

        let outcome = timeout(Duration::from_secs(5), server)
            .await
            .expect("server settles after client close")
            .unwrap()
            .unwrap();
        assert_eq!(outcome.verified().principal_id(), principal);
        let summary = outcome.served().as_ref().unwrap();
        assert_eq!(
            *summary,
            ServedConnection {
                rounds: 3,
                termination: ConnectionTermination::ClientClosed,
            }
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_serve_connection_retires_on_idle_timeout() {
        let path = short_socket("idle");
        let (_root, identity, key, principal) = bootstrap(0x42);
        let nonces = HandshakeNonceRegistry::new(8).unwrap();
        let binding_bytes = endpoint_channel_binding(&path).to_vec();
        let listener = UnixListenerAdapter::bind(&path).unwrap();
        let config = fast_config(4_096);

        let server = tokio::spawn(async move {
            let (stream, peer) = accept(&listener, config).await;
            authenticated_serve_connection(
                stream,
                peer,
                config,
                &identity,
                &nonces,
                &binding_bytes,
                &super::Allow,
                Duration::from_millis(150),
                |_verified| {
                    move |validated: nlos_schema::ValidatedExchangeRequest| {
                        std::future::ready(Ok(OutboundResponse::Typed(ExchangeResponse {
                            envelope: Some(validated.envelope().clone()),
                        })))
                    }
                },
                || NONCE,
                5_000,
            )
            .await
        });

        let framed = authenticated_connect(&path, config, principal, |digest: &[u8; 32]| {
            Ok(key.sign(digest).to_bytes())
        })
        .await
        .unwrap();
        let client = LocalRpcClient::new(framed.into_inner(), config);
        client.exchange_validated(request(1)).await.unwrap();
        // Silent past the idle window: the connection is retired fail-closed.
        sleep(Duration::from_millis(400)).await;
        let second = client.exchange_validated(request(2)).await;
        assert!(
            second.is_err(),
            "idle-expired connection is closed: {second:?}"
        );

        let outcome = timeout(Duration::from_secs(5), server)
            .await
            .expect("idle timeout retires the authenticated server")
            .unwrap()
            .unwrap();
        let summary = outcome.served().as_ref().unwrap();
        assert_eq!(
            *summary,
            ServedConnection {
                rounds: 1,
                termination: ConnectionTermination::IdleExpired,
            }
        );
        let _ = std::fs::remove_file(&path);
    }
}
