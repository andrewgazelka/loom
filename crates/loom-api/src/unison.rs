use super::*;

#[derive(Deserialize)]
struct ItemDocument {
    items: BTreeMap<String, Item>,
}
#[derive(Deserialize)]
struct Item {
    hash: String,
}
impl Service {
    fn items(&self, hash: &str) -> Result<BTreeMap<String, String>> {
        let identity = self
            .store
            .build_identity(hash)?
            .context("definition has no compiler item identity")?;
        let bytes = self
            .store
            .get(&identity.item_hashes_ref)?
            .context("item hash document missing from CAS")?;
        let document: ItemDocument = serde_json::from_slice(&bytes)?;
        let mut items = BTreeMap::new();
        for entry in document.items {
            items.insert(entry.0, entry.1.hash);
        }
        Ok(items)
    }
    fn view_definition(&self, target: &str) -> Result<Value> {
        let def = self
            .store
            .resolve(target)?
            .with_context(|| format!("definition {target:?} not found"))?;
        let identity = self
            .store
            .build_identity(&def.hash)?
            .context("definition has no compiler item identity")?;
        let stored_source = self
            .store
            .source(&def.hash)?
            .context("definition source missing from CAS")?;
        let source = if stored_source.trim_start().starts_with('{') {
            let bundle: loom_check::SourceBundle = serde_json::from_str(&stored_source)?;
            bundle
                .files
                .get("src/lib.rs")
                .and_then(loom_check::SourceFile::as_text)
                .context("source bundle missing UTF-8 src/lib.rs")?
                .to_owned()
        } else {
            stored_source
        };
        let mut entries = BTreeMap::new();
        for entry in &def.sig.exports {
            entries.insert(
                entry.name.clone(),
                json!({"effects": {
                    "labels": entry.effects.labels,
                    "unknown": entry.effects.unknown,
                }}),
            );
        }
        Ok(
            json!({"name":self.store.current_names()?.get(target).map(|_| target.to_owned()).or(self.store.definition_name(&def.hash)?),"hash":def.hash,"def":def,
            "behavior_hash":identity.behavior_hash,"wasm_hash":identity.wasm_hash,
            "toolchain_hash":identity.toolchain_hash,"items":self.items(&def.hash)?,
            "source":source,"entries":entries}),
        )
    }
    fn diff_items(&self, old: &str, new: &str) -> Result<Value> {
        let old = self
            .store
            .resolve(old)?
            .context("old definition not found")?;
        let new = self
            .store
            .resolve(new)?
            .context("new definition not found")?;
        let before = self.items(&old.hash)?;
        let after = self.items(&new.hash)?;
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut changed = Vec::new();
        for entry in &before {
            match after.get(entry.0) {
                None => removed.push(json!({"name":entry.0,"hash":entry.1})),
                Some(hash) if hash != entry.1 => {
                    changed.push(json!({"name":entry.0,"old":entry.1,"new":hash}))
                }
                _ => {}
            }
        }
        for entry in &after {
            if !before.contains_key(entry.0) {
                added.push(json!({"name":entry.0,"hash":entry.1}));
            }
        }
        Ok(json!({"old":old.hash,"new":new.hash,"added":added,"removed":removed,"changed":changed}))
    }
    pub(super) async fn unison(&self, operation: &str, args: &Value) -> Result<Value> {
        match operation {
            "add" | "update" => {
                let name = if operation == "update" {
                    field(args, "name")?
                } else {
                    match args.get("name").filter(|value| !value.is_null()) {
                        Some(value) => value.as_str().context("name must be a string")?,
                        None => "main",
                    }
                };
                ensure!(!name.is_empty(), "definition name is empty");
                let _guard = self.definitions_gate.lock().await;
                let previous = if operation == "update" {
                    ensure!(
                        self.store.current_names()?.contains_key(name),
                        "name {name:?} not found"
                    );
                    self.store.resolve(name)?
                } else {
                    None
                };
                let deps = match args.get("deps") {
                    Some(value) => serde_json::from_value(value.clone())?,
                    None => match &previous {
                        Some(definition) => self.store.definition_deps(&definition.hash)?,
                        None => BTreeMap::new(),
                    },
                };
                let allowed_effects = match args.get("allowed_effects") {
                    Some(value) => serde_json::from_value(value.clone())?,
                    None => previous.and_then(|definition| definition.allowed_effects),
                };
                let response = self
                    .define_inner(DefineRequest {
                        lang: Lang::Rust,
                        name: name.into(),
                        source: field(args, "source")?.into(),
                        deps,
                        allowed_effects,
                    })
                    .await?;
                ensure!(
                    response.ok,
                    "definition build failed: {}",
                    serde_json::to_string(&response)?
                );
                let mut result = self.view_definition(
                    response.result["def"]["hash"]
                        .as_str()
                        .context("built definition hash missing")?,
                )?;
                result["name"] = json!(name);
                result["build"] = response.result["build"].clone();
                Ok(result)
            }
            "view" => self.view_definition(field(args, "target")?),
            "diff" => self.diff_items(field(args, "old")?, field(args, "new")?),
            "history" => {
                let mut history = Vec::new();
                let mut previous: Option<String> = None;
                for revision in self.store.name_history(field(args, "name")?)? {
                    let changes = previous
                        .as_deref()
                        .map(|old| self.diff_items(old, &revision.hash))
                        .transpose()?;
                    history.push(json!({"name":revision.name,"hash":revision.hash,"timestamp":self.store.revision_timestamp(revision.since_seq)?,"changes":changes}));
                    previous = Some(revision.hash);
                }
                Ok(json!(history))
            }
            "run" => {
                let def = self
                    .store
                    .resolve(field(args, "target")?)?
                    .context("definition not found")?;
                let call = self
                    .runtime
                    .call_def_timed(
                        &def.hash,
                        args.get("args").cloned().unwrap_or_else(|| json!([])),
                    )
                    .await?;
                let trace = self
                    .store
                    .load_call_trace(&call.scope)?
                    .context("completed call trace missing")?;
                let effects = trace.trace.entries.iter().map(|entry| -> Result<Value> {
                    Ok(json!({"descriptor":self.store.get_value::<Value>(&entry.descriptor_hash)?.context("effect descriptor missing")?,"outcome":entry.outcome}))
                }).collect::<Result<Vec<_>>>()?;
                Ok(
                    json!({"hash":def.hash,"output":call.value,"scope":call.scope,"effects":effects}),
                )
            }
            "find" => {
                let text = field(args, "text")?;
                let mut matches = Vec::new();
                for entry in self.store.current_names()? {
                    let items: BTreeMap<_, _> = self
                        .items(&entry.1)?
                        .into_iter()
                        .filter(|item| item.0.contains(text))
                        .collect();
                    if entry.0.contains(text) || !items.is_empty() {
                        matches.push(json!({"name":entry.0,"hash":entry.1,"items":items}));
                    }
                }
                Ok(json!(matches))
            }
            "dependents" => Ok(json!(
                self.store
                    .dependents(field(args, "hash")?.trim_start_matches('#'))?
            )),
            _ => bail!("unknown definition operation {operation}"),
        }
    }
}
