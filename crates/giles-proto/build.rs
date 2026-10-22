//! Generates the Rust code for the Protobuf contracts.
//!
//! The sources are in `proto/`, a symlink to the repository's `proto/` directory, so they ship in
//! the published package. `protoc` comes from `PROTOC` if it is set, else from the vendored
//! `protoc-bin-vendored` crate, so building needs no system `protoc`.

use std::env;
use std::error::Error;
use std::path::PathBuf;

/// Every package, one file each. The generated module tree in `src/lib.rs` mirrors this list.
const PROTOS: [&str; 5] = [
    "proto/giles/types/v1/types.proto",
    "proto/giles/plugin/v1/plugin.proto",
    "proto/giles/provider/v1/provider.proto",
    "proto/giles/channel/v1/channel.proto",
    "proto/giles/admin/v1/admin.proto",
];

fn main() -> Result<(), Box<dyn Error>> {
    // Any rerun-if line turns off Cargo's default rerun on package changes, and neither
    // tonic-prost-build nor prost-build emits lines for the sources, so list every file.
    println!("cargo::rerun-if-env-changed=PROTOC");
    for proto in PROTOS {
        println!("cargo::rerun-if-changed={proto}");
    }
    let mut config = prost_build::Config::new();
    if env::var_os("PROTOC").is_none() {
        config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    let protos = PROTOS.map(PathBuf::from);
    // The well-known types (`google/protobuf/timestamp.proto`) come from the vendored include
    // directory, which works with any `protoc`.
    let includes = [PathBuf::from("proto"), protoc_bin_vendored::include_path()?];
    tonic_prost_build::configure()
        .file_descriptor_set_path(out_dir.join("giles_descriptor.bin"))
        .compile_with_config(config, &protos, &includes)?;
    Ok(())
}
