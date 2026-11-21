//! SSH keys users keep in Doozers (issue #136): generated or uploaded on the web, stored in the
//! secret store, and used by the core to sign for a user's own workspaces. The design is in
//! `Docs/technical/ssh.md`.
//!
//! [`SshKeys`] is the service. A key is a row of `ssh_keys` (its metadata: label, public key,
//! fingerprint, times) and a secret at `users/<user>/ssh-keys/<id>` holding the private key as
//! unencrypted OpenSSH PEM, which the secret store seals. Every call takes the user who acts. A
//! user lists, renames, and deletes only their own keys, and a key of someone else's is reported
//! as not found. An admin may also list every user's key metadata and delete any key
//! ([`SshKeys::all_keys`], [`SshKeys::delete_any`]). Nothing returns a private key: only the
//! signing path inside the core reads one ([`SshKeys::private_key`]), and only the acting user's.
//!
//! The host keys workspaces trust are [`KnownHosts`] (issue #138), asked through the same relay.
//!
//! The signing path is the SSH agent relay (issue #137). [`relay`] keeps one task per workspace on
//! an environment that can relay, which answers the agent requests of clients in the workspace
//! with an [`Agent`], and [`agent`] is the ssh-agent protocol.

mod agent;
mod formats;
mod keys;
pub mod known_hosts;
pub(crate) mod relay;

#[cfg(test)]
pub(crate) mod agent_tests;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;

use doozers_store::{NewSshKey, SshKeyId, SshKeyRecord, Store};
use doozers_types::{Timestamp, UserId};
use ssh_key::PrivateKey;
use ssh_key::private::KeypairData;
use tracing::warn;
use zeroize::Zeroizing;

use crate::config::Secret;
use crate::secrets::{SecretError, SecretPath, SecretProvider};
use crate::sessions::Clock;
use crate::users::UserList;

pub use agent::Agent;
pub use formats::{KeyFileError, MAX_UPLOAD_BYTES};
pub use keys::KeyType;
pub use known_hosts::{Caller, KnownHost, KnownHosts, KnownHostsError};

#[cfg(test)]
impl SshKeys {
    /// The service over `store` with a secret store of its own in `dir`, for tests of the code
    /// that holds one. It knows no users, so nobody is an admin.
    pub(crate) async fn for_tests(
        store: Store,
        dir: &std::path::Path,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let root = crate::paths::DataRoot::new(dir, std::path::Path::new("/tmp")).unwrap();
        let secrets = crate::secrets::LocalSecrets::open(
            store.clone(),
            &crate::secrets::KeyFile::new(&root, &root.secret_key_file()),
            clock.clone(),
        )
        .await
        .unwrap();
        Self::new(store, Arc::new(secrets), clock, UserList::default())
    }
}

/// The longest label, in characters.
pub const MAX_LABEL_CHARS: usize = 100;

/// The leaf of a user's secret space that holds their keys: `users/<user>/ssh-keys/<id>`.
const SSH_KEYS: &str = "ssh-keys";

/// A key's metadata, as the pages and the signing path see it. Never the private key.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshKey {
    /// The key's id. Never reused.
    pub id: SshKeyId,
    /// The user who owns the key.
    pub user: UserId,
    /// The user's name for the key.
    pub label: String,
    /// The SSH algorithm name of the public key: `ssh-ed25519`, `ssh-rsa`,
    /// `ecdsa-sha2-nistp256`, `ecdsa-sha2-nistp384`, or `ecdsa-sha2-nistp521`.
    pub algorithm: String,
    /// The type as the page shows it: `Ed25519`, `RSA 3072`, `ECDSA P-256`.
    pub type_label: String,
    /// The public key as `<algorithm> <base64>`.
    pub public_key: String,
    /// The `SHA256:...` fingerprint, as `ssh-keygen -l` prints it.
    pub fingerprint: String,
    /// When the key was added.
    pub created_at: Timestamp,
    /// When the key last signed, if it ever has.
    pub last_used_at: Option<Timestamp>,
}

impl SshKey {
    /// The public key in `authorized_keys` form, `<algorithm> <base64> <label>`: what to register
    /// at GitHub or in a server's `authorized_keys`.
    pub fn authorized_key(&self) -> String {
        format!("{} {}", self.public_key, self.label)
    }
}

impl From<SshKeyRecord> for SshKey {
    fn from(record: SshKeyRecord) -> Self {
        Self {
            type_label: keys::type_label(&record.public_key),
            id: record.id,
            user: record.user,
            label: record.label,
            algorithm: record.algorithm,
            public_key: record.public_key,
            fingerprint: record.fingerprint,
            created_at: record.created_at,
            last_used_at: record.last_used_at,
        }
    }
}

/// An SSH key call failed. No message quotes key material.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SshKeysError {
    /// There is no such key, or it isn't the acting user's. The two look the same on purpose.
    #[error("no such SSH key")]
    NotFound,
    /// The acting user may not do this: the admin calls are the admins'.
    #[error("only an admin can do that")]
    Forbidden,
    /// The label is empty, too long, or has a control character.
    #[error("{0}")]
    InvalidLabel(String),
    /// The upload isn't an acceptable key.
    #[error("{0}")]
    Upload(#[from] KeyFileError),
    /// The user already has this key.
    #[error("you already have this key, labelled \"{label}\"")]
    Duplicate {
        /// The label of the key they have.
        label: String,
    },
    /// The user id can't name a secret, so the user can't keep keys.
    #[error(
        "the user `{0}` can't keep SSH keys: a user id must be lower case letters, digits, `-`, \
         `_`, and `.` to name a secret"
    )]
    UserCantKeepKeys(String),
    /// Generating the key failed.
    #[error("couldn't generate the key: {0}")]
    Generate(String),
    /// The database failed.
    #[error(transparent)]
    Store(#[from] doozers_store::Error),
    /// The secret store failed, or holds a key that can't be read.
    #[error(transparent)]
    Secrets(#[from] SecretError),
}

/// The SSH keys service: metadata in the database, private keys in the secret store.
#[derive(Clone)]
pub struct SshKeys {
    store: Store,
    secrets: Arc<dyn SecretProvider>,
    clock: Arc<dyn Clock>,
    /// Who is an admin: the admin calls ask it each time.
    users: UserList,
}

impl fmt::Debug for SshKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshKeys").finish_non_exhaustive()
    }
}

/// `users/<user>/ssh-keys/<id>`: where the private key of key `id` is kept.
fn secret_path(user: &UserId, id: SshKeyId) -> Result<SecretPath, SshKeysError> {
    SecretPath::new(format!("users/{user}/{SSH_KEYS}/{id}"))
        .map_err(|_| SshKeysError::UserCantKeepKeys(user.to_string()))
}

/// The label after trimming, or why it isn't one: 1 to [`MAX_LABEL_CHARS`] characters, no control
/// characters.
fn check_label(label: &str) -> Result<String, SshKeysError> {
    let label = label.trim();
    let chars = label.chars().count();
    if chars == 0 {
        return Err(SshKeysError::InvalidLabel(
            "the label can't be empty".to_owned(),
        ));
    }
    if chars > MAX_LABEL_CHARS {
        return Err(SshKeysError::InvalidLabel(format!(
            "the label is {chars} characters; the limit is {MAX_LABEL_CHARS}"
        )));
    }
    if label.chars().any(char::is_control) {
        return Err(SshKeysError::InvalidLabel(
            "the label can't have control characters".to_owned(),
        ));
    }
    Ok(label.to_owned())
}

/// The label of a key whose user gave none: `<user>@doozers`.
fn default_label(user: &UserId) -> String {
    format!("{user}@doozers")
}

impl SshKeys {
    /// The service over `store`, with private keys in `secrets`, times from `clock`, and the
    /// admin calls open to the admins of `users`.
    pub fn new(
        store: Store,
        secrets: Arc<dyn SecretProvider>,
        clock: Arc<dyn Clock>,
        users: UserList,
    ) -> Self {
        Self {
            store,
            secrets,
            clock,
            users,
        }
    }

    /// Whether `user` may use the admin calls: an active admin, as for the admin pages. Read from
    /// the user list now, so a demoted or disabled admin loses it at once.
    pub fn may_administer(&self, user: &UserId) -> bool {
        self.users.is_admin(user)
    }

    /// `user`'s keys, oldest first.
    ///
    /// # Errors
    ///
    /// [`SshKeysError::Store`] if the database fails.
    pub async fn list(&self, user: &UserId) -> Result<Vec<SshKey>, SshKeysError> {
        Ok(self
            .store
            .ssh_keys(user)
            .await?
            .into_iter()
            .map(SshKey::from)
            .collect())
    }

    /// `user`'s key `id`.
    ///
    /// # Errors
    ///
    /// [`SshKeysError::NotFound`] if there is no key `id` or it belongs to someone else.
    pub async fn get(&self, user: &UserId, id: SshKeyId) -> Result<SshKey, SshKeysError> {
        self.owned(user, id).await.map(SshKey::from)
    }

    async fn owned(&self, user: &UserId, id: SshKeyId) -> Result<SshKeyRecord, SshKeysError> {
        match self.store.ssh_key(id).await? {
            Some(record) if record.user == *user => Ok(record),
            _ => Err(SshKeysError::NotFound),
        }
    }

    /// Generates a key of `kind` for `user`. The label defaults to `<user>@doozers`.
    ///
    /// # Errors
    ///
    /// [`SshKeysError::InvalidLabel`], [`SshKeysError::UserCantKeepKeys`],
    /// [`SshKeysError::Generate`], or a store or secret store failure. A failed call leaves no row
    /// behind.
    pub async fn generate(
        &self,
        user: &UserId,
        kind: KeyType,
        label: Option<&str>,
    ) -> Result<SshKey, SshKeysError> {
        let label = match label.map(str::trim).filter(|label| !label.is_empty()) {
            Some(label) => check_label(label)?,
            None => default_label(user),
        };
        secret_path(user, SshKeyId::new(0))?;
        let key = tokio::task::spawn_blocking(move || keys::generate(kind))
            .await
            .map_err(|error| SshKeysError::Generate(error.to_string()))?
            .map_err(|error| SshKeysError::Generate(error.to_string()))?;
        self.add(user, &label, &key).await
    }

    /// Adds the private key in `file` (OpenSSH, PKCS#1, SEC1, or PKCS#8 text, encrypted or not) for
    /// `user`, decrypting it with `passphrase` if it is encrypted. The label defaults to the key's
    /// comment, else `<user>@doozers`.
    ///
    /// # Errors
    ///
    /// [`SshKeysError::Upload`] if `file` isn't an acceptable key (or needs a passphrase, or the
    /// passphrase is wrong), [`SshKeysError::Duplicate`] if the user has the key already, and the
    /// errors of [`generate`](Self::generate).
    pub async fn upload(
        &self,
        user: &UserId,
        file: &[u8],
        passphrase: Option<&str>,
        label: Option<&str>,
    ) -> Result<SshKey, SshKeysError> {
        secret_path(user, SshKeyId::new(0))?;
        // Decrypting is deliberately slow (bcrypt, scrypt, PBKDF2), so it stays off the async
        // workers.
        let file = file.to_vec();
        let passphrase = passphrase.map(|p| zeroize::Zeroizing::new(p.to_owned()));
        let key = tokio::task::spawn_blocking(move || {
            formats::parse_private_key(&file, passphrase.as_deref().map(String::as_str))
        })
        .await
        .map_err(|error| SshKeysError::Generate(error.to_string()))??;
        let label = match label.map(str::trim).filter(|label| !label.is_empty()) {
            Some(label) => check_label(label)?,
            None => check_label(key.comment()).unwrap_or_else(|_| default_label(user)),
        };
        self.add(user, &label, &key).await
    }

    /// Stores `key`: the row first, so its id names the secret, then the secret; a failed secret
    /// takes the row back out.
    async fn add(
        &self,
        user: &UserId,
        label: &str,
        key: &PrivateKey,
    ) -> Result<SshKey, SshKeysError> {
        let parts = keys::public_parts(key).map_err(|e| SshKeysError::Generate(e.to_string()))?;
        // The comment is the label's business, not part of the stored secret.
        let mut bare = key.clone();
        bare.set_comment("");
        let pem = bare
            .to_openssh(ssh_key::LineEnding::LF)
            .map_err(|e| SshKeysError::Generate(e.to_string()))?;
        let row = NewSshKey {
            user: user.clone(),
            label: label.to_owned(),
            algorithm: parts.algorithm,
            public_key: parts.public_key,
            fingerprint: parts.fingerprint,
        };
        let record = self
            .store
            .ssh_key_insert(row, self.clock.now())
            .await
            .map_err(|error| match error {
                doozers_store::Error::SshKeyExists { label } => SshKeysError::Duplicate { label },
                other => other.into(),
            })?;
        let path = secret_path(user, record.id)?;
        if let Err(error) = self
            .secrets
            .put(&path, &Secret::new(pem.as_str()), Some(user))
            .await
        {
            if let Err(cleanup) = self.store.ssh_key_delete(record.id).await {
                warn!(key = %record.id, error = %cleanup, "couldn't remove the row of an SSH key whose secret wasn't stored");
            }
            return Err(error.into());
        }
        Ok(record.into())
    }

    /// Gives `user`'s key `id` a new label.
    ///
    /// # Errors
    ///
    /// [`SshKeysError::NotFound`] or [`SshKeysError::InvalidLabel`], or a store failure.
    pub async fn rename(
        &self,
        user: &UserId,
        id: SshKeyId,
        label: &str,
    ) -> Result<SshKey, SshKeysError> {
        let label = check_label(label)?;
        self.owned(user, id).await?;
        if !self.store.ssh_key_rename(id, &label).await? {
            return Err(SshKeysError::NotFound);
        }
        self.get(user, id).await
    }

    /// Deletes `user`'s key `id`: the row, then the secret.
    ///
    /// # Errors
    ///
    /// [`SshKeysError::NotFound`], or a store failure. A secret that can't be deleted is logged,
    /// not reported: the key is gone for its user, and the sealed value left behind is harmless.
    pub async fn delete(&self, user: &UserId, id: SshKeyId) -> Result<(), SshKeysError> {
        let record = self.owned(user, id).await?;
        self.remove(record).await.map(|_| ())
    }

    async fn remove(&self, record: SshKeyRecord) -> Result<SshKey, SshKeysError> {
        if !self.store.ssh_key_delete(record.id).await? {
            return Err(SshKeysError::NotFound);
        }
        match secret_path(&record.user, record.id) {
            Ok(path) => {
                if let Err(error) = self.secrets.delete(&path).await {
                    warn!(%path, %error, "couldn't delete the secret of a deleted SSH key");
                }
            }
            Err(error) => warn!(key = %record.id, %error, "a deleted SSH key has no secret path"),
        }
        Ok(record.into())
    }

    /// Every user's keys: metadata only. For the admin page.
    ///
    /// # Errors
    ///
    /// [`SshKeysError::Forbidden`] unless `acting` is an admin.
    pub async fn all_keys(&self, acting: &UserId) -> Result<Vec<SshKey>, SshKeysError> {
        self.require_admin(acting)?;
        Ok(self
            .store
            .ssh_keys_all()
            .await?
            .into_iter()
            .map(SshKey::from)
            .collect())
    }

    /// Any user's key `id`, metadata only. For the delete confirmation.
    ///
    /// # Errors
    ///
    /// [`SshKeysError::Forbidden`] unless `acting` is an admin; [`SshKeysError::NotFound`].
    pub async fn any_key(&self, acting: &UserId, id: SshKeyId) -> Result<SshKey, SshKeysError> {
        self.require_admin(acting)?;
        self.store
            .ssh_key(id)
            .await?
            .map(SshKey::from)
            .ok_or(SshKeysError::NotFound)
    }

    /// Deletes key `id` whoever owns it, and returns what it was. An admin can delete a key, for
    /// example one of a user who left; they can't use it.
    ///
    /// # Errors
    ///
    /// [`SshKeysError::Forbidden`] unless `acting` is an admin; [`SshKeysError::NotFound`].
    pub async fn delete_any(&self, acting: &UserId, id: SshKeyId) -> Result<SshKey, SshKeysError> {
        self.require_admin(acting)?;
        let record = self
            .store
            .ssh_key(id)
            .await?
            .ok_or(SshKeysError::NotFound)?;
        self.remove(record).await
    }

    fn require_admin(&self, acting: &UserId) -> Result<(), SshKeysError> {
        if self.may_administer(acting) {
            Ok(())
        } else {
            Err(SshKeysError::Forbidden)
        }
    }

    /// `user`'s keys in the order an ssh-agent offers them: most recently used first, keys never
    /// used last and the newest of those first. For the signing path.
    pub(crate) async fn agent_keys(&self, user: &UserId) -> Result<Vec<SshKey>, SshKeysError> {
        Ok(self
            .store
            .ssh_keys_by_recent_use(user)
            .await?
            .into_iter()
            .map(SshKey::from)
            .collect())
    }

    /// The decrypted private key of `user`'s key `id`, read from the secret store. Only the
    /// signing path calls this, and only for the user the workspace belongs to; nothing it
    /// returns reaches a page, a log, or a plugin.
    ///
    /// # Errors
    ///
    /// [`SshKeysError::NotFound`] if there is no key `id` or it belongs to someone else, or the
    /// secret is missing; [`SshKeysError::Secrets`] if the secret store fails or the stored value
    /// isn't a private key.
    pub(crate) async fn private_key(
        &self,
        user: &UserId,
        id: SshKeyId,
    ) -> Result<PrivateKey, SshKeysError> {
        self.owned(user, id).await?;
        let path = secret_path(user, id)?;
        let secret = self
            .secrets
            .get(&path)
            .await?
            .ok_or(SshKeysError::NotFound)?;
        let pem = Zeroizing::new(secret.expose().to_owned());
        let key = PrivateKey::from_openssh(pem.as_str()).map_err(|_| SecretError::Damaged {
            path: path.to_string(),
            reason: "it isn't an OpenSSH private key".to_owned(),
        })?;
        if key.is_encrypted() || matches!(key.key_data(), KeypairData::Encrypted(_)) {
            return Err(SecretError::Damaged {
                path: path.to_string(),
                reason: "the private key is encrypted".to_owned(),
            }
            .into());
        }
        Ok(key)
    }

    /// Records that key `id` signed at `now`.
    ///
    /// # Errors
    ///
    /// [`SshKeysError::NotFound`] if the key is gone, or a store failure.
    pub(crate) async fn touch_used(
        &self,
        id: SshKeyId,
        now: Timestamp,
    ) -> Result<(), SshKeysError> {
        if self.store.ssh_key_touch_used(id, now).await? {
            Ok(())
        } else {
            Err(SshKeysError::NotFound)
        }
    }
}
