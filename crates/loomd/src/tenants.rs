use crate::{Args, cluster_worker::ClusterWorker};
use anyhow::{Context, Result, ensure};
use loom_api::{Authorizer, Service, ServiceDirectory, TenantId, WebSocketHub};
use serde::Deserialize;
use std::{collections::BTreeSet, path::Path, sync::Arc};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessPreset {
    #[serde(default)]
    tenant: TenantId,
    name: String,
    spec: loom_process::ProcessSpec,
    sandbox: loom_process::ProcessSandbox,
}

pub async fn open(
    args: &Args,
    authorizer: &Authorizer,
    config: loom_actor::Config,
) -> Result<ServiceDirectory> {
    let tenants = authorizer.tenants();
    let presets: Vec<ProcessPreset> = match &args.processes_file {
        Some(path) => serde_json::from_slice(
            &tokio::fs::read(path)
                .await
                .context("read --processes-file")?,
        )?,
        None => Vec::new(),
    };
    let mut names = BTreeSet::new();
    for preset in &presets {
        ensure!(
            tenants.contains(&preset.tenant),
            "process preset tenant {} has no configured token",
            preset.tenant
        );
        ensure!(!preset.name.is_empty(), "process preset name is empty");
        ensure!(
            names.insert(format!("{}\0{}", preset.tenant, preset.name)),
            "duplicate process preset name in tenant"
        );
    }
    let parent = args.db.parent().unwrap_or(Path::new("."));
    let tenant_root = args
        .tenant_root
        .clone()
        .unwrap_or_else(|| parent.join("tenants"));
    let mut protected = vec![args.db.clone(), tenant_root.clone()];
    protected.push(
        args.actors_dir
            .clone()
            .unwrap_or_else(|| parent.join("actors")),
    );
    protected.push(
        args.backup_dir
            .clone()
            .unwrap_or_else(|| parent.join("backups")),
    );
    protected.extend(args.cluster_key_file.iter().cloned());
    protected.extend(args.tokens_file.iter().cloned());
    for preset in &presets {
        preset.sandbox.validate(&preset.spec)?;
        validate_process_paths(preset, &protected, &args.root)?;
    }
    let engine = Arc::new(loom_v8::V8Engine::new(loom_v8::Limits::default())?);
    let root = args.root.canonicalize()?;
    let mut services = Vec::new();
    for tenant in tenants {
        let directory = tenant_root.join(tenant.as_str());
        let default = tenant == TenantId::default();
        let database = if default {
            args.db.clone()
        } else {
            directory.join("loom.sqlite")
        };
        let actors_dir = if default {
            args.actors_dir
                .clone()
                .unwrap_or_else(|| parent.join("actors"))
        } else {
            directory.join("actors")
        };
        let backups = if default {
            args.backup_dir
                .clone()
                .unwrap_or_else(|| parent.join("backups"))
        } else {
            directory.join("backups")
        };
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut service = Service::new_with_engine(
            loom_store::Store::open(database)?,
            root.clone(),
            vec![
                loom_proto::Lang::Rust,
                loom_proto::Lang::JavaScript,
                loom_proto::Lang::TypeScript,
            ],
            Some(engine.clone()),
        )?
        .with_tenant(tenant.clone())
        .with_backup_directory(backups);
        if !default {
            service = service.with_build_directory(directory.join("build"));
        }
        let hub = WebSocketHub::new();
        let mut drivers = vec![hub.driver()];
        let mut behaviors: Vec<Arc<dyn loom_actor::Behavior>> =
            vec![Arc::new(loom_actor::process_actor::ProcessActor)];
        if let Some(executable) = &args.docker_executable {
            let driver = loom_actor::drivers::container::ContainerDriver::new(
                service.runtime.process_supervisor(),
                loom_actor::drivers::container::DockerConfig {
                    executable: executable.clone(),
                    host: args.docker_host.clone(),
                    root: root.clone(),
                    tenant: tenant.to_string(),
                    node: format!(
                        "{}:{}",
                        native_resource_owner(&service.store)?,
                        canonical_location(&actors_dir)?.display()
                    ),
                },
            )?;
            driver.recover().await?;
            drivers.push(Arc::new(driver));
            behaviors.push(Arc::new(loom_actor::container_actor::ContainerActor));
        }
        for preset in presets.iter().filter(|preset| preset.tenant == tenant) {
            let hash = process_hash(preset)?;
            drivers.push(Arc::new(loom_actor::drivers::process::ProcessDriver::new(
                hash,
                service.runtime.process_supervisor(),
                preset.spec.clone(),
                preset.sandbox.clone(),
            )));
        }
        let processes = presets
            .iter()
            .filter(|preset| preset.tenant == tenant)
            .map(|preset| Ok((preset.name.clone(), process_hash(preset)?)))
            .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
        service = service
            .with_process_presets(processes)?
            .with_native_drivers(behaviors, drivers)?
            .with_websockets(hub);
        let node = loom_actor::Node::new(
            actors_dir,
            service.actor_registry(),
            Arc::new(loom_actor::DefaultEffects),
            tenant_config(&config, &tenant),
        )
        .await?;
        for preset in presets.iter().filter(|preset| preset.tenant == tenant) {
            let driver = process_hash(preset)?;
            if let Some(id) = node.whereis(&preset.name).await? {
                ensure!(
                    node.info(&id).await?.behavior_hash == loom_actor::process_actor::HASH,
                    "registered process name {} is owned by another behavior",
                    preset.name
                );
                validate_registered_process(&node, &id, &preset.name, &driver).await?;
            } else {
                let init = serde_json::to_vec(
                    &serde_json::json!({"process":preset.name,"expected_driver":driver}),
                )?;
                let id = node
                    .spawn_root(loom_actor::process_actor::HASH, &init)
                    .await?;
                node.register(&preset.name, &id).await?;
            }
        }
        services.push(Arc::new(service.with_actors(node)));
    }
    ServiceDirectory::new(services)
}
fn tenant_config(base: &loom_actor::Config, tenant: &TenantId) -> loom_actor::Config {
    let mut config = base.clone();
    // Default keeps existing actor keys, leases, and persisted signing identity.
    // Named tenants have separate authority even when sharing one S3 bucket and
    // listener address; ingress bearer selection then chooses exactly one node.
    if tenant != &TenantId::default() {
        if let Some(cluster) = config.cluster.as_mut() {
            cluster.key = *blake3::keyed_hash(
                &cluster.key,
                format!("loom-tenant-cluster-v1:{}", tenant.as_str()).as_bytes(),
            )
            .as_bytes();
        }
        if let Some(store) = config.store.take() {
            config.store = Some(loom_actor::StoreConfig::Namespace {
                store: Box::new(store),
                prefix: format!("tenants/{}", tenant.as_str()),
            });
        }
    }
    config
}

fn process_hash(preset: &ProcessPreset) -> Result<String> {
    let bytes = serde_json::to_vec(
        &serde_json::json!({"abi":"process-v1", "tenant":preset.tenant, "name":preset.name, "spec":preset.spec, "sandbox":preset.sandbox}),
    )?;
    Ok(format!("process:{}", blake3::hash(&bytes)))
}

pub struct RunningTenant {
    node: loom_actor::Node,
    worker: ClusterWorker,
}
pub fn start_workers(services: &ServiceDirectory) -> Vec<RunningTenant> {
    services
        .services()
        .filter_map(|service| service.actor_node())
        .map(|node| RunningTenant {
            node: node.clone(),
            worker: ClusterWorker::start(node),
        })
        .collect()
}
pub async fn finish_workers(workers: Vec<RunningTenant>) -> Result<()> {
    let mut failure = None;
    for running in workers {
        if let Err(error) = running.worker.finish(&running.node).await {
            failure.get_or_insert(error);
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn process_preset(root: &Path) -> Result<ProcessPreset> {
        Ok(serde_json::from_value(serde_json::json!({
            "tenant":"alice", "name":"echo",
            "spec":{"machine":"local","program":"/bin/echo","args":[],"cwd":root,"root":root},
            "sandbox":{"readonly":[],"network":false}
        }))?)
    }
    #[test]
    fn process_mounts_cannot_expose_tenant_state() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let state = directory.path().join("state");
        let source = directory.path().join("source");
        let workspace = directory.path().join("workspace");
        for path in [&state, &source, &workspace] {
            std::fs::create_dir_all(path)?;
        }
        let mut preset = process_preset(&workspace)?;
        validate_process_paths(&preset, &[state.clone()], &source)?;
        preset.sandbox.readonly.push(state.clone());
        assert!(validate_process_paths(&preset, &[state.clone()], &source).is_err());
        preset.sandbox.readonly.clear();
        preset.spec.root = directory.path().to_path_buf();
        assert!(validate_process_paths(&preset, &[state.clone()], &source).is_err());
        preset.spec.root = state.join("child");
        assert!(validate_process_paths(&preset, &[state], &source).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn pending_named_process_survives_restart_before_first_worker_turn() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let service = Service::new(
            loom_store::Store::memory()?,
            directory.path().to_path_buf(),
            vec![loom_proto::Lang::Rust],
        )?
        .with_native_drivers(
            vec![Arc::new(loom_actor::process_actor::ProcessActor)],
            Vec::new(),
        )?;
        let path = directory.path().join("actors");
        let node = loom_actor::Node::new(
            &path,
            service.actor_registry(),
            Arc::new(loom_actor::DefaultEffects),
            Default::default(),
        )
        .await?;
        let id = node
            .spawn_root(
                loom_actor::process_actor::HASH,
                br#"{"process":"echo","expected_driver":"expected-driver"}"#,
            )
            .await?;
        node.register("echo", &id).await?;
        validate_registered_process(&node, &id, "echo", "expected-driver").await?;
        assert!(
            validate_registered_process(&node, &id, "another-preset", "expected-driver")
                .await
                .is_err()
        );
        assert!(
            validate_registered_process(&node, &id, "echo", "changed-policy-driver")
                .await
                .is_err()
        );
        drop(node);
        let reopened = loom_actor::Node::new(
            &path,
            service.actor_registry(),
            Arc::new(loom_actor::DefaultEffects),
            Default::default(),
        )
        .await?;
        assert_eq!(reopened.whereis("echo").await?, Some(id.clone()));
        validate_registered_process(&reopened, &id, "echo", "expected-driver").await?;
        // Validation did not execute the init or grant an unregistered driver.
        let actor = reopened.open(&id).await?;
        assert!(
            actor
                .inspect_sql("SELECT preset FROM process_state", Vec::new())
                .await?
                .rows
                .is_empty()
        );
        Ok(())
    }
    #[test]
    fn native_resource_owners_are_stable_and_distinct_across_stores() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("owner.sqlite");
        let first = loom_store::Store::open(&path)?;
        let owner = native_resource_owner(&first)?;
        assert_eq!(owner, native_resource_owner(&first)?);
        drop(first);
        let reopened = loom_store::Store::open(&path)?;
        assert_eq!(owner, native_resource_owner(&reopened)?);
        assert_ne!(owner, native_resource_owner(&loom_store::Store::memory()?)?);
        Ok(())
    }
    #[test]
    fn process_identity_includes_sandbox_authority() -> Result<()> {
        let mut preset = process_preset(Path::new("/workspace"))?;
        let original = process_hash(&preset)?;
        preset.sandbox.network = true;
        assert_ne!(original, process_hash(&preset)?);
        preset.sandbox.network = false;
        preset.sandbox.readonly.push("/usr".into());
        assert_ne!(original, process_hash(&preset)?);
        Ok(())
    }
    #[test]
    fn cluster_tenants_have_distinct_keys_and_storage_with_stable_default() -> Result<()> {
        let base = loom_actor::Config {
            store: Some(loom_actor::StoreConfig::S3 {
                endpoint: "http://localhost:9000".into(),
                bucket: "actors".into(),
                region: "test".into(),
            }),
            cluster: Some(loom_actor::ClusterConfig {
                node_id: "node".into(),
                addr: "localhost:8787".into(),
                key: [7; 32],
            }),
            ..Default::default()
        };
        let default = tenant_config(&base, &TenantId::default());
        assert_eq!(default.cluster.context("cluster missing")?.key, [7; 32]);
        assert!(matches!(
            default.store,
            Some(loom_actor::StoreConfig::S3 { .. })
        ));
        let alice = tenant_config(&base, &TenantId::new("alice")?);
        let bob = tenant_config(&base, &TenantId::new("bob")?);
        assert_ne!(
            alice.cluster.context("alice cluster missing")?.key,
            bob.cluster.context("bob cluster missing")?.key
        );
        let Some(loom_actor::StoreConfig::Namespace { prefix, store }) = alice.store else {
            anyhow::bail!("tenant prefix missing");
        };
        assert_eq!(prefix, "tenants/alice");
        assert!(matches!(*store, loom_actor::StoreConfig::S3 { .. }));
        Ok(())
    }
}

fn canonical_location(path: &Path) -> Result<std::path::PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let absolute = std::path::absolute(path)?;
    let parent = absolute
        .parent()
        .context("invalid process authority path")?;
    let name = absolute
        .file_name()
        .context("invalid process authority path")?;
    Ok(canonical_location(parent)?.join(name))
}
fn validate_process_paths(
    preset: &ProcessPreset,
    protected: &[std::path::PathBuf],
    source_root: &Path,
) -> Result<()> {
    let source_root = canonical_location(source_root)?;
    for granted in std::iter::once(&preset.spec.root).chain(preset.sandbox.readonly.iter()) {
        let granted = canonical_location(granted)?;
        ensure!(
            !source_root.starts_with(&granted),
            "process preset {} exposes daemon source through {}",
            preset.name,
            granted.display()
        );
        for private in protected {
            let private = canonical_location(private)?;
            ensure!(
                !private.starts_with(&granted) && !granted.starts_with(&private),
                "process preset {} mounts private daemon storage through {}",
                preset.name,
                granted.display()
            );
        }
    }
    Ok(())
}

/// Registry construction precedes Node::new, so its root actor does not yet
/// exist on first start. Persist an equally strong owner identity in the tenant
/// database: paths and configured cluster names can collide across hosts using
/// the same Docker daemon. Recovery must only remove this owner's resources.
fn native_resource_owner(store: &loom_store::Store) -> Result<String> {
    store.with_connection(|connection| {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS native_resource_owner (singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL);
             INSERT OR IGNORE INTO native_resource_owner(singleton,id) VALUES(1,lower(hex(randomblob(32))));"
        )?;
        Ok(connection.query_row("SELECT id FROM native_resource_owner WHERE singleton=1", [], |row| row.get(0))?)
    })
}

async fn validate_registered_process(
    node: &loom_actor::Node,
    id: &str,
    name: &str,
    driver: &str,
) -> Result<()> {
    let actor = node.open(id).await?;
    let rows = actor
        .inspect_sql("SELECT preset FROM process_state WHERE id=1", Vec::new())
        .await?;
    if let Some(row) = rows.rows.first() {
        ensure!(
            rows.rows.len() == 1 && row.get::<String>(0)? == driver,
            "process preset {name} changed; stop and explicitly replace its registered actor"
        );
        return Ok(());
    }
    // A crash can occur after host spawn/name registration but before the first
    // worker turn initializes process_state. Validate its durable spawn input;
    // startup must neither reject that pending turn nor execute it prematurely.
    let root = node.open(&node.root()).await?;
    let children = root
        .inspect_sql(
            "SELECT init FROM children WHERE id=? AND behavior_hash=?",
            vec![
                id.to_owned().into(),
                loom_actor::process_actor::HASH.to_owned().into(),
            ],
        )
        .await?;
    ensure!(
        children.rows.len() == 1,
        "pending process {name} is not owned by this node root"
    );
    let init: serde_json::Value = serde_json::from_slice(&children.rows[0].get::<Vec<u8>>(0)?)?;
    ensure!(
        init == serde_json::json!({"process":name,"expected_driver":driver}),
        "pending process {name} has different spawn authority"
    );
    Ok(())
}
