use crate::local_store::ConditionalLocalStore;
use anyhow::{Context, Result, ensure};
use object_store::{
    ObjectStore, ObjectStoreExt, PutMode, PutOptions, UpdateVersion,
    aws::{AmazonS3Builder, S3ConditionalPut},
    path::Path,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::Debug,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub enum StoreConfig {
    Local {
        path: PathBuf,
    },
    S3 {
        endpoint: String,
        bucket: String,
        region: String,
    },
    /// Apply one object-key namespace to every operation, including listing and
    /// conditional writes. This wraps local and S3 stores at the same boundary.
    Namespace {
        store: Box<StoreConfig>,
        prefix: String,
    },
}
fn open_store(config: &StoreConfig) -> Result<Arc<dyn ObjectStore>> {
    let store: Arc<dyn ObjectStore> = match config {
        StoreConfig::Local { path } => Arc::new(ConditionalLocalStore::new(path)?),
        StoreConfig::S3 { endpoint, bucket, region } => Arc::new(
            AmazonS3Builder::from_env()
                .with_endpoint(endpoint)
                .with_bucket_name(bucket)
                .with_region(region)
                .with_allow_http(endpoint.starts_with("http://"))
                .with_conditional_put(S3ConditionalPut::ETagMatch)
                .build()?,
        ),
        StoreConfig::Namespace { store, prefix } => {
            ensure!(
                !prefix.is_empty()
                    && !prefix.starts_with('/')
                    && !prefix.ends_with('/')
                    && !prefix.split('/').any(|part| part.is_empty() || part == "." || part == ".."),
                "invalid object-store namespace"
            );
            Arc::new(object_store::prefix::PrefixStore::new(open_store(store)?, Path::parse(prefix)?))
        }
    };
    Ok(store)
}

pub trait Clock: Debug + Send + Sync {
    fn now_ms(&self) -> Result<u64>;
}
#[derive(Debug)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> Result<u64> {
        Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis().try_into()?)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Head {
    pub epoch: u64,
    pub seq: i64,
    pub snapshot_seq: i64,
    pub snapshot: Option<String>,
    pub segments: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Lease {
    pub owner: String,
    pub epoch: u64,
    pub expires_at: u64,
}
#[derive(Debug, Clone)]
struct Owned {
    lease: Lease,
    version: UpdateVersion,
    head_version: UpdateVersion,
    head: Head,
}
#[derive(Debug, thiserror::Error)]
#[error("actor {actor} lease_lost at epoch {epoch}")]
pub(crate) struct LeaseLost {
    pub actor: String,
    pub epoch: u64,
}
pub(crate) struct Stored<T> {
    pub value: T,
    pub version: UpdateVersion,
}
#[derive(Default)]
struct ActorGuards {
    lease: tokio::sync::Mutex<()>,
    head: tokio::sync::Mutex<()>,
}
pub(crate) struct RemoteStore {
    pub store: Arc<dyn ObjectStore>,
    owner: String,
    clock: Arc<dyn Clock>,
    ttl: u64,
    owned: Mutex<HashMap<String, Owned>>,
    guards: Mutex<HashMap<String, Arc<ActorGuards>>>,
}
impl RemoteStore {
    pub fn new(config: &StoreConfig, ttl: Duration, clock: Arc<dyn Clock>, owner: String) -> Result<Self> {
        let ttl: u64 = ttl.as_millis().try_into()?;
        ensure!(ttl >= 3, "lease TTL must be at least 3 ms");
        let store = open_store(config)?;
        Ok(Self { store, owner, clock, ttl, owned: Mutex::new(HashMap::new()), guards: Mutex::new(HashMap::new()) })
    }
    fn guards(&self, id: &str) -> Result<Arc<ActorGuards>> {
        Ok(self.guards.lock().map_err(|_| anyhow::anyhow!("actor guards poisoned"))?.entry(id.into()).or_default().clone())
    }
    fn owned(&self, id: &str) -> Result<Owned> {
        self.owned
            .lock()
            .map_err(|_| anyhow::anyhow!("lease cache poisoned"))?
            .get(id)
            .cloned()
            .ok_or_else(|| LeaseLost { actor: id.into(), epoch: 0 }.into())
    }
    fn lost(&self, id: &str, epoch: u64) -> anyhow::Error {
        LeaseLost { actor: id.into(), epoch }.into()
    }
    pub fn epoch(&self, id: &str) -> Result<u64> {
        Ok(self.owned(id)?.lease.epoch)
    }
    pub fn check(&self, id: &str) -> Result<()> {
        let owned = self.owned(id)?;
        ensure!(self.clock.now_ms()? < owned.lease.expires_at, self.lost(id, owned.lease.epoch));
        Ok(())
    }
    pub(crate) async fn read<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<Stored<T>>> {
        let result = match self.store.get(&Path::from(key)).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let version = UpdateVersion { e_tag: result.meta.e_tag.clone(), version: result.meta.version.clone() };
        ensure!(version.e_tag.is_some() || version.version.is_some(), "object {key} has no conditional-write version");
        Ok(Some(Stored { value: serde_json::from_slice(&result.bytes().await?).with_context(|| format!("decode object {key}"))?, version }))
    }
    pub(crate) async fn write<T: Serialize>(&self, key: &str, value: &T, mode: PutMode) -> Result<UpdateVersion> {
        let result =
            self.store.put_opts(&Path::from(key), serde_json::to_vec(value)?.into(), PutOptions { mode, ..Default::default() }).await?;
        Ok(UpdateVersion { e_tag: result.e_tag, version: result.version })
    }
    pub async fn acquire(&self, id: &str) -> Result<Head> {
        let guards = self.guards(id)?;
        let _lease = guards.lease.lock().await;
        let _head = guards.head.lock().await;
        let now = self.clock.now_ms()?;
        let key = format!("actors/{id}/lease");
        let old = self.read::<Lease>(&key).await?;
        let mut epoch = 1;
        let mode = if let Some(old) = old {
            ensure!(
                old.value.expires_at <= now || old.value.owner == self.owner,
                "actor {id} lease held by {} until {}",
                old.value.owner,
                old.value.expires_at
            );
            epoch = old.value.epoch.checked_add(1).context("lease epoch overflow")?;
            PutMode::Update(old.version)
        } else {
            PutMode::Create
        };
        let lease = Lease { owner: self.owner.clone(), epoch, expires_at: now.checked_add(self.ttl).context("lease expiry overflow")? };
        let version = self.write(&key, &lease, mode).await?;
        let head_key = format!("actors/{id}/head");
        loop {
            let current_lease = self.read::<Lease>(&key).await?.context("claimed lease disappeared")?;
            ensure!(
                current_lease.value.owner == self.owner && current_lease.value.epoch == epoch && self.clock.now_ms()? < lease.expires_at,
                self.lost(id, epoch)
            );
            let old = self.read::<Head>(&head_key).await?;
            let mut head = Head { epoch, seq: 0, snapshot_seq: -1, snapshot: None, segments: Vec::new() };
            let mode = if let Some(old) = old {
                ensure!(old.value.epoch <= epoch, self.lost(id, epoch));
                head = old.value;
                head.epoch = epoch;
                PutMode::Update(old.version)
            } else {
                PutMode::Create
            };
            match self.write(&head_key, &head, mode).await {
                Ok(head_version) => {
                    self.owned
                        .lock()
                        .map_err(|_| anyhow::anyhow!("lease cache poisoned"))?
                        .insert(id.into(), Owned { lease, version, head_version, head: head.clone() });
                    self.check(id)?;
                    return Ok(head);
                }
                Err(error) if conflict(&error) => continue,
                Err(error) => return Err(error),
            }
        }
    }
    pub(crate) async fn has_snapshot(&self, id: &str) -> Result<bool> {
        Ok(self.read::<Head>(&format!("actors/{id}/head")).await?.is_some_and(|head| head.value.snapshot.is_some()))
    }
    pub async fn publish(&self, id: &str, head: Head) -> Result<()> {
        let guards = self.guards(id)?;
        let _head = guards.head.lock().await;
        self.check(id)?;
        self.cas_head(id, head).await
    }
    /// Flush admission probe: caller passes its unchanged cached head, never new history.
    /// Even an expired owner may probe its old version; takeover rejects that CAS.
    pub async fn fence(&self, id: &str, head: Head) -> Result<()> {
        let guards = self.guards(id)?;
        let _head = guards.head.lock().await;
        ensure!(self.owned(id)?.head == head, "actor {id} fence probe must not change history");
        self.cas_head(id, head).await
    }
    async fn cas_head(&self, id: &str, head: Head) -> Result<()> {
        let owned = self.owned(id)?;
        ensure!(head.epoch == owned.lease.epoch, self.lost(id, owned.lease.epoch));
        let key = format!("actors/{id}/head");
        let version = match self.write(&key, &head, PutMode::Update(owned.head_version)).await {
            Ok(version) => version,
            Err(error) if conflict(&error) => {
                self.owned
                    .lock()
                    .map_err(|_| anyhow::anyhow!("lease cache poisoned"))?
                    .get_mut(id)
                    .context("lease cache missing")?
                    .lease
                    .expires_at = 0;
                return Err(self.lost(id, owned.lease.epoch).context(error));
            }
            Err(error) => {
                // A PUT can become durable before its response is lost. Recover
                // that acknowledgement only from the exact proposed head.
                match self.read::<Head>(&key).await {
                    Ok(Some(stored)) if stored.value == head => stored.version,
                    _ => return Err(error),
                }
            }
        };
        {
            let mut cache = self.owned.lock().map_err(|_| anyhow::anyhow!("lease cache poisoned"))?;
            let owned = cache.get_mut(id).context("lease cache missing")?;
            owned.head_version = version;
            owned.head = head;
        }
        self.check(id)
    }
    pub async fn renew(&self, id: &str) -> Result<()> {
        let guards = self.guards(id)?;
        let _lease = guards.lease.lock().await;
        self.check(id)?;
        let mut owned = self.owned(id)?;
        owned.lease.expires_at = self.clock.now_ms()?.checked_add(self.ttl).context("lease expiry overflow")?;
        match self.write(&format!("actors/{id}/lease"), &owned.lease, PutMode::Update(owned.version.clone())).await {
            Ok(version) => {
                {
                    let mut cache = self.owned.lock().map_err(|_| anyhow::anyhow!("lease cache poisoned"))?;
                    let current = cache.get_mut(id).context("lease cache missing")?;
                    // Publication may have advanced the head while renewal was in flight.
                    // A concurrent fence failure permanently invalidates this ownership.
                    ensure!(current.lease.expires_at != 0, self.lost(id, owned.lease.epoch));
                    current.lease = owned.lease;
                    current.version = version;
                }
                self.check(id)
            }
            Err(error) => {
                self.owned
                    .lock()
                    .map_err(|_| anyhow::anyhow!("lease cache poisoned"))?
                    .get_mut(id)
                    .context("lease cache missing")?
                    .lease
                    .expires_at = 0;
                Err(self.lost(id, owned.lease.epoch).context(error))
            }
        }
    }
    pub async fn release(&self, id: &str) -> Result<()> {
        let guards = self.guards(id)?;
        let _lease = guards.lease.lock().await;
        let owned = self.owned(id)?;
        let key = format!("actors/{id}/lease");
        let mut current = self.read::<Lease>(&key).await?.context("lease disappeared during release")?;
        ensure!(current.value.owner == self.owner && current.value.epoch == owned.lease.epoch, self.lost(id, owned.lease.epoch));
        current.value.expires_at = 0;
        self.write(&key, &current.value, PutMode::Update(current.version)).await?;
        self.owned.lock().map_err(|_| anyhow::anyhow!("lease cache poisoned"))?.remove(id);
        Ok(())
    }
    pub async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        match self.store.put_opts(&Path::from(key), bytes.clone().into(), PutMode::Create.into()).await {
            Ok(_) => {}
            Err(object_store::Error::AlreadyExists { .. }) => {
                ensure!(self.get(key).await? == bytes, "immutable object {key} differs from existing bytes")
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    pub async fn get(&self, key: &str) -> Result<Vec<u8>> {
        Ok(self.store.get(&Path::from(key)).await?.bytes().await?.to_vec())
    }
}
fn conflict(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<object_store::Error>(),
        Some(object_store::Error::Precondition { .. } | object_store::Error::AlreadyExists { .. })
    )
}

#[cfg(test)]
mod namespace_tests {
    use super::*;
    #[tokio::test]
    async fn object_namespaces_separate_conditional_writes_and_listings() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let base = StoreConfig::Local { path: directory.path().to_owned() };
        let alice = open_store(&StoreConfig::Namespace { store: Box::new(base.clone()), prefix: "tenants/alice".into() })?;
        let bob = open_store(&StoreConfig::Namespace { store: Box::new(base.clone()), prefix: "tenants/bob".into() })?;
        let key = Path::from("actors/same/head");
        alice.put_opts(&key, b"alice".to_vec().into(), PutOptions { mode: PutMode::Create, ..Default::default() }).await?;
        assert!(matches!(bob.get(&key).await, Err(object_store::Error::NotFound { .. })));
        bob.put_opts(&key, b"bob".to_vec().into(), PutOptions { mode: PutMode::Create, ..Default::default() }).await?;
        assert_eq!(alice.get(&key).await?.bytes().await?.as_ref(), b"alice");
        assert_eq!(bob.get(&key).await?.bytes().await?.as_ref(), b"bob");
        let listing = alice.list_with_delimiter(Some(&Path::from("actors/same"))).await?;
        assert_eq!(listing.objects.len(), 1);
        assert_eq!(listing.objects[0].location, key);
        assert!(open_store(&base)?.get(&Path::from("tenants/alice/actors/same/head")).await.is_ok());
        assert!(open_store(&StoreConfig::Namespace { store: Box::new(base), prefix: "../bob".into() }).is_err());
        Ok(())
    }
}
