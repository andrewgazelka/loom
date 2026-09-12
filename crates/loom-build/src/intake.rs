use super::*;

impl Builder {
    /// Resolve the Rust dependency lock before assigning the executable identity.
    /// Untrusted dependencies are fetched in the network-only sandbox phase.
    pub async fn prepare_rust_source(
        &self,
        definition: &CheckedDef,
        dependencies: &BTreeMap<String, CheckedDef>,
    ) -> Result<String, BuildError> {
        if !definition.diagnostics.is_empty() {
            return Err(BuildError::Rejected("definition has diagnostics".into()));
        }
        if definition.hash.len() != 64
            || !definition.hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(BuildError::Rejected("invalid definition hash".into()));
        }
        if is_vendored(definition) {
            return Ok(definition.source.clone());
        }
        let _guard = self.gate.lock().await;
        let mut bundle = if definition.source.trim_start().starts_with('{') {
            let bundle: SourceBundle = serde_json::from_str(&definition.source)
                .map_err(|error| BuildError::Rejected(error.to_string()))?;
            bundle.validate().map_err(BuildError::Rejected)?;
            bundle
        } else {
            let mut files = BTreeMap::new();
            files.insert(
                "src/lib.rs".into(),
                SourceFile::Text(definition.source.clone()),
            );
            files.insert("Cargo.toml".into(),SourceFile::Text("[package]\nname=\"loom-definition\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nserde={version=\"1\",features=[\"derive\"]}\nserde_json=\"1\"\n".into()));
            SourceBundle { files }
        };
        if bundle.files.keys().any(|name| name.starts_with(".cargo/")) {
            return Err(BuildError::Rejected(
                "Caller Cargo configuration is forbidden".into(),
            ));
        }
        let manifest = bundle
            .files
            .get("Cargo.toml")
            .and_then(SourceFile::as_text)
            .ok_or_else(|| BuildError::Rejected("Cargo.toml must be UTF-8".into()))?
            .parse::<toml::Value>()
            .map_err(|error| BuildError::Rejected(error.to_string()))?;
        fn untrusted(value: &toml::Value) -> bool {
            value.as_table().is_some_and(|table| {
                table.iter().any(|(key, value)| {
                    if ["dependencies", "build-dependencies", "dev-dependencies"]
                        .contains(&key.as_str())
                    {
                        value.as_table().is_some_and(|deps| {
                            deps.iter()
                                .any(|(name, value)| !trusted_dependency(name, value))
                        })
                    } else {
                        untrusted(value)
                    }
                })
            })
        }
        let isolated = manifest
            .get("loom")
            .and_then(|loom| loom.get("crates"))
            .is_some()
            || untrusted(&manifest)
            || bundle.files.contains_key("build.rs")
            || manifest
                .get("package")
                .is_some_and(|package| package.get("build").is_some())
            || dependencies.values().any(is_vendored);
        if !isolated {
            prepare_compiler_dependencies(&self.root).await?;
        }
        let preparation_inputs = serde_json::json!({
            "contract": "loom-preparation-v2-registry-identity",
            "manifest": manifest,
            "lock": bundle.files.get("Cargo.lock"),
            "definitions": definition.deps,
            "sdk": build_fingerprint(&self.root)?,
            "isolated": isolated,
        });
        let preparation_key = blake3::hash(
            &serde_json::to_vec(&preparation_inputs)
                .map_err(|error| BuildError::Rejected(error.to_string()))?,
        )
        .to_hex()
        .to_string();
        if let Some(overlay) = preparation::load(&self.store, &preparation_key)? {
            bundle.files.extend(overlay);
            bundle.validate().map_err(BuildError::Rejected)?;
            return serde_json::to_string(&bundle)
                .map_err(|error| BuildError::Rejected(error.to_string()));
        }
        let staging = self.cache.join("intake").join(&definition.hash);
        if staging.exists() {
            fs::remove_dir_all(&staging).await?;
        }
        let crate_dir = staging.join("crate");
        fs::create_dir_all(&crate_dir).await?;
        for dependency in dependencies.values() {
            if dependency.hash.len() != 64
                || !dependency.hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(BuildError::Rejected("invalid dependency hash".into()));
            }
            materialize_rust(Materialization {
                store: &self.store,
                root: &self.root,
                cache: &staging,
                directory: &staging.join("sources").join(&dependency.hash),
                definition: dependency,
                dependencies,
                dependency: true,
                isolated,
            })
            .await?;
        }
        materialize_rust(Materialization {
            store: &self.store,
            root: &self.root,
            cache: &staging,
            directory: &crate_dir,
            definition,
            dependencies,
            dependency: false,
            isolated,
        })
        .await?;
        let mut command = if isolated {
            let mut command = Command::new(self.root.join("rustc/sandbox.sh"));
            command
                .arg("vendor")
                .env("LOOM_RUST_TARGET", "wasm32-unknown-unknown")
                .arg(&staging)
                .arg(&crate_dir)
                .arg(staging.join("target"))
                .arg(&self.root);
            command
        } else {
            // Only the fixed repository guest + serde graph reaches this path.
            // Seed from its checked-in lock; metadata updates path package entries
            // without running any dependency build scripts.
            if !crate_dir.join("Cargo.lock").exists() {
                seed_build_lock(&self.root.join("Cargo.lock"), &crate_dir.join("Cargo.lock"))
                    .await?;
            }
            let toolchain = crate::resolve_guest_toolchain(&self.root).await?;
            let mut command = Command::new(&toolchain.cargo);
            toolchain.configure(&mut command)?;
            command
                .args(["metadata", "--format-version=1"])
                .current_dir(&crate_dir);
            command
        };
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            command.kill_on_drop(true).output(),
        )
        .await
        .map_err(|_| {
            BuildError::Rejected("Rust dependency preparation exceeded 300 seconds".into())
        })??;
        if !output.status.success() {
            return Err(BuildError::Rejected(format!(
                "dependency preparation: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        bundle.files.insert(
            "Cargo.lock".into(),
            SourceFile::from_bytes(fs::read(crate_dir.join("Cargo.lock")).await?),
        );
        if isolated {
            let hash = registry::snapshot_directory(&self.store, &crate_dir.join("vendor"))
                .map_err(|error| BuildError::Rejected(error.to_string()))?;
            bundle
                .files
                .insert(preparation::VENDOR_TREE.into(), SourceFile::Text(hash));
        }
        preparation::save(&self.store, &preparation_key, &bundle.files)?;
        bundle.validate().map_err(BuildError::Rejected)?;
        let result = serde_json::to_string(&bundle)
            .map_err(|error| BuildError::Rejected(error.to_string()))?;
        fs::remove_dir_all(staging).await?;
        Ok(result)
    }
}
