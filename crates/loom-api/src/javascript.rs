//! JavaScript admission shares its engine and actor ABI with execution.
use super::*;

impl Service {
    pub(super) async fn define_javascript(
        &self,
        request: DefineRequest,
        destination: Option<&Store>,
        progress: &build_progress::BuildGuard,
    ) -> Result<Response> {
        ensure!(
            request.deps.is_empty(),
            "script deps must be empty; use source import declarations"
        );
        ensure!(
            source_reference(&request.source).is_none(),
            "script source must be inline; Rust source bundles are unsupported"
        );
        progress.stage("compile");
        let engine = self.v8_engine.as_ref().context("V8 scripts are disabled")?;
        let bundled = loom_imports::requires_bundle(&request.source, request.lang.as_str())
            .with_context(|| format!("{} source rejected", request.lang.as_str()))?;
        let abi = if request.lang == Lang::TypeScript {
            loom_v8::typescript_abi()
        } else {
            loom_v8::ABI_VERSION.to_owned()
        };
        let component_hash;
        let identity;
        if bundled {
            let artifact = loom_imports::compile(
                &request.source,
                request.lang.as_str(),
                &self.script_compiler,
            )
            .await?;
            artifact.validate()?;
            let _sandbox = engine.compile(&artifact.javascript).await?;
            component_hash = self
                .store
                .put("javascript_module", &serde_json::to_vec(&artifact)?)?;
            identity = loom_proto::module_definition_identity(
                request.lang,
                &request.source,
                &request.deps,
                request.allowed_effects.as_deref(),
                &abi,
                &component_hash,
                &artifact.compiler,
            )?;
        } else {
            let _sandbox = if request.lang == Lang::TypeScript {
                engine.compile_typescript(&request.source).await?
            } else {
                engine.compile(&request.source).await?
            };
            component_hash = self
                .store
                .put("javascript_source", request.source.as_bytes())?;
            identity = loom_proto::script_definition_identity(
                request.lang,
                &request.source,
                &request.deps,
                request.allowed_effects.as_deref(),
                &abi,
            )?;
        }
        let hash = self.store.put("javascript_definition", &identity)?;
        let def = Def {
            hash,
            lang: request.lang,
            component_hash: Some(component_hash.clone()),
            sig: loom_proto::TypeSig {
                exports: vec![loom_proto::ExportSig {
                    name: "main".into(),
                    params: vec![loom_proto::ParamSig {
                        name: "message".into(),
                        shape: loom_proto::ValueShape::Value,
                    }],
                    returns: loom_proto::ValueShape::Value,
                    effects: Default::default(),
                }],
                effects: Default::default(),
            },
            allowed_effects: request.allowed_effects,
            observed_effects: Vec::new(),
        };
        progress.stage("publish");
        let build_event = json!({"type":"component_built", "component_hash":component_hash, "backend":"v8", "size":request.source.len(), "rustc_invocations":0});
        let published = if let Some(destination) = destination {
            destination.commit_intake(
                &self.store,
                loom_store::IntakePublication {
                    def: &def,
                    name: Some(&request.name),
                    source: &request.source,
                    deps: &request.deps,
                    identity: None,
                    build_event: &build_event,
                },
            )?;
            destination
        } else {
            self.store
                .define(&def, Some(&request.name), &request.source, &request.deps)?;
            &self.store
        };
        Ok(Response {
            ok: true,
            seq: published.latest_seq()?,
            diagnostics: Vec::new(),
            result: json!({"def":def, "build":{"backend":"v8", "component_hash":component_hash, "size":request.source.len(), "rustc_invocations":0}}),
        })
    }
}
