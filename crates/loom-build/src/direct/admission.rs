use super::*;

/// Decide from Cargo metadata, before any build script or proc macro runs,
/// whether every host-executed package is an approved source. `toolchain` is
/// the builder's memoized resolution; `compiler_sysroot` is `Some` when the
/// compiler's own library sources must be admitted as well (every real build;
/// tests of the package walk pass `None`).
pub(super) async fn graph_shareable(
    root: &Path,
    cache: &Path,
    directory: &Path,
    target: &Path,
    isolated: bool,
    compiler_sysroot: Option<&Path>,
    toolchain: &crate::GuestToolchain,
) -> Result<bool, BuildError> {
    let mut command = if isolated {
        let mut command = Command::new(root.join("rustc/sandbox.sh"));
        command
            .arg("metadata")
            .arg(cache)
            .arg(directory)
            .arg(target)
            .arg(root);
        command
    } else {
        let mut command = Command::new(&toolchain.cargo);
        command.current_dir(directory).args([
            "metadata",
            "--locked",
            "--offline",
            "--format-version=1",
        ]);
        command
    };
    compiler_environment(&mut command);
    toolchain.configure(&mut command)?;
    let output = run(command).await?;
    if !output.status.success() {
        return Err(rejected(String::from_utf8_lossy(&output.stderr)));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(rejected)?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or_else(|| rejected("Cargo metadata has no packages"))?;
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or_else(|| rejected("Cargo metadata has no resolved graph"))?;
    let mut trusted = Vec::new();
    if let Some(sysroot) = compiler_sysroot {
        trusted.extend(
            trusted_sources::compiler_sources(
                sysroot,
                directory
                    .join("vendor")
                    .is_dir()
                    .then(|| directory.join("vendor"))
                    .as_deref(),
            )?
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned()),
        );
    }
    for package in packages {
        let manifest = package["manifest_path"]
            .as_str()
            .ok_or_else(|| rejected("package source missing"))?;
        let source = Path::new(manifest)
            .parent()
            .ok_or_else(|| rejected("package source missing"))?;
        if trusted_sources::approved(root, source)? {
            trusted.push(source.canonicalize()?.to_string_lossy().into_owned());
        } else {
            let is_root = source.canonicalize()? == directory.canonicalize()?;
            // What a dependency compiles is its library-kind targets (and build scripts and
            // procedural macros, refused elsewhere). Its tests, benches, examples and binaries are
            // never built, so their sources are not scanned; a crate that ships a `tests/` helper
            // with `#[path]` is not refused for it. The root package is ours: every target counts.
            let mut rust_roots: Vec<PathBuf> = Vec::new();
            for target in package["targets"]
                .as_array()
                .ok_or_else(|| rejected("package targets missing"))?
            {
                let kinds: Vec<&str> = target["kind"]
                    .as_array()
                    .map(|kinds| kinds.iter().filter_map(|kind| kind.as_str()).collect())
                    .unwrap_or_default();
                let compiled = is_root
                    || kinds.iter().any(|kind| {
                        [
                            "lib",
                            "rlib",
                            "cdylib",
                            "dylib",
                            "staticlib",
                            "proc-macro",
                            "custom-build",
                        ]
                        .contains(kind)
                    });
                if !compiled {
                    continue;
                }
                let input = target["src_path"]
                    .as_str()
                    .ok_or_else(|| rejected("compiler source input missing"))?;
                let input = Path::new(input).canonicalize()?;
                if !input.starts_with(source.canonicalize()?) {
                    return Err(rejected("compiler source escapes admitted package"));
                }
                let diagnostics =
                    loom_check::untrusted_source_diagnostics(&std::fs::read_to_string(&input)?);
                if !diagnostics.is_empty() {
                    return Err(rejected(format!(
                        "untrusted compiler input {}: {}",
                        input.display(),
                        serde_json::to_string(&diagnostics).map_err(rejected)?
                    )));
                }
                if let Some(parent) = input.parent() {
                    rust_roots.push(parent.to_owned());
                }
            }
            inspect_untrusted_source(source, is_root, (!is_root).then_some(rust_roots.as_slice()))?;
        }
    }
    let trusted_path = target
        .parent()
        .ok_or_else(|| rejected("graph directory missing"))?
        .join("trusted-sources");
    fs::write(trusted_path, trusted.join("\n") + "\n").await?;
    let mut pending = Vec::new();
    for package in packages {
        let targets = package["targets"]
            .as_array()
            .ok_or_else(|| rejected("Cargo package has no targets"))?;
        if targets.iter().any(|target| {
            target["kind"].as_array().is_some_and(|kinds| {
                kinds
                    .iter()
                    .any(|kind| kind == "proc-macro" || kind == "custom-build")
            })
        }) {
            pending.push(
                package["id"]
                    .as_str()
                    .ok_or_else(|| rejected("Cargo package has no identity"))?
                    .to_owned(),
            );
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let package = packages
            .iter()
            .find(|package| package["id"] == id)
            .ok_or_else(|| rejected("Cargo host dependency is missing"))?;
        let manifest = package["manifest_path"]
            .as_str()
            .ok_or_else(|| rejected("Cargo host dependency has no source"))?;
        if !trusted_sources::approved(root, Path::new(manifest))? {
            return Ok(false);
        }
        let node = nodes
            .iter()
            .find(|node| node["id"] == id)
            .ok_or_else(|| rejected("Cargo host dependency has no resolution"))?;
        for dependency in node["deps"]
            .as_array()
            .ok_or_else(|| rejected("Cargo host node has no dependencies"))?
        {
            if let Some(id) = dependency["pkg"].as_str() {
                pending.push(id.into());
            }
        }
    }
    Ok(true)
}

pub(super) fn materialize_root_workspace(
    source: &Path,
    workspace: &Path,
) -> Result<(), BuildError> {
    fn copy(source: &Path, destination: &Path, root: bool) -> Result<(), BuildError> {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let name = entry.file_name();
            // Build outputs recorded beside the sources: the compiled text is
            // generated code (wrappers), not source the admission policy reads.
            if root
                && (name == "component.wasm" || name == "component.inputs" || name == "compiled.rs")
            {
                continue;
            }
            let output = destination.join(&name);
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(std::fs::canonicalize(entry.path())?, output)?;
                #[cfg(not(unix))]
                return Err(rejected("root workspace requires Unix source links"));
            } else if kind.is_dir() {
                copy(&entry.path(), &output, false)?;
            } else if kind.is_file() {
                std::fs::copy(entry.path(), output)?;
            } else {
                return Err(rejected("unsupported root source file type"));
            }
        }
        Ok(())
    }
    let temporary = workspace.with_extension(format!("pending-{}", std::process::id()));
    if temporary.exists() {
        std::fs::remove_dir_all(&temporary)?;
    }
    copy(source, &temporary, true)?;
    let manifest = temporary.join("Cargo.toml");
    let content = std::fs::read_to_string(&manifest)?.replace(
        source.to_string_lossy().as_ref(),
        workspace.to_string_lossy().as_ref(),
    );
    std::fs::write(manifest, content)?;
    if workspace.exists() {
        std::fs::remove_dir_all(workspace)?;
    }
    std::fs::rename(temporary, workspace)?;
    Ok(())
}

/// `rust_roots`, when given, limits the scan of `.rs` files to the directories a dependency's
/// compiled targets live in (`build.rs` is always scanned); manifests, `.cargo` and toolchain
/// files are scanned package-wide either way.
pub(super) fn inspect_untrusted_source(
    directory: &Path,
    generated_root: bool,
    rust_roots: Option<&[PathBuf]>,
) -> Result<(), BuildError> {
    fn collect(
        root: &Path,
        directory: &Path,
        generated_root: bool,
        rust_roots: Option<&[PathBuf]>,
        files: &mut BTreeMap<String, loom_check::SourceFile>,
    ) -> Result<(), BuildError> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            // Host-placed trees of the root package. A tenant can also ship a `vendor/` directory
            // (materialize.rs accepts `vendor/` files), which this skips, so `include!` refuses a
            // path whose first component is one of these names (`UNSCANNED_ROOTS` in
            // loom-check's rust_effects/admission.rs): nothing compiled can reach an unscanned file.
            if generated_root
                && ["vendor", "loom-crates", ".cargo"].contains(&name.to_string_lossy().as_ref())
            {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(rejected(format!(
                    "untrusted source symlink: {}",
                    path.display()
                )));
            }
            if kind.is_dir() {
                collect(root, &path, false, rust_roots, files)?;
            } else if kind.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(rejected)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let rust_source = path.extension().is_some_and(|extension| extension == "rs")
                    && (name == "build.rs"
                        || rust_roots.is_none_or(|roots| {
                            path.canonicalize()
                                .is_ok_and(|path| roots.iter().any(|root| path.starts_with(root)))
                        }));
                if rust_source
                    || name == "Cargo.toml"
                    || name == "rust-toolchain"
                    || name == "rust-toolchain.toml"
                    || relative.split('/').any(|part| part == ".cargo")
                {
                    files.insert(
                        relative,
                        loom_check::SourceFile::Text(std::fs::read_to_string(&path)?),
                    );
                }
            } else {
                return Err(rejected("untrusted source contains a special file"));
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    collect(directory, directory, generated_root, rust_roots, &mut files)?;
    let diagnostics =
        loom_check::untrusted_package_diagnostics(&loom_check::SourceBundle { files });
    if !diagnostics.is_empty() {
        return Err(rejected(format!(
            "untrusted package {}: {}",
            directory.display(),
            serde_json::to_string(&diagnostics).map_err(rejected)?
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(files: &[(&str, &str)]) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = directory.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        directory
    }

    const MANIFEST: &str = "[package]\nname=\"dep\"\nversion=\"0.1.0\"\nedition=\"2024\"\n";

    #[test]
    fn a_dependency_that_ships_a_tests_helper_with_a_path_attribute_is_admitted_for_its_library() {
        let dependency = package(&[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "pub fn f() -> u32 { 1 }\n"),
            (
                "tests/support.rs",
                "#[path = \"../src/lib.rs\"]\nmod lib;\n",
            ),
        ]);
        let root = dependency.path().canonicalize().unwrap();
        let compiled = vec![root.join("src")];
        inspect_untrusted_source(&root, false, Some(&compiled)).unwrap();
        // Scanning the whole package, as the root package is, still refuses the helper: the filter is what admits it.
        assert!(inspect_untrusted_source(&root, false, None).is_err());
    }

    #[test]
    fn the_compiled_directory_and_build_scripts_are_still_scanned() {
        let in_source = package(&[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "pub fn f() -> u32 { 1 }\n"),
            ("src/more.rs", "include!(\"/etc/passwd\");\n"),
        ]);
        let root = in_source.path().canonicalize().unwrap();
        assert!(inspect_untrusted_source(&root, false, Some(&[root.join("src")])).is_err());
        let build_script = package(&[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "pub fn f() -> u32 { 1 }\n"),
            ("build.rs", "fn main() {}\n"),
        ]);
        let root = build_script.path().canonicalize().unwrap();
        assert!(
            inspect_untrusted_source(&root, false, Some(&[root.join("src")])).is_err(),
            "a build script outside the compiled directory is refused anyway"
        );
    }

    #[test]
    fn include_reaches_only_rust_files_the_scan_reads() {
        let scanned = |files: &[(&str, &str)], generated_root: bool| {
            let directory = package(files);
            let root = directory.path().canonicalize().unwrap();
            inspect_untrusted_source(
                &root,
                generated_root,
                (!generated_root).then(|| vec![root.join("src")]).as_deref(),
            )
        };
        let lib = |body: &'static str| [("Cargo.toml", MANIFEST), ("src/lib.rs", body)];
        // A `.rs` file under the compiled directory is scanned, so including it is admitted and its content is checked.
        let mut files = lib("include!(\"more.rs\");").to_vec();
        files.push(("src/more.rs", "pub fn f() {}\n"));
        assert!(scanned(&files, false).is_ok());
        files[2] = ("src/more.rs", "#[no_mangle] pub fn f() {}\n");
        assert!(scanned(&files, false).is_err());
        // A payload with any other extension is never read by the scan, so `include!` of it is refused.
        let mut files = lib("include!(\"payload.txt\");").to_vec();
        files.push(("src/payload.txt", "#[no_mangle] pub fn f() {}\n"));
        assert!(scanned(&files, false).is_err());
        // Data stays includable in any extension.
        let mut files = lib("pub const S: &str = include_str!(\"payload.txt\");").to_vec();
        files.push(("src/payload.txt", "anything"));
        assert!(scanned(&files, false).is_ok());
        // The root package skips its top-level `vendor/`; a root target outside `src/` must not reach it.
        let root_manifest = format!("{MANIFEST}[[bin]]\nname=\"tool\"\npath=\"main.rs\"\n");
        let files = [
            ("Cargo.toml", root_manifest.as_str()),
            ("src/lib.rs", "pub fn f() {}\n"),
            ("main.rs", "include!(\"vendor/hidden.rs\");\nfn main() {}\n"),
            ("vendor/hidden.rs", "#[no_mangle] pub fn f() {}\n"),
        ];
        assert!(scanned(&files, true).is_err());
    }
}
