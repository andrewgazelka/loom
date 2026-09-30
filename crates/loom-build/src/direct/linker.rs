//! The pre-armed linker: `rust-lld` costs about 20 ms to start, so the compiler server starts one at
//! the beginning of each request and `loom-link` (this crate's sibling binary, rustc's `-C linker`)
//! hands it the real arguments when rustc reaches the link. See `crates/loom-link` and
//! `tools/hash-rustc/src/serve.rs`. Both files must exist; otherwise the root compile links with
//! the toolchain's own lld exactly as before. `LOOM_PREARM_LINKER=0` turns it off.
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// The two programs the recipe names.
#[derive(Clone, Debug)]
pub(super) struct Front {
    /// `loom-link`, rustc's linker.
    pub link: PathBuf,
    /// The host toolchain's `rust-lld`, which the server keeps waiting.
    pub lld: PathBuf,
}

pub(super) fn find(sysroot: &Path) -> Option<Front> {
    find_with(
        sysroot,
        std::env::var_os("LOOM_PREARM_LINKER"),
        std::env::var_os("LOOM_LINK"),
        std::env::current_exe().ok(),
        host_triple,
    )
}

/// `find` with its inputs given, so the decision is testable without touching the process
/// environment: `disabled` is `LOOM_PREARM_LINKER`, `link` is `LOOM_LINK`, `executable` this
/// program's own path (where `loom-link` is built beside it), and `host` the sysroot's host triple.
fn find_with(
    sysroot: &Path,
    disabled: Option<OsString>,
    link: Option<OsString>,
    executable: Option<PathBuf>,
    host: impl FnOnce(&Path) -> Option<String>,
) -> Option<Front> {
    if disabled.is_some_and(|value| value == "0") {
        return None;
    }
    let link = link
        .map(PathBuf::from)
        .or_else(|| Some(executable?.parent()?.join("loom-link")))?;
    if !link.is_file() {
        return None;
    }
    // The host's own lld (`lib/rustlib/<host triple>/bin/rust-lld`), not a `wasm32-*` target's and
    // not whichever directory a listing happens to return first.
    let lld = sysroot
        .join("lib/rustlib")
        .join(host(sysroot)?)
        .join("bin/rust-lld");
    lld.is_file().then_some(Front { link, lld })
}

/// The `host:` triple of the sysroot's compiler, from `<sysroot>/bin/rustc -vV` run once per
/// sysroot and process (the builder's `GuestToolchain::version` holds the same text, but the
/// recipe builder that calls `find` is not handed it).
fn host_triple(sysroot: &Path) -> Option<String> {
    static HOSTS: OnceLock<Mutex<BTreeMap<PathBuf, Option<String>>>> = OnceLock::new();
    HOSTS
        .get_or_init(Default::default)
        .lock()
        .ok()?
        .entry(sysroot.to_owned())
        .or_insert_with(|| {
            let output = std::process::Command::new(sysroot.join("bin/rustc"))
                .arg("-vV")
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| parse_host(&String::from_utf8_lossy(&output.stdout)))?
        })
        .clone()
}

fn parse_host(version: &str) -> Option<String> {
    version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(|host| host.trim().to_owned())
        .filter(|host| !host.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERSION: &str = "rustc 1.97.0-nightly (abcdef 2026-07-01)\nbinary: rustc\ncommit-hash: abcdef\nhost: aarch64-apple-darwin\nrelease: 1.97.0-nightly\nLLVM version: 21.1.0\n";

    #[test]
    fn the_host_triple_is_read_from_the_verbose_version() {
        assert_eq!(parse_host(VERSION).as_deref(), Some("aarch64-apple-darwin"));
        assert_eq!(parse_host("rustc 1.0.0\n"), None);
        assert_eq!(parse_host("host: \n"), None);
    }

    #[test]
    fn the_host_triple_comes_from_the_sysroots_own_compiler_once() {
        use std::os::unix::fs::PermissionsExt;
        let sysroot = tempfile::tempdir().unwrap();
        let bin = sysroot.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let rustc = bin.join("rustc");
        let script: String = VERSION
            .lines()
            .map(|line| format!("echo '{line}'\n"))
            .collect();
        std::fs::write(&rustc, format!("#!/bin/sh\n{script}")).unwrap();
        std::fs::set_permissions(&rustc, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            host_triple(sysroot.path()).as_deref(),
            Some("aarch64-apple-darwin")
        );
        // Memoized: the answer outlives the compiler.
        std::fs::remove_file(&rustc).unwrap();
        assert_eq!(
            host_triple(sysroot.path()).as_deref(),
            Some("aarch64-apple-darwin")
        );
        // A sysroot without a compiler has no host, and so no pre-armed linker.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(host_triple(empty.path()), None);
    }

    #[test]
    fn the_host_lld_is_the_one_under_the_host_triple_and_the_wrapper_is_required() {
        let sysroot = tempfile::tempdir().unwrap();
        // Several targets with an lld of their own, none of them first by name or by listing order.
        for name in [
            "wasm32-unknown-unknown",
            "aarch64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
        ] {
            let bin = sysroot.path().join("lib/rustlib").join(name).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("rust-lld"), b"").unwrap();
        }
        let wrapper = sysroot.path().join("loom-link");
        let find = |disabled: Option<&str>, host: &'static str| {
            find_with(
                sysroot.path(),
                disabled.map(OsString::from),
                Some(wrapper.clone().into_os_string()),
                None,
                move |_| Some(host.to_owned()),
            )
        };
        assert!(
            find(None, "aarch64-apple-darwin").is_none(),
            "no wrapper file, no pre-armed linker"
        );
        std::fs::write(&wrapper, b"").unwrap();
        for host in ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"] {
            let front = find(None, host).expect("both programs exist");
            assert_eq!(
                front.lld,
                sysroot
                    .path()
                    .join("lib/rustlib")
                    .join(host)
                    .join("bin/rust-lld")
            );
            assert_eq!(front.link, wrapper);
        }
        assert!(
            find(None, "riscv64gc-unknown-linux-gnu").is_none(),
            "the host has no lld here"
        );
        assert!(find(Some("0"), "aarch64-apple-darwin").is_none());
        assert!(
            find(Some("1"), "aarch64-apple-darwin").is_some(),
            "only `0` disables it"
        );
        // The wrapper beside the running executable when `LOOM_LINK` is not set.
        let beside = find_with(
            sysroot.path(),
            None,
            None,
            Some(sysroot.path().join("loom-build")),
            |_| Some("aarch64-apple-darwin".to_owned()),
        );
        assert_eq!(beside.map(|front| front.link), Some(wrapper));
    }
}
