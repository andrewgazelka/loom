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
        let PreparationInputs {
            mut bundle,
            isolated,
            key: preparation_key,
        } = self.preparation_inputs(definition, dependencies)?;
        if !isolated {
            let (toolchain, _) = self.prepared().await?;
            prepare_compiler_dependencies(&self.compiler_dependencies, &toolchain).await?;
        }
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
            let (toolchain, _) = self.prepared().await?;
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

/// The cached resolver overlay a rebuild of a stored definition would reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preparation {
    /// BLAKE3 over the manifest, lock, dependency pins, SDK fingerprint and
    /// isolation flag; the `rust_preparations` row key.
    pub key: String,
    /// DAG-CBOR object holding `Cargo.lock` and, for isolated builds, the
    /// `loom.vendor-tree` hash.
    pub overlay_hash: String,
}

struct PreparationInputs {
    bundle: SourceBundle,
    isolated: bool,
    key: String,
}

impl Builder {
    /// Look up the resolver overlay for `definition` without resolving anything.
    /// `None` when the definition carries its own vendor tree (no overlay is
    /// ever written for it) or when this node has not resolved it yet.
    pub fn preparation(
        &self,
        definition: &CheckedDef,
        dependencies: &BTreeMap<String, CheckedDef>,
    ) -> Result<Option<Preparation>, BuildError> {
        if is_vendored(definition) {
            return Ok(None);
        }
        let inputs = self.preparation_inputs(definition, dependencies)?;
        Ok(
            preparation::overlay_hash(&self.store, &inputs.key)?.map(|overlay_hash| Preparation {
                key: inputs.key,
                overlay_hash,
            }),
        )
    }

    /// The `rust_preparations` row key this node derives for `definition`:
    /// BLAKE3 over the manifest, lock, dependency pins, SDK fingerprint and
    /// isolation flag. `None` when the definition carries its own vendor tree.
    /// Import compares a bundle's claimed key against this before seeding.
    pub fn preparation_key(
        &self,
        definition: &CheckedDef,
        dependencies: &BTreeMap<String, CheckedDef>,
    ) -> Result<Option<String>, BuildError> {
        if is_vendored(definition) {
            return Ok(None);
        }
        Ok(Some(self.preparation_inputs(definition, dependencies)?.key))
    }

    fn preparation_inputs(
        &self,
        definition: &CheckedDef,
        dependencies: &BTreeMap<String, CheckedDef>,
    ) -> Result<PreparationInputs, BuildError> {
        let bundle = if definition.source.trim_start().starts_with('{') {
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
            files.insert(
                "Cargo.toml".into(),
                SourceFile::Text(DEFAULT_MANIFEST.into()),
            );
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
        let preparation_inputs = serde_json::json!({
            "contract": "loom-preparation-v2-registry-identity",
            "manifest": manifest,
            "lock": bundle.files.get("Cargo.lock"),
            "definitions": definition.deps,
            "sdk": build_fingerprint(&self.root)?,
            "isolated": isolated,
        });
        let key = blake3::hash(
            &serde_json::to_vec(&preparation_inputs)
                .map_err(|error| BuildError::Rejected(error.to_string()))?,
        )
        .to_hex()
        .to_string();
        Ok(PreparationInputs {
            bundle,
            isolated,
            key,
        })
    }
}

/// Manifest assumed for a bare `src/lib.rs` definition; mirrors materialize.
const DEFAULT_MANIFEST: &str = "[package]\nname=\"loom-definition\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nserde={version=\"1\",features=[\"derive\"]}\nserde_json=\"1\"\n";
