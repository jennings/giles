//! The plugin's own state in the data directory: `discord-state.json`.
//!
//! ```json
//! {
//!   "version": 1,
//!   "dm_channels": {
//!     "319674150115610528": { "cursor": "1234567890123456789", "user": "82198898841029460" }
//!   }
//! }
//! ```
//!
//! One entry per direct message channel the plugin knows (a bot can't list them, so it learns them
//! from the messages it receives and the channels it creates for a send). `cursor` is the highest
//! message id of that channel the plugin has finished handling, which catch-up fetches after
//! (`Docs/technical/discord.md`, "Catch-Up"). `user` is the other participant. Ids are strings in
//! the file, because snowflakes are more than JSON numbers carry safely.
//!
//! The SDK's own state is in `giles-sdk/` of the same directory; this file is the plugin's.
//!
//! Writes are atomic: a temporary file in the same directory, `fsync`, rename, and `fsync` of the
//! directory, so a crash leaves the old file or the new one. A file that can't be read or
//! understood is renamed to `discord-state.json.corrupt` (replacing an older one, never deleted
//! outright) and the plugin starts
//! without it: losing it costs a catch-up of at most `catch_up_max_age`, which the core
//! deduplicates.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

/// The file's name in the data directory.
pub const FILE_NAME: &str = "discord-state.json";

/// The format this plugin reads and writes.
const VERSION: u32 = 1;

/// What the plugin knows about one direct message channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DmChannel {
    /// The highest message id handled: delivered to the SDK, or dropped on purpose. `None` before
    /// the first.
    pub cursor: Option<u64>,
    /// The other participant, if known.
    pub user: Option<u64>,
}

/// The state file, in memory and on disk. Cloning gives another handle to the same state; every
/// change is written before the method returns.
#[derive(Clone)]
pub struct StateFile {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    channels: Mutex<BTreeMap<u64, DmChannel>>,
}

impl StateFile {
    /// The state in `dir` (the plugin's data directory). A missing file is an empty state; one
    /// that can't be used is set aside as `.corrupt` and logged.
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        let channels = match load(&path) {
            Ok(channels) => channels,
            Err(error) => {
                let aside = corrupt_path(&path);
                match fs::rename(&path, &aside) {
                    Ok(()) => tracing::warn!(
                        "the Discord state file couldn't be used ({error}); moved it to {} and \
                         started without it",
                        aside.display()
                    ),
                    Err(rename) => tracing::warn!(
                        "the Discord state file couldn't be used ({error}) and couldn't be moved \
                         aside ({rename}); starting without it"
                    ),
                }
                BTreeMap::new()
            }
        };
        Self {
            inner: Arc::new(Inner {
                path,
                channels: Mutex::new(channels),
            }),
        }
    }

    /// Every known DM channel, in id order.
    pub fn channels(&self) -> Vec<u64> {
        self.lock().keys().copied().collect()
    }

    /// What is known about `channel`.
    pub fn get(&self, channel: u64) -> Option<DmChannel> {
        self.lock().get(&channel).copied()
    }

    /// The DM channel with `user`, if one is known.
    pub fn channel_for_user(&self, user: u64) -> Option<u64> {
        self.lock()
            .iter()
            .find(|(_, dm)| dm.user == Some(user))
            .map(|(channel, _)| *channel)
    }

    /// Records that `channel` is the DM channel with `user`, adding it if it is new. The cursor
    /// is not touched.
    pub fn record_dm(&self, channel: u64, user: u64) {
        self.update(|channels| {
            let dm = channels.entry(channel).or_default();
            let changed = dm.user != Some(user);
            dm.user = Some(user);
            changed
        });
    }

    /// Moves the cursor of `channel` to `message`, if that is further than it is, adding the
    /// channel if it is new.
    pub fn advance(&self, channel: u64, message: u64) {
        self.update(|channels| {
            let dm = channels.entry(channel).or_default();
            if dm.cursor.is_some_and(|cursor| cursor >= message) {
                return false;
            }
            dm.cursor = Some(message);
            true
        });
    }

    /// Applies `change` (which says whether it changed anything) and writes the file if it did.
    /// A failed write is logged and the change stays in memory: the next change writes it again.
    fn update(&self, change: impl FnOnce(&mut BTreeMap<u64, DmChannel>) -> bool) {
        let mut channels = self.lock();
        if change(&mut channels)
            && let Err(error) = save(&self.inner.path, &channels)
        {
            tracing::warn!("couldn't write the Discord state file: {error}");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, DmChannel>> {
        self.inner
            .channels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The file as written.
#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    dm_channels: BTreeMap<String, StoredChannel>,
}

#[derive(Serialize, Deserialize)]
struct StoredChannel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<String>,
}

/// Reads the state at `path`. A missing file is an empty state; anything else that is wrong is an
/// error that says what, without quoting the file (its content is ids only, but an editor's
/// leftovers could be anything).
fn load(path: &Path) -> Result<BTreeMap<u64, DmChannel>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(format!("unreadable: {error}")),
    };
    let stored: Stored =
        serde_json::from_slice(&bytes).map_err(|error| format!("not valid: {error}"))?;
    if stored.version != VERSION {
        return Err(format!("version {} is not {VERSION}", stored.version));
    }
    let id = |text: &str| {
        text.parse::<u64>()
            .map_err(|_| "an id that isn't a number".to_owned())
    };
    let mut channels = BTreeMap::new();
    for (channel, dm) in stored.dm_channels {
        channels.insert(
            id(&channel)?,
            DmChannel {
                cursor: dm.cursor.as_deref().map(id).transpose()?,
                user: dm.user.as_deref().map(id).transpose()?,
            },
        );
    }
    Ok(channels)
}

/// Writes `channels` to `path` atomically: a temporary file next to it, synced, renamed over it,
/// and the directory synced.
fn save(path: &Path, channels: &BTreeMap<u64, DmChannel>) -> io::Result<()> {
    let stored = Stored {
        version: VERSION,
        dm_channels: channels
            .iter()
            .map(|(channel, dm)| {
                (
                    channel.to_string(),
                    StoredChannel {
                        cursor: dm.cursor.map(|id| id.to_string()),
                        user: dm.user.map(|id| id.to_string()),
                    },
                )
            })
            .collect(),
    };
    let mut bytes = serde_json::to_vec_pretty(&stored).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let mut file = File::create(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    // A directory can be opened and synced on Unix, which is where the plugin runs.
    File::open(dir)?.sync_all()
}

fn corrupt_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".corrupt");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests;
