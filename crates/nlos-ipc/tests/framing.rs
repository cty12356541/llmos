use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;

use nlos_ipc::{
    FramedIo, IoOperation, IpcError, LocalRpcClient, OutboundResponse, PeerAuthorizer,
    PeerIdentity, TransportConfig, serve_one,
};
use nlos_schema::sabi::v1::{Envelope, ExchangeRequest, ExchangeResponse, SchemaIdentity};
use nlos_schema::{SABI_ENVELOPE_SCHEMA, decode_exchange_response, encode_exchange_response};
use tokio::io::{AsyncWriteExt, duplex};
use tokio::time::sleep;

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
async fn mismatched_response_request_id_fails_closed() {
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

    assert!(matches!(
        LocalRpcClient::new(client_stream, config)
            .exchange_validated(request(8))
            .await,
        Err(IpcError::RequestIdMismatch)
    ));
    server.await.unwrap().unwrap();
}
