use std::future::poll_fn;
use std::sync::Arc;

use argh::FromArgs;
use http::Uri;
use rand::Rng;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tonic_h3::msquic_async::h3_msquic_async::{msquic, msquic_async};
use tracing::{debug, info};

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
                .set_StreamMultiReceiveEnabled()
                .set_ServerMigrationEnabled(),
        ),
    )?;

    #[cfg(not(windows))]
    {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let cert = include_bytes!("../../certs/client.crt");
        let key = include_bytes!("../../certs/client.key");
        let ca_cert = include_bytes!("../../certs/ca.crt");

        let mut cert_file = NamedTempFile::new()?;
        cert_file.write_all(cert)?;
        let cert_path = cert_file.into_temp_path();
        let cert_path = cert_path.to_string_lossy().into_owned();

        let mut key_file = NamedTempFile::new()?;
        key_file.write_all(key)?;
        let key_path = key_file.into_temp_path();
        let key_path = key_path.to_string_lossy().into_owned();

        let mut ca_cert_file = NamedTempFile::new()?;
        ca_cert_file.write_all(ca_cert)?;
        let ca_cert_path = ca_cert_file.into_temp_path();
        let ca_cert_path = ca_cert_path.to_string_lossy().into_owned();

        let cred_config = msquic::CredentialConfig::new_client()
            .set_credential(msquic::Credential::CertificateFile(
                msquic::CertificateFile::new(key_path, cert_path),
            ))
            .set_ca_certificate_file(ca_cert_path);

        configuration.load_credential(&cred_config)?;
    }

    #[cfg(windows)]
    {
        use schannel::cert_context::{CertContext, KeySpec};
        use schannel::cert_store::{CertAdd, Memory};
        use schannel::crypt_prov::{AcquireOptions, ProviderType};
        use schannel::RawPointer;

        let cert = include_str!("../../certs/client.crt");
        let key = include_bytes!("../../certs/client.key");

        let mut store = Memory::new().unwrap().into_store();

        let name = String::from("msquic-async-example-client");

        let cert_ctx = CertContext::from_pem(cert).unwrap();

        let mut options = AcquireOptions::new();
        options.container(&name);

        let type_ = ProviderType::rsa_full();

        let mut container = match options.acquire(type_) {
            Ok(container) => container,
            Err(_) => options.new_keyset(true).acquire(type_).unwrap(),
        };
        container.import().import_pkcs8_pem(key).unwrap();

        cert_ctx
            .set_key_prov_info()
            .container(&name)
            .type_(type_)
            .keep_open(true)
            .key_spec(KeySpec::key_exchange())
            .set()
            .unwrap();

        let context = store.add_cert(&cert_ctx, CertAdd::Always).unwrap();

        let cred_config = msquic::CredentialConfig::new_client().set_credential(
            msquic::Credential::CertificateContext(unsafe { context.as_ptr() }),
        );

        configuration.load_credential(&cred_config)?;
    }

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
                    match event {
                        msquic_async::ConnectionEvent::NotifyObservedAddress {
                            local_address,
                            observed_address,
                        } => {
                            info!(
                                "local address: {}, observed address: {}",
                                local_address, observed_address
                            );
                            let mut rng = rand::thread_rng();
                            let bound_addr = loop {
                                let port = rng.gen_range(32768..=65535);
                                let mut bound_addr = local_address.clone();
                                bound_addr.set_port(port);
                                if conn.add_bound_addr(bound_addr.clone()).is_ok() {
                                    info!("added bound address: {}", bound_addr);
                                    break bound_addr;
                                }
                            };
                            conn.add_observed_addr(bound_addr.clone(), bound_addr)?;
                        }
                        msquic_async::ConnectionEvent::NotifyRemoteAddressAdded {
                            address,
                            sequence_number,
                        } => {
                            info!(
                                "Added remote address: {}, sequence number: {}",
                                address, sequence_number
                            );
                        }
                        msquic_async::ConnectionEvent::PathValidated {
                            local_address,
                            remote_address,
                        } => {
                            info!(
                                "path validated local address: {}, remote address: {}",
                                local_address, remote_address
                            );
                        }
                        msquic_async::ConnectionEvent::NotifyRemoteAddressRemoved {
                            sequence_number,
                        } => {
                            info!(
                                "Removed remote address with sequence number: {}",
                                sequence_number
                            );
                        }
                    }
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
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
    Ok(())
}
