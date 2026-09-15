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
            "JavaScript definitions do not support imports or dependencies"
        );
        ensure!(
            source_reference(&request.source).is_none(),
            "JavaScript source must be inline; Rust source bundles are unsupported"
        );
        progress.stage("compile");
        let engine = self.v8_engine.as_ref().context("JavaScript is disabled")?;
        let sandbox = engine.compile(&request.source).await?;
        let _schema = sandbox.schema();
        let identity = loom_proto::javascript_definition_identity(
            &request.source,
            &request.deps,
            request.allowed_effects.as_deref(),
            loom_v8::ABI_VERSION,
        )?;
        let hash = self.store.put("javascript_definition", &identity)?;
        // component_hash is the executable payload address. Rust stores Wasm;
        // JavaScript stores source and selects V8 through the persisted language.
        let component_hash = self
            .store
            .put("javascript_source", request.source.as_bytes())?;
        let def = Def {
            hash,
            lang: Lang::JavaScript,
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
