//! Inject PSoXide's PSX linker script into the final link, by absolute path
//! derived from this crate's location (identical approach to oot-psx). The SDK
//! lives under `<repo>/.psoxide` (a git submodule, or a symlink to a
//! sibling working copy during local development).

use std::path::PathBuf;

fn main() {
    // Host builds (the `make test` unit-test suite) must not get the PSX
    // linker script or the flat-binary output format.
    if std::env::var("TARGET").as_deref() != Ok("mipsel-sony-psx") {
        return;
    }
    // This crate lives at <repo>/game, so the repo root is one level up.
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest.parent().expect("crate must live at <repo>/game");
    let ld = repo_root.join(".psoxide/sdk/psoxide.ld");
    let ld = ld.canonicalize().unwrap_or(ld);

    println!("cargo:rustc-link-arg=-T{}", ld.display());
    println!("cargo:rustc-link-arg=--oformat=binary");
    println!("cargo:rerun-if-changed={}", ld.display());
}
