//! A channel served as a remote plugin (`ChannelPlugin::serve_remote`) on `127.0.0.1`: the bearer
//! check on every service, a custom verifier, TLS with a test CA, a core disconnecting and
//! connecting again, and stopping on the shutdown signal.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    Channel, ChannelEvent, ChannelInfo, EventId, MessageId, OutboundMessage, OutputMode, Result,
};
use giles_plugin_sdk::{ChannelPlugin, Configure, RemoteAuth, RemoteServer, async_trait};
use giles_proto::plugin::v1::plugin_service_client::PluginServiceClient;
use giles_proto::plugin::v1::{ConfigureRequest, DescribeRequest, ShutdownRequest};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::Code;
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel as GrpcChannel, Endpoint, Uri};

const WAIT: Duration = Duration::from_secs(10);

struct Quiet {
    info: ChannelInfo,
}

#[async_trait]
impl Channel for Quiet {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }
    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        Ok(stream::pending().boxed())
    }
    async fn ack(&self, _event: &EventId) -> Result<()> {
        Ok(())
    }
    async fn send(&self, _msg: OutboundMessage) -> Result<MessageId> {
        Ok(MessageId::new("1")?)
    }
}

/// A running remote plugin.
struct Plugin {
    _data: tempfile::TempDir,
    addr: SocketAddr,
    stop: CancellationToken,
    task: JoinHandle<io::Result<()>>,
}

impl Plugin {
    async fn start(auth: RemoteAuth, tls: Option<rustls::ServerConfig>) -> Self {
        let data = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stop = CancellationToken::new();
        let mut server = RemoteServer::new(listener, data.path().join("state"), auth)
            .shutdown_on(stop.clone().cancelled_owned());
        if let Some(tls) = tls {
            server = server.tls(tls);
        }
        let addr = server.local_addr().unwrap();
        let task = tokio::spawn(ChannelPlugin::new("quiet", "1.0.0").serve_remote(
            server,
            |ctx: Configure| async move {
                Ok(Quiet {
                    info: ChannelInfo {
                        identity_namespace: format!("quiet:{}", ctx.instance()),
                        output: OutputMode::Blocks,
                        can_initiate: false,
                    },
                })
            },
        ));
        Self {
            _data: data,
            addr,
            stop,
            task,
        }
    }

    async fn stop(self) {
        self.stop.cancel();
        tokio::time::timeout(WAIT, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[derive(Clone)]
struct Bearer(Option<&'static str>);

impl Interceptor for Bearer {
    fn call(&mut self, mut request: tonic::Request<()>) -> tonic::Result<tonic::Request<()>> {
        if let Some(token) = self.0 {
            let value: MetadataValue<_> = format!("Bearer {token}").parse().unwrap();
            request.metadata_mut().insert("authorization", value);
        }
        Ok(request)
    }
}

type Client = PluginServiceClient<InterceptedService<GrpcChannel, Bearer>>;

async fn plain(addr: SocketAddr, token: Option<&'static str>) -> Client {
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    PluginServiceClient::with_interceptor(channel, Bearer(token))
}

/// Connects over TLS to `addr` as `localhost`, trusting `ca` only.
async fn tls(
    addr: SocketAddr,
    ca: &rustls::pki_types::CertificateDer<'static>,
) -> io::Result<Client> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.clone()).unwrap();
    let mut config = giles_plugin_sdk::tls::client_config_with_roots(roots);
    config.alpn_protocols = vec![b"h2".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let channel = Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let connector = connector.clone();
            async move {
                let tcp = TcpStream::connect(addr).await?;
                let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
                Ok::<_, io::Error>(TokioIo::new(connector.connect(name, tcp).await?))
            }
        }))
        .await
        .map_err(io::Error::other)?;
    Ok(PluginServiceClient::with_interceptor(
        channel,
        Bearer(Some("s3cret")),
    ))
}

async fn describe(client: &mut Client) -> std::result::Result<String, Code> {
    client
        .describe(DescribeRequest::default())
        .await
        .map(|r| r.into_inner().name)
        .map_err(|status| status.code())
}

async fn configure(client: &mut Client) -> std::result::Result<String, Code> {
    client
        .configure(ConfigureRequest {
            instance: "remote".into(),
            config_json: "{}".into(),
        })
        .await
        .map(|r| r.into_inner().identity_namespace)
        .map_err(|status| status.code())
}

#[tokio::test]
async fn every_call_needs_the_bearer_token() {
    let plugin = Plugin::start(RemoteAuth::Bearer("s3cret".into()), None).await;
    for token in [None, Some("wrong"), Some("s3cret2")] {
        let mut client = plain(plugin.addr, token).await;
        assert_eq!(describe(&mut client).await, Err(Code::Unauthenticated));
        let mut health = tonic_health::pb::health_client::HealthClient::with_interceptor(
            Endpoint::from_shared(format!("http://{}", plugin.addr))
                .unwrap()
                .connect()
                .await
                .unwrap(),
            Bearer(token),
        );
        let checked = health
            .check(tonic_health::pb::HealthCheckRequest::default())
            .await;
        assert_eq!(checked.unwrap_err().code(), Code::Unauthenticated);
    }
    let mut client = plain(plugin.addr, Some("s3cret")).await;
    assert_eq!(describe(&mut client).await.as_deref(), Ok("quiet"));
    plugin.stop().await;
}

#[tokio::test]
async fn an_empty_bearer_token_accepts_no_call() {
    let plugin = Plugin::start(RemoteAuth::Bearer(String::new()), None).await;
    for token in [None, Some(""), Some("anything")] {
        let mut client = plain(plugin.addr, token).await;
        assert_eq!(
            describe(&mut client).await,
            Err(Code::Unauthenticated),
            "{token:?}"
        );
    }
    plugin.stop().await;
}

#[tokio::test]
async fn a_verifier_decides_which_tokens_pass() {
    let auth = RemoteAuth::verify(|token| token.starts_with("issued-"));
    let plugin = Plugin::start(auth, None).await;
    let mut client = plain(plugin.addr, Some("issued-1")).await;
    assert_eq!(describe(&mut client).await.as_deref(), Ok("quiet"));
    let mut client = plain(plugin.addr, Some("forged")).await;
    assert_eq!(describe(&mut client).await, Err(Code::Unauthenticated));
    plugin.stop().await;
}

#[tokio::test]
async fn a_core_can_disconnect_and_connect_again() {
    let plugin = Plugin::start(RemoteAuth::Bearer("s3cret".into()), None).await;
    let mut first = plain(plugin.addr, Some("s3cret")).await;
    assert_eq!(configure(&mut first).await.as_deref(), Ok("quiet:remote"));
    first.shutdown(ShutdownRequest {}).await.unwrap();
    drop(first);

    // Still serving: a new connection configures it the same way.
    let mut second = plain(plugin.addr, Some("s3cret")).await;
    assert_eq!(configure(&mut second).await.as_deref(), Ok("quiet:remote"));
    assert!(!plugin.task.is_finished());
    plugin.stop().await;
}

#[tokio::test]
async fn tls_serves_cores_that_trust_the_certificate() {
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    let config = giles_plugin_sdk::tls::server_config(
        format!("{}{}", cert.pem(), ca.pem()).as_bytes(),
        key.serialize_pem().as_bytes(),
    )
    .unwrap();
    let plugin = Plugin::start(RemoteAuth::Bearer("s3cret".into()), Some(config)).await;

    let mut client = tls(plugin.addr, ca.der()).await.unwrap();
    assert_eq!(describe(&mut client).await.as_deref(), Ok("quiet"));

    // A core that doesn't trust the CA, or speaks plain text, gets nowhere.
    let other = CertifiedIssuer::self_signed(
        {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params
        },
        KeyPair::generate().unwrap(),
    )
    .unwrap();
    assert!(tls(plugin.addr, other.der()).await.is_err());
    let plain = Endpoint::from_shared(format!("http://{}", plugin.addr))
        .unwrap()
        .connect()
        .await;
    if let Ok(channel) = plain {
        let mut client = PluginServiceClient::with_interceptor(channel, Bearer(Some("s3cret")));
        assert!(describe(&mut client).await.is_err());
    }
    plugin.stop().await;
}

#[test]
fn bad_tls_material_is_refused() {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let other = rcgen::KeyPair::generate().unwrap();
    let config = |cert: &str, key: &str| {
        giles_plugin_sdk::tls::server_config(cert.as_bytes(), key.as_bytes())
            .map(drop)
            .map_err(|e| e.to_string())
    };
    assert!(config(&cert.pem(), &key.serialize_pem()).is_ok());
    let no_cert = config("", &key.serialize_pem()).unwrap_err();
    assert!(no_cert.contains("no certificate"), "{no_cert}");
    let no_key = config(&cert.pem(), "").unwrap_err();
    assert!(no_key.contains("private key"), "{no_key}");
    let mismatch = config(&cert.pem(), &other.serialize_pem()).unwrap_err();
    assert!(mismatch.contains("certificate and key"), "{mismatch}");
}
