mod turn;
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
    pub(crate) registry: Arc<dyn Registry>,
    pub(crate) drivers: Arc<crate::drivers::Drivers>,
    pub(crate) driver_lifetime: Option<Arc<crate::drivers::DriverLifetime>>,
    pub(crate) effects: Arc<dyn EffectHandler>,
    pub(crate) config: Config,
    root_id: ActorId,
    pub(crate) capability_key: [u8; 32],
    pub(crate) cluster: Option<Arc<crate::cluster::ClusterState>>,
    pub(crate) remote: Option<Arc<crate::remote_store::RemoteStore>>,
    pub(crate) shipping: Arc<crate::durability::ShippingState>,
    pub(crate) background: Option<Arc<crate::durability_worker::Background>>,
    pub(crate) memory_ids: Arc<std::sync::Mutex<Vec<ActorId>>>,
    // Node drop forgets ephemeral snapshots; reset replaces an actor's incarnation.
    pub(crate) memory_snapshots: Arc<Mutex<HashMap<String, crate::history::MemorySnapshot>>>,
    // Socket close removes its sender and subscribers through Node::close_stream.
    pub(crate) streams: Arc<Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>,
    pub(crate) scheduling: Arc<std::sync::Mutex<crate::scheduler::Scheduling>>,
    pub(crate) wake: Arc<tokio::sync::Notify>,
    pub(crate) shutdown_deadlines: Arc<Mutex<HashMap<String, crate::messaging::ShutdownTimer>>>,
    pub(crate) connections: Arc<Mutex<HashMap<ActorId, Arc<Mutex<Connection>>>>>,
    gates: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    pub(crate) activations: Arc<Mutex<HashMap<ActorId, i64>>>,
    pub(crate) shutdown_hooks: Arc<Mutex<std::collections::HashSet<ActorId>>>,
    pub(crate) admission: Arc<tokio::sync::RwLock<()>>,
    pub(crate) run_gate: Arc<Mutex<()>>,
    pub(crate) send_outcomes: crate::send_outcome::Outcomes,
    pub(crate) tasks: Arc<Mutex<HashMap<ActorId, Arc<tokio::sync::Notify>>>>,
    pub(crate) indexed: Arc<Mutex<HashMap<ActorId, crate::directory::IndexedState>>>,
    pub(crate) names: Arc<Mutex<Option<Connection>>>,
}

impl Node {
    pub fn shares_storage(&self, other: &Self) -> Result<bool> {
        Ok(Arc::ptr_eq(&self.connections, &other.connections) || self.dir.canonicalize()? == other.dir.canonicalize()?)
    }

    pub async fn open(&self, id: &str) -> Result<Actor> {
        let _admission = self.admit().await?;
        self.open_actor(id).await
    }
    pub(crate) async fn open_actor(&self, id: &str) -> Result<Actor> {
        ensure!(!self.shipping.closed.load(std::sync::atomic::Ordering::Acquire), "actor {id}: node is closed");
        self.open_inner(id).await.with_context(|| format!("actor {} seq {}: open", id, -1))
    }
    pub async fn send(&self, id: &str, key: &str, msg: &[u8]) -> Result<()> {
        let _admission = self.admit().await?;
        let applied = self
            .route_delivery(crate::DeliveryOp::Message { target: id.into(), key: key.into(), sender: crate::EXTERNAL_SENDER.into(), msg: msg.into() })
            .await
            .with_context(|| format!("actor {id} seq -1: send"))?;
        ensure!(applied, "actor {id} seq -1: send remains pending");
        Ok(())
    }
    pub async fn pump(&self, id: &str) -> Result<bool> {
        let _admission = self.admit().await?;
        self.pump_inner(id).await.with_context(|| format!("actor {} seq {}: pump", id, -1))
    }
    /// Drain all deliverable outbox rows, runnable messages and due timers via
    /// actor-specific wakes. Deferred/parked messages require their lifecycle wake;
    /// future armed timers are awaited, preserving the drain contract.
    pub async fn run_until_idle(&self) -> Result<usize> {
        let _admission = self.admit().await?;
        let _run = self.run_gate.lock().await;
        self.run_until_idle_inner().await.with_context(|| format!("actor {} seq {}: run_until_idle", "<node>", -1))
    }
    pub async fn promote(&self, id: &str, hash: &str, author: &str, rationale: &str) -> Result<()> {
        let _admission = self.admit().await?;
        self.promote_inner(id, hash, author, rationale).await.with_context(|| format!("actor {} seq {}: promote", id, -1))
    }
    pub async fn skip(&self, id: &str) -> Result<()> {
        let _admission = self.admit().await?;
        self.skip_inner(id).await.with_context(|| format!("actor {} seq {}: skip", id, -1))
    }
    pub async fn fork(&self, id: &str, at: i64) -> Result<ActorId> {
        let _admission = self.admit().await?;
        self.fork_inner(id, at).await.with_context(|| format!("actor {} seq {}: fork", id, at))
    }
    pub async fn validate(&self, id: &str, candidate: &str, k: i64) -> Result<Verdict> {
        self.validate_inner(id, candidate, k).await.with_context(|| format!("actor {} seq {}: validate", id, -1))
    }

    pub async fn new(dir: impl AsRef<Path>, registry: Arc<dyn Registry>, effects: Arc<dyn EffectHandler>, config: Config) -> Result<Self> {
        config.io.name().context("actor <node> seq -1: I/O selection")?;
        ensure!(config.snapshot_every > 0, "actor <node> seq -1: snapshot_every must be positive");
        ensure!(config.batch_limit > 0, "actor <node> seq -1: batch_limit must be positive");
        ensure!(u32::try_from(config.max_retries).is_ok(), "actor <node> seq -1: max_retries exceeds backoff range");
        std::fs::create_dir_all(dir.as_ref()).context("actor <node> seq -1: create directory")?;
        ensure!(!config.ship_interval.is_zero(), "ship_interval must be positive");
        ensure!(config.store.is_none() || config.io != crate::Io::Memory, "object-store shipping requires file I/O");
        ensure!(config.store.is_none() || config.cluster.is_some(), "--advertise and --cluster-key-file are required with --store");
        ensure!(config.cluster.is_none() || config.store.is_some(), "--store is required with --cluster-key-file");
        let mut names = None;
        let index = crate::directory::connection(&mut names, dir.as_ref(), config.io).await?;
        let capability_key = crate::capability::node_key(index, config.cluster.as_ref()).await?;
        let root_rows = actor::query(index, "SELECT value FROM meta WHERE key='root_id'", ()).await?;
        let persisted_root = root_rows.rows.first().map(|row| row.get::<String>(0)).transpose()?;
        let cluster = match &config.cluster {
            Some(cluster) => Some(Arc::new(crate::cluster::ClusterState::open(index, cluster, config.lease_clock.now_ms()?).await?)),
            None => None,
        };
        let remote = match &config.store {
            Some(store) => {
                let owner = cluster.as_ref().context("--cluster-key-file is required with --store")?.identity().node_id;
                Some(Arc::new(crate::remote_store::RemoteStore::new(store, config.lease_ttl, config.lease_clock.clone(), owner)?))
            }
            None => None,
        };
        let mut node = Self {
            drivers: Arc::new(crate::drivers::Drivers::default()),
            driver_lifetime: None,
            capability_key,
            cluster,
            remote,
            shipping: Arc::new(crate::durability::ShippingState::default()),
            background: None,
            root_id: persisted_root.clone().unwrap_or_default(),
            memory_ids: Arc::new(std::sync::Mutex::new(Vec::new())),
            memory_snapshots: Arc::new(Mutex::new(HashMap::new())),
            streams: Default::default(),
            scheduling: Default::default(),
            wake: Arc::new(tokio::sync::Notify::new()),
            shutdown_deadlines: Arc::new(Mutex::new(HashMap::new())),
            dir: std::fs::canonicalize(dir.as_ref()).context("actor <node> seq -1: canonicalize directory")?,
            registry,
            effects,
            config,
            connections: Arc::new(Mutex::new(HashMap::new())),
            gates: Arc::new(Mutex::new(HashMap::new())),
            run_gate: Arc::new(Mutex::new(())),
            send_outcomes: Default::default(),
            activations: Default::default(),
            shutdown_hooks: Default::default(),
            admission: Arc::new(tokio::sync::RwLock::new(())),
            indexed: Default::default(),
            names: Arc::new(Mutex::new(names)),
            tasks: Arc::new(Mutex::new(HashMap::new())),
        };
        node.renew_node().await?;
        node.driver_lifetime = Some(Arc::new(crate::drivers::DriverLifetime(node.drivers.clone())));
        node.start_shipper();
        for id in node.actor_ids()? {
            if matches!(node.resolve(&id).await?, crate::Placement::Remote { .. }) {
                node.archive_unowned_file(&id).await?;
                continue;
            }
            let actor = node.open_actor(&id).await?;
            let conn = actor.conn.lock().await;
            let marker = actor::query(&conn, "SELECT value FROM meta WHERE key='node_root'", ()).await?;
            if persisted_root.is_none() && marker.rows.first().map(|row| row.get::<String>(0)).transpose()?.as_deref() == Some("true") {
                ensure!(node.root_id.is_empty(), "actor {id} seq -1: multiple node roots");
                node.root_id = id;
            }
        }
        if node.root_id.is_empty() {
            node.root_id = ids::root();
            node.create(
                &node.root_id,
                "",
                crate::supervisor::HASH,
                br#"{"type":"configure","strategy":"one_for_one"}"#,
                crate::Durability::Local,
            )
            .await?;
        }
        {
            let mut names = node.names.lock().await;
            let index = crate::directory::connection(&mut names, &node.dir, node.config.io).await?;
            actor::set_meta(index, "root_id", &node.root_id).await?;
        }
        if !matches!(node.resolve(&node.root_id).await?, crate::Placement::Remote { .. }) {
            node.open_actor(&node.root_id).await?;
            node.sync_index(&node.root_id).await?;
        }
        node.forget_missing_index().await?;
        node.prune_subscribers().await?;
        // Recovery is the only roster scan: persisted work enters the same wake set.
        for id in node.actor_ids()? {
            node.scheduling()?.index_dirty.insert(id.clone());
            node.wake_actor(&id)?;
            node.recover_drivers(&id).await?;
            let owner = node.open_actor(&id).await?;
            let conn = owner.conn.lock().await;
            for row in actor::query(&conn, "SELECT child FROM shutdowns", ()).await?.rows {
                let mut state = node.scheduling()?;
                state.shutdown_requesters.entry(row.get::<String>(0)?).or_default().insert(id.clone());
                state.shutdown_dirty.insert(id.clone());
            }
        }
        node.request_timer_scan()?;
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
            if self.is_memory(id)? {
                return Ok(Actor { id: id.into(), conn, managed: self.remote.is_some(), node: self.clone() });
            }
            if let Err(error) = self.check_lease(id) {
                if error.downcast_ref::<crate::remote_store::LeaseLost>().is_none() {
                    return Err(error);
                }
                self.archive_stale(id, &mut *conn.lock().await).await?;
                if matches!(self.resolve(id).await?, crate::Placement::Remote { .. }) {
                    return Err(error);
                }
                drop(_opening);
                return Box::pin(self.open_inner(id)).await;
            }
            if let Some(store) = &self.remote {
                let mut slot = conn.lock().await;
                let published = self.shipping.get(id)?;
                if actor::meta(&slot, "durability_seq").await?.parse::<i64>()? < published.head.seq {
                    self.archive_stale(id, &mut slot).await?;
                    drop(slot);
                    drop(_opening);
                    return Box::pin(self.open_inner(id)).await;
                }
                if published.head.snapshot_seq < 0 {
                    self.initialize_durability(id, &slot).await?;
                }
                store.check(id)?;
            }
            if self.path(id).with_extension("reset-publish").exists() {
                let mut slot = conn.lock().await;
                if self.path(id).with_extension("reset-publish").exists() {
                    let old = std::mem::replace(&mut *slot, actor::connect(Path::new(":memory:"), crate::Io::Memory).await?);
                    drop(old);
                    crate::reset::recover(&self.path(id))?;
                    *slot = actor::connect(&self.path(id), self.config.io).await?;
                }
            }
            return Ok(Actor { id: id.into(), conn, managed: self.remote.is_some(), node: self.clone() });
        }
        crate::reset::recover(&self.path(id))?;
        if !self.path(id).exists()
            && let Some(store) = &self.remote
        {
            ensure!(store.has_snapshot(id).await?, "actor {id} seq -1: actor does not exist");
        }
        self.restore_on_open(id).await?;
        ensure!(self.path(id).is_file(), "actor {id} seq -1: actor file does not exist");
        let mut conn = actor::connect(&self.path(id), self.config.io).await.with_context(|| format!("actor {id} seq -1: open"))?;
        ensure!(actor::meta(&conn, "id").await? == id, "actor {id} seq -1: file identity mismatch");
        crate::capability::migrate(&conn).await?;
        self.migrate_authority(&conn).await?;
        self.initialize_durability(id, &conn).await?;
        self.recover_drivers_on(id, &mut conn).await?;
        self.admit_driver_owner(id)?;
        self.activations.lock().await.remove(id);
        self.activate_on(id, &mut conn).await?;
        let conn = Arc::new(Mutex::new(conn));
        self.connections.lock().await.insert(id.into(), conn.clone());
        self.wake_actor(id)?;
        Ok(Actor { id: id.into(), conn, managed: self.remote.is_some(), node: self.clone() })
    }

    pub(crate) async fn create(&self, id: &str, parent: &str, hash: &str, msg: &[u8], durability: crate::Durability) -> Result<Actor> {
        let durability = if hash == crate::view::HASH { crate::Durability::Ephemeral } else { durability };
        let _creation = self.guard(&format!("create:{id}")).await;
        // Initialization publishes the file before reopening it and installing
        // its shared connection. Directory scans can discover that file in the
        // gap, so serialize their open/migrations with the entire publication.
        let opening = self.guard(&format!("open:{id}")).await;
        if durability != crate::Durability::Ephemeral && !self.connections.lock().await.contains_key(id) {
            self.restore_on_open(id).await?;
        }
        if !self.path(id).exists() && !self.connections.lock().await.contains_key(id) {
            let spec = crate::ChildSpec::new(hash, msg, crate::ChildType::Worker);
            let behavior = match crate::view::from_spec(&self.registry, &spec).await? {
                Some(behavior) => behavior,
                None => actor::behavior(&self.registry, hash).await?,
            };
            let conn = actor::initialize(&self.path(id), id, parent, behavior.as_ref(), msg, self.config.io, durability).await?;
            self.connections.lock().await.insert(id.into(), Arc::new(Mutex::new(conn)));
            if self.config.io == crate::Io::Memory || durability == crate::Durability::Ephemeral {
                self.memory_ids.lock().map_err(|_| anyhow!("memory actor registry poisoned"))?.push(id.into());
            }
        }
        drop(opening);
        let actor = self.open_actor(id).await?;
        let mut conn = actor.conn.lock().await;
        ensure!(actor::meta(&conn, "parent").await? == parent, "actor {id} seq 0: parent mismatch");
        // A retry after publication but before snapshot registration finishes creation.
        if actor::cursor(&conn).await? == 0 {
            self.snapshot_actor(&conn, id, 0).await?;
        }
        self.initialize_durability(id, &conn).await?;
        self.activate_on(id, &mut conn).await?;
        drop(conn);
        self.scheduling()?.index_dirty.insert(id.into());
        self.wake_actor(id)?;
        Ok(actor)
    }

    pub fn root(&self) -> ActorId {
        self.root_id.clone()
    }

    pub fn actor_ids(&self) -> Result<Vec<ActorId>> {
        if self.config.io == crate::Io::Memory {
            return Ok(self.memory_ids.lock().map_err(|_| anyhow!("memory actor registry poisoned"))?.clone());
        }
        let mut actors = self.memory_ids.lock().map_err(|_| anyhow!("memory actor registry poisoned"))?.clone();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("db") {
                continue;
            }
            let name = path.file_stem().and_then(|n| n.to_str()).context("actor <node> seq -1: invalid filename")?;
            if name == "_node"
                || name.contains(".snap.")
                || name.contains(".reset.")
                // Stale files are preserved by design; never listed; removed only by an operator.
                || name.contains(".stale.")
                || name.contains(".remote-base")
            {
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
        if hash == crate::view::HASH {
            return self.spawn(&self.root_id, &crate::ChildSpec::new(hash, msg, crate::ChildType::Worker)).await;
        }
        let behavior = self.behavior(hash).await?;
        let mut spec = crate::ChildSpec::new(behavior.hash(), msg, behavior.child_type());
        spec.restart = crate::RestartPolicy::Temporary;
        self.spawn(&self.root_id, &spec).await
    }

    pub async fn spawn(&self, parent: &str, spec: &crate::ChildSpec) -> Result<ActorId> {
        let _admission = self.admit().await?;
        let pinned = crate::view::pin_spec(&self.registry, spec).await.with_context(|| format!("actor {parent} seq -1: spawn"))?;
        if let crate::Placement::Remote { addr, .. } = self.resolve(parent).await? {
            let acks = self.forward(&addr, &[crate::DeliveryOp::HostSpawn { target: parent.into(), spec: pinned }]).await?;
            ensure!(acks.len() == 1, "actor {parent} seq -1: spawn ingress returned wrong ack count");
            let ack = &acks[0];
            ensure!(ack.ok, "actor {parent} seq -1: spawn ingress: {}", ack.error.as_deref().unwrap_or("ownership changed"));
            return serde_json::from_value(ack.result.clone().context(format!("actor {parent} seq -1: spawn ingress missing id"))?)
                .with_context(|| format!("actor {parent} seq -1: decode spawned actor id"));
        }
        self.spawn_inner(parent, &pinned).await.with_context(|| format!("actor {parent} seq -1: spawn"))
    }

    pub(crate) async fn spawn_inner(&self, parent: &str, spec: &crate::ChildSpec) -> Result<ActorId> {
        let _creation = self.guard("host-spawn").await;
        crate::view::pin_spec(&self.registry, spec).await?;
        let root = self.open_actor(parent).await?;
        let mut conn = root.conn.lock().await;
        let tx = conn.transaction().await?;
        ensure!(actor::status(&tx).await? == Status::Running, "actor {} seq -1: root is not running", parent);
        let latest = actor::query(&tx, "SELECT COALESCE(MAX(seq),0) FROM outbox", ()).await?;
        let seq = actor::cursor(&tx).await?.max(latest.rows.first().context("missing outbox sequence")?.get::<i64>(0)?);
        let indices = actor::query(&tx, "SELECT COALESCE(MAX(idx),-1)+1 FROM outbox WHERE seq=?", [seq]).await?;
        let idx: i64 = indices.rows.first().context("missing outbox index")?.get(0)?;
        let generation: i64 = actor::meta(&tx, "generation").await?.parse()?;
        let id = ids::child(&ids::incarnation(parent, generation), seq, idx);
        let cap = self.mint_child_cap(&id, id.as_bytes());
        crate::capability::store_cap(&tx, &cap).await?;
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
        let parent_behavior = crate::view::behavior_on(&self.registry, &tx, &actor::code(&tx).await?.hash).await?;
        if parent_behavior.child_type() == crate::ChildType::Supervisor {
            crate::supervisor::record_child(&tx, &cap, spec).await?;
            crate::supervisor_store::record_host_spawn(&tx, &cap, spec).await?;
        }
        let spawn = Spawn::Child { id: id.clone(), spec: spec.clone(), origin_seq: seq, origin_idx: idx };
        tx.execute("INSERT INTO outbox(seq,idx,target,msg) VALUES (?,?,'spawn',?)", turso::params![seq, idx, serde_json::to_vec(&spawn)?])
            .await?;
        self.commit_control(parent, tx).await?;
        drop(conn);
        self.pump_unlocked(parent).await?;
        self.wake_actor(&id)?;
        Ok(id)
    }
}

impl Node {
    pub async fn behavior(&self, hash: &str) -> Result<Arc<dyn crate::Behavior>> {
        actor::behavior(&self.registry, hash).await
    }

    pub async fn behaviors(&self) -> Result<Vec<crate::builtin::BehaviorInfo>> {
        let mut behaviors = self.registry.behaviors().await?;
        behaviors.push(crate::builtin::BehaviorInfo {
            hash: crate::view::HASH.to_owned(),
            description: "Ephemeral materialized view; init selects a pure template definition.".to_owned(),
        });
        Ok(behaviors)
    }
}
