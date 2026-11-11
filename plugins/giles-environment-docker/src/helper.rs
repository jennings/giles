//! The static Linux helper copied into every workspace container.
//!
//! `giles-workspace-helper` is built for the engine's architecture and shipped next to the plugin
//! (or in `helper_dir`) as `giles-workspace-helper-<arch>`, where `<arch>` is `x86_64` or
//! `aarch64`. The plugin picks the file that matches the engine, which may not be the plugin's own
//! architecture (a plugin on macOS driving a Linux VM).

use std::path::{Path, PathBuf};

use bytes::Bytes;

use crate::error::{Error, ErrorExt as _, Result};

/// Where the helper lives inside a container.
pub const HELPER_PATH: &str = "/.giles/bin/giles-workspace-helper";

/// A CPU architecture the helper is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    /// x86-64 (Docker's `amd64`).
    X86_64,
    /// 64-bit ARM (Docker's `arm64`).
    Aarch64,
}

impl Arch {
    /// The architecture a Docker `info.Architecture` value names (`x86_64`, `amd64`, `aarch64`,
    /// `arm64`).
    pub fn from_engine(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "x86_64" | "amd64" => Ok(Self::X86_64),
            "aarch64" | "arm64" => Ok(Self::Aarch64),
            other => Err(Error::invalid(format!(
                "the container engine's architecture is {other:?}; the workspace helper is only \
                 built for x86_64 and aarch64"
            ))),
        }
    }

    /// The suffix of the helper's file name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    /// The Rust target the helper is built for.
    pub fn musl_target(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-unknown-linux-musl",
            Self::Aarch64 => "aarch64-unknown-linux-musl",
        }
    }

    /// The helper's file name for this architecture.
    pub fn file_name(self) -> String {
        format!("giles-workspace-helper-{}", self.as_str())
    }
}

/// Where to look for helper files when `helper_dir` isn't set: the directory of this executable.
pub fn default_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::other(format!("can't find the plugin's own executable: {e}")))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::other("the plugin's executable has no directory"))
}

/// Reads the helper for `arch` from `dir`.
pub fn load(dir: &Path, arch: Arch) -> Result<Vec<u8>> {
    let path = dir.join(arch.file_name());
    std::fs::read(&path).map_err(|e| {
        Error::invalid(format!(
            "no workspace helper for {} at {}: {e}. Build it with `cargo build --release -p \
             giles-workspace-helper --target {}` and copy it there as {}, or point `helper_dir` \
             at the directory that holds it",
            arch.as_str(),
            path.display(),
            arch.musl_target(),
            arch.file_name(),
        ))
    })
}

/// A tar archive, to extract at `/`, holding `/.giles/bin/giles-workspace-helper` and the empty
/// directory `/.giles/run` where `run` records process groups.
pub fn archive(helper: &[u8]) -> Result<Bytes> {
    let mut builder = tar::Builder::new(Vec::with_capacity(helper.len() + 4096));
    directory(&mut builder, ".giles/", 0o755)?;
    directory(&mut builder, ".giles/bin/", 0o755)?;
    // Sticky and world-writable, so commands that run as another user can still record a group.
    directory(&mut builder, ".giles/run/", 0o1777)?;
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(helper.len() as u64);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    builder
        .append_data(&mut header, ".giles/bin/giles-workspace-helper", helper)
        .map_err(|e| Error::other(format!("building the helper archive: {e}")))?;
    builder
        .into_inner()
        .map(Bytes::from)
        .map_err(|e| Error::other(format!("building the helper archive: {e}")))
}

fn directory(builder: &mut tar::Builder<Vec<u8>>, path: &str, mode: u32) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    builder
        .append_data(&mut header, path, std::io::empty())
        .map_err(|e| Error::other(format!("building the helper archive: {e}")))
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn engine_architectures_map_to_helper_architectures() {
        for (engine, arch) in [
            ("x86_64", Arch::X86_64),
            ("amd64", Arch::X86_64),
            ("AMD64", Arch::X86_64),
            ("aarch64", Arch::Aarch64),
            ("arm64", Arch::Aarch64),
            ("arm64\n", Arch::Aarch64),
        ] {
            assert_eq!(Arch::from_engine(engine).unwrap(), arch, "{engine:?}");
        }
        let error = Arch::from_engine("riscv64").unwrap_err().to_string();
        assert!(error.contains("riscv64"), "{error}");
    }

    #[test]
    fn helper_files_are_named_by_architecture() {
        assert_eq!(Arch::X86_64.file_name(), "giles-workspace-helper-x86_64");
        assert_eq!(Arch::Aarch64.file_name(), "giles-workspace-helper-aarch64");
    }

    #[test]
    fn a_missing_helper_says_where_it_looked_and_how_to_build_it() {
        let dir = std::env::temp_dir().join("giles-no-such-helper-dir");
        let error = load(&dir, Arch::Aarch64).unwrap_err().to_string();
        assert!(error.contains("giles-workspace-helper-aarch64"), "{error}");
        assert!(error.contains("aarch64-unknown-linux-musl"), "{error}");
        assert!(error.contains("helper_dir"), "{error}");
    }

    #[test]
    fn the_archive_holds_the_helper_and_the_run_directory() {
        let bytes = archive(b"#!helper").unwrap();
        let mut archive = tar::Archive::new(bytes.as_ref());
        let mut seen = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().display().to_string();
            let mode = entry.header().mode().unwrap();
            let mut content = String::new();
            entry.read_to_string(&mut content).unwrap();
            seen.push((path, mode, content));
        }
        assert_eq!(
            seen,
            [
                (".giles/".to_owned(), 0o755, String::new()),
                (".giles/bin/".to_owned(), 0o755, String::new()),
                (".giles/run/".to_owned(), 0o1777, String::new()),
                (
                    ".giles/bin/giles-workspace-helper".to_owned(),
                    0o755,
                    "#!helper".to_owned()
                ),
            ]
        );
    }
}
