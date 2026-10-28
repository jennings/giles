//! The toolset RPCs' side of the toolset manager: the [`ToolsetAdmin`] seam, the proto messages
//! built from a [`ToolsetView`], and the status code of each [`ToolsetManagerError`] (see
//! `Docs/technical/admin.md`).

use async_trait::async_trait;
use giles_plugin_sdk::convert::timestamp_to_proto;
use giles_proto::admin::v1 as pb;
use giles_types::Timestamp;
use tonic::Status;
use tracing::error;

use crate::config::ToolsetSpec;
use crate::toolsets::{Removed, ToolsetManager, ToolsetManagerError, ToolsetSource, ToolsetView};

/// What the toolset RPCs need of the toolset manager: [`ToolsetManager`] implements it, and every
/// method is the manager's own. A seam so that tests can plug in a fake. Without one the RPCs
/// answer `UNAVAILABLE`.
#[async_trait]
pub trait ToolsetAdmin: Send + Sync {
    /// [`ToolsetManager::list`].
    async fn list(&self) -> Result<Vec<ToolsetView>, ToolsetManagerError>;

    /// [`ToolsetManager::get`].
    async fn get(&self, name: &str) -> Result<ToolsetView, ToolsetManagerError>;

    /// [`ToolsetManager::add`].
    async fn add(&self, name: &str, spec: &ToolsetSpec)
    -> Result<ToolsetView, ToolsetManagerError>;

    /// [`ToolsetManager::update`].
    async fn update(
        &self,
        name: &str,
        spec: &ToolsetSpec,
        expected_updated_at: Option<Timestamp>,
    ) -> Result<ToolsetView, ToolsetManagerError>;

    /// [`ToolsetManager::remove`].
    async fn remove(&self, name: &str) -> Result<Removed, ToolsetManagerError>;
}

#[async_trait]
impl ToolsetAdmin for ToolsetManager {
    async fn list(&self) -> Result<Vec<ToolsetView>, ToolsetManagerError> {
        ToolsetManager::list(self).await
    }

    async fn get(&self, name: &str) -> Result<ToolsetView, ToolsetManagerError> {
        ToolsetManager::get(self, name).await
    }

    async fn add(
        &self,
        name: &str,
        spec: &ToolsetSpec,
    ) -> Result<ToolsetView, ToolsetManagerError> {
        ToolsetManager::add(self, name, spec).await
    }

    async fn update(
        &self,
        name: &str,
        spec: &ToolsetSpec,
        expected_updated_at: Option<Timestamp>,
    ) -> Result<ToolsetView, ToolsetManagerError> {
        ToolsetManager::update(self, name, spec, expected_updated_at).await
    }

    async fn remove(&self, name: &str) -> Result<Removed, ToolsetManagerError> {
        ToolsetManager::remove(self, name).await
    }
}

/// The caller's status for a failed toolset call. `name` is the toolset the call was about. Only a
/// database failure is the server's: it is logged with the RPC's name and the caller gets
/// `INTERNAL`.
pub(super) fn manager_status(rpc: &'static str, name: &str, err: ToolsetManagerError) -> Status {
    match err {
        ToolsetManagerError::Invalid { .. } => Status::invalid_argument(err.to_string()),
        ToolsetManagerError::Exists => {
            Status::already_exists(format!("there is already a toolset called `{name}`"))
        }
        ToolsetManagerError::NotFound => {
            Status::not_found(format!("there is no toolset called `{name}`"))
        }
        ToolsetManagerError::Stale => Status::failed_precondition(format!(
            "toolset `{name}` was changed since it was read: read it again and redo the change"
        )),
        ToolsetManagerError::Unsupported => Status::failed_precondition(format!(
            "toolset `{name}` is configured in giles.toml, so it can only be changed there"
        )),
        err => {
            error!(rpc, toolset = name, error = %err, "admin call failed");
            Status::internal(err.to_string())
        }
    }
}

/// `view` as the `Toolset` message: its spec as TOML text with the references as written.
pub(super) fn toolset_to_proto(view: ToolsetView) -> pb::Toolset {
    let mut failure = view.failure;
    let spec_toml = view.spec.and_then(|spec| match spec.to_toml() {
        Ok(text) => Some(text),
        Err(err) => {
            if failure.is_none() {
                failure = Some(format!("the stored spec can't be shown as TOML: {err}"));
            }
            None
        }
    });
    let (source, updated_at) = match view.source {
        ToolsetSource::Stored { updated_at, .. } => (
            pb::ToolsetSource::Stored,
            Some(timestamp_to_proto(updated_at)),
        ),
        ToolsetSource::File => (pb::ToolsetSource::GilesToml, None),
    };
    pb::Toolset {
        name: view.name,
        source: source.into(),
        spec_toml,
        updated_at,
        failure,
    }
}
