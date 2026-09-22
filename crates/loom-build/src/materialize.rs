use super::*;

pub(super) struct Materialization<'a> {
    pub store: &'a loom_store::Store,
    pub root: &'a Path,
    pub cache: &'a Path,
    pub directory: &'a Path,
    pub definition: &'a CheckedDef,
    pub dependencies: &'a BTreeMap<String, CheckedDef>,
    pub dependency: bool,
    pub isolated: bool,
}

pub(super) async fn materialize_rust(request: Materialization<'_>) -> Result<(), BuildError> {
    let Materialization {
        store,
        root,
        cache,
        directory,
        definition,
        dependencies,
        dependency,
        isolated,
    } = request;
    handler_dependencies::validate(definition, dependencies)?;
    let mut files = if definition.source.trim_start().starts_with('{') {
        let bundle: loom_check::SourceBundle = serde_json::from_str(&definition.source)
            .map_err(|error| BuildError::Rejected(error.to_string()))?;
        bundle.validate().map_err(BuildError::Rejected)?;
        bundle.files
    } else {
        BTreeMap::from([(
            "src/lib.rs".to_string(),
            SourceFile::Text(definition.source.clone()),
        )])
    };
    if files.keys().any(|name| name.starts_with("loom-crates/")) {
        return Err(BuildError::Rejected(
            "loom-crates source paths are host-owned".into(),
        ));
    }
    let mut manifest = if let Some(source) = files.remove("Cargo.toml") {
        source
            .as_text()
            .ok_or_else(|| BuildError::Rejected("Cargo.toml must be UTF-8".into()))?
            .parse::<toml::Value>()
            .map_err(|error| BuildError::Rejected(error.to_string()))?
    } else {
        "[package]\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nserde={version=\"1\",features=[\"derive\"]}\nserde_json=\"1\"\n".parse::<toml::Value>().map_err(|error|BuildError::Rejected(error.to_string()))?
    };
    let table = manifest
        .as_table_mut()
        .ok_or_else(|| BuildError::Rejected("Cargo.toml must be a table".into()))?;
    super::manifest::validate_manifest(&toml::Value::Table(table.clone()), isolated)?;
    let crates = loom_check::crate_dependencies(
        &toml::to_string(&toml::Value::Table(table.clone()))
            .map_err(|error| BuildError::Rejected(error.to_string()))?,
    )
    .map_err(BuildError::Rejected)?;
    let crate_aliases: std::collections::BTreeSet<_> = crates.keys().cloned().collect();
    struct CratePin {
        alias: Option<String>,
        dependency: loom_check::CrateDependency,
    }
    let mut pins: Vec<CratePin> = crates
        .into_iter()
        .map(|entry| CratePin {
            alias: Some(entry.0),
            dependency: entry.1,
        })
        .collect();
    // Cargo applies patches only from the root package. Include the exact pins
    // of definition dependencies so their SDK types keep one package identity.
    if !dependency {
        for checked in dependencies.values() {
            if let Ok(bundle) = serde_json::from_str::<SourceBundle>(&checked.source)
                && let Some(manifest) = bundle.files.get("Cargo.toml").and_then(SourceFile::as_text)
            {
                pins.extend(
                    loom_check::crate_dependencies(manifest)
                        .map_err(BuildError::Rejected)?
                        .into_values()
                        .map(|dependency| CratePin {
                            alias: None,
                            dependency,
                        }),
                );
            }
        }
    }
    let mut materialized_crates = std::collections::BTreeSet::new();
    let mut crate_patches = toml::map::Map::new();
    let mut package_sources = BTreeMap::new();
    for pin in pins {
        let dependency = pin.dependency;
        let relative = format!("loom-crates/{}", dependency.hash);
        let destination = directory.join(&relative);
        if materialized_crates.insert(dependency.hash.clone()) {
            preparation::materialize_tree(store, cache, &destination, &dependency.hash)?;
        }
        let source = std::fs::read_to_string(destination.join("Cargo.toml"))?;
        let crate_manifest: toml::Value = source
            .parse()
            .map_err(|error: toml::de::Error| BuildError::Rejected(error.to_string()))?;
        let package = crate_manifest
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| BuildError::Rejected("crate package name missing".into()))?;
        let version = crate_manifest
            .get("package")
            .and_then(|package| package.get("version"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| BuildError::Rejected("crate package version missing".into()))?;
        let identity = format!("{package}@{version}");
        if let Some(previous) = package_sources.insert(identity.clone(), dependency.hash.clone())
            && previous != dependency.hash
        {
            return Err(BuildError::Rejected(format!(
                "conflicting source hashes for {identity}: {previous} and {}",
                dependency.hash
            )));
        }
        let mut patch = toml::map::Map::new();
        patch.insert("path".into(), toml::Value::String(relative));
        patch.insert("package".into(), toml::Value::String(package.into()));
        crate_patches.insert(
            format!("loom-pin-{}", dependency.hash),
            toml::Value::Table(patch),
        );
        let Some(alias) = pin.alias else {
            continue;
        };
        let mut specification = toml::map::Map::new();
        specification.insert("version".into(), toml::Value::String(format!("={version}")));
        specification.insert("package".into(), toml::Value::String(package.into()));
        specification.insert(
            "features".into(),
            toml::Value::Array(
                dependency
                    .features
                    .into_iter()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
        specification.insert(
            "default-features".into(),
            toml::Value::Boolean(dependency.default_features),
        );
        let dependencies = table
            .entry("dependencies")
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .ok_or_else(|| BuildError::Rejected("dependencies must be a table".into()))?;
        if dependencies
            .insert(alias.clone(), toml::Value::Table(specification))
            .is_some()
        {
            return Err(BuildError::Rejected(format!(
                "crate alias {alias} declared twice"
            )));
        }
    }
    if !crate_patches.is_empty() {
        let mut patches = toml::map::Map::new();
        patches.insert("crates-io".into(), toml::Value::Table(crate_patches));
        table.insert("patch".into(), toml::Value::Table(patches));
    }
    table.remove("loom");
    table.insert("workspace".into(), toml::Value::Table(Default::default()));
    let package = table
        .get_mut("package")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| BuildError::Rejected("Cargo.toml requires [package]".into()))?;
    if package.contains_key("workspace")
        || package
            .get("metadata")
            .and_then(|metadata| metadata.get("component"))
            .is_some()
    {
        return Err(BuildError::Rejected("Workspace redirects and component metadata are host-owned; the guest boundary is core wasm".into()));
    }
    if !isolated && (package.contains_key("build") || files.contains_key("build.rs")) {
        return Err(BuildError::Rejected(
            "User build scripts require an isolated build worker, which is not configured".into(),
        ));
    }
    if dependency {
        package.insert(
            "name".into(),
            toml::Value::String(format!("loom-definition-{}", &definition.hash[..16])),
        );
    } else {
        package
            .entry("name")
            .or_insert_with(|| toml::Value::String("loom-definition".into()));
    }
    let library = table
        .entry("lib")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| BuildError::Rejected("[lib] must be a table".into()))?;
    if library.get("proc-macro").and_then(toml::Value::as_bool) == Some(true) {
        return Err(BuildError::Rejected(
            "A definition cannot be a procedural macro crate".into(),
        ));
    }
    library.insert("path".into(), toml::Value::String("src/lib.rs".into()));
    if dependency {
        // The driver finds a dependency's staged item document by rustc crate
        // name; a caller-supplied `[lib] name` would break that link silently.
        library.insert(
            "name".into(),
            toml::Value::String(crate::identity::dependency_crate_name(&definition.hash)),
        );
    }
    library.insert(
        "crate-type".into(),
        toml::Value::Array(vec![toml::Value::String(
            if dependency { "rlib" } else { "cdylib" }.into(),
        )]),
    );
    let features = table
        .entry("features")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| BuildError::Rejected("[features] must be a table".into()))?;
    features.insert("loom-dependency".into(), toml::Value::Array(Vec::new()));
    let manifest_deps = table
        .entry("dependencies")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| BuildError::Rejected("[dependencies] must be a table".into()))?;
    for (name, value) in manifest_deps.iter() {
        if crate_aliases.contains(name) {
            continue;
        }
        if value.get("path").is_some()
            || value.get("git").is_some()
            || value.get("registry").is_some()
        {
            return Err(BuildError::Rejected(format!(
                "dependency {name}: use loom.deps hashes or locked crates.io sources"
            )));
        }
        if !isolated && !trusted_dependency(name, value) {
            return Err(BuildError::Rejected(format!(
                "dependency {name} requires the isolated vendored build worker, which is not configured"
            )));
        }
    }
    let mut guest_dependency = toml::map::Map::new();
    guest_dependency.insert(
        "package".into(),
        toml::Value::String("loom-guest-rs".into()),
    );

    guest_dependency.insert(
        "path".into(),
        toml::Value::String(
            root.join("crates/loom-guest-rs")
                .to_string_lossy()
                .into_owned(),
        ),
    );
    manifest_deps.insert("loom".into(), toml::Value::Table(guest_dependency));
    for (name, hash) in &definition.deps {
        if !dependencies.contains_key(hash) {
            return Err(BuildError::Rejected(format!(
                "missing dependency source {hash}"
            )));
        }
        let mut entry = toml::map::Map::new();
        entry.insert(
            "package".into(),
            toml::Value::String(format!("loom-definition-{}", &hash[..16])),
        );
        entry.insert(
            "path".into(),
            toml::Value::String(
                cache
                    .join("sources")
                    .join(hash)
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
        entry.insert(
            "features".into(),
            toml::Value::Array(vec![toml::Value::String("loom-dependency".into())]),
        );
        manifest_deps.insert(name.clone(), toml::Value::Table(entry));
    }
    if files
        .keys()
        .any(|name| name.starts_with("vendor/") || name == preparation::VENDOR_TREE)
    {
        files.insert(
            ".cargo/config.toml".into(),
            SourceFile::Text(VENDOR_CONFIG.into()),
        );
    } else if files.keys().any(|name| name.starts_with(".cargo/")) {
        return Err(BuildError::Rejected(
            "Caller cargo configuration is forbidden".into(),
        ));
    }
    fs::create_dir_all(directory).await?;
    if let Some(tree) = files.get(preparation::VENDOR_TREE) {
        let hash = tree
            .as_text()
            .ok_or_else(|| BuildError::Rejected("vendor tree must be a hash".into()))?;
        preparation::materialize_tree(store, cache, &directory.join("vendor"), hash)?;
    }
    for (name, source) in files {
        let path = directory.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(path, source.bytes().map_err(BuildError::Rejected)?).await?;
    }
    fs::write(
        directory.join("Cargo.toml"),
        toml::to_string(&manifest).map_err(|error| BuildError::Rejected(error.to_string()))?,
    )
    .await?;
    Ok(())
}
