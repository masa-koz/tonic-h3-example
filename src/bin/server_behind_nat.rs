use std::future::poll_fn;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use tempfile::NamedTempFile;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tonic_h3::msquic_async::h3_msquic_async::{msquic, msquic_async};
use tracing::{debug, error, info};

tonic::include_proto!("helloworld");

#[derive(Default)]
pub struct HelloWorldService {}

#[tonic::async_trait]
impl crate::greeter_server::Greeter for HelloWorldService {
    async fn say_hello(
        &self,
        req: tonic::Request<HelloRequest>,
    ) -> Result<tonic::Response<HelloReply>, tonic::Status> {
        tracing::debug!("say_hello: {:?}", req);
        let name = req.into_inner().name;
        Ok(tonic::Response::new(HelloReply {
            message: format!("hello {name}"),
        }))
    }
}

fn make_msquic_async_listner(
    addr: Option<SocketAddr>,
) -> anyhow::Result<(Arc<msquic::Registration>, msquic_async::Listener)> {
    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default())?;
    let alpn = [msquic::BufferRef::from("h3")];
    let configuration = msquic::Configuration::open(
        &registration,
        &alpn,
        Some(
            &msquic::Settings::new()
                .set_IdleTimeoutMs(10000)
                .set_PeerBidiStreamCount(100)
                .set_PeerUnidiStreamCount(100)
                .set_DatagramReceiveEnabled()
                .set_StreamMultiReceiveEnabled()
                .set_ServerMigrationEnabled(),
        ),
    )?;

    let cert = include_bytes!("cert.pem");
    let key = include_bytes!("key.pem");

    let mut cert_file = NamedTempFile::new()?;
    cert_file.write_all(cert)?;
    let cert_path = cert_file.into_temp_path();
    let cert_path = cert_path.to_string_lossy().into_owned();

    let mut key_file = NamedTempFile::new()?;
    key_file.write_all(key)?;
    let key_path = key_file.into_temp_path();
    let key_path = key_path.to_string_lossy().into_owned();

    let cred_config = msquic::CredentialConfig::new().set_credential(
        msquic::Credential::CertificateFile(msquic::CertificateFile::new(key_path, cert_path)),
    );

    configuration.load_credential(&cred_config)?;
    let listner = msquic_async::Listener::new(&registration, configuration)?;
    listner.start(&alpn, addr)?;
    Ok((Arc::new(registration), listner))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let token = CancellationToken::new();
    let addr: SocketAddr = "127.0.0.1:5047".parse()?;
    let (registration, listener) = make_msquic_async_listner(Some(addr))?;
    let listen_addr = listener.local_addr()?;
    debug!("listenaddr : {}", listen_addr);

    let local_bind_addr: SocketAddr = "127.0.0.1:5047".parse()?;
    let server_addr: SocketAddr = "153.127.33.247:4443".parse()?;
    let target_addr: Option<SocketAddr> = None;

    let (handle_masque, mut event_receiver) = h3_masque::client::connect_udp_bind_proxy(
        &registration,
        local_bind_addr,
        server_addr,
        target_addr,
    )
    .await?;
    let (observed_sender, mut observed_receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        while let Some(event) = event_receiver.recv().await {
            match event {
                h3_masque::client::BoundProxyEvent::NotifyPublicAddress(public_addr) => {
                    info!("Received public addresses: {}", public_addr);
                }
                h3_masque::client::BoundProxyEvent::NotifyObservedAddress {
                    local_address,
                    observed_address,
                } => {
                    info!(
                        "Observed address for local address {} is {}",
                        local_address, observed_address
                    );
                    observed_sender
                        .send((local_address, observed_address))
                        .await?;
                }
            }
        }
        anyhow::Ok(())
    });

    let acceptor = tonic_h3::msquic_async::H3MsQuicAsyncAcceptor::new(listener);
    let (conn_sender, mut conn_receiver) = mpsc::channel(1);
    let acceptor = acceptor.with_channel(conn_sender);

    let hello_svc = HelloWorldService {};
    let router = tonic::service::Routes::builder()
        .add_service(crate::greeter_server::GreeterServer::new(hello_svc))
        .clone()
        .routes();

    // run server in background
    let token_cloned = token.clone();
    let handle_svc = tokio::spawn(async move {
        tonic_h3::server::H3Router::new(router)
            .serve_with_shutdown(acceptor, async move { token_cloned.cancelled().await })
            .await
    });
    tokio::spawn(async move {
        let Some((_local_address, _observed_address)) = observed_receiver.recv().await else {
            error!("did not receive observed address");
            return Ok(());
        };
        let mut set = JoinSet::new();
        while let Some(conn) = conn_receiver.recv().await {
            set.spawn(async move {
                let unspecified_address = "0.0.0.0:0".parse::<SocketAddr>()?;
                conn.add_local_addr(unspecified_address.clone())?;

                while let Ok(event) = poll_fn(|cx| conn.poll_event(cx)).await {
                    debug!("conn event: {:?}", event);
                    match event {
                        msquic_async::ConnectionEvent::PathValidated { local_address, remote_address } => {
                            if !local_address.ip().is_loopback() {
                                info!("Activated path: local_address={}, remote_address={}", local_address, remote_address);
                                conn.activate_path(local_address, remote_address)?;
                            }
                        }
                        _ => {}
                    }
                }
                debug!("connection task ended");
                anyhow::Ok(())
            });
        }
        set.join_all().await;
        debug!("all connection tasks ended");
        anyhow::Ok(())
    });

    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for event");
    token.cancel();
    let _ = handle_svc.await?;
    handle_masque.abort();
    Ok(())
}
