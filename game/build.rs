//! Inject PSoXide's PSX linker scripts into the final link, by absolute path
//! derived from this crate's location (identical approach to oot-psx). The SDK
//! lives under `<repo>/.psoxide` (a git submodule, or a symlink to a
//! sibling working copy during local development).
//!
//! PSXcel runs on the scene core (SCENES.md): the scripts go in the order the core's ABI gives,
//! the RAM size, psoxide-core.ld (the core's stored copy and `__core_boot`), psoxide-boot.ld (the
//! boot splash's boot-only memory), then psoxide.ld. Sections run apart from where they are stored,
//! so the link is an ELF and the Makefile writes the EXE from it by load address (psx-exe).

use std::path::{Path, PathBuf};

fn script(repo_root: &Path, name: &str) {
    let path = repo_root.join(".psoxide/sdk").join(name);
    let path = path.canonicalize().unwrap_or(path);
    println!("cargo:rustc-link-arg=-T{}", path.display());
    println!("cargo:rerun-if-changed={}", path.display());
}

fn main() {
    // Host builds (the `make test` unit-test suite) must not get the PSX
    // linker scripts.
    if std::env::var("TARGET").as_deref() != Ok("mipsel-sony-psx") {
        return;
    }
    // This crate lives at <repo>/game, so the repo root is one level up.
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest.parent().expect("crate must live at <repo>/game");

    // RAM the program links for, as a `-T` file ahead of psoxide.ld (the way psoxide.ld asks, so
    // the link map records the value). 0x1FE000 puts STACK_INIT at 0x801FDF00 and leaves
    // 0x801FDF00..0x80200000 to the scene core's window (8,192 B) and its 256 B handoff block.
    const RAM_SIZE_OVERRIDE: u32 = 0x1F_E000;
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("ram-size.ld");
    std::fs::write(&out, format!("RAM_SIZE_OVERRIDE = {RAM_SIZE_OVERRIDE:#X};\n"))
        .expect("write ram-size.ld");
    println!("cargo:rustc-link-arg=-T{}", out.display());
    script(repo_root, "psoxide-core.ld");
    script(repo_root, "psoxide-boot.ld");
    script(repo_root, "psoxide.ld");
    // Optional linker map for `make gate`; link-only, the image is unchanged.
    println!("cargo:rerun-if-env-changed=PSX_LINK_MAP");
    if let Some(map) = std::env::var_os("PSX_LINK_MAP").filter(|path| !path.is_empty()) {
        println!("cargo:rustc-link-arg=-Map={}", PathBuf::from(map).display());
    }
}
