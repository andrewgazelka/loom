use super::*;

impl Builder {
    /// Resolve the Rust dependency lock before assigning the executable identity.
    /// Untrusted dependencies are fetched in the network-only sandbox phase,
    /// except plain crates.io dependencies pinned by a caller-supplied
    /// `Cargo.lock` (see `validate_registry_lock`): the host toolchain resolves
    /// those itself and verifies each archive against the lock's checksum.
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
            locked_registry,
            pinned,
            lock_hint,
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
                locked_registry,
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
            locked_registry,
        })
        .await?;
        // The caller's lock plus the SDK graph from the workspace lock; cargo may
        // only add to it.
        let supplied_lock = if pinned {
            let seeded = sdk::seed_sdk_graph(
                &fs::read(crate_dir.join("Cargo.lock")).await?,
                &fs::read(self.root.join("Cargo.lock")).await?,
            )?;
            fs::write(crate_dir.join("Cargo.lock"), &seeded).await?;
            Some(seeded)
        } else {
            None
        };
        let output = if isolated {
            let mut command = Command::new(self.root.join("rustc/sandbox.sh"));
            command
                .arg("vendor")
                .env("LOOM_RUST_TARGET", "wasm32-unknown-unknown")
                .arg(&staging)
                .arg(&crate_dir)
                .arg(staging.join("target"))
                .arg(&self.root);
            run_preparation(command).await?
        } else {
            // No dependency build script or procedural macro runs here: metadata
            // resolves and downloads, and the build refuses such packages later
            // (`direct::admission::graph_shareable`). Without a supplied lock only
            // the fixed repository guest + serde graph reaches this path, seeded
            // from the checked-in lock; metadata updates path package entries.
            if !crate_dir.join("Cargo.lock").exists() {
                seed_build_lock(&self.root.join("Cargo.lock"), &crate_dir.join("Cargo.lock"))
                    .await?;
            }
            let (toolchain, _) = self.prepared().await?;
            let metadata = |offline: bool| -> Result<Command, BuildError> {
                let mut command = Command::new(&toolchain.cargo);
                if supplied_lock.is_some() {
                    direct::compiler_environment(&mut command);
                }
                toolchain.configure(&mut command)?;
                command
                    .args(["metadata", "--format-version=1"])
                    .current_dir(&crate_dir);
                if offline {
                    command.arg("--offline");
                }
                Ok(command)
            };
            let offline = supplied_lock.is_some();
            let output = run_preparation(metadata(offline)?).await?;
            if offline
                && !output.status.success()
                && String::from_utf8_lossy(&output.stderr).contains("offline")
            {
                // A crate archive or index entry is missing from the local cargo
                // registry cache. Downloading is the daemon operator's choice.
                if std::env::var_os("LOOM_ALLOW_CARGO_FETCH").is_some_and(|value| value == "1") {
                    run_preparation(metadata(false)?).await?
                } else {
                    return Err(BuildError::Rejected(format!(
                        "dependency preparation: a crates.io dependency is not in this host's cargo registry cache and downloads are off. Run `cargo fetch` for it on this host, or start the daemon with LOOM_ALLOW_CARGO_FETCH=1 so the first prepare may download it (the archive is still checked against the lock's checksum): {}",
                        String::from_utf8_lossy(&output.stderr)
                    )));
                }
            } else {
                output
            }
        };
        if !output.status.success() {
            let hint = if lock_hint {
                "\nCrates.io dependencies build with the host toolchain when the bundle carries a complete Cargo.lock: pass `lock` (with `manifest`) to eval or add"
            } else {
                ""
            };
            return Err(BuildError::Rejected(format!(
                "dependency preparation: {}{hint}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        if let Some(supplied) = supplied_lock {
            // Cargo has resolved the manifest; it must not have moved a pin.
            confirm_pins(
                &supplied,
                &fs::read(crate_dir.join("Cargo.lock")).await?,
                &fs::read_to_string(crate_dir.join("Cargo.toml")).await?,
            )?;
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
    /// The bundle's `Cargo.lock` passed `validate_registry_lock`, so plain
    /// crates.io dependencies are admitted without the isolated worker.
    locked_registry: bool,
    /// The host resolves crates.io dependencies from the supplied lock: the
    /// manifest has some, the lock passed, and nothing forces isolation.
    pinned: bool,
    /// The manifest has crates.io dependencies but the bundle has no lock.
    lock_hint: bool,
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
        fn any_dependency(value: &toml::Value, test: &dyn Fn(&str, &toml::Value) -> bool) -> bool {
            value.as_table().is_some_and(|table| {
                table.iter().any(|(key, value)| {
                    if ["dependencies", "build-dependencies", "dev-dependencies"]
                        .contains(&key.as_str())
                    {
                        value
                            .as_table()
                            .is_some_and(|deps| deps.iter().any(|(name, value)| test(name, value)))
                    } else {
                        any_dependency(value, test)
                    }
                })
            })
        }
        let lock = lock_verdict(&bundle);
        let locked_registry = matches!(lock, Some(Ok(())));
        let wants_registry = any_dependency(&manifest, &|name, value| {
            !trusted_dependency(name, value) && registry_dependency(name, value)
        });
        if wants_registry && let Some(Err(error)) = &lock {
            return Err(BuildError::Rejected(format!(
                "Cargo.lock cannot pin crates.io dependencies for the host build: {error}"
            )));
        }
        let untrusted = any_dependency(&manifest, &|name, value| {
            !trusted_dependency(name, value)
                && !(locked_registry && registry_dependency(name, value))
        });
        let isolated = manifest
            .get("loom")
            .and_then(|loom| loom.get("crates"))
            .is_some()
            || untrusted
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
            locked_registry,
            pinned: wants_registry && locked_registry && !isolated,
            lock_hint: wants_registry && lock.is_none(),
            key,
        })
    }
}

/// Manifest assumed for a bare `src/lib.rs` definition; mirrors materialize.
const DEFAULT_MANIFEST: &str = "[package]\nname=\"loom-definition\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nserde={version=\"1\",features=[\"derive\"]}\nserde_json=\"1\"\n";

/// The one registry a caller-supplied lock may name.
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// A caller-supplied `Cargo.lock` is trusted to pin host-resolved crates.io
/// dependencies only when every package that has a source is a crates.io
/// package with a full SHA-256 checksum (cargo rejects an archive whose hash
/// differs), and the lock carries no unused `[patch]` entry. Git and
/// alternative registries never qualify.
pub(super) fn validate_registry_lock(bytes: &[u8]) -> Result<(), BuildError> {
    let lock = sdk::Lock::parse(bytes)?;
    if lock
        .raw
        .get("patch")
        .and_then(|patch| patch.get("unused"))
        .is_some()
    {
        return Err(BuildError::Rejected(
            "Cargo.lock carries [[patch.unused]] entries; patches are unavailable".into(),
        ));
    }
    for package in &lock.packages {
        let key = &package.key;
        let Some(source) = &key.source else {
            continue;
        };
        if source != CRATES_IO {
            return Err(BuildError::Rejected(format!(
                "Cargo.lock package {} {} comes from {source}; only crates.io is admitted",
                key.name, key.version
            )));
        }
        let checksum = package.raw.get("checksum").and_then(toml::Value::as_str);
        if !checksum
            .is_some_and(|sum| sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(BuildError::Rejected(format!(
                "Cargo.lock package {} {} has no 64-hex SHA-256 checksum",
                key.name, key.version
            )));
        }
    }
    Ok(())
}

/// `None` when the bundle carries no lock.
fn lock_verdict(bundle: &SourceBundle) -> Option<Result<(), BuildError>> {
    let file = bundle.files.get("Cargo.lock")?;
    Some(match file.as_text() {
        Some(text) => validate_registry_lock(text.as_bytes()),
        None => Err(BuildError::Rejected("Cargo.lock must be UTF-8".into())),
    })
}

/// Whether `source` (a stored definition's text) is a bundle whose lock passes
/// `validate_registry_lock`. Build and replay recompute this from the prepared
/// bundle so they agree with preparation without recording a flag.
pub(super) fn locked_registry(source: &str) -> bool {
    serde_json::from_str::<SourceBundle>(source)
        .is_ok_and(|bundle| matches!(lock_verdict(&bundle), Some(Ok(()))))
}

/// After cargo resolved the manifest against the caller's lock: the lock it
/// wrote must still be crates.io only, keep every pin the caller supplied, and
/// add no registry package that only the caller's manifest reaches.
fn confirm_pins(supplied: &[u8], resolved: &[u8], manifest: &str) -> Result<(), BuildError> {
    let original = sdk::Lock::parse(supplied)?;
    let updated = sdk::Lock::parse(resolved)?;
    let manifest: toml::Value = manifest
        .parse()
        .map_err(|error: toml::de::Error| BuildError::Rejected(error.to_string()))?;
    sdk::validate_pins(&original, &updated, &manifest)?;
    sdk::validate_additions(&original, &updated)?;
    validate_registry_lock(resolved)
}

/// Run a dependency-resolution command with the 300 second preparation limit.
async fn run_preparation(mut command: Command) -> Result<std::process::Output, BuildError> {
    tokio::time::timeout(
        std::time::Duration::from_secs(300),
        command.kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| BuildError::Rejected("Rust dependency preparation exceeded 300 seconds".into()))?
    .map_err(BuildError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROBUST_SUM: &str = "4e27ee8bb91ca0adcf0ecb116293afa12d393f9c2b9b9cd54d33e8078fe19839";

    fn lock(packages: &str) -> String {
        format!("version = 4\n{packages}")
    }

    fn robust_lock() -> String {
        lock(&format!(
            "[[package]]\nname = \"robust\"\nversion = \"1.2.0\"\nsource = \"{CRATES_IO}\"\nchecksum = \"{ROBUST_SUM}\"\n"
        ))
    }

    #[test]
    fn a_lock_pins_host_resolved_crates_only_from_crates_io_with_checksums() {
        validate_registry_lock(robust_lock().as_bytes()).unwrap();
        // Path packages (the definition itself) carry no source and are fine.
        validate_registry_lock(
            lock("[[package]]\nname = \"cell\"\nversion = \"0.1.0\"\n").as_bytes(),
        )
        .unwrap();
        let rejected = |text: String, expect: &str| {
            let error = validate_registry_lock(text.as_bytes())
                .unwrap_err()
                .to_string();
            assert!(error.contains(expect), "{expect}: {error}");
        };
        rejected(
            lock(
                "[[package]]\nname = \"robust\"\nversion = \"1.2.0\"\nsource = \"git+https://example.com/robust#abc\"\n",
            ),
            "only crates.io",
        );
        rejected(
            lock(
                "[[package]]\nname = \"robust\"\nversion = \"1.2.0\"\nsource = \"registry+https://mirror.example/index\"\nchecksum = \"4e27ee8bb91ca0adcf0ecb116293afa12d393f9c2b9b9cd54d33e8078fe19839\"\n",
            ),
            "only crates.io",
        );
        rejected(
            lock(&format!(
                "[[package]]\nname = \"robust\"\nversion = \"1.2.0\"\nsource = \"{CRATES_IO}\"\n"
            )),
            "checksum",
        );
        rejected(
            lock(&format!(
                "[[package]]\nname = \"robust\"\nversion = \"1.2.0\"\nsource = \"{CRATES_IO}\"\nchecksum = \"abc\"\n"
            )),
            "checksum",
        );
        rejected(
            format!(
                "{}\n[[patch.unused]]\nname = \"robust\"\nversion = \"1.2.0\"\n",
                robust_lock()
            ),
            "patch.unused",
        );
        rejected("not = [toml".into(), "");
    }

    #[test]
    fn a_supplied_lock_that_passes_makes_registry_dependencies_host_built() {
        let builder = Builder::new(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            loom_store::Store::memory().unwrap(),
        );
        let definition = |manifest: &str, lock: Option<String>| {
            let mut files = BTreeMap::from([
                (
                    "src/lib.rs".to_owned(),
                    SourceFile::Text("pub fn main() -> i64 { 1 }".into()),
                ),
                ("Cargo.toml".to_owned(), SourceFile::Text(manifest.into())),
            ]);
            if let Some(lock) = lock {
                files.insert("Cargo.lock".into(), SourceFile::Text(lock));
            }
            CheckedDef {
                hash: "a".repeat(64),
                lang: Lang::Rust,
                name: "cell".into(),
                source: serde_json::to_string(&SourceBundle { files }).unwrap(),
                deps: BTreeMap::new(),
                sig: Default::default(),
                diagnostics: vec![],
            }
        };
        let manifest = "[package]\nname='cell'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nrobust='=1.2.0'\n";
        let none = BTreeMap::new();

        let locked = builder
            .preparation_inputs(&definition(manifest, Some(robust_lock())), &none)
            .unwrap();
        assert!(locked.locked_registry && locked.pinned && !locked.isolated && !locked.lock_hint);
        assert!(locked_registry(
            &definition(manifest, Some(robust_lock())).source
        ));

        // No lock: today's behaviour, the isolated worker, with a hint to supply one.
        let unlocked = builder
            .preparation_inputs(&definition(manifest, None), &none)
            .unwrap();
        assert!(unlocked.isolated && !unlocked.pinned && unlocked.lock_hint);
        assert!(!locked_registry(&definition(manifest, None).source));

        // A lock that fails validation is an early error, not a silent fallback.
        let bad = lock(
            "[[package]]\nname = \"robust\"\nversion = \"1.2.0\"\nsource = \"git+https://example.com/r#a\"\n",
        );
        let error = builder
            .preparation_inputs(&definition(manifest, Some(bad)), &none)
            .err()
            .expect("git lock is refused")
            .to_string();
        assert!(error.contains("Cargo.lock cannot pin"), "{error}");

        // Path, git and registry-table dependencies never qualify, lock or not.
        for dependency in [
            "robust={path='../robust'}",
            "robust={git='https://example.com/robust'}",
            "robust={version='1',registry='other'}",
        ] {
            let manifest =
                format!("[package]\nname='cell'\nversion='0.1.0'\n[dependencies]\n{dependency}\n");
            let inputs = builder
                .preparation_inputs(&definition(&manifest, Some(robust_lock())), &none)
                .unwrap();
            assert!(inputs.isolated && !inputs.pinned, "{dependency}");
        }
        // Only the SDK's own dependencies: no lock needed, nothing pinned.
        let trusted = builder
            .preparation_inputs(&definition(DEFAULT_MANIFEST, None), &none)
            .unwrap();
        assert!(!trusted.isolated && !trusted.pinned && !trusted.lock_hint);
    }

    #[test]
    fn cargo_may_add_sdk_packages_but_not_move_or_add_caller_pins() {
        let manifest = "[package]\nname='cell'\n[dependencies]\nrobust='=1.2.0'\nloom={package='loom-guest-rs',path='/sdk'}\n";
        let path = |name: &str, dependencies: &str| {
            format!("[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\n{dependencies}")
        };
        let registry = |name: &str, version: &str, sum: &str| {
            format!(
                "[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"{CRATES_IO}\"\nchecksum = \"{sum}\"\n"
            )
        };
        let supplied = lock(&registry("robust", "1.2.0", ROBUST_SUM));
        let serde_sum = "a".repeat(64);
        // Cargo adds the cell, the SDK and the SDK's serde: allowed.
        let resolved = lock(&format!(
            "{}{}{}{}",
            path("cell", "dependencies = [\"loom-guest-rs\", \"robust\"]\n"),
            path("loom-guest-rs", "dependencies = [\"serde\"]\n"),
            registry("robust", "1.2.0", ROBUST_SUM),
            registry("serde", "1.0.0", &serde_sum),
        ));
        confirm_pins(supplied.as_bytes(), resolved.as_bytes(), manifest).unwrap();
        // A moved checksum is refused.
        let moved = resolved.replace(ROBUST_SUM, &"b".repeat(64));
        assert!(confirm_pins(supplied.as_bytes(), moved.as_bytes(), manifest).is_err());
        // A caller-reachable registry package the lock did not pin is refused.
        let unpinned = lock(&format!(
            "{}{}{}{}",
            path(
                "cell",
                "dependencies = [\"loom-guest-rs\", \"robust\", \"extra\"]\n"
            ),
            path("loom-guest-rs", ""),
            registry("robust", "1.2.0", ROBUST_SUM),
            registry("extra", "1.0.0", &serde_sum),
        ));
        let error = confirm_pins(supplied.as_bytes(), unpinned.as_bytes(), manifest)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not pin extra"), "{error}");
        // A pin cargo dropped (the lock named a version the manifest no longer wants).
        let dropped = lock(&format!(
            "{}{}",
            path("cell", "dependencies = [\"loom-guest-rs\"]\n"),
            path("loom-guest-rs", "")
        ));
        assert!(confirm_pins(supplied.as_bytes(), dropped.as_bytes(), manifest).is_err());
    }
}
