use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;
use tonic_h3::msquic_async::h3_msquic_async::{msquic, msquic_async};

tonic::include_proto!("helloworld");

#[derive(Default)]
pub struct HelloWorldService {}

#[tonic::async_trait]
impl crate::greeter_server::Greeter for HelloWorldService {
    async fn say_hello(
        &self,
        req: tonic::Request<HelloRequest>,
    ) -> Result<tonic::Response<HelloReply>, tonic::Status> {
        let name = req.into_inner().name;
        tracing::debug!("say_hello: {}", name);
        Ok(tonic::Response::new(HelloReply {
            message: format!("hello {name}"),
        }))
    }
}

fn make_msquic_async_listner(addr: Option<SocketAddr>) -> anyhow::Result<(Arc<msquic::Registration>, msquic_async::Listener)> {
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
                .set_StreamMultiReceiveEnabled(),
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
        .with_max_level(tracing::Level::TRACE)
        .init();

    let token = CancellationToken::new();
    let addr: SocketAddr = "127.0.0.1:5047".parse()?;
    let (_registration, listener) = make_msquic_async_listner(Some(addr))?;
    let listen_addr = listener.local_addr()?;
    tracing::debug!("listenaddr : {}", listen_addr);
    let acceptor = tonic_h3::msquic_async::H3MsQuicAsyncAcceptor::new(listener);

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
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for event");
    token.cancel();
    let _ = handle_svc.await?;
    Ok(())
}
