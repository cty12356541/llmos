use std::error::Error;
use std::ffi::OsString;
use std::io::{self, Write};

use nlos_ipc::{OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one};
use nlos_schema::sabi::v1::{
    DirectoryError, DirectoryErrorCode, ExchangeResponse, LocalEndpoint, LocalTransportKind,
    NegotiateServiceResponse, OperationReference, ReceiptReference, SabiResponseContext,
    ServiceCandidate, ServiceVersion, envelope, negotiate_service_response,
};
use nlos_schema::{
    MethodSemantics, SABI_ENVELOPE_SCHEMA, decode_negotiate_service_request,
    encode_negotiate_service_response, service_directory_schema_identity,
    validate_sabi_request_context,
};
use nlos_service_directory::{ServiceRegistration, SnapshotDirectory};

const DIRECTORY_SERVICE: &str = "service_directory";
const NEGOTIATE_METHOD: &str = "negotiate";
const BUSINESS_SERVICE: &str = "operation";

struct AllowConformancePeer;

impl PeerAuthorizer for AllowConformancePeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let directory_endpoint = arguments.next().ok_or("missing directory endpoint")?;
    let business_endpoint = arguments.next().ok_or("missing business endpoint")?;
    if arguments.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    run(directory_endpoint, business_endpoint).await
}

fn announce_ready() -> io::Result<()> {
    println!("READY");
    io::stdout().flush()
}

fn directory(
    transport: LocalTransportKind,
    business_endpoint: String,
) -> Result<SnapshotDirectory, Box<dyn Error>> {
    Ok(SnapshotDirectory::new([ServiceRegistration {
        candidate: ServiceCandidate {
            binding_id: vec![0x44; 16],
            generation: 7,
            service: BUSINESS_SERVICE.to_owned(),
            version: Some(ServiceVersion {
                schema_name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: 1,
                minor: 1,
            }),
            feature_ids: Vec::new(),
            transport_kinds: vec![transport.into()],
        },
        endpoint: LocalEndpoint {
            kind: transport.into(),
            address: business_endpoint,
        },
    }])?)
}

#[cfg(unix)]
async fn run(
    directory_endpoint: OsString,
    business_endpoint: OsString,
) -> Result<(), Box<dyn Error>> {
    use std::fs;
    use std::path::PathBuf;

    use nlos_ipc::unix::UnixListenerAdapter;

    struct EndpointGuard(Vec<PathBuf>);

    impl Drop for EndpointGuard {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = fs::remove_file(path);
            }
        }
    }

    let directory_path = PathBuf::from(directory_endpoint);
    let business_path = PathBuf::from(business_endpoint);
    let business_address = business_path
        .to_str()
        .ok_or("business endpoint must be UTF-8")?
        .to_owned();
    let directory_listener = UnixListenerAdapter::bind(&directory_path)?;
    let business_listener = UnixListenerAdapter::bind(&business_path)?;
    let _guard = EndpointGuard(vec![directory_path, business_path]);
    let snapshot = directory(LocalTransportKind::UnixSocket, business_address)?;
    announce_ready()?;

    let (directory_stream, directory_peer) = directory_listener
        .accept(TransportConfig::default())
        .await?;
    serve_directory(directory_stream, directory_peer, snapshot).await?;
    let (business_stream, business_peer) =
        business_listener.accept(TransportConfig::default()).await?;
    serve_business(business_stream, business_peer).await?;
    Ok(())
}

#[cfg(windows)]
async fn run(
    directory_endpoint: OsString,
    business_endpoint: OsString,
) -> Result<(), Box<dyn Error>> {
    use nlos_ipc::windows::NamedPipeListenerAdapter;

    let business_address = business_endpoint
        .clone()
        .into_string()
        .map_err(|_| "business endpoint must be UTF-8")?;
    let mut directory_listener =
        NamedPipeListenerAdapter::bind(directory_endpoint, 2, TransportConfig::default())?;
    let mut business_listener =
        NamedPipeListenerAdapter::bind(business_endpoint, 2, TransportConfig::default())?;
    let snapshot = directory(LocalTransportKind::WindowsNamedPipe, business_address)?;
    announce_ready()?;

    let (directory_stream, directory_peer) = directory_listener
        .accept(TransportConfig::default())
        .await?;
    serve_directory(directory_stream, directory_peer, snapshot).await?;
    let (business_stream, business_peer) =
        business_listener.accept(TransportConfig::default()).await?;
    serve_business(business_stream, business_peer).await?;
    Ok(())
}

async fn serve_directory<S>(
    stream: S,
    peer: PeerIdentity,
    directory: SnapshotDirectory,
) -> Result<(), nlos_ipc::IpcError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    serve_one(
        stream,
        TransportConfig::default(),
        peer,
        &AllowConformancePeer,
        move |request| async move {
            let mut envelope = request.envelope().clone();
            let response =
                if envelope.service == DIRECTORY_SERVICE && envelope.method == NEGOTIATE_METHOD {
                    let negotiation = decode_negotiate_service_request(&envelope.payload)?;
                    directory.negotiate(&negotiation)
                } else {
                    NegotiateServiceResponse {
                        schema: Some(service_directory_schema_identity()),
                        result: Some(negotiate_service_response::Result::Error(DirectoryError {
                            code: DirectoryErrorCode::InvalidRequest.into(),
                            service: String::new(),
                        })),
                    }
                };
            envelope.payload = encode_negotiate_service_response(&response)?;
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(envelope),
            }))
        },
    )
    .await
}

async fn serve_business<S>(stream: S, peer: PeerIdentity) -> Result<(), nlos_ipc::IpcError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    serve_one(
        stream,
        TransportConfig::default(),
        peer,
        &AllowConformancePeer,
        |request| async move {
            let request_context = validate_sabi_request_context(
                request.envelope(),
                MethodSemantics::LONG_RUNNING_MUTATION,
                123_455,
            )?;
            let mut envelope = request.envelope().clone();
            envelope.common_context = Some(envelope::CommonContext::ResponseContext(
                SabiResponseContext {
                    correlation_id: request_context.correlation_id.clone(),
                    operation: Some(OperationReference {
                        operation_id: vec![8; 16],
                        generation: 4,
                    }),
                    receipts: vec![ReceiptReference {
                        receipt_id: vec![9; 16],
                    }],
                    failure: None,
                },
            ));
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(envelope),
            }))
        },
    )
    .await
}
