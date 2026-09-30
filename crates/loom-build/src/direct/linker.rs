//! The pre-armed linker: `rust-lld` costs about 20 ms to start, so the compiler server starts one at
//! the beginning of each request and `loom-link` (this crate's sibling binary, rustc's `-C linker`)
//! hands it the real arguments when rustc reaches the link. See `crates/loom-link` and
//! `tools/hash-rustc/src/serve.rs`. Both files must exist; otherwise the root compile links with
//! the toolchain's own lld exactly as before. `LOOM_PREARM_LINKER=0` turns it off.
use std::path::{Path, PathBuf};

/// The two programs the recipe names.
#[derive(Clone, Debug)]
pub(super) struct Front {
    /// `loom-link`, rustc's linker.
    pub link: PathBuf,
    /// The host toolchain's `rust-lld`, which the server keeps waiting.
    pub lld: PathBuf,
}

pub(super) fn find(sysroot: &Path) -> Option<Front> {
    if std::env::var_os("LOOM_PREARM_LINKER").is_some_and(|value| value == "0") {
        return None;
    }
    let link = std::env::var_os("LOOM_LINK")
        .map(PathBuf::from)
        .or_else(|| Some(std::env::current_exe().ok()?.parent()?.join("loom-link")))?;
    if !link.is_file() {
        return None;
    }
    // The host's own lld (`lib/rustlib/<host triple>/bin/rust-lld`), not a `wasm32-*` target's.
    let lld = std::fs::read_dir(sysroot.join("lib/rustlib"))
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with("wasm32"))
        .map(|entry| entry.path().join("bin/rust-lld"))
        .find(|path| path.is_file())?;
    Some(Front { link, lld })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_lld_is_found_beside_the_target_directories_and_the_wrapper_is_required() {
        let sysroot = tempfile::tempdir().unwrap();
        for name in ["wasm32-unknown-unknown", "aarch64-apple-darwin"] {
            let bin = sysroot.path().join("lib/rustlib").join(name).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
        }
        std::fs::write(
            sysroot.path().join("lib/rustlib/aarch64-apple-darwin/bin/rust-lld"),
            b"",
        )
        .unwrap();
        // A wasm32 directory with an lld of its own is not the host's.
        std::fs::write(
            sysroot.path().join("lib/rustlib/wasm32-unknown-unknown/bin/rust-lld"),
            b"",
        )
        .unwrap();
        let wrapper = sysroot.path().join("loom-link");
        // SAFETY: tests in this module run one at a time on this variable.
        unsafe { std::env::set_var("LOOM_LINK", &wrapper) };
        assert!(find(sysroot.path()).is_none(), "no wrapper file, no pre-armed linker");
        std::fs::write(&wrapper, b"").unwrap();
        let front = find(sysroot.path()).expect("both programs exist");
        assert_eq!(front.lld, sysroot.path().join("lib/rustlib/aarch64-apple-darwin/bin/rust-lld"));
        assert_eq!(front.link, wrapper);
        unsafe { std::env::set_var("LOOM_PREARM_LINKER", "0") };
        assert!(find(sysroot.path()).is_none());
        unsafe {
            std::env::remove_var("LOOM_PREARM_LINKER");
            std::env::remove_var("LOOM_LINK");
        }
    }
}
