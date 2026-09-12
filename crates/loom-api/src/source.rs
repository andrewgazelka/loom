use super::*;

pub(super) fn decode_source_bundle(bytes: &[u8]) -> Result<String> {
    use std::io::Read;
    ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "Rust source archive exceeds 16 MB"
    );
    let mut archive = tar::Archive::new(bytes);
    let mut files = BTreeMap::new();
    let mut total = 0_u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        ensure!(
            kind.is_file(),
            "Rust source archive permits regular files only"
        );
        let path = entry.path()?.into_owned();
        ensure!(
            path.components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
            "invalid source archive path"
        );
        ensure!(files.len() < 1024, "source archive exceeds 1024 files");
        total = total
            .checked_add(entry.size())
            .context("source archive size overflow")?;
        ensure!(
            total <= 16 * 1024 * 1024,
            "expanded source archive exceeds 16 MB"
        );
        let path = path
            .to_str()
            .context("source paths must be UTF-8")?
            .to_string();
        ensure!(
            path != "target" && !path.starts_with("target/"),
            "source archive contains build artifacts"
        );
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        let source = loom_check::SourceFile::from_bytes(bytes);
        ensure!(
            files.insert(path, source).is_none(),
            "duplicate archive path"
        );
    }
    ensure!(
        files.contains_key("Cargo.toml") && files.contains_key("src/lib.rs"),
        "source archive needs Cargo.toml and src/lib.rs"
    );
    Ok(serde_json::to_string(&json!({"files":files}))?)
}

pub(super) struct BuildResolver {
    pub(super) store: Store,
    pub(super) builder: Arc<loom_build::Builder>,
    pub(super) gate: tokio::sync::Mutex<()>,
}
impl loom_rt::ComponentResolver for BuildResolver {
    fn ensure_built<'a>(
        &'a self,
        hash: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let _guard = self.gate.lock().await;
            let mut def = self
                .store
                .definition(hash)?
                .context("definition not found")?;
            if let Some(component_hash) = &def.component_hash {
                let component = self
                    .store
                    .get(component_hash)?
                    .context("component missing from CAS")?;
                ensure!(
                    loom_proto::core_protocol::is_current(&component),
                    "executable uses an obsolete or unsupported Loom ABI; update its SDK and redefine it before execution"
                );
                return Ok(());
            }
            let checked = stored_definition(&self.store, hash)?;
            let dependencies = dependency_closure(&self.store, &checked.deps)?;
            let built = self
                .builder
                .build_with_dependencies(&checked, &dependencies)
                .await?;
            ensure!(
                built.diagnostics.is_empty(),
                "component build diagnostics: {}",
                serde_json::to_string(&built.diagnostics)?
            );
            ensure!(
                !built.component.is_empty(),
                "builder returned empty component"
            );
            let component_hash = self.store.put("component", &built.component)?;
            let logs_ref = self.store.put("blob", built.logs.as_bytes())?;
            def.component_hash = Some(component_hash.clone());
            self.store.define_with_identity(
                &def,
                None,
                &checked.source,
                &checked.deps,
                Some(
                    built
                        .identity
                        .as_ref()
                        .context("builder returned no item identity")?,
                ),
            )?;
            self.store.record_definition_event(&json!({"type":"component_built","component_hash":component_hash,"logs_ref":logs_ref,"ms":built.ms,"size":built.component.len(),"rustc_invocations":built.rustc_invocations}))?;
            Ok(())
        })
    }
}
pub(super) fn stored_definition(store: &Store, hash: &str) -> Result<loom_check::CheckedDef> {
    let def = store
        .definition(hash)?
        .context("dependency definition missing")?;
    Ok(loom_check::CheckedDef {
        hash: def.hash,
        lang: def.lang,
        name: store.definition_name(hash)?.unwrap_or_else(|| hash.into()),
        source: store.source(hash)?.context("definition source missing")?,
        deps: store.definition_deps(hash)?,
        sig: def.sig,
        diagnostics: vec![],
    })
}
pub(super) fn dependency_closure(
    store: &Store,
    deps: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, loom_check::CheckedDef>> {
    let mut pending: Vec<String> = deps.values().cloned().collect();
    let mut closure = BTreeMap::new();
    while let Some(hash) = pending.pop() {
        if closure.contains_key(&hash) {
            continue;
        }
        ensure!(
            closure.len() < 1024,
            "definition closure exceeds 1024 definitions"
        );
        let checked = stored_definition(store, &hash)?;
        pending.extend(checked.deps.values().cloned());
        closure.insert(hash, checked);
    }
    Ok(closure)
}

pub(super) fn dependency_signatures(
    store: &Store,
    deps: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, loom_proto::TypeSig>> {
    let mut signatures = BTreeMap::new();
    for entry in deps {
        signatures.insert(
            entry.0.clone(),
            store
                .definition(entry.1)?
                .context("dependency signature not found")?
                .sig,
        );
    }
    Ok(signatures)
}

pub(super) fn source_reference(source: &str) -> Option<&str> {
    source
        .strip_prefix('#')
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}
