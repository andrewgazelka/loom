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
        let definition = self.store.resolve(hash)?.context("definition missing")?;
        if definition.lang.is_v8() {
            return Ok(BTreeMap::from([("main".into(), definition.hash)]));
        }
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
    pub(super) fn view_definition(&self, target: &str) -> Result<Value> {
        let def = self
            .store
            .resolve(target)?
            .with_context(|| format!("definition {target:?} not found"))?;
        if def.lang.is_v8() {
            return Ok(json!({
                "name": self.store.definition_name(&def.hash)?,
                "hash": def.hash,
                "behavior_hash": def.hash,
                "def": def,
                "source": self.store.source(&def.hash)?.context("JavaScript source missing")?,
                "items": self.items(&def.hash)?,
                "entries": {"main": {"hash": def.hash, "effects": def.sig.effects}},
                "entry": null,
                "backend": "v8"
            }));
        }
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
        let items = self.items(&def.hash)?;
        let selected_entry = self.store.resolve_entry(target)?;
        let mut entries = BTreeMap::new();
        for entry in &def.sig.exports {
            entries.insert(
                entry.name.clone(),
                json!({"hash": items.get(&entry.name), "effects": {
                    "labels": entry.effects.labels,
                    "unknown": entry.effects.unknown,
                }}),
            );
        }
        Ok(
            json!({"name":self.store.current_names()?.get(target).map(|_| target.to_owned()).or(self.store.definition_name(&def.hash)?),"hash":def.hash,"def":def,
            "behavior_hash":identity.behavior_hash,"wasm_hash":identity.wasm_hash,
            "toolchain_hash":identity.toolchain_hash,"items":self.items(&def.hash)?,
            "source":source,"entries":entries,"entry":selected_entry.map(|entry| entry.name)}),
        )
    }
    fn resolve_run_target(&self, target: &str) -> Result<Def> {
        if let Some(definition) = self.store.resolve(target)? {
            return Ok(definition);
        }
        let hashes: std::collections::BTreeSet<_> =
            self.store.current_names()?.into_values().collect();
        let mut candidates = Vec::new();
        for hash in hashes {
            let definition = self
                .store
                .resolve(&hash)?
                .with_context(|| format!("definition {hash:?} disappeared"))?;
            if definition
                .sig
                .exports
                .iter()
                .any(|entry| entry.name == target)
            {
                candidates.push(definition);
            }
        }
        ensure!(
            candidates.len() <= 1,
            "entry {target:?} is ambiguous; definition candidates: {}",
            candidates
                .iter()
                .map(|definition| definition.hash.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        candidates
            .pop()
            .with_context(|| format!("definition {target:?} not found"))
    }
    fn diff_items(&self, old: &str, new: &str) -> Result<Value> {
        let old = self
            .store
            .resolve(old)?
            .with_context(|| format!("definition {old:?} not found"))?;
        let new = self
            .store
            .resolve(new)?
            .with_context(|| format!("definition {new:?} not found"))?;
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
            "update" | "update_view" | "update_repair" | "update_abort" | "update_rebase" => {
                self.evolution(operation, args).await
            }
            "add" => {
                let name = match args.get("name").filter(|value| !value.is_null()) {
                    Some(value) => value.as_str().context("name must be a string")?,
                    None => "main",
                };
                crate::bundles::validate_name(name)?;
                let _guard = self.definitions_gate.lock().await;
                ensure!(
                    !self.store.current_names()?.contains_key(name),
                    "name {name:?} already exists; use update to propagate changes"
                );
                let deps = args
                    .get("deps")
                    .filter(|value| !value.is_null())
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()?
                    .unwrap_or_default();
                let allowed_effects = args
                    .get("allowed_effects")
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()?
                    .flatten();
                let response = self
                    .admit(
                        DefineRequest {
                            lang: serde_json::from_value(
                                args.get("lang")
                                    .cloned()
                                    .unwrap_or_else(|| json!("typescript")),
                            )?,
                            name: name.into(),
                            source: field(args, "source")?.into(),
                            deps,
                            allowed_effects,
                        },
                        crate::definitions::Destination::Live,
                    )
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
                let name = field(args, "name")?;
                ensure!(
                    self.store.current_names()?.contains_key(name),
                    "definition name {name:?} not found"
                );
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
                let target = field(args, "target")?;
                let def = self.resolve_run_target(target)?;
                let selected = self.store.resolve_entry(target)?;
                let entry = match &selected {
                    Some(entry) => entry.name.as_str(),
                    None => select_entry(&def, target)?,
                };
                let call = self
                    .runtime
                    .call_entry_timed(
                        &def.hash,
                        entry,
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
                    json!({"hash":def.hash,"entry":entry,"output":call.value,"scope":call.scope,"effects":effects}),
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
            "dependents" => {
                let target = field(args, "hash")?;
                let definition = self
                    .store
                    .resolve(target)?
                    .with_context(|| format!("definition {target:?} not found"))?;
                Ok(json!(self.store.dependents(&definition.hash)?))
            }
            "export" => self.export_bundle(args),
            "import" => self.import_bundle(args).await,
            _ => bail!("unknown definition operation {operation}"),
        }
    }
}

fn select_entry<'a>(def: &'a Def, target: &str) -> Result<&'a str> {
    if target != def.hash
        && let Some(entry) = def.sig.exports.iter().find(|entry| entry.name == target)
    {
        return Ok(&entry.name);
    }
    if def.sig.exports.len() == 1 {
        return Ok(&def.sig.exports[0].name);
    }
    bail!(
        "definition {target:?} requires an entry name; candidates: {}",
        def.sig
            .exports
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}
