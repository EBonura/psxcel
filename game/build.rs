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

    // RAM the program links for, as a `-T` file ahead of psoxide.ld (the way psoxide.ld:16-21
    // asks, so the link map records the value). 0x1FE000 puts STACK_INIT at 0x801FDF00 and
    // leaves 0x801FDF00..0x80200000 to the scene system's resident core window (8,192 B)
    // and its 256 B handoff block. PROVISIONAL: the number comes from the scene-system plan
    // (campaign scene-system-1123, PLAN.md 3.2) and the first real core build may move it.
    const RAM_SIZE_OVERRIDE: u32 = 0x1F_E000;
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("ram-size.ld");
    std::fs::write(&out, format!("RAM_SIZE_OVERRIDE = {RAM_SIZE_OVERRIDE:#X};\n"))
        .expect("write ram-size.ld");
    println!("cargo:rustc-link-arg=-T{}", out.display());
    // `scene-core`: the boot splash runs from the SDK's boot-only memory (sdk/psoxide-boot.ld,
    // ahead of psoxide.ld). Its sections run apart from where they are stored, so the link is an
    // ELF and the Makefile writes the EXE from it by load address (psx-exe).
    let boot = std::env::var_os("CARGO_FEATURE_SCENE_CORE").is_some();
    if boot {
        let boot_ld = repo_root.join(".psoxide/sdk/psoxide-boot.ld");
        let boot_ld = boot_ld.canonicalize().unwrap_or(boot_ld);
        println!("cargo:rustc-link-arg=-T{}", boot_ld.display());
        println!("cargo:rerun-if-changed={}", boot_ld.display());
    }
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    if !boot {
        println!("cargo:rustc-link-arg=--oformat=binary");
    }
    println!("cargo:rerun-if-changed={}", ld.display());
    // Optional linker map for `make gate`; link-only, the image is unchanged.
    println!("cargo:rerun-if-env-changed=PSX_LINK_MAP");
    if let Some(map) = std::env::var_os("PSX_LINK_MAP").filter(|path| !path.is_empty()) {
        println!("cargo:rustc-link-arg=-Map={}", PathBuf::from(map).display());
    }
}
