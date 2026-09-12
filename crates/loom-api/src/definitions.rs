use super::*;

impl Service {
    pub async fn define(&self, request: DefineRequest) -> Response {
        if let Err(error) = self.access.require(Scope::Define) {
            return self.response(Err(error));
        }
        self.define_authorized(request).await
    }
    pub(super) async fn define_authorized(&self, request: DefineRequest) -> Response {
        let _guard = self.definitions_gate.lock().await;
        match self.define_inner(request).await {
            Ok(response) => response,
            Err(error) => self.response(Err(error)),
        }
    }
    pub(super) async fn define_inner(&self, mut request: DefineRequest) -> Result<Response> {
        ensure!(
            self.languages.contains(&request.lang),
            "language {} is disabled",
            request.lang.as_str()
        );
        ensure!(request.source.len() <= 16 * 1024 * 1024, "source too large");
        if let Some(reference) = source_reference(&request.source) {
            let bundle = self
                .store
                .get(reference)?
                .context("Rust source bundle not found")?;
            request.source = decode_source_bundle(&bundle)?;
        }
        for hash in request.deps.values_mut() {
            *hash = self
                .store
                .resolve(hash)?
                .context("dependency not found")?
                .hash;
        }
        let checked = self.check_definition(&request).await?;
        if !checked.diagnostics.is_empty() {
            return Ok(Response {
                ok: false,
                seq: self.store.latest_seq()?,
                result: Value::Null,
                diagnostics: checked.diagnostics,
            });
        }
        let dependencies = dependency_closure(&self.store, &checked.deps)?;
        let built = self
            .builder
            .build_with_dependencies(&checked, &dependencies)
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
        let component_hash = self.store.put("component", &built.component)?;
        let logs_ref = self.store.put("blob", built.logs.as_bytes())?;
        let def = Def {
            allowed_effects: request.allowed_effects.clone(),
            observed_effects: Vec::new(),
            hash: checked.hash,
            lang: checked.lang,
            component_hash: Some(component_hash.clone()),
            sig: checked.sig,
        };
        self.store
            .define(&def, Some(&request.name), &checked.source, &checked.deps)?;
        self.store.append("system",&json!({"type":"component_built","component_hash":component_hash,"logs_ref":logs_ref,"ms":built.ms,"size":built.component.len(),"rustc_invocations":built.rustc_invocations}),0)?;
        Ok(self.response(Ok(json!({"def":def,"build":{"ms":built.ms,"component_hash":component_hash,"size":built.component.len(),"logs_ref":logs_ref,"rustc_invocations":built.rustc_invocations}}))))
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
    pub(super) async fn rehash_dependents(
        &self,
        previous: Option<&Def>,
        current: &Def,
    ) -> Result<Redefinitions> {
        let Some(previous) = previous.filter(|def| def.hash != current.hash) else {
            return Ok(Redefinitions::default());
        };
        let mut replacements = BTreeMap::new();
        replacements.insert(previous.hash.clone(), current.hash.clone());
        let mut pending = vec![previous.hash.clone()];
        let mut candidates = BTreeMap::new();
        while let Some(hash) = pending.pop() {
            for dependent in self.store.dependents(&hash)? {
                if candidates.contains_key(&dependent) {
                    continue;
                }
                let Some(name) = self.store.definition_name(&dependent)? else {
                    continue;
                };
                if self
                    .store
                    .resolve(&name)?
                    .is_none_or(|def| def.hash != dependent)
                {
                    continue;
                }
                ensure!(
                    candidates.len() < 1024,
                    "dependent closure exceeds 1024 definitions"
                );
                candidates.insert(
                    dependent.clone(),
                    stored_definition(&self.store, &dependent)?,
                );
                pending.push(dependent);
            }
        }
        let mut updates = Redefinitions::default();
        while !candidates.is_empty() {
            let hash = candidates
                .iter()
                .find(|entry| {
                    entry
                        .1
                        .deps
                        .values()
                        .all(|dependency| !candidates.contains_key(dependency))
                })
                .map(|entry| entry.0.clone())
                .context("cyclic definition dependencies")?;
            let mut candidate = candidates.remove(&hash).context("dependent disappeared")?;
            for dependency in candidate.deps.values_mut() {
                if let Some(replacement) = replacements.get(dependency) {
                    *dependency = replacement.clone();
                }
            }
            if candidate.source.trim_start().starts_with('{') {
                let mut bundle: loom_check::SourceBundle = serde_json::from_str(&candidate.source)?;
                if let Some(manifest) = bundle
                    .files
                    .get_mut("Cargo.toml")
                    .and_then(loom_check::SourceFile::text_mut)
                {
                    let mut document: toml::Value = manifest.parse()?;
                    if let Some(deps) = document
                        .get_mut("loom")
                        .and_then(|loom| loom.get_mut("deps"))
                        .and_then(toml::Value::as_table_mut)
                    {
                        for dependency in deps.iter_mut().map(|entry| entry.1) {
                            if let Some(replacement) = dependency
                                .as_str()
                                .and_then(|hash| replacements.get(hash.trim_start_matches('#')))
                            {
                                *dependency = toml::Value::String(replacement.clone());
                            }
                        }
                    }
                    *manifest = toml::to_string(&document)?;
                }
                bundle
                    .files
                    .retain(|name, _| !name.starts_with("vendor/") && !name.starts_with(".cargo/"));
                candidate.source = serde_json::to_string(&bundle)?;
            }
            let request = DefineRequest {
                allowed_effects: self
                    .store
                    .definition(&hash)?
                    .context("dependent definition missing")?
                    .allowed_effects,
                lang: candidate.lang,
                name: candidate.name.clone(),
                source: candidate.source,
                deps: candidate.deps,
            };
            let response = self.define_inner(request.clone()).await?;
            ensure!(
                response.ok,
                "upgrade of {} failed: {}",
                request.name,
                serde_json::to_string(&response)?
            );
            let def: Def = serde_json::from_value(response.result["def"].clone())?;
            replacements.insert(hash.clone(), def.hash.clone());
            updates
                .rehashed
                .push(json!({"name":request.name,"previous":hash,"def":def}));
        }
        updates.stale_actors = self
            .store
            .actors()?
            .into_iter()
            .filter(|actor| replacements.contains_key(&actor.behavior_hash))
            .map(|actor| actor.id)
            .collect();
        Ok(updates)
    }
}
