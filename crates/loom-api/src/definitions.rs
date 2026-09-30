use super::*;

impl Service {
    #[cfg(test)]
    pub(super) async fn define(&self, request: DefineRequest) -> Response {
        if let Err(error) = self.access.require(Scope::Define) {
            return self.response(Err(error));
        }
        self.define_authorized(request).await
    }
    #[cfg(test)]
    pub(super) async fn define_authorized(&self, request: DefineRequest) -> Response {
        let _guard = self.definitions_gate.lock().await;
        match self.define_inner(request).await {
            Ok(response) => response,
            Err(error) => self.response(Err(error)),
        }
    }
    /// Admit one definition through the `add` path: resolve dependency pins,
    /// check, prepare, build and publish. `import` reuses this per definition.
    pub(super) async fn admit(
        &self,
        mut request: DefineRequest,
        destination: Destination,
    ) -> Result<Response> {
        request.deps = resolve_dependency_pins(&self.store, &request.deps)?;
        match destination {
            Destination::Live => self.define_inner(request).await,
            Destination::Staged => self.define_update_node(request).await,
        }
    }
    pub(super) async fn define_inner(&self, request: DefineRequest) -> Result<Response> {
        let progress = self.build_progress.start(&request.name);
        let mut intake = self.clone();
        intake.store = self.store.stage_intake()?;
        intake.builder = Arc::new(self.builder.for_store(intake.store.clone()));
        intake
            .define_staged(
                request,
                Some(&self.store),
                &progress,
                true,
                loom_build::BuildProfile::Standard,
            )
            .await
    }
    /// Compile one node directly into an already private staged store; update
    /// sessions and bundle imports publish the staged graph as one transaction.
    pub(super) async fn define_update_node(&self, request: DefineRequest) -> Result<Response> {
        let progress = self.build_progress.start(&request.name);
        self.define_staged(
            request,
            None,
            &progress,
            true,
            loom_build::BuildProfile::Standard,
        )
        .await
    }
    /// Compile one definition and keep it in memory only (`bind_name` false):
    /// no name, no staged copy of the store, no durable rows. `eval`'s cells are
    /// addressed by hash until they are dropped; `add` keeps one.
    pub(super) async fn define_ephemeral(
        &self,
        request: DefineRequest,
        profile: loom_build::BuildProfile,
    ) -> Result<Response> {
        let progress = self.build_progress.start(&request.name);
        self.define_staged(request, None, &progress, false, profile)
            .await
    }

    async fn define_staged(
        &self,
        mut request: DefineRequest,
        destination: Option<&Store>,
        progress: &build_progress::BuildGuard,
        bind_name: bool,
        profile: loom_build::BuildProfile,
    ) -> Result<Response> {
        progress.stage("check");
        ensure!(
            self.languages.contains(&request.lang),
            "language {} is disabled",
            request.lang.as_str()
        );
        ensure!(request.source.len() <= 16 * 1024 * 1024, "source too large");
        if request.lang.is_v8() {
            return self.define_javascript(request, destination, progress).await;
        }
        if let Some(reference) = source_reference(&request.source) {
            let bundle = self
                .store
                .get(reference)?
                .with_context(|| format!("Rust source bundle {reference} not found"))?;
            request.source = decode_source_bundle(&bundle)?;
        }
        for hash in request.deps.values_mut() {
            *hash = self
                .store
                .resolve(hash)?
                .with_context(|| format!("dependency {hash} not found"))?
                .hash;
        }
        let mut checked = self.check_definition(&request).await?;
        if !checked.diagnostics.is_empty() {
            return Ok(Response {
                ok: false,
                seq: self.store.latest_seq()?,
                result: Value::Null,
                diagnostics: checked.diagnostics,
            });
        }
        let dependencies = dependency_closure(&self.store, &checked.deps)?;
        // Invalid source or dependencies are admission errors even on a host
        // without the Rust toolchain; report them before compiler setup.
        progress.stage("preflight");
        self.builder.preflight().await?;
        progress.stage("compile");
        let built = self
            .builder
            .build_with_profile(&checked, &dependencies, profile)
            .await?;
        if !built.diagnostics.is_empty() {
            return Ok(Response {
                ok: false,
                seq: self.store.latest_seq()?,
                result: json!({"build":{"ms":built.ms,"logs":built.logs,"rustc_invocations":built.rustc_invocations}}),
                diagnostics: built.diagnostics,
            });
        }
        ensure!(
            !built.component.is_empty(),
            "builder returned an empty component"
        );
        progress.stage("publish");
        let identity = built
            .identity
            .as_ref()
            .context("builder returned no item identity")?;
        let item_json = self
            .store
            .get(&identity.item_hashes_ref)?
            .context("driver item document missing")?;
        checked.apply_driver_effects_json(std::str::from_utf8(&item_json)?)?;
        if !checked.diagnostics.is_empty() {
            return Ok(Response {
                ok: false,
                seq: self.store.latest_seq()?,
                result: Value::Null,
                diagnostics: checked.diagnostics,
            });
        }
        if !bind_name {
            // An `eval` cell: executable from memory, nothing durable (see
            // `loom_store::Store::install_transient`).
            let component_hash = loom_store::content_hash(&built.component);
            let size = built.component.len();
            let def = Def {
                allowed_effects: request.allowed_effects.clone(),
                observed_effects: Vec::new(),
                hash: identity.behavior_hash.clone(),
                lang: checked.lang,
                component_hash: Some(component_hash.clone()),
                sig: checked.sig,
            };
            self.store.install_transient(def.clone(), built.component)?;
            return Ok(Response {
                ok: true,
                seq: self.store.latest_seq()?,
                diagnostics: Vec::new(),
                result: json!({"def":def,"build":{"ms":built.ms,"component_hash":component_hash,"size":size,"rustc_invocations":built.rustc_invocations,
                    "driver_timing":built.logs.lines().filter(|line| line.contains("driver-timing")).collect::<Vec<_>>(),
                    "stages":built.logs.lines().find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok().and_then(|v| v.get("build_stages").cloned()))}}),
            });
        }
        let component_hash = self.store.put("component", &built.component)?;
        let compiled = built
            .compiled_source
            .as_deref()
            .context("builder returned a component without its compiled text")?;
        self.store
            .record_compiled_source(&component_hash, compiled)?;
        let logs_ref = self.store.put("blob", built.logs.as_bytes())?;
        let def = Def {
            allowed_effects: request.allowed_effects.clone(),
            observed_effects: Vec::new(),
            hash: identity.behavior_hash.clone(),
            lang: checked.lang,
            component_hash: Some(component_hash.clone()),
            sig: checked.sig,
        };
        let build_event = json!({"type":"component_built","component_hash":component_hash,"logs_ref":logs_ref,"ms":built.ms,"size":built.component.len(),"rustc_invocations":built.rustc_invocations});
        let published = if let Some(destination) = destination {
            destination.commit_intake(
                &self.store,
                loom_store::IntakePublication {
                    def: &def,
                    name: Some(&request.name),
                    source: &request.source,
                    deps: &checked.deps,
                    identity: Some(identity),
                    build_event: &build_event,
                },
            )?;
            destination
        } else {
            self.store.define_with_identity(
                &def,
                Some(request.name.as_str()),
                &request.source,
                &checked.deps,
                Some(identity),
            )?;
            &self.store
        };
        Ok(Response {
            ok: true,
            seq: published.latest_seq()?,
            diagnostics: Vec::new(),
            result: json!({"def":def,"build":{"ms":built.ms,"component_hash":component_hash,"size":built.component.len(),"logs_ref":logs_ref,"rustc_invocations":built.rustc_invocations}}),
        })
    }

    pub(super) async fn check_definition(
        &self,
        request: &DefineRequest,
    ) -> Result<loom_check::CheckedDef> {
        let checked = self
            .checker
            .check_with_signatures(request, &dependency_signatures(&self.store, &request.deps)?)
            .await?;
        if !checked.diagnostics.is_empty() {
            return Ok(checked);
        }
        let dependencies = dependency_closure(&self.store, &checked.deps)?;
        let source = self
            .builder
            .prepare_rust_source(&checked, &dependencies)
            .await?;
        let prepared = DefineRequest {
            allowed_effects: request.allowed_effects.clone(),
            lang: Lang::Rust,
            name: request.name.clone(),
            source,
            deps: checked.deps,
        };
        Ok(self
            .checker
            .check_with_signatures(
                &prepared,
                &dependency_signatures(&self.store, &prepared.deps)?,
            )
            .await?)
    }
}

/// The definition source `eval` and `add` submit. With a `manifest` argument the
/// Rust `source` becomes `src/lib.rs` of a source bundle beside that
/// `Cargo.toml` and, when given, the `lock` as `Cargo.lock`; a plain crates.io
/// dependency is admitted on the host when the lock pins it (see
/// `loom_build`'s intake). Without `manifest` the source is used as written.
pub(super) fn with_crates(args: &Value, lang: Lang, source: String) -> Result<String> {
    fn text<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>> {
        args.get(name)
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_str()
                    .with_context(|| format!("{name} must be a string"))
            })
            .transpose()
    }
    let (manifest, lock) = (text(args, "manifest")?, text(args, "lock")?);
    let Some(manifest) = manifest else {
        ensure!(
            lock.is_none(),
            "lock needs manifest: a Cargo.lock pins the dependencies a Cargo.toml names"
        );
        return Ok(source);
    };
    ensure!(
        lang == Lang::Rust,
        "manifest and lock apply to Rust definitions only"
    );
    let mut files = serde_json::Map::new();
    files.insert("src/lib.rs".into(), Value::String(source));
    files.insert("Cargo.toml".into(), Value::String(manifest.into()));
    if let Some(lock) = lock {
        files.insert("Cargo.lock".into(), Value::String(lock.into()));
    }
    Ok(json!({ "files": files }).to_string())
}

/// Where an admitted definition is published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    /// Stage privately, then commit into this service's live store.
    Live,
    /// This service's store is already a private snapshot; write into it.
    Staged,
}

/// Turn every dependency pin into a definition hash. A 64-character hexadecimal
/// value must be a stored definition; anything else is a definition name and
/// resolves to its current hash. Every transport sends the same `deps`, so the
/// CLI never resolves names itself.
pub(super) fn resolve_dependency_pins(
    store: &Store,
    deps: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut resolved = BTreeMap::new();
    let names = store.current_names()?;
    for (alias, target) in deps {
        ensure!(!alias.is_empty(), "dependency alias is empty");
        let hash = if target.len() == 64 && target.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            ensure!(
                store.definition(target)?.is_some(),
                "dependency {alias}: definition {target} not found"
            );
            target.clone()
        } else {
            names
                .get(target)
                .cloned()
                .with_context(|| format!("dependency {alias}: name {target:?} not found"))?
        };
        resolved.insert(alias.clone(), hash);
    }
    Ok(resolved)
}

#[cfg(test)]
mod crate_tests {
    use super::*;

    #[test]
    fn a_manifest_makes_the_source_a_bundle_and_a_lock_needs_a_manifest() -> Result<()> {
        let source = "pub fn f() -> i64 { 1 }".to_owned();
        // No manifest: the source is used as written, null counting as absent.
        for args in [json!({}), json!({"manifest":null,"lock":null})] {
            assert_eq!(with_crates(&args, Lang::Rust, source.clone())?, source);
        }
        let bundle = with_crates(
            &json!({"manifest":"[package]\nname='c'","lock":"version = 4\n"}),
            Lang::Rust,
            source.clone(),
        )?;
        let bundle: Value = serde_json::from_str(&bundle)?;
        assert_eq!(
            bundle,
            json!({"files":{
                "src/lib.rs": source,
                "Cargo.toml": "[package]\nname='c'",
                "Cargo.lock": "version = 4\n",
            }})
        );
        // A manifest alone carries no Cargo.lock entry.
        let unlocked = with_crates(
            &json!({"manifest":"[package]\nname='c'"}),
            Lang::Rust,
            source.clone(),
        )?;
        assert!(serde_json::from_str::<Value>(&unlocked)?["files"]["Cargo.lock"].is_null());
        for (args, lang, expect) in [
            (
                json!({"lock":"version = 4\n"}),
                Lang::Rust,
                "lock needs manifest",
            ),
            (
                json!({"manifest":"[package]"}),
                Lang::TypeScript,
                "Rust definitions only",
            ),
            (
                json!({"manifest":7}),
                Lang::Rust,
                "manifest must be a string",
            ),
        ] {
            let error = with_crates(&args, lang, source.clone())
                .unwrap_err()
                .to_string();
            assert!(error.contains(expect), "{expect}: {error}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    #[test]
    fn pins_take_hashes_as_is_and_names_by_current_hash() -> Result<()> {
        let store = Store::memory()?;
        let hash = store.put("item-preimage", b"util entry")?;
        let definition = Def {
            hash: hash.clone(),
            lang: Lang::Rust,
            component_hash: None,
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        };
        store.define(
            &definition,
            Some("util"),
            "pub fn twice() {}",
            &BTreeMap::new(),
        )?;
        let resolved = resolve_dependency_pins(
            &store,
            &BTreeMap::from([
                ("a".to_owned(), "util".to_owned()),
                ("b".to_owned(), hash.clone()),
            ]),
        )?;
        assert_eq!(resolved["a"], hash);
        assert_eq!(resolved["b"], hash);
        let missing = resolve_dependency_pins(
            &store,
            &BTreeMap::from([("util".to_owned(), "absent".to_owned())]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            missing.contains("dependency util") && missing.contains("\"absent\""),
            "{missing}"
        );
        let unknown_hash = "0".repeat(64);
        let error = resolve_dependency_pins(
            &store,
            &BTreeMap::from([("util".to_owned(), unknown_hash.clone())]),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(&unknown_hash), "{error}");
        Ok(())
    }
}
