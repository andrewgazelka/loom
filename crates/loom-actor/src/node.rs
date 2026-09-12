use crate::{Actor, ActorId, Config, EffectHandler, Registry, Spawn, Status, Verdict, actor, ids};
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use turso::Connection;

#[derive(Clone)]
pub struct Node {
    pub(crate) dir: PathBuf,
    pub(crate) registry: Registry,
    pub(crate) effects: Arc<dyn EffectHandler>,
    pub(crate) config: Config,
    root_id: ActorId,
    memory_ids: Arc<std::sync::Mutex<Vec<ActorId>>>,
    pub(crate) wake: Arc<tokio::sync::Notify>,
    pub(crate) shutdown_deadlines: Arc<Mutex<HashMap<String, crate::messaging::ShutdownTimer>>>,
    connections: Arc<Mutex<HashMap<ActorId, Arc<Mutex<Connection>>>>>,
    gates: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    run_gate: Arc<Mutex<()>>,
    pub(crate) tasks: Arc<Mutex<HashMap<ActorId, Arc<tokio::sync::Notify>>>>,
    pub(crate) names: Arc<Mutex<Option<Connection>>>,
}

impl Node {
    pub async fn open(&self, id: &str) -> Result<Actor> {
        self.open_inner(id).await.with_context(|| format!("actor {} seq {}: open", id, -1))
    }
    pub async fn send(&self, id: &str, key: &str, msg: &[u8]) -> Result<()> {
        self.send_inner(id, key, msg).await.with_context(|| format!("actor {} seq {}: send", id, -1))
    }
    pub async fn pump(&self, id: &str) -> Result<bool> {
        self.pump_inner(id).await.with_context(|| format!("actor {} seq {}: pump", id, -1))
    }
    pub async fn run_until_idle(&self) -> Result<usize> {
        let _run = self.run_gate.lock().await;
        self.run_until_idle_inner().await.with_context(|| format!("actor {} seq {}: run_until_idle", "<node>", -1))
    }
    pub async fn promote(&self, id: &str, hash: &str, author: &str, rationale: &str) -> Result<()> {
        self.promote_inner(id, hash, author, rationale).await.with_context(|| format!("actor {} seq {}: promote", id, -1))
    }
    pub async fn skip(&self, id: &str) -> Result<()> {
        self.skip_inner(id).await.with_context(|| format!("actor {} seq {}: skip", id, -1))
    }
    pub async fn fork(&self, id: &str, at: i64) -> Result<ActorId> {
        self.fork_inner(id, at).await.with_context(|| format!("actor {} seq {}: fork", id, at))
    }
    pub async fn validate(&self, id: &str, candidate: &str, k: i64) -> Result<Verdict> {
        self.validate_inner(id, candidate, k).await.with_context(|| format!("actor {} seq {}: validate", id, -1))
    }

    pub async fn new(dir: impl AsRef<Path>, mut registry: Registry, effects: Arc<dyn EffectHandler>, config: Config) -> Result<Self> {
        crate::builtin::register(&mut registry);
        config.io.name().context("actor <node> seq -1: I/O selection")?;
        registry.entry(crate::supervisor::HASH.into()).or_insert_with(|| Arc::new(crate::Supervisor));
        ensure!(config.snapshot_every > 0, "actor <node> seq -1: snapshot_every must be positive");
        ensure!(u32::try_from(config.max_retries).is_ok(), "actor <node> seq -1: max_retries exceeds backoff range");
        for (hash, behavior) in &registry {
            ensure!(hash == behavior.hash(), "actor <node> seq -1: registry key differs from behavior hash {hash}");
        }
        std::fs::create_dir_all(dir.as_ref()).context("actor <node> seq -1: create directory")?;
        let mut node = Self {
            root_id: String::new(),
            memory_ids: Arc::new(std::sync::Mutex::new(Vec::new())),
            wake: Arc::new(tokio::sync::Notify::new()),
            shutdown_deadlines: Arc::new(Mutex::new(HashMap::new())),
            dir: std::fs::canonicalize(dir.as_ref()).context("actor <node> seq -1: canonicalize directory")?,
            registry,
            effects,
            config,
            connections: Arc::new(Mutex::new(HashMap::new())),
            gates: Arc::new(Mutex::new(HashMap::new())),
            run_gate: Arc::new(Mutex::new(())),
            names: Arc::new(Mutex::new(None)),
            tasks: Arc::new(Mutex::new(HashMap::new())),
        };
        for id in node.actor_ids()? {
            let actor = node.open(&id).await?;
            let conn = actor.conn.lock().await;
            let marker = actor::query(&conn, "SELECT value FROM meta WHERE key='node_root'", ()).await?;
            if marker.rows.first().map(|row| row.get::<String>(0)).transpose()?.as_deref() == Some("true") {
                ensure!(node.root_id.is_empty(), "actor {id} seq -1: multiple node roots");
                node.root_id = id;
            }
        }
        if node.root_id.is_empty() {
            node.root_id = ids::root();
            node.create(&node.root_id, "", crate::supervisor::HASH, br#"{"type":"configure","strategy":"one_for_one"}"#).await?;
        }
        node.sync_index(&node.root_id).await?;
        Ok(node)
    }

    pub(crate) fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.db"))
    }
    pub(crate) fn snapshot_path(&self, id: &str, generation: i64, seq: i64) -> PathBuf {
        self.dir.join(if generation == 0 { format!("{id}.snap.{seq}.db") } else { format!("{id}.snap.{generation}.{seq}.db") })
    }

    /// The map lock only chooses a guard; waiting is always scoped to one key.
    pub(crate) async fn guard(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let gate = self.gates.lock().await.entry(key.to_owned()).or_insert_with(|| Arc::new(Mutex::new(()))).clone();
        gate.lock_owned().await
    }

    async fn open_inner(&self, id: &str) -> Result<Actor> {
        self.config.io.name()?;
        ids::check(id)?;
        let _opening = self.guard(&format!("open:{id}")).await;
        let cached = self.connections.lock().await.get(id).cloned();
        if let Some(conn) = cached {
            if self.path(id).with_extension("reset-publish").exists() {
                let mut slot = conn.lock().await;
                if self.path(id).with_extension("reset-publish").exists() {
                    let old = std::mem::replace(&mut *slot, actor::connect(Path::new(":memory:"), crate::Io::Memory).await?);
                    drop(old);
                    crate::reset::recover(&self.path(id))?;
                    *slot = actor::connect(&self.path(id), self.config.io).await?;
                }
            }
            return Ok(Actor { id: id.into(), conn });
        }
        crate::reset::recover(&self.path(id))?;
        ensure!(self.path(id).is_file(), "actor {id} seq -1: actor file does not exist");
        let conn = actor::connect(&self.path(id), self.config.io).await.with_context(|| format!("actor {id} seq -1: open"))?;
        ensure!(actor::meta(&conn, "id").await? == id, "actor {id} seq -1: file identity mismatch");
        let conn = Arc::new(Mutex::new(conn));
        self.connections.lock().await.insert(id.into(), conn.clone());
        Ok(Actor { id: id.into(), conn })
    }

    pub(crate) async fn create(&self, id: &str, parent: &str, hash: &str, msg: &[u8]) -> Result<Actor> {
        let _creation = self.guard(&format!("create:{id}")).await;
        if !self.path(id).exists() && !self.connections.lock().await.contains_key(id) {
            let behavior = actor::behavior(&self.registry, hash)?;
            let conn = actor::initialize(&self.path(id), id, parent, behavior.as_ref(), msg, self.config.io).await?;
            self.connections.lock().await.insert(id.into(), Arc::new(Mutex::new(conn)));
            if self.config.io == crate::Io::Memory {
                self.memory_ids.lock().map_err(|_| anyhow!("memory actor registry poisoned"))?.push(id.into());
            }
        }
        let actor = self.open(id).await?;
        let conn = actor.conn.lock().await;
        ensure!(actor::meta(&conn, "parent").await? == parent, "actor {id} seq 0: parent mismatch");
        // A retry after publication but before snapshot registration finishes creation.
        if self.config.io != crate::Io::Memory && actor::cursor(&conn).await? == 0 {
            actor::snapshot(&conn, &self.snapshot_path(id, actor::meta(&conn, "generation").await?.parse()?, 0), 0).await?;
        }
        drop(conn);
        Ok(actor)
    }

    pub fn root(&self) -> ActorId {
        self.root_id.clone()
    }

    pub fn actor_ids(&self) -> Result<Vec<ActorId>> {
        if self.config.io == crate::Io::Memory {
            return Ok(self.memory_ids.lock().map_err(|_| anyhow!("memory actor registry poisoned"))?.clone());
        }
        let mut actors = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("db") {
                continue;
            }
            let name = path.file_stem().and_then(|n| n.to_str()).context("actor <node> seq -1: invalid filename")?;
            if name == "_node" || name.contains(".snap.") || name.contains(".reset.") {
                continue;
            }
            ids::check(name)?;
            actors.push(name.to_owned());
        }
        actors.sort();
        Ok(actors)
    }

    /// Host-created actors are temporary children of the node's durable supervisor.
    pub async fn spawn_root(&self, hash: &str, msg: &[u8]) -> Result<ActorId> {
        let mut spec = crate::ChildSpec::new(hash, msg, self.behavior(hash)?.child_type());
        spec.restart = crate::RestartPolicy::Temporary;
        self.spawn(&self.root_id, &spec).await
    }

    pub async fn spawn(&self, parent: &str, spec: &crate::ChildSpec) -> Result<ActorId> {
        self.spawn_inner(parent, spec).await.with_context(|| format!("actor {parent} seq -1: spawn"))
    }

    async fn spawn_inner(&self, parent: &str, spec: &crate::ChildSpec) -> Result<ActorId> {
        let _creation = self.guard("host-spawn").await;
        actor::behavior(&self.registry, &spec.behavior_hash)?;
        let root = self.open(parent).await?;
        let mut conn = root.conn.lock().await;
        let tx = conn.transaction().await?;
        ensure!(actor::status(&tx).await? == Status::Running, "actor {} seq -1: root is not running", parent);
        let latest = actor::query(&tx, "SELECT COALESCE(MAX(seq),0) FROM outbox", ()).await?;
        let seq = actor::cursor(&tx).await?.max(latest.rows.first().context("missing outbox sequence")?.get::<i64>(0)?);
        let indices = actor::query(&tx, "SELECT COALESCE(MAX(idx),-1)+1 FROM outbox WHERE seq=?", [seq]).await?;
        let idx: i64 = indices.rows.first().context("missing outbox index")?.get(0)?;
        let generation: i64 = actor::meta(&tx, "generation").await?.parse()?;
        let id = ids::child(&ids::incarnation(parent, generation), seq, idx);
        let hash = spec.behavior_hash.as_str();
        let msg = spec.init.as_slice();
        let shutdown = serde_json::to_string(&spec.shutdown)?;
        tx.execute(
            "INSERT INTO children(id,spawned_seq,behavior_hash,init,restart,shutdown,link,monitor,child_type) VALUES (?,?,?,?,?,?,?,?,?)",
            turso::params![
                id.as_str(),
                seq,
                hash,
                msg,
                serde_json::to_value(spec.restart)?.as_str().context("restart policy")?,
                shutdown.as_str(),
                spec.link,
                spec.monitor,
                serde_json::to_string(&spec.child_type)?
            ],
        )
        .await?;
        if self.behavior(&actor::code(&tx).await?.hash)?.child_type() == crate::ChildType::Supervisor {
            crate::supervisor::record_child(&tx, &id, spec).await?;
        }
        let spawn = Spawn::Child { id: id.clone(), spec: spec.clone(), origin_seq: seq, origin_idx: idx };
        tx.execute("INSERT INTO outbox(seq,idx,target,msg) VALUES (?,?,'spawn',?)", turso::params![seq, idx, serde_json::to_vec(&spawn)?])
            .await?;
        tx.commit().await?;
        drop(conn);
        self.pump_unlocked(parent).await?;
        self.wake.notify_one();
        Ok(id)
    }

    async fn send_inner(&self, id: &str, key: &str, msg: &[u8]) -> Result<()> {
        let actor = self.open(id).await?;
        actor::inject(&*actor.conn.lock().await, key, "external", msg).await.with_context(|| format!("actor {id} seq -1: send"))?;
        self.wake.notify_one();
        Ok(())
    }

    pub(crate) async fn step(&self, id: &str, cancellation: &tokio::sync::Notify) -> Result<bool> {
        let admission = self.guard(&format!("lifecycle:{id}")).await;
        let actor = self.open(id).await?;
        let mut conn = actor.conn.lock().await;
        if actor::status(&conn).await? != Status::Running || actor::meta(&conn, "ready").await? != "true" {
            return Ok(false);
        }
        drop(admission);
        let generation: i64 = actor::meta(&conn, "generation").await?.parse()?;
        let cursor = actor::cursor(&conn).await?;
        if self.config.io != crate::Io::Memory
            && cursor > 0
            && cursor % self.config.snapshot_every == 0
            && actor::query(&conn, "SELECT seq FROM inbox WHERE state='done' AND seq>? LIMIT 1", [cursor]).await?.rows.is_empty()
        {
            actor::snapshot(&conn, &self.snapshot_path(id, generation, cursor), cursor).await?;
        }
        let Some(message) = actor::next(&conn).await? else {
            return Ok(false);
        };
        let code = actor::code(&conn).await?;
        let behavior = actor::behavior(&self.registry, &code.hash)?;
        for retry in 0..=self.config.max_retries {
            match actor::attempt(
                &mut conn,
                id,
                &message,
                behavior.as_ref(),
                code.revision,
                &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
                Some(cancellation),
            )
            .await
            {
                Ok(false) => return Ok(false),
                Ok(true) => {
                    let cursor = actor::cursor(&conn).await?;
                    if self.config.io != crate::Io::Memory
                        && cursor > 0
                        && cursor % self.config.snapshot_every == 0
                        && actor::query(&conn, "SELECT seq FROM inbox WHERE state='done' AND seq>? LIMIT 1", [cursor])
                            .await?
                            .rows
                            .is_empty()
                    {
                        actor::snapshot(&conn, &self.snapshot_path(id, generation, cursor), cursor).await?;
                    }
                    return Ok(true);
                }
                Err(error) if error.runtime && retry < self.config.max_retries => {
                    tokio::time::sleep(self.config.retry_backoff.saturating_mul(u32::try_from(retry + 1)?)).await;
                }
                Err(error) => {
                    actor::poison(
                        &mut conn,
                        id,
                        &message,
                        &error.message,
                        behavior.as_ref(),
                        &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
                    )
                    .await?;
                    return Ok(true);
                }
            }
        }
        Err(anyhow!("actor {id} seq {}: retry loop exhausted unexpectedly", message.seq))
    }

    async fn promote_inner(&self, id: &str, hash: &str, author: &str, rationale: &str) -> Result<()> {
        let actor = self.open(id).await?;
        let behavior = actor::behavior(&self.registry, hash).with_context(|| format!("actor {id} seq -1: promote"))?;
        actor::promote(
            &mut *actor.conn.lock().await,
            behavior.as_ref(),
            author,
            rationale,
            &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
        )
        .await
        .with_context(|| format!("actor {id} seq -1: promote"))?;
        self.sync_index(id).await
    }

    async fn skip_inner(&self, id: &str) -> Result<()> {
        let actor = self.open(id).await?;
        let mut conn = actor.conn.lock().await;
        let tx = conn.transaction().await?;
        ensure!(actor::status(&tx).await? == Status::Parked, "actor {id} seq -1: skip requires parked status");
        let message = actor::next(&tx).await?.context(format!("actor {id} seq -1: no message to skip"))?;
        crate::mailbox::complete(&tx, message.seq).await?;
        actor::set_meta(&tx, &format!("skipped:{}", message.seq), "1").await?;
        actor::set_meta(&tx, &format!("code_at:{}", message.seq), &actor::code(&tx).await?.revision.to_string()).await?;
        actor::set_meta(&tx, "status", "running").await?;
        tx.commit().await?;
        Ok(())
    }
}

impl Node {
    pub fn behavior(&self, hash: &str) -> Result<Arc<dyn crate::Behavior>> {
        actor::behavior(&self.registry, hash)
    }

    pub fn behaviors(&self) -> Vec<crate::builtin::BehaviorInfo> {
        let mut values: Vec<_> = self
            .registry
            .iter()
            .map(|(hash, behavior)| crate::builtin::BehaviorInfo { hash: hash.clone(), description: behavior.description().into() })
            .collect();
        values.sort_by(|left, right| left.hash.cmp(&right.hash));
        values
    }
}
