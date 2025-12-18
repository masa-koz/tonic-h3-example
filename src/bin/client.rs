use http::Uri;
use std::sync::Arc;
use tonic_h3::msquic_async::h3_msquic_async::msquic;

tonic::include_proto!("helloworld");

fn make_msquic_async_reg_and_config() -> anyhow::Result<(Arc<msquic::Registration>, Arc<msquic::Configuration>)> {
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

    let cred_config = msquic::CredentialConfig::new_client()
        .set_credential_flags(msquic::CredentialFlags::NO_CERTIFICATE_VALIDATION);
    configuration.load_credential(&cred_config)?;
    Ok((Arc::new(registration), Arc::new(configuration)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::TRACE)
        .init();

    let uri: Uri = "https://127.0.0.1:5047".parse().unwrap();
    let (reg, config) = make_msquic_async_reg_and_config()?;
    let connector = tonic_h3::msquic_async::H3MsQuicAsyncConnector::new(uri.clone(), config, reg.clone());
    let channel = tonic_h3::H3Channel::new(connector, uri.clone());

    tracing::debug!("making greeter client.");
    let mut client = greeter_client::GreeterClient::new(channel);

    tracing::debug!("sending request.");
    {
        let request = tonic::Request::new(HelloRequest {
            name: "Tonic".into(),
        });
        let response = client.say_hello(request).await.unwrap();

        tracing::debug!("RESPONSE={:?}", response);
    }
    Ok(())
}
