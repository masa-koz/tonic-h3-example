use argh::FromArgs;
use http::Uri;
use std::future::poll_fn;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tonic_h3::msquic_async::h3_msquic_async::msquic;
use tracing::debug;

#[derive(FromArgs, Clone)]
/// client_public_ip args
struct CmdOptions {
    /// target server address
    #[argh(option, default = "String::from(\"127.0.0.1:5047\")")]
    target: String,
}

tonic::include_proto!("helloworld");

fn make_msquic_async_reg_and_config()
-> anyhow::Result<(Arc<msquic::Registration>, Arc<msquic::Configuration>)> {
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
    let cmd_opts: CmdOptions = argh::from_env();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let uri: Uri = format!("https://{}", cmd_opts.target).parse().unwrap();
    let (reg, config) = make_msquic_async_reg_and_config()?;
    let connector =
        tonic_h3::msquic_async::H3MsQuicAsyncConnector::new(uri.clone(), config, reg.clone());
    let (conn_sender, mut conn_receiver) = mpsc::channel(1);
    let connector = connector.with_channel(conn_sender);
    let channel = tonic_h3::H3Channel::new(connector, uri.clone());

    debug!("making greeter client.");
    let mut client = greeter_client::GreeterClient::new(channel);

    tokio::spawn(async move {
        let mut set = JoinSet::new();
        while let Some(conn) = conn_receiver.recv().await {
            set.spawn(async move {
                while let Ok(event) = poll_fn(|cx| conn.poll_event(cx)).await {
                    debug!("conn event: {:?}", event);
                }
                anyhow::Ok(())
            });
        }
        set.join_all().await;
        anyhow::Ok(())
    });

    debug!("sending request.");
    {
        let request = tonic::Request::new(HelloRequest {
            name: "Tonic".into(),
        });
        let response = client.say_hello(request).await.unwrap();

        debug!("RESPONSE={:?}", response);
    }
    Ok(())
}
