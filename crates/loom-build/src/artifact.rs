use super::*;

pub(super) fn trusted_dependency(name: &str, value: &toml::Value) -> bool {
    ["loom", "serde", "serde_json"].contains(&name)
        && value
            .get("package")
            .and_then(toml::Value::as_str)
            .is_none_or(|package| package == name || name == "loom" && package == "loom-guest-rs")
        && value.get("registry").is_none()
        && value.get("git").is_none()
        && value.get("path").is_none()
}

pub(super) fn is_vendored(definition: &CheckedDef) -> bool {
    serde_json::from_str::<SourceBundle>(&definition.source).is_ok_and(|bundle| {
        bundle
            .files
            .keys()
            .any(|name| name.starts_with("vendor/") || name == preparation::VENDOR_TREE)
    })
}

pub(super) const VENDOR_CONFIG: &str = include_str!("../../../rustc/vendor-config.toml");

pub(super) fn build_fingerprint(root: &Path) -> Result<String, BuildError> {
    fn collect(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if !["node_modules", "target", ".git"]
                    .contains(&entry.file_name().to_string_lossy().as_ref())
                {
                    collect(&path, files)?;
                }
            } else if path.extension().is_some_and(|extension| {
                ["rs", "toml", "lock", "json", "sh"]
                    .iter()
                    .any(|allowed| extension == *allowed)
            }) {
                files.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    for directory in ["crates/loom-guest-rs", "crates/loom-proto", "rustc"] {
        collect(&root.join(directory), &mut files)?;
    }
    files.push(root.join("Cargo.lock"));
    files.sort();
    let mut hash = blake3::Hasher::new();
    hash.update(b"loom-core-build-v3-dag-cbor");
    hash.update(include_bytes!("materialize.rs"));
    hash.update(include_bytes!("intake.rs"));
    hash.update(loom_check::safety_policy_bytes());
    hash.update(include_bytes!("identity.rs"));
    hash.update(include_bytes!("direct.rs"));
    hash.update(include_bytes!("direct/recipe.rs"));
    hash.update(include_bytes!("direct/compile.rs"));
    hash.update(include_bytes!("direct/entry_abi.rs"));
    hash.update(include_bytes!("direct/graph.rs"));
    hash.update(include_bytes!("direct/admission.rs"));
    hash.update(include_bytes!("manifest.rs"));
    hash.update(include_bytes!("threaded_module.rs"));
    hash.update(include_bytes!("dwarf.rs"));
    hash.update(include_bytes!("direct/artifacts.rs"));
    hash.update(include_bytes!("direct/definition_rlibs.rs"));
    hash.update(include_bytes!("direct/compiler_cache.rs"));
    hash.update(include_bytes!("direct/trusted_sources.rs"));
    hash.update(include_bytes!("preparation.rs"));
    hash.update(include_bytes!("handler_dependencies.rs"));
    for path in files {
        let relative = path
            .strip_prefix(root)
            .map_err(|error| BuildError::Rejected(error.to_string()))?
            .to_string_lossy();
        let bytes = std::fs::read(&path)?;
        hash.update(&(relative.len() as u64).to_le_bytes());
        hash.update(relative.as_bytes());
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    Ok(hash.finalize().to_hex().to_string())
}

pub(super) fn cargo_artifact(
    output: &str,
    manifest: &Path,
    target: &Path,
) -> Result<PathBuf, BuildError> {
    let manifest = std::fs::canonicalize(manifest)?;
    let target = std::fs::canonicalize(target)?;
    let mut artifact = None;
    for line in output.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-artifact" {
            continue;
        }
        let Some(path) = message["manifest_path"].as_str() else {
            continue;
        };
        if std::fs::canonicalize(path).ok().as_ref() != Some(&manifest) {
            continue;
        }
        if !message["target"]["crate_types"]
            .as_array()
            .is_some_and(|types| types.iter().any(|kind| kind == "cdylib"))
        {
            continue;
        }
        if let Some(paths) = message["filenames"].as_array() {
            for path in paths.iter().filter_map(serde_json::Value::as_str) {
                if Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension == "wasm")
                {
                    let path = std::fs::canonicalize(path)?;
                    if !path.starts_with(&target) {
                        return Err(BuildError::Rejected(
                            "Cargo artifact escaped the build target".into(),
                        ));
                    }
                    artifact = Some(path);
                }
            }
        }
    }
    artifact.ok_or_else(|| {
        BuildError::Rejected("Cargo did not report a component-producing root artifact".into())
    })
}

pub(super) fn validate_component(bytes: &[u8]) -> Result<(), BuildError> {
    if !loom_proto::core_protocol::is_current(bytes) {
        return Err(BuildError::Rejected(
            "builder output has no supported executable ABI".into(),
        ));
    }
    Ok(())
}
pub fn cargo_diagnostics(output: &str) -> Vec<Diagnostic> {
    output
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            if value.get("reason")?.as_str()? != "compiler-message" {
                return None;
            }
            let message = value.get("message")?;
            if message.get("level")?.as_str()? != "error" {
                return None;
            }
            let span = message
                .get("spans")
                .and_then(|spans| spans.as_array())
                .and_then(|spans| spans.iter().find(|span| span["is_primary"] == true));
            Some(Diagnostic {
                lang: Lang::Rust,
                file: span
                    .and_then(|span| span["file_name"].as_str())
                    .unwrap_or("src/lib.rs")
                    .into(),
                line: span
                    .and_then(|span| span["line_start"].as_u64())
                    .unwrap_or(1) as usize,
                col: span
                    .and_then(|span| span["column_start"].as_u64())
                    .unwrap_or(1) as usize,
                code: message["code"]["code"]
                    .as_str()
                    .unwrap_or("RUST_BUILD")
                    .into(),
                message: message["message"]
                    .as_str()
                    .unwrap_or("Rust build failed")
                    .into(),
                snippet: span
                    .and_then(|span| span["text"][0]["text"].as_str())
                    .map(str::to_owned),
                hint: message["children"]
                    .as_array()
                    .and_then(|children| children.iter().find(|child| child["level"] == "help"))
                    .and_then(|child| child["message"].as_str())
                    .map(str::to_owned),
            })
        })
        .collect()
}
