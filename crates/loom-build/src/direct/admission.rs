use super::*;

pub(super) async fn graph_shareable(
    root: &Path,
    cache: &Path,
    directory: &Path,
    target: &Path,
    isolated: bool,
    compiler_sysroot: Option<&Path>,
) -> Result<bool, BuildError> {
    let toolchain = crate::resolve_guest_toolchain(root).await?;
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
            for target in package["targets"]
                .as_array()
                .ok_or_else(|| rejected("package targets missing"))?
            {
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
            }
            inspect_untrusted_source(source, source.canonicalize()? == directory.canonicalize()?)?;
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
            if root && (name == "component.wasm" || name == "component.inputs") {
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

pub(super) fn inspect_untrusted_source(
    directory: &Path,
    generated_root: bool,
) -> Result<(), BuildError> {
    fn collect(
        root: &Path,
        directory: &Path,
        generated_root: bool,
        files: &mut BTreeMap<String, loom_check::SourceFile>,
    ) -> Result<(), BuildError> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
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
                collect(root, &path, false, files)?;
            } else if kind.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(rejected)?
                    .to_string_lossy()
                    .replace('\\', "/");
                if path.extension().is_some_and(|extension| extension == "rs")
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
    collect(directory, directory, generated_root, &mut files)?;
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
