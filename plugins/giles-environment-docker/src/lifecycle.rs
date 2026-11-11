//! Creating, starting, stopping, listing, and deleting workspaces.

use std::collections::HashMap;

use bollard::auth::DockerCredentials;
use bollard::body_full;
use bollard::models::{
    ContainerCreateBody, ContainerSummary, ContainerSummaryStateEnum, HostConfig, ImageInspect,
    Volume, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
    ListVolumesOptionsBuilder, RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder,
    RemoveVolumeOptionsBuilder, StopContainerOptionsBuilder, UploadToContainerOptionsBuilder,
};
use futures::StreamExt;
use jiff::Timestamp;
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::exec::Plain;
use crate::helper::{self, HELPER_PATH};
use crate::image::Reference;
use crate::types::{StartOutcome, Workspace, WorkspaceList, WorkspaceState};
use crate::{DockerEnvironment, WORKSPACE_DIR, check_workspace_id, label_map};

const LABEL_INSTANCE: &str = "giles.instance";
const LABEL_WORKSPACE: &str = "giles.workspace";
/// The image as the model asked for it, normalized (`docker.io/library/ubuntu:24.04`).
const LABEL_IMAGE: &str = "giles.image";
/// What the container is created from: the image's repository digest
/// (`docker.io/library/ubuntu@sha256:...`), or its ID when it has none (a local build).
const LABEL_DIGEST: &str = "giles.image-digest";
/// The image's ID (`sha256:...`) at creation, for removal.
const LABEL_IMAGE_ID: &str = "giles.image-id";
/// `true` when this plugin pulled the image.
const LABEL_PULLED: &str = "giles.pulled";
/// When the workspace was created, RFC 3339.
const LABEL_CREATED: &str = "giles.created";

/// How long a container gets to stop on SIGTERM before it is killed.
const STOP_GRACE_SECS: i32 = 5;

/// The options a workspace is created with.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateOptions {
    image: String,
}

/// An image a container can be created from.
struct ResolvedImage {
    /// The reference the model asked for, as recorded.
    requested: String,
    /// What the workspace is pinned to.
    digest: String,
    /// The local image ID.
    id: String,
    /// Whether this call pulled it.
    pulled: bool,
}

impl DockerEnvironment {
    /// Creates a workspace under `id`: pulls the image if the engine doesn't have it, then creates
    /// the volume and the container, copies the helper in, and starts it.
    ///
    /// `options` is `{"image": "..."}`. An image outside the allowlist, any other option, and an id
    /// that is already in use are [`Error::InvalidRequest`].
    pub async fn create_workspace(
        &self,
        id: &str,
        options: serde_json::Value,
    ) -> Result<Workspace> {
        check_workspace_id(id)?;
        let options: CreateOptions = serde_json::from_value(options).map_err(|e| {
            Error::invalid(format!(
                "invalid options: {e}. Expected {{\"image\": \"...\"}}"
            ))
        })?;
        let reference =
            Reference::parse(&options.image).map_err(|e| Error::invalid(e.to_string()))?;
        let allowlist = self.settings().allowlist();
        if !allowlist.allows(&reference) {
            return Err(Error::invalid(format!(
                "the image {} isn't allowed. Allowed images: {}",
                options.image,
                allowlist.describe()
            )));
        }

        let _guard = self.lock(id).await;
        if self.find_volume(id).await?.is_some() {
            return Err(Error::invalid(format!("the workspace {id} already exists")));
        }

        let image = self.resolve_image(&reference).await?;
        let created_at = Timestamp::now();
        let name = self.resource_name(id);
        let labels = label_map(&[
            (LABEL_INSTANCE, self.instance()),
            (LABEL_WORKSPACE, id),
            (LABEL_IMAGE, &image.requested),
            (LABEL_DIGEST, &image.digest),
            (LABEL_IMAGE_ID, &image.id),
            (LABEL_PULLED, if image.pulled { "true" } else { "false" }),
            (LABEL_CREATED, &created_at.to_string()),
        ]);
        let volume = self
            .docker()
            .create_volume(VolumeCreateRequest {
                name: Some(name.clone()),
                labels: Some(labels.clone()),
                ..Default::default()
            })
            .await
            .map_err(|e| Error::docker("creating the workspace volume", &e))?;

        if let Err(error) = self.create_container(id, &name, &labels, &image.id).await {
            // Don't leave a half-made workspace behind.
            self.remove_container_named(&name).await;
            if let Err(cleanup) = self.remove_volume_named(&name).await {
                warn!(workspace = id, error = %cleanup, "couldn't remove the volume of a failed create");
            }
            // An image pulled for this attempt has no workspace now.
            self.remove_unused_image(&labels).await;
            return Err(error);
        }
        info!(workspace = id, image = %image.digest, "created workspace");
        let container = self.find_container(id).await?;
        Ok(self.describe(&volume, container.as_ref(), None))
    }

    /// Deletes a workspace's container and volume, then the image if this plugin pulled it and no
    /// other workspace of this instance uses it. Succeeds if there is no such workspace.
    pub async fn delete_workspace(&self, id: &str) -> Result<()> {
        check_workspace_id(id)?;
        let _guard = self.lock(id).await;
        let volume = self.find_volume(id).await?;
        let name = self.resource_name(id);
        // The container goes first: a volume in use can't be removed.
        if let Some(container) = self.find_container(id).await? {
            let container_id = container.id.clone().unwrap_or_else(|| name.clone());
            self.docker()
                .remove_container(
                    &container_id,
                    Some(
                        RemoveContainerOptionsBuilder::default()
                            .force(true)
                            .v(true)
                            .build(),
                    ),
                )
                .await
                .or_else(ignore_missing)
                .map_err(|e| Error::docker("removing the workspace container", &e))?;
        }
        if let Some(volume) = &volume {
            self.docker()
                .remove_volume(
                    &volume.name,
                    Some(RemoveVolumeOptionsBuilder::default().force(true).build()),
                )
                .await
                .or_else(ignore_missing)
                .map_err(|e| Error::docker("removing the workspace volume", &e))?;
        }
        self.forget_lock(id);
        if let Some(volume) = volume {
            self.remove_unused_image(&volume.labels).await;
        }
        Ok(())
    }

    /// Makes a workspace ready for commands: starts its container if it is stopped, and recreates
    /// the container from the recorded image if it is gone but the volume remains.
    pub async fn start_workspace(&self, id: &str) -> Result<StartOutcome> {
        check_workspace_id(id)?;
        // The common case, a running container, takes no lock.
        if let Some(container) = self.find_container(id).await?
            && container.state == Some(ContainerSummaryStateEnum::RUNNING)
        {
            return Ok(StartOutcome::Running);
        }
        let _guard = self.lock(id).await;
        let volume = self
            .find_volume(id)
            .await?
            .ok_or_else(|| Error::not_found(format!("there is no workspace {id}")))?;
        match self.find_container(id).await? {
            Some(container) if container.state == Some(ContainerSummaryStateEnum::RUNNING) => {
                Ok(StartOutcome::Running)
            }
            Some(container) => {
                let container_id = container
                    .id
                    .clone()
                    .unwrap_or_else(|| self.resource_name(id));
                if container.state == Some(ContainerSummaryStateEnum::PAUSED) {
                    self.docker()
                        .unpause_container(&container_id)
                        .await
                        .map_err(|e| Error::docker("resuming the workspace container", &e))?;
                } else {
                    self.docker()
                        .start_container(&container_id, None)
                        .await
                        .map_err(|e| Error::docker("starting the workspace container", &e))?;
                }
                Ok(StartOutcome::Started {
                    detail: "The workspace was stopped and has started again. Processes that were \
                             running in it have ended; its files and installed packages are intact."
                        .to_owned(),
                })
            }
            None => {
                let labels = volume.labels.clone();
                let digest = labels.get(LABEL_DIGEST).filter(|d| !d.is_empty()).ok_or_else(|| {
                    Error::other(format!(
                        "the workspace {id} has no recorded image, so its container can't be recreated"
                    ))
                })?;
                let image_id = self
                    .ensure_pinned_image(digest, labels.get(LABEL_IMAGE_ID))
                    .await?;
                let name = self.resource_name(id);
                self.create_container(id, &name, &volume.labels, &image_id)
                    .await?;
                info!(workspace = id, image = %digest, "recreated workspace container");
                Ok(StartOutcome::Recreated {
                    detail: format!(
                        "The workspace's container was missing and has been recreated from the same \
                         image ({digest}). Files under {WORKSPACE_DIR} are intact; anything \
                         installed or changed outside {WORKSPACE_DIR}, such as packages installed \
                         with apt, is gone."
                    ),
                })
            }
        }
    }

    /// Stops a workspace's container, keeping its volume and the container's own files. Succeeds
    /// if it is already stopped.
    pub async fn stop_workspace(&self, id: &str) -> Result<()> {
        check_workspace_id(id)?;
        let _guard = self.lock(id).await;
        if self.find_volume(id).await?.is_none() {
            return Err(Error::not_found(format!("there is no workspace {id}")));
        }
        let Some(container) = self.find_container(id).await? else {
            return Ok(());
        };
        if container.state != Some(ContainerSummaryStateEnum::RUNNING)
            && container.state != Some(ContainerSummaryStateEnum::PAUSED)
        {
            return Ok(());
        }
        let container_id = container
            .id
            .clone()
            .unwrap_or_else(|| self.resource_name(id));
        self.docker()
            .stop_container(
                &container_id,
                Some(
                    StopContainerOptionsBuilder::default()
                        .t(STOP_GRACE_SECS)
                        .build(),
                ),
            )
            .await
            .map_err(|e| Error::docker("stopping the workspace container", &e))
    }

    /// Every workspace of this instance, with the free space where the engine keeps volumes when
    /// that can be measured (it needs a running workspace to measure from).
    pub async fn list_workspaces(&self) -> Result<WorkspaceList> {
        let volumes = self.instance_volumes().await?;
        let containers = self.instance_containers().await?;
        let usage = self.volume_usage().await;
        let mut free_bytes = None;
        let mut workspaces = Vec::with_capacity(volumes.len());
        for volume in &volumes {
            let Some(id) = volume.labels.get(LABEL_WORKSPACE) else {
                continue;
            };
            let container = containers
                .iter()
                .find(|c| c.labels.as_ref().and_then(|l| l.get(LABEL_WORKSPACE)) == Some(id));
            if free_bytes.is_none()
                && let Some(c) =
                    container.filter(|c| c.state == Some(ContainerSummaryStateEnum::RUNNING))
            {
                free_bytes = self.free_bytes(c).await;
            }
            let used = usage.get(&volume.name).copied();
            workspaces.push(self.describe(volume, container, used));
        }
        workspaces.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(WorkspaceList {
            workspaces,
            free_bytes,
        })
    }

    // --- images ---

    /// Finds the image locally or pulls it, and pins it.
    async fn resolve_image(&self, reference: &Reference) -> Result<ResolvedImage> {
        let name = reference.to_string();
        let (inspect, pulled) = match self.docker().inspect_image(&name).await {
            Ok(inspect) => (inspect, false),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                self.pull(reference).await?;
                let inspect = self.docker().inspect_image(&name).await.map_err(|e| {
                    Error::docker(&format!("inspecting the pulled image {name}"), &e)
                })?;
                (inspect, true)
            }
            Err(e) => return Err(Error::docker(&format!("looking for the image {name}"), &e)),
        };
        let id = inspect.id.clone().ok_or_else(|| {
            Error::other(format!("the engine returned no ID for the image {name}"))
        })?;
        check_linux(&inspect, &name)?;
        let digest = pinned_name(reference, &inspect).unwrap_or_else(|| id.clone());
        // An image this plugin pulled for another workspace stays ours to remove.
        let pulled = pulled || self.pulled_by_us(&id).await;
        Ok(ResolvedImage {
            requested: name,
            digest,
            id,
            pulled,
        })
    }

    /// Whether a workspace of this instance records that it pulled `image_id`.
    async fn pulled_by_us(&self, image_id: &str) -> bool {
        self.instance_volumes()
            .await
            .unwrap_or_default()
            .iter()
            .any(|v| {
                v.labels.get(LABEL_IMAGE_ID).map(String::as_str) == Some(image_id)
                    && v.labels.get(LABEL_PULLED).map(String::as_str) == Some("true")
            })
    }

    /// The local ID of a recorded image, pulling it by digest if the engine no longer has it.
    async fn ensure_pinned_image(
        &self,
        digest: &str,
        recorded_id: Option<&String>,
    ) -> Result<String> {
        match self.docker().inspect_image(digest).await {
            Ok(inspect) => {
                return inspect.id.ok_or_else(|| {
                    Error::other(format!("the engine returned no ID for {digest}"))
                });
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {}
            Err(e) => {
                return Err(Error::docker(
                    &format!("looking for the image {digest}"),
                    &e,
                ));
            }
        }
        // A repository digest can be pulled again; a bare image ID (a local build) can't.
        let reference = if digest.contains('@') {
            Reference::parse(digest).ok()
        } else {
            None
        };
        let Some(reference) = reference else {
            return Err(Error::other(format!(
                "the image {digest} this workspace was created from is gone from the engine and \
                 can't be pulled again{}",
                recorded_id.map_or(String::new(), |id| format!(" (image ID {id})"))
            )));
        };
        self.pull(&reference).await?;
        self.docker()
            .inspect_image(digest)
            .await
            .map_err(|e| Error::docker(&format!("inspecting the pulled image {digest}"), &e))?
            .id
            .ok_or_else(|| Error::other(format!("the engine returned no ID for {digest}")))
    }

    async fn pull(&self, reference: &Reference) -> Result<()> {
        let name = reference.name();
        let (from, tag) = match (&reference.digest, &reference.tag) {
            (Some(digest), _) => (format!("{name}@{digest}"), None),
            (None, tag) => (name, tag.clone()),
        };
        let mut options = CreateImageOptionsBuilder::default().from_image(&from);
        if let Some(tag) = &tag {
            options = options.tag(tag);
        }
        let credentials = self
            .settings()
            .registries
            .get(&reference.registry)
            .map(|login| DockerCredentials {
                username: Some(login.username.clone()),
                password: Some(login.password.clone()),
                serveraddress: Some(reference.registry.clone()),
                ..Default::default()
            });
        info!(image = %reference, "pulling image");
        let mut stream = self
            .docker()
            .create_image(Some(options.build()), None, credentials);
        while let Some(item) = stream.next().await {
            let item = item.map_err(|e| Error::docker(&format!("pulling {reference}"), &e))?;
            if let Some(detail) = item.error_detail {
                return Err(Error::other(format!(
                    "pulling {reference} failed: {}",
                    detail
                        .message
                        .unwrap_or_else(|| "the registry refused it".to_owned())
                )));
            }
        }
        Ok(())
    }

    /// Removes the image a deleted workspace used if this plugin pulled it and no workspace of
    /// this instance still records it. The engine refuses to remove an image that a container uses
    /// or that has more than one name, which is the conservative answer: it stays.
    async fn remove_unused_image(&self, labels: &HashMap<String, String>) {
        if labels.get(LABEL_PULLED).map(String::as_str) != Some("true") {
            return;
        }
        let Some(image_id) = labels.get(LABEL_IMAGE_ID).filter(|id| !id.is_empty()) else {
            return;
        };
        let still_used = match self.instance_volumes().await {
            Ok(volumes) => volumes
                .iter()
                .any(|v| v.labels.get(LABEL_IMAGE_ID) == Some(image_id)),
            Err(_) => true,
        };
        if still_used {
            return;
        }
        match self
            .docker()
            .remove_image(
                image_id,
                Some(RemoveImageOptionsBuilder::default().build()),
                None,
            )
            .await
        {
            Ok(_) => info!(image = %image_id, "removed unused image"),
            Err(e) => debug!(image = %image_id, error = %e, "kept the image"),
        }
    }

    // --- containers ---

    /// Creates the container, copies the helper in, starts it, and prepares `$HOME`.
    async fn create_container(
        &self,
        id: &str,
        name: &str,
        labels: &HashMap<String, String>,
        image_id: &str,
    ) -> Result<()> {
        let settings = self.settings();
        let mut env = vec![format!("HOME={WORKSPACE_DIR}/.home")];
        env.extend(settings.env.iter().cloned());
        let body = ContainerCreateBody {
            image: Some(image_id.to_owned()),
            // Idle on any image: the helper is copied in before the container starts.
            entrypoint: Some(vec![HELPER_PATH.to_owned(), "idle".to_owned()]),
            cmd: Some(Vec::new()),
            env: Some(env),
            working_dir: Some(WORKSPACE_DIR.to_owned()),
            // Root inside the container (a rootless engine maps it to an unprivileged user), so the
            // model can install packages and always owns the volume, whatever the image's USER.
            user: Some("0".to_owned()),
            labels: Some(labels.clone()),
            host_config: Some(HostConfig {
                init: Some(true),
                binds: Some(vec![format!("{name}:{WORKSPACE_DIR}")]),
                memory: settings.memory_bytes,
                nano_cpus: settings.nano_cpus,
                pids_limit: settings.pids_limit,
                network_mode: Some(settings.network.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let created = self
            .docker()
            .create_container(
                Some(CreateContainerOptionsBuilder::default().name(name).build()),
                body,
            )
            .await
            .map_err(|e| Error::docker("creating the workspace container", &e))?;
        let archive = helper::archive(self.helper_bytes())?;
        self.docker()
            .upload_to_container(
                &created.id,
                Some(UploadToContainerOptionsBuilder::default().path("/").build()),
                body_full(archive),
            )
            .await
            .map_err(|e| Error::docker("copying the helper into the workspace container", &e))?;
        self.docker()
            .start_container(&created.id, None)
            .await
            .map_err(|e| Error::docker("starting the workspace container", &e))?;
        self.prepare_home(&created.id, id).await
    }

    /// Makes `$HOME`. This also finds out early that the image has no `/bin/sh`.
    async fn prepare_home(&self, container: &str, id: &str) -> Result<()> {
        let result = self
            .exec_plain(
                container,
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("mkdir -p {WORKSPACE_DIR}/.home"),
                ],
            )
            .await;
        match result {
            Ok(Plain { code: 0, .. }) => Ok(()),
            Ok(Plain {
                code: 126 | 127, ..
            })
            | Err(Error::NotFound(_)) => Err(Error::invalid(
                "the image has no /bin/sh, which workspaces need",
            )),
            Ok(Plain { code, stderr, .. }) => Err(Error::other(format!(
                "preparing workspace {id}: mkdir exited {code}: {}",
                String::from_utf8_lossy(&stderr).trim()
            ))),
            Err(Error::Other(message)) if mentions_missing_file(&message) => Err(Error::invalid(
                "the image has no /bin/sh, which workspaces need",
            )),
            Err(e) => Err(e),
        }
    }

    /// Removes a container by name, ignoring a missing one and logging other failures.
    async fn remove_container_named(&self, name: &str) {
        let options = RemoveContainerOptionsBuilder::default()
            .force(true)
            .v(true)
            .build();
        if let Err(e) = self.docker().remove_container(name, Some(options)).await
            && !is_missing(&e)
        {
            warn!(container = name, error = %e, "couldn't remove a container");
        }
    }

    async fn remove_volume_named(&self, name: &str) -> Result<()> {
        self.docker()
            .remove_volume(
                name,
                Some(RemoveVolumeOptionsBuilder::default().force(true).build()),
            )
            .await
            .or_else(ignore_missing)
            .map_err(|e| Error::docker("removing the workspace volume", &e))
    }

    // --- lookup ---

    /// The engine-wide name of a workspace's volume and container.
    fn resource_name(&self, id: &str) -> String {
        let instance: String = self
            .instance()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        // Ids have no ".", so the last "." always separates the id: two instances can't produce
        // the same name.
        format!("giles-{instance}.{id}")
    }

    fn label_filter(&self, id: Option<&str>) -> HashMap<String, Vec<String>> {
        let mut labels = vec![format!("{LABEL_INSTANCE}={}", self.instance())];
        if let Some(id) = id {
            labels.push(format!("{LABEL_WORKSPACE}={id}"));
        }
        HashMap::from([("label".to_owned(), labels)])
    }

    fn owns(&self, labels: Option<&HashMap<String, String>>, id: Option<&str>) -> bool {
        labels.is_some_and(|l| {
            l.get(LABEL_INSTANCE).map(String::as_str) == Some(self.instance())
                && id.is_none_or(|id| l.get(LABEL_WORKSPACE).map(String::as_str) == Some(id))
        })
    }

    async fn instance_volumes(&self) -> Result<Vec<Volume>> {
        let response = self
            .docker()
            .list_volumes(Some(
                ListVolumesOptionsBuilder::default()
                    .filters(&self.label_filter(None))
                    .build(),
            ))
            .await
            .map_err(|e| Error::docker("listing workspace volumes", &e))?;
        Ok(response
            .volumes
            .unwrap_or_default()
            .into_iter()
            .filter(|v| self.owns(Some(&v.labels), None))
            .collect())
    }

    async fn instance_containers(&self) -> Result<Vec<ContainerSummary>> {
        let containers = self
            .docker()
            .list_containers(Some(
                ListContainersOptionsBuilder::default()
                    .all(true)
                    .filters(&self.label_filter(None))
                    .build(),
            ))
            .await
            .map_err(|e| Error::docker("listing workspace containers", &e))?;
        Ok(containers
            .into_iter()
            .filter(|c| self.owns(c.labels.as_ref(), None))
            .collect())
    }

    pub(crate) async fn find_volume(&self, id: &str) -> Result<Option<Volume>> {
        let response = self
            .docker()
            .list_volumes(Some(
                ListVolumesOptionsBuilder::default()
                    .filters(&self.label_filter(Some(id)))
                    .build(),
            ))
            .await
            .map_err(|e| Error::docker("looking for the workspace volume", &e))?;
        Ok(response
            .volumes
            .unwrap_or_default()
            .into_iter()
            .find(|v| self.owns(Some(&v.labels), Some(id))))
    }

    pub(crate) async fn find_container(&self, id: &str) -> Result<Option<ContainerSummary>> {
        let containers = self
            .docker()
            .list_containers(Some(
                ListContainersOptionsBuilder::default()
                    .all(true)
                    .filters(&self.label_filter(Some(id)))
                    .build(),
            ))
            .await
            .map_err(|e| Error::docker("looking for the workspace container", &e))?;
        Ok(containers
            .into_iter()
            .find(|c| self.owns(c.labels.as_ref(), Some(id))))
    }

    /// Sizes of volumes by name, from the engine's disk usage report. Empty when it can't say.
    async fn volume_usage(&self) -> HashMap<String, u64> {
        let Ok(usage) = self.docker().df(None).await else {
            return HashMap::new();
        };
        usage
            .volume_usage
            .and_then(|volumes| volumes.items)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| {
                let name = item.get("Name")?.as_str()?.to_owned();
                let size = item.get("UsageData")?.get("Size")?.as_i64()?;
                // The engine reports -1 for a size it hasn't measured.
                u64::try_from(size).ok().map(|size| (name, size))
            })
            .collect()
    }

    /// Free space where the container's volume is stored, measured from inside it.
    async fn free_bytes(&self, container: &ContainerSummary) -> Option<u64> {
        let id = container.id.as_deref()?;
        let out = self
            .exec_plain(
                id,
                vec![HELPER_PATH.into(), "df".into(), WORKSPACE_DIR.into()],
            )
            .await
            .ok()?;
        if out.code != 0 {
            return None;
        }
        let line: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        line.get("free_bytes")?.as_u64()
    }

    fn describe(
        &self,
        volume: &Volume,
        container: Option<&ContainerSummary>,
        disk: Option<u64>,
    ) -> Workspace {
        let label = |key: &str| volume.labels.get(key).map_or("", String::as_str);
        let created_at = label(LABEL_CREATED)
            .parse::<Timestamp>()
            .ok()
            .or_else(|| {
                volume
                    .created_at
                    .as_ref()
                    .and_then(|d| d.parse::<Timestamp>().ok())
            })
            .unwrap_or(Timestamp::UNIX_EPOCH);
        let state = match container.and_then(|c| c.state) {
            Some(ContainerSummaryStateEnum::RUNNING) => WorkspaceState::Running,
            _ => WorkspaceState::Stopped,
        };
        let digest = label(LABEL_DIGEST);
        let description = if digest.is_empty() || digest == label(LABEL_IMAGE) {
            format!("Container from the image {}", label(LABEL_IMAGE))
        } else {
            format!("Container from the image {} ({digest})", label(LABEL_IMAGE))
        };
        Workspace {
            id: label(LABEL_WORKSPACE).to_owned(),
            description,
            working_directory: WORKSPACE_DIR.to_owned(),
            persistence: format!(
                "Files under {WORKSPACE_DIR} (including your home directory, {WORKSPACE_DIR}/.home) \
                 persist until the workspace is deleted. Packages and other changes outside \
                 {WORKSPACE_DIR} survive a stop but are lost if the container has to be recreated."
            ),
            created_at,
            state,
            disk_used_bytes: disk,
        }
    }
}

fn is_missing(error: &bollard::errors::Error) -> bool {
    matches!(
        error,
        bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

fn ignore_missing(
    error: bollard::errors::Error,
) -> std::result::Result<(), bollard::errors::Error> {
    if is_missing(&error) {
        Ok(())
    } else {
        Err(error)
    }
}

fn check_linux(inspect: &ImageInspect, name: &str) -> Result<()> {
    match inspect.os.as_deref() {
        Some("linux") | None => Ok(()),
        Some(os) => Err(Error::invalid(format!(
            "the image {name} is for {os}, not linux"
        ))),
    }
}

/// The repository digest to pin: the one under the requested repository if the engine lists it.
fn pinned_name(reference: &Reference, inspect: &ImageInspect) -> Option<String> {
    if let Some(digest) = &reference.digest {
        return Some(format!("{}@{digest}", reference.name()));
    }
    let digests = inspect.repo_digests.as_deref()?;
    let wanted = reference.name();
    // Entries look like `docker.io/library/ubuntu@sha256:...` (Podman) or `ubuntu@sha256:...`
    // (Docker, which leaves Docker Hub's defaults off).
    let normalize = |entry: &str| -> Option<String> {
        let (name, digest) = entry.split_once('@')?;
        let parsed = Reference::parse(name).ok()?;
        Some(format!("{}@{digest}", parsed.name()))
    };
    digests
        .iter()
        .filter_map(|d| normalize(d))
        .find(|d| d.split_once('@').is_some_and(|(name, _)| name == wanted))
}

/// Whether an engine error says the executable of a new exec doesn't exist. Docker and Podman
/// word it differently ("no such file or directory", "executable file ... not found").
fn mentions_missing_file(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("no such file") || message.contains("not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect(digests: &[&str]) -> ImageInspect {
        ImageInspect {
            repo_digests: Some(digests.iter().map(|d| (*d).to_owned()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn the_pinned_name_is_the_digest_under_the_requested_repository() {
        let reference = Reference::parse("ubuntu:24.04").unwrap();
        let hex = "a".repeat(64);
        // Docker lists Hub images without the registry and `library/` defaults.
        assert_eq!(
            pinned_name(&reference, &inspect(&[&format!("ubuntu@sha256:{hex}")])).unwrap(),
            format!("docker.io/library/ubuntu@sha256:{hex}")
        );
        // Podman lists them in full.
        assert_eq!(
            pinned_name(
                &reference,
                &inspect(&[&format!("docker.io/library/ubuntu@sha256:{hex}")])
            )
            .unwrap(),
            format!("docker.io/library/ubuntu@sha256:{hex}")
        );
        // A digest under another repository (the same image tagged elsewhere) isn't ours.
        assert_eq!(
            pinned_name(
                &reference,
                &inspect(&[&format!("ghcr.io/x/ubuntu@sha256:{hex}")])
            ),
            None
        );
        assert_eq!(pinned_name(&reference, &inspect(&[])), None);
        assert_eq!(pinned_name(&reference, &ImageInspect::default()), None);
    }

    #[test]
    fn a_digest_reference_pins_itself() {
        let hex = "b".repeat(64);
        let reference = Reference::parse(&format!("python@sha256:{hex}")).unwrap();
        assert_eq!(
            pinned_name(&reference, &ImageInspect::default()).unwrap(),
            format!("docker.io/library/python@sha256:{hex}")
        );
    }

    #[test]
    fn both_engines_wordings_of_a_missing_executable_are_recognized() {
        // Docker.
        assert!(mentions_missing_file(
            "OCI runtime exec failed: exec failed: unable to start container process: exec: \"/bin/sh\": stat /bin/sh: no such file or directory"
        ));
        // Podman.
        assert!(mentions_missing_file(
            "crun: executable file `/bin/sh` not found in $PATH: No such file or directory"
        ));
        assert!(!mentions_missing_file("permission denied"));
    }
}
