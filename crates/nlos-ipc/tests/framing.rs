use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;

use nlos_ipc::{
    ExactPeerAuthorizer, FramedIo, IoOperation, IpcError, LocalRpcClient, OutboundResponse,
    PeerAuthorizer, PeerCredentialBinding, PeerIdentity, TransportConfig, serve_one,
};
use nlos_schema::sabi::v1::local_rpc::Client as _;
use nlos_schema::sabi::v1::{Envelope, ExchangeRequest, ExchangeResponse, SchemaIdentity};
use nlos_schema::{SABI_ENVELOPE_SCHEMA, decode_exchange_response, encode_exchange_response};
use tokio::io::{AsyncWriteExt, duplex};
use tokio::sync::Notify;
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

struct Deny;

impl PeerAuthorizer for Deny {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Err("test policy".to_owned())
    }
}

#[test]
fn exact_peer_authorizer_rejects_credential_drift() {
    let observed = PeerIdentity::Unix {
        process_id: Some(42),
        user_id: 7,
        group_id: 8,
    };
    let authorizer = ExactPeerAuthorizer::new(PeerCredentialBinding::from_peer(observed));
    assert_eq!(authorizer.binding().identity(), observed);
    assert!(authorizer.authorize(&observed).is_ok());
    assert!(matches!(
        authorizer.authorize(&PeerIdentity::Unix {
            process_id: Some(43),
            user_id: 7,
            group_id: 8,
        }),
        Err(reason) if reason == "peer credentials do not match the exact binding"
    ));
    assert!(authorizer.authorize(&PeerIdentity::InMemory).is_err());
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

#[tokio::test]
async fn typed_exchange_round_trips_and_preserves_forwarded_unknown_fields() {
    let (client_stream, server_stream) = duplex(4_096);
    let config = fast_config(4_096);
    let mut response_wire = encode_exchange_response(&ExchangeResponse {
        envelope: Some(envelope(7)),
    })
    .unwrap();
    response_wire.extend_from_slice(&[0xa0, 0x06, 0x07]);
    let forwarded = decode_exchange_response(&response_wire).unwrap();

    let server = tokio::spawn(async move {
        serve_one(
            server_stream,
            config,
            PeerIdentity::InMemory,
            &Allow,
            move |validated| async move {
                assert_eq!(validated.envelope().request_id, vec![7; 16]);
                Ok(OutboundResponse::Forwarded(forwarded))
            },
        )
        .await
    });

    let response = LocalRpcClient::new(client_stream, config)
        .exchange_validated(request(7))
        .await
        .unwrap();

    assert_eq!(response.wire_bytes(), response_wire);
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn oversized_declared_frame_fails_before_body_read() {
    let (mut writer, reader) = duplex(64);
    let config = fast_config(32);
    writer.write_all(&33_u32.to_be_bytes()).await.unwrap();

    let error = FramedIo::new(reader, config).receive().await.unwrap_err();
    assert!(matches!(
        error,
        IpcError::FrameTooLarge {
            actual: 33,
            maximum: 32
        }
    ));
}

#[tokio::test]
async fn disconnected_half_frame_is_an_explicit_read_error() {
    let (mut writer, reader) = duplex(64);
    let config = fast_config(32);
    writer.write_all(&4_u32.to_be_bytes()).await.unwrap();
    writer.write_all(&[1, 2]).await.unwrap();
    drop(writer);

    let error = FramedIo::new(reader, config).receive().await.unwrap_err();
    assert!(matches!(
        error,
        IpcError::Io {
            operation: IoOperation::Read,
            source
        } if source.kind() == ErrorKind::UnexpectedEof
    ));
}

#[tokio::test]
async fn authorization_runs_before_any_request_read() {
    let (_client_stream, server_stream) = duplex(64);
    let error = serve_one(
        server_stream,
        fast_config(32),
        PeerIdentity::InMemory,
        &Deny,
        |_| async { unreachable!("denied peers never reach the handler") },
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        IpcError::AuthorizationDenied(reason) if reason == "test policy"
    ));
}

#[tokio::test]
async fn concurrent_call_gets_immediate_backpressure() {
    let (client_stream, _server_stream) = duplex(4_096);
    let client = Arc::new(LocalRpcClient::new(client_stream, fast_config(4_096)));
    let first_client = Arc::clone(&client);
    let first = tokio::spawn(async move { first_client.exchange_validated(request(1)).await });
    sleep(Duration::from_millis(10)).await;

    assert!(matches!(
        client.exchange_validated(request(2)).await,
        Err(IpcError::Backpressure)
    ));
    assert!(matches!(
        first.await.unwrap(),
        Err(IpcError::Timeout(IoOperation::Read))
    ));
    assert!(matches!(
        client.exchange_validated(request(3)).await,
        Err(IpcError::ConnectionUnusable)
    ));
}

#[tokio::test]
async fn typed_response_request_id_is_echoed_by_serve_one() {
    // 深审计 36 D3 回归:handler 遗漏回写(故意给错 request_id)时,
    // serve_one 必须盖章 echo 请求关联——客户端不得因 RequestIdMismatch
    // 毒化连接。
    let (client_stream, server_stream) = duplex(4_096);
    let config = fast_config(4_096);
    let server = tokio::spawn(async move {
        serve_one(
            server_stream,
            config,
            PeerIdentity::InMemory,
            &Allow,
            |_| async {
                Ok(OutboundResponse::Typed(ExchangeResponse {
                    envelope: Some(envelope(9)),
                }))
            },
        )
        .await
    });

    let response = LocalRpcClient::new(client_stream, config)
        .exchange_validated(request(8))
        .await
        .unwrap();
    assert_eq!(response.envelope().request_id, vec![8; 16]);
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn forwarded_response_request_id_mismatch_is_rejected() {
    // Forwarded 响应保持上游字节不变,serve_one 只校验关联:失配以
    // 类型化 RequestIdMismatch 拒绝发送,客户端看到 EOF 型读错误。
    let (client_stream, server_stream) = duplex(4_096);
    let config = fast_config(4_096);
    let forwarded = decode_exchange_response(
        &encode_exchange_response(&ExchangeResponse {
            envelope: Some(envelope(9)),
        })
        .unwrap(),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        serve_one(
            server_stream,
            config,
            PeerIdentity::InMemory,
            &Allow,
            move |_| async move { Ok(OutboundResponse::Forwarded(forwarded)) },
        )
        .await
    });

    assert!(matches!(
        LocalRpcClient::new(client_stream, fast_config(4_096))
            .exchange_validated(request(8))
            .await,
        Err(IpcError::Io {
            operation: IoOperation::Read,
            ..
        })
    ));
    assert!(matches!(
        server.await.unwrap().unwrap_err(),
        IpcError::RequestIdMismatch
    ));
}

#[tokio::test]
async fn raw_mismatched_response_still_poisons_the_client() {
    // 绕过 serve_one 的回写保护(直连 FramedIo)时,客户端自身的关联
    // 校验仍是纵深防御:失配毒化连接。
    let (client_stream, server_stream) = duplex(4_096);
    let config = fast_config(4_096);
    let server = tokio::spawn(async move {
        let mut framed = FramedIo::new(server_stream, config);
        framed.receive().await.unwrap();
        let response = encode_exchange_response(&ExchangeResponse {
            envelope: Some(envelope(9)),
        })
        .unwrap();
        framed.send(&response).await.unwrap();
    });

    let client = LocalRpcClient::new(client_stream, fast_config(4_096));
    assert!(matches!(
        client.exchange_validated(request(8)).await,
        Err(IpcError::RequestIdMismatch)
    ));
    assert!(matches!(
        client.exchange_validated(request(8)).await,
        Err(IpcError::ConnectionUnusable)
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn frame_too_large_send_leaves_connection_usable() {
    // 深审计 36 D2 回归:send 的写前界检(FrameTooLarge)零字节上网,
    // 连接不得毒化——同一连接上缩小后的下一次交换必须成功。
    let (client_stream, server_stream) = duplex(64_000);
    let mut big = request(1);
    big.envelope
        .as_mut()
        .expect("test request has an envelope")
        .payload = vec![0x5a; 512];
    let big_wire = nlos_schema::encode_exchange_request(&big).unwrap();
    let client = LocalRpcClient::new(client_stream, fast_config(big_wire.len() - 1));
    let server = tokio::spawn(async move {
        serve_one(
            server_stream,
            fast_config(64_000),
            PeerIdentity::InMemory,
            &Allow,
            |validated| async move {
                Ok(OutboundResponse::Typed(ExchangeResponse {
                    envelope: Some(envelope(validated.envelope().request_id[0])),
                }))
            },
        )
        .await
    });

    assert!(matches!(
        client.exchange_validated(big).await,
        Err(IpcError::FrameTooLarge { .. })
    ));
    let response = client.exchange_validated(request(2)).await.unwrap();
    assert_eq!(response.envelope().request_id, vec![2; 16]);
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancelled_exchange_marks_connection_unusable_and_reconnects_cleanly() {
    let config = TransportConfig::new(
        4_096,
        Duration::from_secs(2),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .unwrap();
    let (conn1_client, conn1_server) = duplex(4_096);
    let (conn2_client, conn2_server) = duplex(4_096);

    let request1_received = Arc::new(Notify::new());
    let request1_received_srv = Arc::clone(&request1_received);
    let release_stale_reply = Arc::new(Notify::new());
    let release_stale_reply_srv = Arc::clone(&release_stale_reply);
    let stale_reply_written = Arc::new(Notify::new());
    let stale_reply_written_srv = Arc::clone(&stale_reply_written);

    let server1 = tokio::spawn(async move {
        let _ = serve_one(
            conn1_server,
            config,
            PeerIdentity::InMemory,
            &Allow,
            move |validated| async move {
                assert_eq!(validated.envelope().request_id, vec![1; 16]);
                request1_received_srv.notify_one();
                release_stale_reply_srv.notified().await;
                Ok(OutboundResponse::Typed(ExchangeResponse {
                    envelope: Some(envelope(1)),
                }))
            },
        )
        .await;
        stale_reply_written_srv.notify_one();
    });

    let server2 = tokio::spawn(async move {
        serve_one(
            conn2_server,
            config,
            PeerIdentity::InMemory,
            &Allow,
            |validated| async move {
                assert_eq!(validated.envelope().request_id, vec![2; 16]);
                Ok(OutboundResponse::Typed(ExchangeResponse {
                    envelope: Some(envelope(2)),
                }))
            },
        )
        .await
    });

    let mut client = LocalRpcClient::new(conn1_client, config);
    let mut backup_stream = Some(conn2_client);

    // Cancel the first `exchange` mid-`.await` once the server has read request 1.
    {
        let timed_out = timeout(Duration::from_millis(25), async {
            client.exchange(request(1)).await
        });
        tokio::pin!(timed_out);
        tokio::select! {
            res = &mut timed_out => panic!("expected timeout cancellation, got {res:?}"),
            () = request1_received.notified() => {}
        }
        assert!(timed_out.await.is_err());
    }

    // Let the first server flush its delayed reply for request 1 onto conn1 so
    // stale bytes are sitting in conn1's receive buffer.
    release_stale_reply.notify_one();
    stale_reply_written.notified().await;

    // A caller that reconnects on `ConnectionUnusable` must immediately observe
    // `ConnectionUnusable` on the cancelled client (never reading stale reply 1
    // into a `RequestIdMismatch`) and succeed cleanly on the fresh stream.
    let second_response = match client.exchange(request(2)).await {
        Err(IpcError::ConnectionUnusable) => {
            client = LocalRpcClient::new(
                backup_stream.take().expect("fresh reconnect stream"),
                config,
            );
            client.exchange(request(2)).await.unwrap()
        }
        other => panic!("expected ConnectionUnusable after mid-await drop, got {other:?}"),
    };
    assert_eq!(
        second_response
            .envelope
            .expect("response envelope")
            .request_id,
        vec![2; 16]
    );

    server1.await.unwrap();
    server2.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancelled_exchange_mid_send_marks_connection_unusable() {
    let config = TransportConfig::new(
        4_096,
        Duration::from_secs(2),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .unwrap();
    // Tiny 8-byte duplex buffer with nobody reading blocks `send` mid-frame.
    let (client_stream, _stalled_server_stream) = duplex(8);
    let client = LocalRpcClient::new(client_stream, config);

    let send_cancelled = timeout(
        Duration::from_millis(20),
        client.exchange_validated(request(5)),
    )
    .await;
    assert!(send_cancelled.is_err());

    assert!(matches!(
        client.exchange_validated(request(6)).await,
        Err(IpcError::ConnectionUnusable)
    ));
}
