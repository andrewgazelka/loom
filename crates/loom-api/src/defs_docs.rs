//! Definitions as documents: `export_defs` and `import_defs` (the daemon half of `docs/design/definitions-in-git.md`).
//!
//! A document is `{name, source, deps, allowed_effects, manifest, lock}` (`loom_defdir::Doc`), the same
//! input `add` takes. The daemon never reads or writes the client's files: `loom export-dir` and `loom
//! import-dir` turn documents into a directory and back, and any other client can use the documents directly.
//!
//! `import_defs` adds what is new, updates what changed (callers are updated as `update` always does) and
//! leaves what is identical alone, in dependency order. It is not atomic across the batch: each definition is
//! its own `add` or `update`, a failure stops that definition and everything that depends on it, and the reply
//! says what was applied. An optional `expected` map (the `loom.lock` hashes) is compared with the result and
//! differences are reported, never an error: a different compiler can legitimately change hashes.
use super::*;
use loom_defdir::Doc;
use std::collections::BTreeSet;

impl Service {
    /// The stored input of a Rust definition as a document, or `None` for another language.
    fn stored_doc(
        &self,
        name: &str,
        hash: &str,
        name_of_hash: &BTreeMap<String, String>,
    ) -> Result<Option<Doc>> {
        let def = self
            .store
            .resolve(hash)?
            .with_context(|| format!("definition {name:?} ({hash}) disappeared"))?;
        if def.lang != Lang::Rust {
            return Ok(None);
        }
        let stored = self
            .store
            .source(hash)?
            .with_context(|| format!("source of {name:?} is missing"))?;
        let (source, manifest, lock) = if stored.trim_start().starts_with('{') {
            let bundle: loom_check::SourceBundle = serde_json::from_str(&stored)?;
            for file in bundle.files.keys() {
                ensure!(
                    matches!(file.as_str(), "src/lib.rs" | "Cargo.toml" | "Cargo.lock"),
                    "{name:?} has the extra file {file:?}; definitions with more than lib.rs, Cargo.toml and Cargo.lock are not exported yet"
                );
            }
            let text = |file: &str| -> Result<Option<String>> {
                bundle
                    .files
                    .get(file)
                    .map(|content| {
                        content
                            .as_text()
                            .map(str::to_owned)
                            .with_context(|| format!("{name:?}: {file} is not text"))
                    })
                    .transpose()
            };
            (
                text("src/lib.rs")?.with_context(|| format!("{name:?} has no src/lib.rs"))?,
                text("Cargo.toml")?,
                text("Cargo.lock")?,
            )
        } else {
            (stored, None, None)
        };
        // A dependency is written by name when some name currently points at that hash, else by hash.
        let deps = self
            .store
            .definition_deps(hash)?
            .into_iter()
            .map(|(alias, dep)| {
                let target = name_of_hash.get(&dep).cloned().unwrap_or(dep);
                (alias, target)
            })
            .collect();
        Ok(Some(Doc {
            name: name.to_owned(),
            source,
            deps,
            allowed_effects: def.allowed_effects.clone(),
            manifest,
            lock,
        }))
    }

    /// `export_defs {names?}`: the named current definitions (default: all Rust ones) as documents with their
    /// hashes, and the compiler that built them.
    pub(super) async fn export_defs(&self, args: &Value) -> Result<Value> {
        let current = self.store.current_names()?;
        let mut name_of_hash: BTreeMap<String, String> = BTreeMap::new();
        for (name, hash) in &current {
            name_of_hash.entry(hash.clone()).or_insert_with(|| name.clone());
        }
        let wanted: Vec<String> = match args.get("names").filter(|value| !value.is_null()) {
            Some(value) => serde_json::from_value(value.clone()).context("names must be an array of strings")?,
            None => current.keys().cloned().collect(),
        };
        let mut documents = Vec::new();
        let mut skipped = Vec::new();
        let mut toolchain = String::new();
        for name in &wanted {
            let hash = current
                .get(name)
                .with_context(|| format!("definition {name:?} not found"))?;
            match self.stored_doc(name, hash, &name_of_hash) {
                Ok(Some(doc)) => {
                    if toolchain.is_empty()
                        && let Some(identity) = self.store.build_identity(hash)?
                    {
                        toolchain = identity.toolchain_hash;
                    }
                    let mut value = serde_json::to_value(&doc)?;
                    value["hash"] = json!(hash);
                    documents.push(value);
                }
                Ok(None) => skipped.push(json!({"name": name, "reason": "not a Rust definition"})),
                // Asked for by name: say why it cannot be exported rather than dropping it.
                Err(error) if args.get("names").is_some_and(|names| !names.is_null()) => return Err(error),
                Err(error) => skipped.push(json!({"name": name, "reason": format!("{error:#}")})),
            }
        }
        Ok(json!({"definitions": documents, "skipped": skipped, "toolchain": toolchain}))
    }

    /// `import_defs {definitions, expected?}`.
    pub(super) async fn import_defs(&self, args: &Value) -> Result<Value> {
        let docs: Vec<Doc> = serde_json::from_value(
            args.get("definitions")
                .cloned()
                .context("definitions is an array of documents")?,
        )
        .context("definitions must be an array of {name, source, deps, ...}")?;
        ensure!(docs.len() <= 4096, "import_defs takes at most 4096 definitions");
        let mut seen = BTreeSet::new();
        for doc in &docs {
            crate::bundles::validate_name(&doc.name)?;
            ensure!(seen.insert(doc.name.as_str()), "definition {:?} appears twice", doc.name);
        }
        // Dependency order among the batch: a definition comes after every batch name it depends on.
        let ordered = order(&docs)?;
        let mut results: Vec<Value> = Vec::new();
        let mut failed: BTreeSet<String> = BTreeSet::new();
        for index in ordered {
            let doc = &docs[index];
            if let Some(broken) = doc.deps.values().find(|dep| failed.contains(*dep)) {
                failed.insert(doc.name.clone());
                results.push(json!({"name": doc.name, "action": "failed", "error": format!("its dependency {broken:?} failed")}));
                continue;
            }
            match self.import_one(doc).await {
                Ok((action, hash)) => results.push(json!({"name": doc.name, "action": action, "hash": hash})),
                Err(error) => {
                    failed.insert(doc.name.clone());
                    results.push(json!({"name": doc.name, "action": "failed", "error": format!("{error:#}")}));
                }
            }
        }
        // The lock's hashes against what the daemon now holds.
        let mut mismatches = Vec::new();
        if let Some(expected) = args.get("expected").filter(|value| !value.is_null()) {
            let expected: BTreeMap<String, String> =
                serde_json::from_value(expected.clone()).context("expected maps names to hashes")?;
            let current = self.store.current_names()?;
            for (name, want) in expected {
                if failed.contains(&name) || !seen.contains(name.as_str()) {
                    continue;
                }
                let got = current.get(&name).cloned().unwrap_or_default();
                if got != want {
                    mismatches.push(json!({"name": name, "expected": want, "got": got}));
                }
            }
        }
        let count = |action: &str| results.iter().filter(|row| row["action"] == action).count();
        Ok(json!({
            "results": results,
            "added": count("added"),
            "updated": count("updated"),
            "unchanged": count("unchanged"),
            "failed": count("failed"),
            "mismatches": mismatches,
        }))
    }

    /// One document against the daemon: add, update or leave.
    async fn import_one(&self, doc: &Doc) -> Result<(&'static str, String)> {
        let current = self.store.current_names()?;
        // `deps` values are names (resolved to their current hashes by `add`/`update`) or hashes.
        let deps = serde_json::to_value(&doc.deps)?;
        let crate_args = json!({"manifest": doc.manifest, "lock": doc.lock});
        let source = crate::definitions::with_crates(&crate_args, Lang::Rust, doc.source.clone())?;
        if let Some(hash) = current.get(&doc.name) {
            let name_of_hash: BTreeMap<String, String> =
                current.iter().map(|(n, h)| (h.clone(), n.clone())).collect();
            if let Some(stored) = self.stored_doc(&doc.name, hash, &name_of_hash)?
                && loom_defdir::same(&stored, doc)
            {
                return Ok(("unchanged", hash.clone()));
            }
            // `unison` dispatches back here, so its future is boxed to break the async recursion.
            Box::pin(self.unison(
                "update",
                &json!({"name": doc.name, "source": source, "deps": deps, "allowed_effects": doc.allowed_effects}),
            ))
            .await?;
            let hash = self.store.current_names()?.get(&doc.name).cloned().context("updated name vanished")?;
            return Ok(("updated", hash));
        }
        Box::pin(self.unison(
            "add",
            &json!({"name": doc.name, "source": doc.source, "lang": "rust", "deps": deps,
                    "allowed_effects": doc.allowed_effects, "manifest": doc.manifest, "lock": doc.lock}),
        ))
        .await?;
        let hash = self.store.current_names()?.get(&doc.name).cloned().context("added name vanished")?;
        Ok(("added", hash))
    }
}

/// Indices of `docs` in dependency order (dependencies among the batch first, otherwise input order); a cycle
/// is an error naming a definition on it.
fn order(docs: &[Doc]) -> Result<Vec<usize>> {
    let index: BTreeMap<&str, usize> = docs.iter().enumerate().map(|(i, d)| (d.name.as_str(), i)).collect();
    let mut state = vec![0u8; docs.len()]; // 0 new, 1 visiting, 2 done
    let mut out = Vec::with_capacity(docs.len());
    fn visit(at: usize, docs: &[Doc], index: &BTreeMap<&str, usize>, state: &mut [u8], out: &mut Vec<usize>) -> Result<()> {
        match state[at] {
            2 => return Ok(()),
            1 => bail!("definitions {:?} depend on each other in a cycle", docs[at].name),
            _ => {}
        }
        state[at] = 1;
        for dep in docs[at].deps.values() {
            if let Some(&next) = index.get(dep.as_str()) {
                visit(next, docs, index, state, out)?;
            }
        }
        state[at] = 2;
        out.push(at);
        Ok(())
    }
    for at in 0..docs.len() {
        visit(at, docs, &index, &mut state, &mut out)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(name: &str, deps: &[(&str, &str)]) -> Doc {
        Doc {
            name: name.into(),
            source: String::new(),
            deps: deps.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            allowed_effects: None,
            manifest: None,
            lock: None,
        }
    }

    #[test]
    fn dependencies_come_before_their_dependents_and_a_cycle_is_named() {
        let docs = vec![doc("top", &[("m", "mid")]), doc("mid", &[("b", "base")]), doc("base", &[]), doc("other", &[("x", "0123")])];
        let ordered: Vec<&str> = order(&docs).unwrap().into_iter().map(|i| docs[i].name.as_str()).collect();
        let at = |name: &str| ordered.iter().position(|n| *n == name).unwrap();
        assert!(at("base") < at("mid") && at("mid") < at("top"), "{ordered:?}");
        assert!(ordered.contains(&"other"), "a dependency outside the batch is not an edge");
        let cycle = vec![doc("a", &[("b", "b")]), doc("b", &[("a", "a")])];
        assert!(order(&cycle).unwrap_err().to_string().contains("cycle"));
    }

    #[tokio::test]
    async fn import_and_export_validate_their_input_and_an_empty_daemon_exports_nothing() {
        let service = Service::new(
            Store::memory().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![],
        )
        .unwrap();
        let ask = |command: &str, args: Value| {
            service.command(loom_proto::CommandRequest { session: None, command: command.into(), args })
        };
        let empty = ask("export_defs", json!({})).await;
        assert!(empty.ok, "{empty:?}");
        assert_eq!(empty.result["definitions"], json!([]));
        assert!(!ask("export_defs", json!({"names": ["nope"]})).await.ok, "an unknown name is an error");
        assert!(!ask("import_defs", json!({})).await.ok);
        assert!(!ask("import_defs", json!({"definitions": [{"name": "a b", "source": ""}]})).await.ok, "bad name");
        let twice = json!({"definitions": [{"name": "a", "source": "x"}, {"name": "a", "source": "y"}]});
        assert!(!ask("import_defs", twice).await.ok, "a name twice");
        let cycle = json!({"definitions": [
            {"name": "a", "source": "x", "deps": {"b": "b"}}, {"name": "b", "source": "y", "deps": {"a": "a"}}]});
        let cycle = ask("import_defs", cycle).await;
        assert!(!cycle.ok && format!("{cycle:?}").contains("cycle"), "{cycle:?}");
    }
}
