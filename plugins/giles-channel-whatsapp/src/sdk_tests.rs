//! The channel served by the real plugin SDK (its spool, blob store, and gRPC services) with a
//! fake WhatsApp client behind it: what a core would see over the wire. This is where the
//! durability promise is checked against real files: when a message's acknowledgement is released
//! to the client, the event is on disk.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::stream;
use giles_plugin_sdk::convert::event_from_proto;
use giles_plugin_sdk::giles_types::{ChannelEvent, ChannelStatus};
use giles_plugin_sdk::{ChannelPlugin, Configure, RemoteAuth, RemoteServer};
use giles_proto::channel::v1::channel_service_client::ChannelServiceClient;
use giles_proto::channel::v1::{
    EventAck, EventsRequest, EventsResponse, OutputMode, SendRequest, events_request, send_request,
};
use giles_proto::plugin::v1::plugin_service_client::PluginServiceClient;
use giles_proto::plugin::v1::{
    BlobChunk, ConfigureRequest, DescribeRequest, GetBlobRequest, PluginKind, PutBlobRequest,
    blob_chunk, describe_response,
};
use giles_proto::types::v1::BlobRef;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Streaming};

use crate::backend::{Connector, WaEvent};
use crate::channel::WhatsApp;
use crate::config::Settings;
use crate::datadir::DataDir;
use crate::session::Timing;
use crate::testing::{FakeClient, FakeConnector, PHONE, Sent, WAIT, dm, image, wait_until};

/// A WhatsApp plugin served over loopback TCP, in this process.
struct Served {
    channel: Channel,
    data: PathBuf,
    client: FakeClient,
    stop: Option<oneshot::Sender<()>>,
    _dir: tempfile::TempDir,
}

impl Served {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self::start_in(dir, None).await
    }

    /// Serves with data directory `dir`; `client` is the fake the connector hands out.
    async fn start_in(
        dir: tempfile::TempDir,
        client: Option<(FakeClient, crate::backend::Connection)>,
    ) -> Self {
        let (client, connection) = client.unwrap_or_else(FakeClient::new);
        let connector = Arc::new(FakeConnector::default());
        connector.push(Ok(connection));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = RemoteServer::new(listener, dir.path(), RemoteAuth::None).shutdown_on(async {
            let _ = stopped.await;
        });
        let plugin = ChannelPlugin::new("whatsapp", "test")
            .output(giles_plugin_sdk::giles_types::OutputMode::Blocks)
            .can_initiate(true);
        tokio::spawn(plugin.serve_remote(server, move |ctx: Configure| {
            let connector = Arc::clone(&connector) as Arc<dyn Connector>;
            async move {
                let settings = Arc::new(Settings::new(ctx.parse_config()?)?);
                let data = DataDir::open(ctx.data_dir())?;
                Ok(WhatsApp::start(
                    settings,
                    connector,
                    Arc::new(ctx.blobs().clone()),
                    data,
                    Timing::default(),
                ))
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

    fn plugin(&self) -> PluginServiceClient<Channel> {
        PluginServiceClient::new(self.channel.clone())
    }

    fn rpc(&self) -> ChannelServiceClient<Channel> {
        ChannelServiceClient::new(self.channel.clone())
    }

    async fn configure(&self, config: serde_json::Value) -> Result<String, tonic::Status> {
        let response = self
            .plugin()
            .configure(ConfigureRequest {
                instance: "test".into(),
                config_json: config.to_string(),
            })
            .await?;
        Ok(response.into_inner().identity_namespace)
    }

    async fn events(
        &self,
    ) -> (
        mpsc::UnboundedSender<EventsRequest>,
        Streaming<EventsResponse>,
    ) {
        let (acks, requests) = mpsc::unbounded();
        let events = self.rpc().events(requests).await.unwrap().into_inner();
        (acks, events)
    }

    fn spooled(&self) -> usize {
        std::fs::read_dir(self.data.join("giles-sdk/events")).map_or(0, Iterator::count)
    }

    fn blobs(&self) -> usize {
        std::fs::read_dir(self.data.join("giles-sdk/blobs")).map_or(0, Iterator::count)
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

async fn next_event(events: &mut Streaming<EventsResponse>) -> ChannelEvent {
    let response = tokio::time::timeout(WAIT, events.message())
        .await
        .expect("an event in time")
        .unwrap()
        .expect("the stream is open");
    event_from_proto(response.event.unwrap()).unwrap()
}

fn ack(id: &str) -> EventsRequest {
    EventsRequest {
        request: Some(events_request::Request::Ack(EventAck {
            event_id: id.into(),
        })),
    }
}

async fn resolved(durable: oneshot::Receiver<bool>) -> bool {
    tokio::time::timeout(WAIT, durable)
        .await
        .expect("the commit resolves")
        .expect("the commit isn't dropped")
}

#[tokio::test]
async fn describe_and_configure_over_the_wire() {
    let served = Served::start().await;
    let described = served
        .plugin()
        .describe(DescribeRequest {
            core_protocol_minor: 0,
            core_features: vec!["channel.status".into()],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(described.kind(), PluginKind::Channel);
    assert_eq!(described.name, "whatsapp");
    let Some(describe_response::Capabilities::Channel(capabilities)) = described.capabilities
    else {
        panic!("no channel capabilities");
    };
    assert_eq!(capabilities.output(), OutputMode::Blocks);
    assert!(capabilities.can_initiate);

    // Bad config is refused with the key named, and a good one reports the namespace.
    for (config, needle) in [
        (
            serde_json::json!({"phone": "+15551234567"}),
            "unknown field `phone`",
        ),
        (
            serde_json::json!({"link": "pair_code"}),
            "phone_number: required",
        ),
    ] {
        let status = served.configure(config).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(status.message().contains(needle), "{status:?}");
    }
    assert_eq!(
        served.configure(serde_json::json!({})).await.unwrap(),
        "whatsapp"
    );
}

#[tokio::test]
async fn a_message_is_on_disk_before_whatsapp_is_told_and_survives_until_acked() {
    let served = Served::start().await;
    served.configure(serde_json::json!({})).await.unwrap();
    let (acks, mut events) = served.events().await;
    served.client.emit(WaEvent::Connected);
    let ChannelEvent::Status { id, status } = next_event(&mut events).await else {
        panic!("not a status");
    };
    assert_eq!(status, ChannelStatus::Connected);
    acks.unbounded_send(ack(id.as_str())).unwrap();
    wait_until(|| served.spooled() == 0).await;

    let durable = served.client.deliver(vec![dm("3EB0AAAA", "hello")]);
    let ChannelEvent::Message { id, message } = next_event(&mut events).await else {
        panic!("not a message");
    };
    assert_eq!(message.text, "hello");
    assert_eq!(message.sender.as_str(), format!("+{PHONE}"));
    // The client is released only once the SDK has the event on disk.
    assert!(resolved(durable).await);
    assert_eq!(
        served.spooled(),
        1,
        "the event file exists when WhatsApp is told"
    );

    // Not acked by the core yet: a reconnecting core gets it again, from the spool.
    drop(acks);
    drop(events);
    let (acks, mut events) = served.events().await;
    let again = next_event(&mut events).await;
    assert_eq!(again.id(), &id);
    acks.unbounded_send(ack(id.as_str())).unwrap();
    wait_until(|| served.spooled() == 0).await;
}

#[tokio::test]
async fn an_attachment_is_a_blob_the_core_can_fetch_until_it_acks() {
    let served = Served::start().await;
    served.configure(serde_json::json!({})).await.unwrap();
    let (acks, mut events) = served.events().await;
    served
        .client
        .backend
        .downloads
        .lock()
        .unwrap()
        .insert("/v/img".into(), b"jpeg bytes".to_vec());
    let mut message = dm("3EB0BBBB", "a photo");
    message.media = Some(image("/v/img", Some(10)));
    let durable = served.client.deliver(vec![message]);
    let ChannelEvent::Message { id, message } = next_event(&mut events).await else {
        panic!("not a message");
    };
    assert!(resolved(durable).await);
    let [media] = &message.attachments[..] else {
        panic!("one attachment: {message:?}");
    };
    assert_eq!((media.media_type.as_str(), media.size), ("image/jpeg", 10));

    let mut stream = served
        .plugin()
        .get_blob(GetBlobRequest {
            id: media.id.to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    let mut bytes = Vec::new();
    while let Some(response) = stream.message().await.unwrap() {
        if let blob_chunk::Part::Data(data) = response.chunk.unwrap().part.unwrap() {
            bytes.extend(data);
        }
    }
    assert_eq!(bytes, b"jpeg bytes");
    assert_eq!(served.blobs() % 2, 0);

    acks.unbounded_send(ack(id.as_str())).unwrap();
    wait_until(|| served.blobs() == 0 && served.spooled() == 0).await;
}

#[tokio::test]
async fn a_send_reads_the_blob_the_core_put_and_goes_to_whatsapp() {
    let served = Served::start().await;
    served.configure(serde_json::json!({})).await.unwrap();
    let (_acks, mut events) = served.events().await;
    served.client.emit(WaEvent::Connected);
    let _ = next_event(&mut events).await;

    let chunk = |part| PutBlobRequest {
        chunk: Some(BlobChunk { part: Some(part) }),
    };
    let blob = served
        .plugin()
        .put_blob(stream::iter([
            chunk(blob_chunk::Part::Header(BlobRef {
                id: String::new(),
                media_type: "application/pdf".into(),
                size: 8,
                name: Some("notes.pdf".into()),
            })),
            chunk(blob_chunk::Part::Data(b"%PDF".to_vec())),
            chunk(blob_chunk::Part::Data(b"-1.7".to_vec())),
        ]))
        .await
        .unwrap()
        .into_inner()
        .blob
        .unwrap();
    let response = served
        .rpc()
        .send(SendRequest {
            target: Some(send_request::Target::ConversationId(format!(
                "{PHONE}@s.whatsapp.net"
            ))),
            markdown: "Here is the **file**".into(),
            attachments: vec![blob],
            reply_to: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.message_id, "3EB00000");
    let sent = served.client.backend.sent();
    assert_eq!(sent.len(), 1);
    let Sent::Media { to, media } = &sent[0] else {
        panic!("not media: {sent:?}");
    };
    assert_eq!(to, &format!("{PHONE}@s.whatsapp.net"));
    assert_eq!(media.bytes, b"%PDF-1.7");
    assert_eq!(media.name.as_deref(), Some("notes.pdf"));
    assert_eq!(media.caption.as_deref(), Some("Here is the *file*"));

    // A recipient the number of which isn't on WhatsApp is UNAVAILABLE on the wire.
    let status = served
        .rpc()
        .send(SendRequest {
            target: Some(send_request::Target::Recipient("+15550000000".into())),
            markdown: "hi".into(),
            attachments: vec![],
            reply_to: None,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unavailable);
    // And one that isn't an identity at all is INVALID_ARGUMENT.
    let status = served
        .rpc()
        .send(SendRequest {
            target: Some(send_request::Target::Recipient("account:stephen".into())),
            markdown: "hi".into(),
            attachments: vec![],
            reply_to: None,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn a_pairing_qr_code_reaches_the_core_as_needs_auth() {
    let served = Served::start().await;
    served.configure(serde_json::json!({})).await.unwrap();
    let (_acks, mut events) = served.events().await;
    served.client.emit(WaEvent::PairingQr {
        code: "2@ref,noise,identity,adv,1".into(),
        timeout: Duration::from_secs(60),
    });
    let ChannelEvent::Status { status, .. } = next_event(&mut events).await else {
        panic!("not a status");
    };
    let ChannelStatus::NeedsAuth {
        challenge,
        expires_at,
    } = status
    else {
        panic!("not NeedsAuth");
    };
    assert_eq!(
        challenge,
        giles_plugin_sdk::giles_types::AuthChallenge::QrCode {
            payload: "2@ref,noise,identity,adv,1".into()
        }
    );
    assert!(expires_at.is_some());
}
