use super::*;

impl Service {
    pub async fn command(&self, request: CommandRequest) -> Response {
        let direct = command_returns_direct(&request.command);
        let response = self.command_response(request).await;
        if direct {
            response
        } else {
            self.inline(response)
        }
    }
    async fn command_response(&self, mut request: CommandRequest) -> Response {
        if let Err(error) = loom_proto::encode(&request.args) {
            return self.response(Err(anyhow::Error::msg(error)));
        }
        if let Some(verb) = loom_proto::verbs::lookup(&request.command)
            && let Err(error) = verb.normalize(&mut request.args)
        {
            return self.response(Err(anyhow::Error::msg(error)));
        }
        if let Err(error) = self
            .access
            .require(auth::request_scope(&request.command, &request.args))
        {
            return self.response(Err(error));
        }
        self.response(self.command_inner(request).await)
    }
    async fn command_inner(&self, request: CommandRequest) -> Result<Value> {
        let request = if request.command == "command" {
            let command = field(&request.args, "command")?.to_owned();
            ensure!(command != "command", "nested command is not allowed");
            let mut args = request.args["args"].clone();
            if let Some(verb) = loom_proto::verbs::lookup(&command) {
                verb.normalize(&mut args).map_err(anyhow::Error::msg)?;
            }
            self.access.require(auth::request_scope(&command, &args))?;
            CommandRequest {
                command,
                args,
                ..request
            }
        } else {
            request
        };
        let args = &request.args;
        match request.command.as_str() {
            "view" if args.get("actor").is_some() => self.actor_command("view", args.clone()).await,
            command
                if loom_proto::verbs::lookup(command)
                    .is_some_and(|verb| verb.family == loom_proto::verbs::Family::Definition) =>
            {
                self.unison(&request.command, args).await
            }
            command
                if loom_proto::verbs::lookup(command)
                    .is_some_and(|verb| verb.family == loom_proto::verbs::Family::Actor) =>
            {
                self.actor_command(command, args.clone()).await
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
