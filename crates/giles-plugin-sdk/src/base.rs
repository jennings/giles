//! `giles.plugin.v1.PluginService`, the base service every gRPC plugin serves (PLAN section 8.2).

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt as _};
use giles_proto::MAX_BLOB_CHUNK_SIZE;
use giles_proto::plugin::v1::plugin_service_server::PluginService;
use giles_proto::plugin::v1::{
    BlobChunk, ConfigureRequest, ConfigureResponse, DescribeRequest, DescribeResponse,
    GetBlobRequest, GetBlobResponse, PutBlobRequest, PutBlobResponse, ReleaseBlobRequest,
    ReleaseBlobResponse, ShutdownRequest, ShutdownResponse, blob_chunk,
};
use giles_proto::types::v1::BlobRef;
use giles_types::MediaId;
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};

use crate::blobs::Blobs;

/// The kind-specific half of `Describe` and `Configure`.
#[async_trait]
pub(crate) trait Kind: Send + Sync + 'static {
    /// The whole `Describe` answer.
    fn describe(&self) -> DescribeResponse;

    /// Builds the component from the instance config and returns the identity namespace (empty
    /// for kinds without one).
    async fn configure(
        &self,
        instance: String,
        config: serde_json::Value,
    ) -> Result<String, Status>;
}

/// Serves `PluginService` for one [`Kind`].
pub(crate) struct Base {
    kind: Arc<dyn Kind>,
    blobs: Blobs,
    shutdown: CancellationToken,
}

impl Base {
    /// `shutdown` is cancelled when the core calls `Shutdown`.
    pub(crate) fn new(kind: Arc<dyn Kind>, blobs: Blobs, shutdown: CancellationToken) -> Self {
        Self {
            kind,
            blobs,
            shutdown,
        }
    }
}

#[async_trait]
impl PluginService for Base {
    async fn describe(
        &self,
        request: Request<DescribeRequest>,
    ) -> Result<Response<DescribeResponse>, Status> {
        let request = request.into_inner();
        tracing::debug!(
            core_protocol_minor = request.core_protocol_minor,
            core_features = ?request.core_features,
            "Describe"
        );
        Ok(Response::new(self.kind.describe()))
    }

    async fn configure(
        &self,
        request: Request<ConfigureRequest>,
    ) -> Result<Response<ConfigureResponse>, Status> {
        let ConfigureRequest {
            instance,
            config_json,
        } = request.into_inner();
        let config = serde_json::from_str(&config_json)
            .map_err(|e| Status::invalid_argument(format!("config_json is not JSON: {e}")))?;
        let identity_namespace = self.kind.configure(instance, config).await?;
        Ok(Response::new(ConfigureResponse { identity_namespace }))
    }

    async fn shutdown(
        &self,
        _request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        tracing::info!("the core asked the plugin to shut down");
        self.shutdown.cancel();
        Ok(Response::new(ShutdownResponse {}))
    }

    async fn put_blob(
        &self,
        request: Request<Streaming<PutBlobRequest>>,
    ) -> Result<Response<PutBlobResponse>, Status> {
        let mut chunks = request.into_inner();
        let Some(blob_chunk::Part::Header(header)) = next_part(&mut chunks).await? else {
            return Err(Status::invalid_argument(
                "the first PutBlob chunk must be the header",
            ));
        };
        let mut writer = self.blobs.writer().await.map_err(internal)?;
        while let Some(part) = next_part(&mut chunks).await? {
            let blob_chunk::Part::Data(data) = part else {
                return Err(Status::invalid_argument("a second PutBlob header"));
            };
            if data.len() > MAX_BLOB_CHUNK_SIZE {
                return Err(Status::invalid_argument(format!(
                    "a PutBlob chunk of {} bytes; the limit is {MAX_BLOB_CHUNK_SIZE}",
                    data.len()
                )));
            }
            let len = u64::try_from(data.len()).expect("fits in u64");
            if writer.size() + len > header.size {
                return Err(Status::invalid_argument(format!(
                    "more PutBlob data than the {} bytes in the header",
                    header.size
                )));
            }
            writer.write(&data).await.map_err(internal)?;
        }
        if writer.size() != header.size {
            return Err(Status::invalid_argument(format!(
                "PutBlob ended after {} of the {} bytes in the header",
                writer.size(),
                header.size
            )));
        }
        let size = writer.size();
        if !header.id.is_empty() && header.id != writer.id().as_str() {
            return Err(Status::invalid_argument(format!(
                "PutBlob data has SHA-256 {}, not the header's id {}",
                writer.id(),
                header.id
            )));
        }
        let id = writer
            .commit_outbound(&self.blobs)
            .await
            .map_err(internal)?;
        Ok(Response::new(PutBlobResponse {
            blob: Some(BlobRef {
                id: id.to_string(),
                media_type: header.media_type,
                size,
                name: header.name,
            }),
        }))
    }

    type GetBlobStream = BoxStream<'static, Result<GetBlobResponse, Status>>;

    async fn get_blob(
        &self,
        request: Request<GetBlobRequest>,
    ) -> Result<Response<Self::GetBlobStream>, Status> {
        let id = parse_blob_id(&request.into_inner().id)?;
        let (header, file) = self
            .blobs
            .inbound(&id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no blob {id}")))?;
        let header = response(blob_chunk::Part::Header(header));
        let data = stream::try_unfold(file, |mut file| async move {
            let mut chunk = Vec::with_capacity(MAX_BLOB_CHUNK_SIZE);
            let limit = u64::try_from(MAX_BLOB_CHUNK_SIZE).expect("fits in u64");
            (&mut file)
                .take(limit)
                .read_to_end(&mut chunk)
                .await
                .map_err(internal)?;
            Ok((!chunk.is_empty()).then(|| (response(blob_chunk::Part::Data(chunk)), file)))
        });
        Ok(Response::new(
            stream::once(async { Ok(header) }).chain(data).boxed(),
        ))
    }

    async fn release_blob(
        &self,
        request: Request<ReleaseBlobRequest>,
    ) -> Result<Response<ReleaseBlobResponse>, Status> {
        let ids = request
            .into_inner()
            .ids
            .iter()
            .map(|id| parse_blob_id(id))
            .collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            self.blobs.release(&id).await.map_err(internal)?;
        }
        Ok(Response::new(ReleaseBlobResponse {}))
    }
}

/// The next chunk's part; a chunk without one is invalid.
async fn next_part(
    chunks: &mut Streaming<PutBlobRequest>,
) -> Result<Option<blob_chunk::Part>, Status> {
    match chunks.message().await? {
        None => Ok(None),
        Some(PutBlobRequest {
            chunk: Some(BlobChunk { part: Some(part) }),
        }) => Ok(Some(part)),
        Some(_) => Err(Status::invalid_argument("an empty PutBlob chunk")),
    }
}

fn response(part: blob_chunk::Part) -> GetBlobResponse {
    GetBlobResponse {
        chunk: Some(BlobChunk { part: Some(part) }),
    }
}

/// A blob handle from the core: this SDK's handles are SHA-256 ids.
pub(crate) fn parse_blob_id(id: &str) -> Result<MediaId, Status> {
    MediaId::new(id).map_err(|_| Status::invalid_argument(format!("{id:?} is not a blob handle")))
}

pub(crate) fn internal(error: impl std::fmt::Display) -> Status {
    Status::internal(error.to_string())
}

#[cfg(test)]
mod tests {
    use giles_proto::plugin::v1::plugin_service_client::PluginServiceClient;
    use giles_proto::plugin::v1::plugin_service_server::PluginServiceServer;
    use giles_proto::plugin::v1::{PluginKind, SandboxGrants};
    use tonic::Code;
    use tonic::transport::Channel;

    use super::*;
    use crate::dirs::SdkDirs;
    use crate::test_support::serve;

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    struct FakeKind;

    #[async_trait]
    impl Kind for FakeKind {
        fn describe(&self) -> DescribeResponse {
            DescribeResponse {
                kind: PluginKind::Channel.into(),
                name: "fake".into(),
                version: "1.2.3".into(),
                requested_grants: Some(SandboxGrants::default()),
                capabilities: None,
            }
        }

        async fn configure(
            &self,
            instance: String,
            config: serde_json::Value,
        ) -> Result<String, Status> {
            match config["namespace"].as_str() {
                Some(ns) => Ok(format!("{ns}:{instance}")),
                None => Err(Status::invalid_argument("namespace: missing")),
            }
        }
    }

    struct Fixture {
        _data: tempfile::TempDir,
        blobs: Blobs,
        shutdown: CancellationToken,
        client: PluginServiceClient<Channel>,
    }

    async fn fixture() -> Fixture {
        let data = tempfile::tempdir().unwrap();
        let blobs = Blobs::new(Arc::new(SdkDirs::open(data.path()).unwrap()));
        let shutdown = CancellationToken::new();
        let base = Base::new(Arc::new(FakeKind), blobs.clone(), shutdown.clone());
        let channel = serve(tonic::transport::Server::builder().add_service(
            PluginServiceServer::new(base).max_decoding_message_size(giles_proto::MAX_MESSAGE_SIZE),
        ))
        .await;
        Fixture {
            _data: data,
            blobs,
            shutdown,
            client: PluginServiceClient::new(channel),
        }
    }

    fn header(id: &str, size: u64) -> PutBlobRequest {
        PutBlobRequest {
            chunk: Some(BlobChunk {
                part: Some(blob_chunk::Part::Header(BlobRef {
                    id: id.into(),
                    media_type: "text/plain".into(),
                    size,
                    name: Some("hello.txt".into()),
                })),
            }),
        }
    }

    fn data(bytes: &[u8]) -> PutBlobRequest {
        PutBlobRequest {
            chunk: Some(BlobChunk {
                part: Some(blob_chunk::Part::Data(bytes.to_vec())),
            }),
        }
    }

    async fn put(
        client: &mut PluginServiceClient<Channel>,
        chunks: Vec<PutBlobRequest>,
    ) -> Result<BlobRef, Status> {
        let response = client.put_blob(stream::iter(chunks)).await?;
        Ok(response.into_inner().blob.unwrap())
    }

    async fn get(
        client: &mut PluginServiceClient<Channel>,
        id: &str,
    ) -> Result<Vec<blob_chunk::Part>, Status> {
        let mut stream = client
            .get_blob(GetBlobRequest { id: id.into() })
            .await?
            .into_inner();
        let mut parts = Vec::new();
        while let Some(response) = stream.message().await? {
            parts.push(response.chunk.unwrap().part.unwrap());
        }
        Ok(parts)
    }

    #[tokio::test]
    async fn describe_and_configure_delegate_to_the_kind() {
        let mut f = fixture().await;
        let described = f
            .client
            .describe(DescribeRequest::default())
            .await
            .unwrap()
            .into_inner();
        assert_eq!(described.name, "fake");
        assert_eq!(described.kind(), PluginKind::Channel);

        let configured = f
            .client
            .configure(ConfigureRequest {
                instance: "home".into(),
                config_json: r#"{"namespace": "fake"}"#.into(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(configured.identity_namespace, "fake:home");

        for (config_json, message) in [
            ("{}", "namespace: missing"),
            ("{", "config_json is not JSON"),
        ] {
            let status = f
                .client
                .configure(ConfigureRequest {
                    instance: "home".into(),
                    config_json: config_json.into(),
                })
                .await
                .unwrap_err();
            assert_eq!(status.code(), Code::InvalidArgument);
            assert!(status.message().contains(message), "{status:?}");
        }
    }

    #[tokio::test]
    async fn shutdown_fires_the_token() {
        let mut f = fixture().await;
        assert!(!f.shutdown.is_cancelled());
        f.client.shutdown(ShutdownRequest {}).await.unwrap();
        assert!(f.shutdown.is_cancelled());
    }

    #[tokio::test]
    async fn put_blob_stores_an_outbound_blob_under_its_sha256() {
        let mut f = fixture().await;
        let blob = put(
            &mut f.client,
            vec![header(HELLO_SHA256, 5), data(b"hel"), data(b"lo")],
        )
        .await
        .unwrap();
        assert_eq!(
            blob,
            BlobRef {
                id: HELLO_SHA256.into(),
                media_type: "text/plain".into(),
                size: 5,
                name: Some("hello.txt".into()),
            }
        );
        let id = MediaId::new(HELLO_SHA256).unwrap();
        assert!(f.blobs.exists(&id).await.unwrap());
        // Not offered back through GetBlob: that serves the plugin's own blobs.
        assert_eq!(
            get(&mut f.client, HELLO_SHA256).await.unwrap_err().code(),
            Code::NotFound
        );

        // The id may also be left for the plugin to compute.
        let empty = put(&mut f.client, vec![header("", 0)]).await.unwrap();
        assert_eq!(
            empty.id,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[tokio::test]
    async fn put_blob_refuses_inconsistent_transfers() {
        let mut f = fixture().await;
        let big = vec![0; MAX_BLOB_CHUNK_SIZE + 1];
        let size = u64::try_from(big.len()).unwrap();
        for (chunks, message) in [
            (vec![], "the first PutBlob chunk must be the header"),
            (
                vec![data(b"hello")],
                "the first PutBlob chunk must be the header",
            ),
            (
                vec![header("", 5), header("", 5)],
                "a second PutBlob header",
            ),
            (vec![header("", 4), data(b"hello")], "more PutBlob data"),
            (
                vec![header("", 6), data(b"hello")],
                "ended after 5 of the 6 bytes",
            ),
            (vec![header("", size), data(&big)], "the limit is 1048576"),
            (
                vec![PutBlobRequest { chunk: None }],
                "an empty PutBlob chunk",
            ),
            (
                vec![header(&"0".repeat(64), 5), data(b"hello")],
                "not the header's id",
            ),
        ] {
            let status = put(&mut f.client, chunks).await.unwrap_err();
            assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
            assert!(status.message().contains(message), "{status:?}");
        }
        let left = std::fs::read_dir(f._data.path().join("giles-sdk/outbound"))
            .unwrap()
            .count();
        assert_eq!(left, 0, "refused transfers leave nothing behind");
    }

    #[tokio::test]
    async fn get_blob_streams_the_header_then_one_mib_chunks() {
        let mut f = fixture().await;
        let bytes: Vec<u8> = (0..=255)
            .cycle()
            .take(MAX_BLOB_CHUNK_SIZE * 2 + 3)
            .collect();
        let media = f
            .blobs
            .put(&bytes, "application/octet-stream", None)
            .await
            .unwrap();
        let parts = get(&mut f.client, media.id.as_str()).await.unwrap();
        let [blob_chunk::Part::Header(header), rest @ ..] = &parts[..] else {
            panic!("no header first: {parts:?}");
        };
        assert_eq!(header.id, media.id.as_str());
        assert_eq!(header.size, media.size);
        assert_eq!(header.media_type, "application/octet-stream");
        let sizes: Vec<usize> = rest
            .iter()
            .map(|part| match part {
                blob_chunk::Part::Data(data) => data.len(),
                blob_chunk::Part::Header(_) => panic!("a second header"),
            })
            .collect();
        assert_eq!(sizes, [MAX_BLOB_CHUNK_SIZE, MAX_BLOB_CHUNK_SIZE, 3]);
        let joined: Vec<u8> = rest
            .iter()
            .flat_map(|part| match part {
                blob_chunk::Part::Data(data) => data.clone(),
                blob_chunk::Part::Header(_) => unreachable!(),
            })
            .collect();
        assert_eq!(joined, bytes);
    }

    #[tokio::test]
    async fn release_blob_frees_blobs_and_checks_ids() {
        let mut f = fixture().await;
        let media = f.blobs.put("hello", "text/plain", None).await.unwrap();
        assert_eq!(get(&mut f.client, HELLO_SHA256).await.unwrap().len(), 2);

        let status = f
            .client
            .release_blob(ReleaseBlobRequest {
                ids: vec![HELLO_SHA256.into(), "not-a-handle".into()],
            })
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(f.blobs.exists(&media.id).await.unwrap(), "nothing released");

        for _ in 0..2 {
            f.client
                .release_blob(ReleaseBlobRequest {
                    ids: vec![HELLO_SHA256.into()],
                })
                .await
                .unwrap();
        }
        assert_eq!(
            get(&mut f.client, HELLO_SHA256).await.unwrap_err().code(),
            Code::NotFound
        );
        assert_eq!(
            get(&mut f.client, "nope").await.unwrap_err().code(),
            Code::InvalidArgument
        );
    }
}
