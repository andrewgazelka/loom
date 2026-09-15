//! Immutable, atomic namespace evolution with durable repair sessions.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Edit {
    source: String,
    deps: Option<BTreeMap<String, String>>,
    allowed_effects: Option<Vec<String>>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Change {
    names: Vec<String>,
    old_hash: String,
    new_hash: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Failure {
    hash: String,
    names: Vec<String>,
    source: String,
    diagnostics: Value,
    build: Value,
}
#[derive(Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    Complete,
    NeedsRepair,
    Conflict,
    Aborted,
}
#[derive(Serialize, Deserialize, PartialEq)]
struct OriginalRequest {
    name: String,
    source: String,
    deps: Option<BTreeMap<String, String>>,
    allowed_effects: Option<Vec<String>>,
    expected_hash: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Evolution {
    #[serde(default)]
    original_request: Option<OriginalRequest>,
    target: String,
    status: Status,
    expected_names: BTreeMap<String, String>,
    edits: BTreeMap<String, Edit>,
    changes: Vec<Change>,
    diagnostics: Vec<Failure>,
}
struct Publication {
    definition: Def,
    names: Vec<String>,
    source: String,
    deps: BTreeMap<String, String>,
    identity: Option<loom_proto::BuildIdentity>,
    event: Value,
}

impl Service {
    pub(super) async fn evolution(&self, operation: &str, args: &Value) -> Result<Value> {
        if operation == "update" {
            let target = field(args, "name")?.to_owned();
            let original_request: OriginalRequest = serde_json::from_value(json!({
                "name":target, "source":field(args, "source")?, "deps":args.get("deps"),
                "allowed_effects":args.get("allowed_effects"), "expected_hash":args.get("expected_hash")
            }))?;
            let request_id = args
                .get("request_id")
                .filter(|value| !value.is_null())
                .map(|value| value.as_str().context("request_id must be a string"))
                .transpose()?;
            if let Some(id) = request_id
                && let Some(session) = self.store.update_session(id)?
            {
                return self.existing_update(&session, &original_request);
            }
            let expected_names = self.store.current_names()?;
            let hash = expected_names
                .get(&target)
                .context("update name not found")?;
            if let Some(expected) = args.get("expected_hash").filter(|value| !value.is_null()) {
                ensure!(
                    expected.as_str() == Some(hash.as_str()),
                    "update conflict: expected_hash does not match current name"
                );
            }
            let edit = self.resolve_edit(
                hash,
                serde_json::from_value(json!({
                    "source":field(args, "source")?, "deps":args.get("deps"),
                    "allowed_effects":args.get("allowed_effects")
                }))?,
            )?;
            let state = Evolution {
                original_request: Some(original_request),
                target,
                status: Status::Pending,
                edits: BTreeMap::from([(hash.clone(), edit)]),
                expected_names,
                changes: Vec::new(),
                diagnostics: Vec::new(),
            };
            let serialized = serde_json::to_value(&state)?;
            let session = match request_id {
                Some(id) => match self.store.create_update_session_with_id(id, &serialized) {
                    Ok(session) => session,
                    Err(error) => {
                        if let Some(session) = self.store.update_session(id)? {
                            return self.existing_update(
                                &session,
                                state
                                    .original_request
                                    .as_ref()
                                    .context("original request missing")?,
                            );
                        }
                        return Err(error);
                    }
                },
                None => self.store.create_update_session(&serialized)?,
            };
            return self.attempt_update(session, state).await;
        }
        let id = field(args, "id")?;
        let session = self
            .store
            .update_session(id)?
            .context("update session not found")?;
        if operation == "update_view" {
            return self.update_result(&session);
        }
        let revision = args
            .get("revision")
            .and_then(Value::as_u64)
            .context("revision must be an unsigned integer")?;
        ensure!(
            revision == session.revision,
            "update conflict: stale session revision"
        );
        let mut state: Evolution = serde_json::from_value(session.state.clone())?;
        ensure!(
            state.status == Status::NeedsRepair
                || state.status == Status::Pending
                || state.status == Status::Conflict,
            "update session is terminal"
        );
        if operation == "update_abort" {
            state.status = Status::Aborted;
            let session =
                self.store
                    .save_update_session(id, revision, &serde_json::to_value(state)?)?;
            return self.update_result(&session);
        }
        if operation == "update_rebase" {
            let current_names = self.store.current_names()?;
            let mut conflicts = Vec::new();
            for (hash, edit) in &state.edits {
                let names: Vec<String> = state
                    .expected_names
                    .iter()
                    .filter(|entry| entry.1 == hash)
                    .map(|entry| entry.0.clone())
                    .collect();
                if names
                    .iter()
                    .any(|name| current_names.get(name) != Some(hash))
                {
                    conflicts.push(Failure {
                        hash: hash.clone(), names, source: edit.source.clone(),
                        diagnostics: json!([{"code":"rebase_conflict","message":"An explicitly edited name moved. Start a new update against its current hash and reconcile this saved source."}]),
                        build: Value::Null,
                    });
                }
            }
            let mut reachable = BTreeSet::new();
            for hash in current_names.values() {
                visit(
                    &self.store,
                    hash,
                    &BTreeMap::new(),
                    &mut reachable,
                    &mut BTreeSet::new(),
                    &mut Vec::new(),
                )?;
            }
            for (hash, edit) in &state.edits {
                if !reachable.contains(hash)
                    && !conflicts.iter().any(|failure| &failure.hash == hash)
                {
                    conflicts.push(Failure {
                        hash: hash.clone(), names: Vec::new(), source: edit.source.clone(),
                        diagnostics: json!([{"code":"rebase_conflict","message":"An explicitly edited definition is no longer reachable from the current namespace."}]), build: Value::Null,
                    });
                }
            }
            state.changes.clear();
            if !conflicts.is_empty() {
                state.status = Status::Conflict;
                state.diagnostics = conflicts;
                let saved =
                    self.store
                        .save_update_session(id, revision, &serde_json::to_value(&state)?)?;
                return self.update_result(&saved);
            }
            state.expected_names = current_names;
            state.status = Status::Pending;
            state.diagnostics.clear();
            let saved =
                self.store
                    .save_update_session(id, revision, &serde_json::to_value(&state)?)?;
            return self.attempt_update(saved, state).await;
        }
        ensure!(operation == "update_repair", "unknown update operation");
        ensure!(
            state.status != Status::Conflict,
            "update conflict: rebase before repairing against a changed namespace"
        );
        let repairs: BTreeMap<String, Edit> =
            serde_json::from_value(args.get("changes").context("changes required")?.clone())?;
        let mut edits = BTreeMap::new();
        for (name, edit) in repairs {
            let hash = state
                .expected_names
                .get(&name)
                .cloned()
                .or_else(|| {
                    state
                        .diagnostics
                        .iter()
                        .find(|failure| failure.hash == name)
                        .map(|failure| failure.hash.clone())
                })
                .with_context(|| format!("repair target {name:?} not in update snapshot"))?;
            let mut edit = self.resolve_edit(&hash, edit)?;
            if let Some(previous) = state.edits.get(&hash) {
                if edit.deps.is_none() {
                    edit.deps = previous.deps.clone();
                }
                if edit.allowed_effects.is_none() {
                    edit.allowed_effects = previous.allowed_effects.clone();
                }
            }
            if let Some(existing) = edits.insert(hash, edit.clone()) {
                ensure!(
                    existing == edit,
                    "conflicting repairs to aliases of the same definition"
                );
            }
        }
        state.edits.extend(edits);
        state.status = Status::Pending;
        state.changes.clear();
        state.diagnostics.clear();
        let session =
            self.store
                .save_update_session(id, revision, &serde_json::to_value(&state)?)?;
        self.attempt_update(session, state).await
    }

    fn existing_update(
        &self,
        session: &loom_store::UpdateSession,
        request: &OriginalRequest,
    ) -> Result<Value> {
        let state: Evolution = serde_json::from_value(session.state.clone())?;
        ensure!(
            state.original_request.as_ref() == Some(request),
            "update request_id conflict: original request differs"
        );
        self.update_result(session)
    }

    fn resolve_edit(&self, hash: &str, mut edit: Edit) -> Result<Edit> {
        let definition = self
            .store
            .resolve(hash)?
            .context("edited definition missing")?;
        if definition.lang.is_v8() {
            ensure!(
                edit.deps.as_ref().is_none_or(BTreeMap::is_empty),
                "script deps must be empty; use source import declarations"
            );
            ensure!(
                source_reference(&edit.source).is_none(),
                "script source must be inline; Rust source bundles are unsupported"
            );
        }
        if let Some(deps) = &mut edit.deps {
            for target in deps.values_mut() {
                *target = self
                    .store
                    .resolve(target)?
                    .with_context(|| format!("dependency {target:?} not found"))?
                    .hash;
            }
        }
        if let Some(reference) = source_reference(&edit.source) {
            edit.source = decode_source_bundle(
                &self
                    .store
                    .get(reference)?
                    .context("source bundle not found")?,
            )?;
        }
        Ok(edit)
    }

    fn update_result(&self, session: &loom_store::UpdateSession) -> Result<Value> {
        let mut update = session.state.clone();
        update["id"] = json!(session.id);
        update["revision"] = json!(session.revision);
        let state: Evolution = serde_json::from_value(session.state.clone())?;
        let mut result = if state.status == Status::Complete {
            // Resolve the session's committed hash, not a name another update can move.
            let old = state
                .expected_names
                .get(&state.target)
                .context("update target missing")?;
            let hash = state
                .changes
                .iter()
                .find(|change| &change.old_hash == old)
                .map(|change| &change.new_hash)
                .unwrap_or(old);
            let mut view = self.view_definition(hash)?;
            view["name"] = json!(state.target);
            view
        } else {
            json!({})
        };
        result["update"] = update;
        Ok(result)
    }

    async fn attempt_update(
        &self,
        session: loom_store::UpdateSession,
        mut state: Evolution,
    ) -> Result<Value> {
        if self.store.current_names()? != state.expected_names {
            state.status = Status::Conflict;
            let saved = self.store.save_update_session(
                &session.id,
                session.revision,
                &serde_json::to_value(state)?,
            )?;
            return self.update_result(&saved);
        }
        let mut staged = self.clone();
        staged.store = self.store.stage_intake()?;
        staged.builder = Arc::new(self.builder.for_store(staged.store.clone()));
        let mut order = Vec::new();
        let mut visited = BTreeSet::new();
        let mut visiting = BTreeSet::new();
        for hash in state.expected_names.values() {
            visit(
                &staged.store,
                hash,
                &state.edits,
                &mut visited,
                &mut visiting,
                &mut order,
            )?;
        }
        let mut names_by_hash = BTreeMap::<String, Vec<String>>::new();
        for (name, hash) in &state.expected_names {
            names_by_hash
                .entry(hash.clone())
                .or_default()
                .push(name.clone());
        }
        let mut replacements = BTreeMap::<String, String>::new();
        let mut failed = BTreeSet::new();
        let mut publications = Vec::new();
        for hash in order {
            let edit = state.edits.get(&hash);
            let old_deps = staged.store.definition_deps(&hash)?;
            let mut deps = edit
                .and_then(|edit| edit.deps.clone())
                .unwrap_or(old_deps.clone());
            let blocked = deps.values().any(|dep| failed.contains(dep));
            for dep in deps.values_mut() {
                if let Some(replacement) = replacements.get(dep) {
                    *dep = replacement.clone();
                }
            }
            if !blocked && edit.is_none() && deps == old_deps {
                continue;
            }
            let names = names_by_hash.get(&hash).cloned().unwrap_or_default();
            let source = match edit {
                Some(edit) => edit.source.clone(),
                None => staged
                    .store
                    .source(&hash)?
                    .context("update source missing")?,
            };
            if blocked {
                failed.insert(hash.clone());
                state.diagnostics.push(Failure { hash, names, source, diagnostics: json!([{"code":"blocked_dependency","message":"repair failing dependencies first"}]), build: Value::Null });
                continue;
            }
            let definition = staged
                .store
                .resolve(&hash)?
                .context("update definition missing")?;
            // A private name is never exposed to the runtime or the live namespace.
            let name = names
                .first()
                .cloned()
                .unwrap_or_else(|| format!("__update_{}", hash));
            let response = staged
                .define_update_node(DefineRequest {
                    lang: definition.lang,
                    name,
                    source: source.clone(),
                    deps,
                    allowed_effects: edit
                        .and_then(|edit| edit.allowed_effects.clone())
                        .or(definition.allowed_effects),
                })
                .await;
            let response = match response {
                Ok(response) if response.ok => response,
                other => {
                    let failure = match other {
                        Ok(response) => Failure {
                            hash: hash.clone(),
                            names,
                            source,
                            diagnostics: serde_json::to_value(response.diagnostics)?,
                            build: response.result,
                        },
                        Err(error) => Failure {
                            hash: hash.clone(),
                            names,
                            source,
                            diagnostics: json!([{"code":"build_error","message":format!("{error:#}")}]),
                            build: Value::Null,
                        },
                    };
                    failed.insert(hash.clone());
                    state.diagnostics.push(failure);
                    continue;
                }
            };
            let new_hash = response.result["def"]["hash"]
                .as_str()
                .context("built definition hash missing")?
                .to_owned();
            if new_hash != hash {
                replacements.insert(hash.clone(), new_hash.clone());
                state.changes.push(Change {
                    names: names.clone(),
                    old_hash: hash,
                    new_hash: new_hash.clone(),
                });
            }
            publications.push(Publication {
                definition: staged.store.resolve(&new_hash)?.context("built definition missing")?, names,
                source: staged.store.source(&new_hash)?.context("built source missing")?,
                deps: staged.store.definition_deps(&new_hash)?,
                identity: match definition.lang {
                    Lang::Rust => Some(staged.store.build_identity(&new_hash)?.context("built identity missing")?),
                    Lang::JavaScript | Lang::TypeScript => None,
                },
                event: json!({"type":"component_built", "component_hash":response.result["build"]["component_hash"], "logs_ref":response.result["build"]["logs_ref"], "ms":response.result["build"]["ms"], "size":response.result["build"]["size"], "rustc_invocations":response.result["build"]["rustc_invocations"]}),
            });
        }
        if !state.diagnostics.is_empty() {
            state.status = Status::NeedsRepair;
            let saved = self.store.save_update_session(
                &session.id,
                session.revision,
                &serde_json::to_value(state)?,
            )?;
            return self.update_result(&saved);
        }
        state.status = Status::Complete;
        let mut intake = Vec::new();
        for publication in &publications {
            let names: Vec<Option<&str>> = if publication.names.is_empty() {
                vec![None]
            } else {
                publication
                    .names
                    .iter()
                    .map(|name| Some(name.as_str()))
                    .collect()
            };
            for name in names {
                intake.push(loom_store::IntakePublication {
                    def: &publication.definition,
                    name,
                    source: &publication.source,
                    deps: &publication.deps,
                    identity: publication.identity.as_ref(),
                    build_event: &publication.event,
                });
            }
        }
        let saved = self.store.commit_update(
            &staged.store,
            &intake,
            &state.expected_names,
            &session.id,
            session.revision,
            &serde_json::to_value(&state)?,
        );
        let saved = match saved {
            Ok(saved) => saved,
            Err(error) => {
                if self.store.current_names()? == state.expected_names {
                    return Err(error);
                }
                state.status = Status::Conflict;
                self.store.save_update_session(
                    &session.id,
                    session.revision,
                    &serde_json::to_value(&state)?,
                )?
            }
        };
        self.update_result(&saved)
    }
}

fn visit(
    store: &Store,
    hash: &str,
    edits: &BTreeMap<String, Edit>,
    visited: &mut BTreeSet<String>,
    visiting: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) -> Result<()> {
    if visited.contains(hash) {
        return Ok(());
    }
    ensure!(
        visiting.insert(hash.into()),
        "update dependency cycle at {hash}"
    );
    let deps = edits
        .get(hash)
        .and_then(|edit| edit.deps.clone())
        .unwrap_or(store.definition_deps(hash)?);
    for dependency in deps.values() {
        visit(store, dependency, edits, visited, visiting, order)?;
    }
    visiting.remove(hash);
    visited.insert(hash.into());
    order.push(hash.into());
    Ok(())
}
