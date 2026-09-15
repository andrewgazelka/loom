mod registry;
use loom_actor::{
    Actor, Config, DefaultEffects, Node,
    container_actor::{ContainerActor, HASH},
    drivers::container::{ContainerDriver, ContainerSpec, DockerConfig},
};
use loom_process::Supervisor;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

fn docker_config(root: &std::path::Path, node: &str) -> DockerConfig {
    DockerConfig {
        executable: std::env::var_os("LOOM_TEST_DOCKER").expect("LOOM_TEST_DOCKER must name the real Docker executable").into(),
        host: std::env::var("LOOM_TEST_DOCKER_HOST").ok(),
        root: root.into(),
        tenant: "container-test".into(),
        node: node.into(),
    }
}
async fn bounded<T>(label: &str, future: impl std::future::Future<Output = T>) -> T {
    if label != "run_until_idle" {
        eprintln!("container e2e: {label}");
    }
    let value = tokio::time::timeout(Duration::from_secs(30), future).await.unwrap_or_else(|_| panic!("container e2e timed out: {label}"));
    if label != "run_until_idle" {
        eprintln!("container e2e complete: {label}");
    }
    value
}
async fn drain(node: &Node, actor: &Actor, phase: &str) {
    eprintln!("container e2e: await phase {phase}");
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            bounded("run_until_idle", node.run_until_idle()).await.unwrap();
            let rows = actor.sql("SELECT phase FROM process_state", ()).await.unwrap();
            if let Some(row) = rows.rows.first() {
                let actual: String = row.get(0).unwrap();
                if actual == phase {
                    break;
                }
                if matches!(actual.as_str(), "failed" | "interrupted" | "cancelled" | "completed") {
                    let error = actor.sql("SELECT error FROM process_state", ()).await.unwrap().rows[0].get::<Option<String>>(0).unwrap();
                    panic!("container expected {phase}, reached {actual}: {error:?}; events={:?}", events(actor).await);
                }
            }
            let failures = actor.sql("SELECT error FROM dead_letters", ()).await.unwrap();
            assert!(
                failures.rows.is_empty(),
                "container actor dead letters: {:?}",
                failures.rows.iter().map(|row| row.get::<String>(0).unwrap()).collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("container failed to reach {phase}"));
}
async fn events(actor: &Actor) -> Vec<Value> {
    actor
        .sql("SELECT body FROM process_events ORDER BY seq", ())
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|r| serde_json::from_slice(&r.get::<Vec<u8>>(0).unwrap()).unwrap())
        .collect()
}
#[test]
fn container_options_reject_escape_hatches_and_unbounded_limits() {
    for bad in [
        json!({"image":"alpine","privileged":true}),
        json!({"image":"alpine","mounts":["/:/host"]}),
        json!({"image":"alpine","network":"host"}),
        json!({"image":"alpine","host":"unix:///tmp/docker.sock"}),
    ] {
        assert!(serde_json::from_value::<ContainerSpec>(bad).is_err());
    }
    for bad in [
        json!({"image":"--privileged"}),
        json!({"image":"alpine","memoryMb":0}),
        json!({"image":"alpine","cpus":0}),
        json!({"image":"alpine","pids":0}),
        json!({"image":"alpine","ttlMs":0}),
    ] {
        assert!(serde_json::from_value::<ContainerSpec>(bad).unwrap().validate().is_err());
    }
}
#[tokio::test]
async fn real_docker_echo_limits_exit_cancel_ttl_and_recovery() {
    let root = tempfile::tempdir().unwrap();
    let config = docker_config(root.path(), root.path().to_str().unwrap());
    let supervisor = Supervisor::new(loom_store::Store::open(root.path().join("process.sqlite")).unwrap()).unwrap();
    let driver = Arc::new(ContainerDriver::new(supervisor, config.clone()).unwrap());
    bounded("driver.recover", driver.recover()).await.unwrap();
    let registry = Arc::new(
        loom_actor::CompositeRegistry::new(Arc::new(registry::Registry::new()), vec![Arc::new(ContainerActor)], vec![driver.clone()])
            .unwrap(),
    );
    let node = Node::new(root.path().join("actors"), registry.clone(), Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let image = std::env::var("LOOM_TEST_CONTAINER_IMAGE").unwrap_or_else(|_| "alpine:3.22".into());
    let id=node.spawn_root(HASH,&serde_json::to_vec(&json!({"container":{"image":image,"command":"/bin/sh","args":["-c","cat; printf diagnostic >&2; exit 7"],"network":"none","memoryMb":64,"cpus":0.5,"pids":32}})).unwrap()).await.unwrap();
    let actor = node.open(&id).await.unwrap();
    drain(&node, &actor, "running").await;
    let started = events(&actor).await.into_iter().find(|v| v["type"] == "process.started").unwrap();
    assert!(started["image_id"].as_str().unwrap().starts_with("sha256:"));
    let container = started["container_id"].as_str().unwrap();
    let inspect = docker(&config, &["inspect", container]).await;
    let inspect: Value = serde_json::from_str(&inspect).unwrap();
    assert_eq!(inspect[0]["HostConfig"]["Memory"], 64 * 1024 * 1024);
    assert_eq!(inspect[0]["HostConfig"]["NanoCpus"], 500_000_000);
    assert_eq!(inspect[0]["HostConfig"]["PidsLimit"], 32);
    assert_eq!(inspect[0]["HostConfig"]["NetworkMode"], "none");
    node.send(&id, "input", &serde_json::to_vec(&json!({"type":"stdin","data":"雪😀 café\n"})).unwrap()).await.unwrap();
    bounded("run_until_idle", node.run_until_idle()).await.unwrap();
    actor.sql("UPDATE outbox SET delivered=0 WHERE json_extract(CAST(msg AS TEXT),'$.type')='stdin'", ()).await.unwrap();
    bounded("dedupe pump", node.pump(&id)).await.unwrap();
    node.send(&id, "eof", br#"{"type":"close_stdin"}"#).await.unwrap();
    drain(&node, &actor, "completed").await;
    let stdout: Vec<u8> = events(&actor)
        .await
        .iter()
        .filter(|v| v["stream"] == "stdout")
        .flat_map(|v| serde_json::from_value::<Vec<u8>>(v["bytes"].clone()).unwrap())
        .collect();
    assert_eq!(stdout, "雪😀 café\n".as_bytes());
    assert_eq!(actor.sql("SELECT code FROM process_state", ()).await.unwrap().rows[0].get::<i64>(0).unwrap(), 7);
    assert!(docker(&config, &["ps", "-aq", "--filter", &format!("id={container}")]).await.is_empty());
    for ttl in [false, true] {
        let id=node.spawn_root(HASH,&serde_json::to_vec(&json!({"container":{"image":image,"command":"/bin/sh","args":["-c","sleep 60"],"network":"none","ttlMs":if ttl {2000} else {60000}}})).unwrap()).await.unwrap();
        let actor = node.open(&id).await.unwrap();
        drain(&node, &actor, "running").await;
        if !ttl {
            node.send(&id, "cancel", br#"{"type":"cancel"}"#).await.unwrap();
        }
        drain(&node, &actor, if ttl { "failed" } else { "cancelled" }).await;
    }
    let start_live = || {
        serde_json::to_vec(
            &json!({"container":{"image":image,"command":"/bin/sh","args":["-c","printf ready; sleep 60"],"network":"none"}}),
        )
        .unwrap()
    };
    let stopped_id = node.spawn_root(HASH, &start_live()).await.unwrap();
    let stopped_actor = node.open(&stopped_id).await.unwrap();
    drain(&node, &stopped_actor, "running").await;
    let stopped_container = events(&stopped_actor).await.into_iter().find(|v| v["type"] == "process.started").unwrap()["container_id"]
        .as_str()
        .unwrap()
        .to_owned();
    bounded("owner.stop", node.stop(&stopped_id, "owner stopped")).await.unwrap();
    bounded("run_until_idle", node.run_until_idle()).await.unwrap();
    assert!(docker(&config, &["ps", "-aq", "--filter", &format!("id={stopped_container}")]).await.is_empty());
    let resumed_id = node.spawn_root(HASH, &start_live()).await.unwrap();
    let resumed_actor = node.open(&resumed_id).await.unwrap();
    drain(&node, &resumed_actor, "running").await;
    let resumed_container = events(&resumed_actor).await.into_iter().find(|v| v["type"] == "process.started").unwrap()["container_id"]
        .as_str()
        .unwrap()
        .to_owned();
    bounded("node.close active container", node.close()).await.unwrap();
    assert!(docker(&config, &["ps", "-aq", "--filter", &format!("id={resumed_container}")]).await.is_empty());
    drop(resumed_actor);
    drop(stopped_actor);
    drop(actor);
    drop(node);
    bounded("driver.recover", driver.recover()).await.unwrap();
    let reopened = Node::new(root.path().join("actors"), registry, Arc::new(DefaultEffects), Config::default()).await.unwrap();
    let resumed_actor = reopened.open(&resumed_id).await.unwrap();
    drain(&reopened, &resumed_actor, "interrupted").await;
    assert_eq!(
        events(&resumed_actor).await.iter().filter(|v| v["type"] == "process.started").count(),
        1,
        "restart must not reexecute container command"
    );
    bounded("reopened.close", reopened.close()).await.unwrap();
    assert!(
        docker(&config, &["ps", "-aq", "--filter", &format!("label=loom.node={}", blake3::hash(config.node.as_bytes()).to_hex())])
            .await
            .is_empty()
    );
    // Recovery owns only this node's durable namespace, even when another node
    // uses the same tenant name on the same Docker daemon.
    let own_label = format!("loom.node={}", blake3::hash(config.node.as_bytes()).to_hex());
    let own = docker(
        &config,
        &[
            "create",
            "--label",
            "loom.resource=container-v1",
            "--label",
            "loom.tenant=container-test",
            "--label",
            &own_label,
            &image,
            "true",
        ],
    )
    .await;
    let foreign = docker(
        &config,
        &[
            "create",
            "--label",
            "loom.resource=container-v1",
            "--label",
            "loom.tenant=container-test",
            "--label",
            "loom.node=other-live-node",
            &image,
            "true",
        ],
    )
    .await;
    bounded("driver.recover", driver.recover()).await.unwrap();
    assert!(docker(&config, &["ps", "-aq", "--filter", &format!("id={own}")]).await.is_empty());
    assert!(!docker(&config, &["ps", "-aq", "--filter", &format!("id={foreign}")]).await.is_empty());
    docker(&config, &["rm", "-f", &foreign]).await;
}
async fn docker(config: &DockerConfig, args: &[&str]) -> String {
    let mut cmd = tokio::process::Command::new(&config.executable);
    cmd.env_clear();
    if let Some(host) = &config.host {
        cmd.args(["--host", host]);
    }
    let output = bounded(&format!("docker {}", args.join(" ")), cmd.kill_on_drop(true).args(args).output()).await.unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().into()
}
