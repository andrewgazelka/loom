use super::*;

impl Service {
    pub async fn command(&self, request: CommandRequest) -> Response {
        if let Err(error) = loom_proto::encode(&request.args) {
            return self.response(Err(anyhow::Error::msg(error)));
        }
        if let Err(error) = self.access.require(auth::command_scope(&request.command)) {
            return self.response(Err(error));
        }
        self.response(self.command_inner(request).await)
    }
    async fn command_inner(&self, request: CommandRequest) -> Result<Value> {
        let args = &request.args;
        match request.command.as_str() {
            "crate.add" => Ok(serde_json::to_value(
                loom_build::registry::CrateRegistry::new(self.store.clone())
                    .add(field(args, "name")?, field(args, "version")?)
                    .await?,
            )?),
            "upgrade" => {
                let _guard = self.definitions_gate.lock().await;
                let old = field(args, "old")?;
                let new = field(args, "new")?;
                if let Some(previous) = self.store.definition(old)? {
                    let current = self
                        .store
                        .definition(new)?
                        .context("replacement definition missing")?;
                    let updates = self.rehash_dependents(Some(&previous), &current).await?;
                    return Ok(serde_json::json!({"rehashed":updates.rehashed}));
                }
                let _: loom_proto::Tree = self
                    .store
                    .get_value(old)?
                    .context("old crate tree missing")?;
                let _: loom_proto::Tree = self
                    .store
                    .get_value(new)?
                    .context("replacement crate tree missing")?;
                let mut changed = Vec::new();
                struct CrateReplacement {
                    previous: Def,
                    current: Def,
                }
                let mut replacements = Vec::new();
                for def in self.store.definitions()? {
                    let Some(name) = self.store.definition_name(&def.hash)? else {
                        continue;
                    };
                    if self
                        .store
                        .resolve(&name)?
                        .is_none_or(|current| current.hash != def.hash)
                    {
                        continue;
                    }
                    let source = self
                        .store
                        .source(&def.hash)?
                        .context("definition source missing")?;
                    let Ok(mut bundle) = serde_json::from_str::<loom_check::SourceBundle>(&source)
                    else {
                        continue;
                    };
                    let Some(manifest) = bundle
                        .files
                        .get_mut("Cargo.toml")
                        .and_then(loom_check::SourceFile::text_mut)
                    else {
                        continue;
                    };
                    let mut document: toml::Value = manifest.parse()?;
                    let Some(crates) = document
                        .get_mut("loom")
                        .and_then(|loom| loom.get_mut("crates"))
                        .and_then(toml::Value::as_table_mut)
                    else {
                        continue;
                    };
                    let mut replaced = false;
                    for entry in crates.iter_mut().map(|entry| entry.1) {
                        if entry.get("hash").and_then(toml::Value::as_str) == Some(old) {
                            entry["hash"] = toml::Value::String(new.into());
                            replaced = true;
                        }
                    }
                    if !replaced {
                        continue;
                    }
                    *manifest = toml::to_string(&document)?;
                    bundle.files.retain(|name, _| {
                        !name.starts_with("vendor/") && !name.starts_with(".cargo/")
                    });
                    let response = self
                        .define_inner(DefineRequest {
                            lang: def.lang,
                            name,
                            source: serde_json::to_string(&bundle)?,
                            deps: self.store.definition_deps(&def.hash)?,
                            allowed_effects: def.allowed_effects.clone(),
                        })
                        .await?;
                    ensure!(
                        response.ok,
                        "crate upgrade failed: {}",
                        serde_json::to_string(&response)?
                    );
                    let current: Def = serde_json::from_value(response.result["def"].clone())?;
                    changed.push(serde_json::json!({"old":def.hash,"result":response.result}));
                    replacements.push(CrateReplacement {
                        previous: def,
                        current,
                    });
                }
                let mut rehashed = Vec::new();
                for replacement in replacements {
                    let current = if let Some(name) =
                        self.store.definition_name(&replacement.current.hash)?
                    {
                        self.store
                            .resolve(&name)?
                            .context("upgraded definition name missing")?
                    } else {
                        replacement.current
                    };
                    let updates = self
                        .rehash_dependents(Some(&replacement.previous), &current)
                        .await?;
                    rehashed.extend(updates.rehashed);
                }
                Ok(serde_json::json!({"upgraded":changed,"rehashed":rehashed}))
            }

            "cas.list" => {
                let query = if request.args.is_null() {
                    loom_proto::CasListRequest::default()
                } else {
                    serde_json::from_value(request.args.clone())?
                };
                Ok(serde_json::to_value(self.store.cas_list(&query)?)?)
            }
            "cas.inspect" => Ok(serde_json::to_value(
                self.inspect_cas(serde_json::from_value(request.args.clone())?)?,
            )?),
            "trace.effects" => {
                let offset = match args.get("offset") {
                    None => 0,
                    Some(value) => usize::try_from(
                        value
                            .as_u64()
                            .context("offset must be a nonnegative integer")?,
                    )?,
                };
                let limit = match args.get("limit") {
                    None => 256,
                    Some(value) => usize::try_from(
                        value.as_u64().context("limit must be a positive integer")?,
                    )?,
                };
                Ok(serde_json::to_value(self.store.trace_effects(
                    field(args, "hash")?,
                    offset,
                    limit,
                )?)?)
            }
            "model.state" => Ok(serde_json::to_value(self.runtime.model().state()?)?),
            "model.list" => Ok(serde_json::to_value(self.runtime.model().list()?)?),
            "process.start" => Ok(serde_json::to_value(
                self.runtime.start_process(request.args.clone()).await?,
            )?),
            "process.list" => Ok(serde_json::to_value(self.runtime.processes().list()?)?),
            "process.status" => Ok(serde_json::to_value(
                self.runtime.processes().status(field(args, "id")?)?,
            )?),
            "process.wait" => Ok(serde_json::to_value(
                self.runtime.processes().wait(field(args, "id")?).await?,
            )?),
            "process.cancel" => Ok(serde_json::to_value(
                self.runtime.processes().cancel(field(args, "id")?).await?,
            )?),
            "backup" => {
                let name = field(args, "name")?;
                ensure!(
                    !name.is_empty()
                        && name.len() <= 128
                        && name.bytes().all(|byte| byte.is_ascii_alphanumeric()
                            || matches!(byte, b'-' | b'_' | b'.'))
                        && name != "."
                        && name != "..",
                    "backup name must be a filename"
                );
                tokio::fs::create_dir_all(&self.backup_directory).await?;
                let destination = self.backup_directory.join(name);
                let store = self.store.clone();
                Ok(serde_json::to_value(
                    tokio::task::spawn_blocking(move || {
                        loom_maintenance::backup(&store, &destination)
                    })
                    .await??,
                )?)
            }
            "machine.create" => Ok(serde_json::to_value(
                self.runtime
                    .create_machine(std::path::Path::new(field(args, "root")?))?,
            )?),
            "stats" => {
                let mut stats = serde_json::to_value(loom_maintenance::stats(&self.store)?)?;
                stats["recording_commits"] = json!(self.store.recording_commit_count());
                let recording = self.store.recording_timings();
                stats["recording_transaction_nanos"] = json!(recording.transaction_nanos);
                stats["recording_checkpoint_nanos"] = json!(recording.checkpoint_nanos);
                stats["last_reply_storage_nanos"] =
                    json!(self.last_reply_storage_nanos.load(Ordering::Relaxed));
                stats["effect_wire_bytes"] = json!(self.runtime.effect_wire_bytes());
                stats["handler_round_trip_us"] = self.runtime.handler_round_trip_us();
                Ok(stats)
            }
            "gc" => Ok(serde_json::to_value(
                loom_maintenance::collect_effect_index(
                    &self.store,
                    args["limit"]
                        .as_u64()
                        .context("limit required")?
                        .try_into()?,
                )?,
            )?),
            "cache_evict" => Ok(serde_json::to_value(
                loom_maintenance::evict_build_cache(
                    &self.builder,
                    &loom_maintenance::CachePolicy {
                        max_bytes: args["max_bytes"].as_u64().context("max_bytes required")?,
                        max_age: Duration::from_secs(
                            args["max_age_secs"]
                                .as_u64()
                                .context("max_age_secs required")?,
                        ),
                        max_entries: args["max_entries"]
                            .as_u64()
                            .context("max_entries required")?
                            .try_into()?,
                    },
                )
                .await?,
            )?),
            "defs" => {
                let mut defs = Vec::new();
                for def in self.store.definitions()? {
                    let size = def
                        .component_hash
                        .as_deref()
                        .map(|h| self.store.get(h))
                        .transpose()?
                        .flatten()
                        .map(|b| b.len());
                    let name = self.store.definition_name(&def.hash)?;
                    let mut value = serde_json::to_value(def)?;
                    value["name"] = json!(name);
                    value["component_size"] = json!(size);
                    defs.push(value);
                }
                Ok(json!(defs))
            }
            "build" => self.build_record(field(args, "hash")?),
            "events" => Ok(serde_json::to_value(self.store.definition_events(
                args["after"].as_i64().unwrap_or(0),
                args["limit"].as_u64().unwrap_or(1000).min(1000) as usize,
            )?)?),
            "call" => {
                self.runtime
                    .call_def(field(args, "hash")?, args["args"].clone())
                    .await
            }
            "call.replay" => Ok(self
                .runtime
                .replay_def_timed(
                    field(args, "hash")?,
                    args["args"].clone(),
                    field(args, "scope")?,
                )
                .await?
                .value),
            "resolve" => {
                let hash = field(args, "hash")?;
                if let Some(def) = self.store.resolve(hash)? {
                    Ok(serde_json::to_value(def)?)
                } else {
                    self.store
                        .get_value::<Value>(hash)?
                        .context("CAS value not found")
                }
            }
            "deps" => Ok(serde_json::to_value(
                self.store.dependencies(field(args, "hash")?)?,
            )?),
            _ => bail!("unknown command {}", request.command),
        }
    }
}
