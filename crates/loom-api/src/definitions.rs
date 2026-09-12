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
    pub(super) async fn define_inner(&self, request: DefineRequest) -> Result<Response> {
        let progress = self.build_progress.start(&request.name);
        self.builder.preflight().await?;
        let mut intake = self.clone();
        intake.store = self.store.stage_intake()?;
        intake.builder = Arc::new(self.builder.for_store(intake.store.clone()));
        intake.define_staged(request, &self.store, &progress).await
    }
    async fn define_staged(
        &self,
        mut request: DefineRequest,
        destination: &Store,
        progress: &build_progress::BuildGuard,
    ) -> Result<Response> {
        progress.stage("check");
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
        progress.stage("compile");
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
        progress.stage("publish");
        let component_hash = self.store.put("component", &built.component)?;
        let logs_ref = self.store.put("blob", built.logs.as_bytes())?;
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
        let def = Def {
            allowed_effects: request.allowed_effects.clone(),
            observed_effects: Vec::new(),
            hash: identity.behavior_hash.clone(),
            lang: checked.lang,
            component_hash: Some(component_hash.clone()),
            sig: checked.sig,
        };
        let build_event = json!({"type":"component_built","component_hash":component_hash,"logs_ref":logs_ref,"ms":built.ms,"size":built.component.len(),"rustc_invocations":built.rustc_invocations});
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
        Ok(Response {
            ok: true,
            seq: destination.latest_seq()?,
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
