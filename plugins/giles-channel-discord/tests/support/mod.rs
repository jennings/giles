//! A Discord plugin served by the real plugin SDK (its spool and gRPC services) with a fake Discord
//! client behind it: what a core would see over the wire.

// Each test binary uses part of it.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use giles_channel_discord::channel::{SessionSlot, configure_with};
use giles_channel_discord::client::Discord;
use giles_channel_discord::fake::FakeDiscord;
use giles_channel_discord::session::Timing;
use giles_plugin_sdk::convert::event_from_proto;
use giles_plugin_sdk::giles_types::{ChannelEvent, ChannelStatus, OutputMode};
use giles_plugin_sdk::{ChannelPlugin, Configure, RemoteAuth, RemoteServer};
use giles_proto::channel::v1::channel_service_client::ChannelServiceClient;
use giles_proto::channel::v1::{EventAck, EventsRequest, EventsResponse, events_request};
use giles_proto::plugin::v1::ConfigureRequest;
use giles_proto::plugin::v1::plugin_service_client::PluginServiceClient;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::Streaming;
use tonic::transport::{Channel, Endpoint};

/// A token-shaped string for tests. Not a real credential.
pub const TOKEN: &str = "MTIzNDU2.Gabcde.not-a-real-token-1234567890";

/// Upper bound for anything in a test that must not hang.
pub const WAIT: Duration = Duration::from_secs(10);

/// A Discord plugin served over loopback TCP, in this process.
pub struct Served {
    channel: Channel,
    data: PathBuf,
    pub client: Arc<FakeDiscord>,
    stop: Option<oneshot::Sender<()>>,
    _dir: tempfile::TempDir,
}

impl Served {
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(FakeDiscord::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = RemoteServer::new(listener, dir.path(), RemoteAuth::None).shutdown_on(async {
            let _ = stopped.await;
        });
        let plugin = ChannelPlugin::new("discord", "test")
            .output(OutputMode::Blocks)
            .can_initiate(true);
        let fake = Arc::clone(&client);
        tokio::spawn(plugin.serve_remote(server, move |ctx: Configure| {
            let fake = Arc::clone(&fake);
            async move {
                configure_with(ctx, &SessionSlot::default(), Timing::default(), |_| {
                    fake as Arc<dyn Discord>
                })
                .await
            }
        }));
        let channel = Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        Self {
            channel,
            data: dir.path().to_owned(),
            client,
            stop: Some(stop),
            _dir: dir,
        }
    }

    pub fn plugin(&self) -> PluginServiceClient<Channel> {
        PluginServiceClient::new(self.channel.clone())
    }

    pub fn rpc(&self) -> ChannelServiceClient<Channel> {
        ChannelServiceClient::new(self.channel.clone())
    }

    pub async fn configure(&self, config: serde_json::Value) -> Result<String, tonic::Status> {
        let response = self
            .plugin()
            .configure(ConfigureRequest {
                instance: "test".into(),
                config_json: config.to_string(),
            })
            .await?;
        Ok(response.into_inner().identity_namespace)
    }

    pub async fn events(
        &self,
    ) -> (
        mpsc::UnboundedSender<EventsRequest>,
        Streaming<EventsResponse>,
    ) {
        let (acks, requests) = mpsc::unbounded();
        let events = self.rpc().events(requests).await.unwrap().into_inner();
        (acks, events)
    }

    /// The events the SDK holds on disk, unacknowledged.
    pub fn spooled(&self) -> usize {
        std::fs::read_dir(self.data.join("giles-sdk/events")).map_or(0, Iterator::count)
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

pub async fn next_status(events: &mut Streaming<EventsResponse>) -> (String, ChannelStatus) {
    let response = tokio::time::timeout(WAIT, events.message())
        .await
        .expect("an event in time")
        .unwrap()
        .expect("the stream is open");
    match event_from_proto(response.event.unwrap()).unwrap() {
        ChannelEvent::Status { id, status } => (id.to_string(), status),
        other => panic!("not a status: {other:?}"),
    }
}

pub fn ack(id: &str) -> EventsRequest {
    EventsRequest {
        request: Some(events_request::Request::Ack(EventAck {
            event_id: id.into(),
        })),
    }
}

pub async fn wait_until(mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
