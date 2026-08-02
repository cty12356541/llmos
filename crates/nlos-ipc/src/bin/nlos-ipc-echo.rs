use std::error::Error;
use std::ffi::OsString;
use std::io::{self, Write};
use std::time::Duration;

use nlos_ipc::{OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one};
use nlos_schema::sabi::v1::ExchangeResponse;
use tokio::time::sleep;

struct AllowConformancePeer;

impl PeerAuthorizer for AllowConformancePeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let endpoint = arguments.next().ok_or("missing endpoint argument")?;
    let delay = parse_delay(arguments.next())?;
    if arguments.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    run(endpoint, delay).await
}

fn parse_delay(value: Option<OsString>) -> Result<Duration, Box<dyn Error>> {
    let milliseconds = value
        .map(|value| value.into_string().map_err(|_| "delay must be UTF-8"))
        .transpose()?
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or_default();
    Ok(Duration::from_millis(milliseconds))
}

fn announce_ready() -> io::Result<()> {
    println!("READY");
    io::stdout().flush()
}

#[cfg(unix)]
async fn run(endpoint: OsString, delay: Duration) -> Result<(), Box<dyn Error>> {
    use std::fs;
    use std::path::PathBuf;

    use nlos_ipc::unix::UnixListenerAdapter;

    struct EndpointGuard(PathBuf);

    impl Drop for EndpointGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    let endpoint = PathBuf::from(endpoint);
    let listener = UnixListenerAdapter::bind(&endpoint)?;
    let _guard = EndpointGuard(endpoint);
    announce_ready()?;
    let (stream, peer) = listener.accept(TransportConfig::default()).await?;
    serve_echo(stream, peer, delay).await?;
    Ok(())
}

#[cfg(windows)]
async fn run(endpoint: OsString, delay: Duration) -> Result<(), Box<dyn Error>> {
    use nlos_ipc::windows::NamedPipeListenerAdapter;

    let mut listener = NamedPipeListenerAdapter::bind(endpoint, 2, TransportConfig::default())?;
    announce_ready()?;
    let (stream, peer) = listener.accept(TransportConfig::default()).await?;
    serve_echo(stream, peer, delay).await?;
    Ok(())
}

async fn serve_echo<S>(
    stream: S,
    peer: PeerIdentity,
    delay: Duration,
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
            sleep(delay).await;
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(request.envelope().clone()),
            }))
        },
    )
    .await
}
