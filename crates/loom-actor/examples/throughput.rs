//! Message throughput of the actor runtime: one message = one transaction.
//! Run: cargo run --release -p loom-actor --example throughput
use async_trait::async_trait;
use loom_actor::{
    Behavior, Cap, ChildSpec, ChildType, Config, Ctx, DefaultEffects, Io, Node, Rights, Trap,
    builtin::{BehaviorInfo, Counter},
};
use std::{collections::HashMap, sync::Arc, time::Instant};

struct Registry(HashMap<String, Arc<dyn Behavior>>);
#[async_trait]
impl loom_actor::Registry for Registry {
    async fn resolve(&self, reference: &str) -> anyhow::Result<Arc<dyn Behavior>> {
        self.0.get(reference).cloned().ok_or_else(|| anyhow::anyhow!("unknown behavior {reference}"))
    }
    async fn behaviors(&self) -> anyhow::Result<Vec<BehaviorInfo>> {
        Ok(self.0.iter().map(|(h, b)| BehaviorInfo { hash: h.clone(), description: b.description().to_owned() }).collect())
    }
}

/// Sends every message to every target: the "broadcast to N players" shape.
struct Broadcast {
    targets: Vec<Cap>,
}
#[async_trait]
impl Behavior for Broadcast {
    fn hash(&self) -> &str {
        "broadcast-v1"
    }
    fn schema(&self) -> &str {
        ""
    }
    async fn handle(&self, cx: &mut Ctx<'_>, msg: &[u8]) -> Result<(), Trap> {
        for t in &self.targets {
            cx.send(t, msg).await?;
        }
        Ok(())
    }
}

/// Handler with no SQL write at all: the floor of one transaction.
struct Noop;
#[async_trait]
impl Behavior for Noop {
    fn hash(&self) -> &str {
        "noop-v1"
    }
    fn schema(&self) -> &str {
        ""
    }
    async fn handle(&self, _cx: &mut Ctx<'_>, _msg: &[u8]) -> Result<(), Trap> {
        Ok(())
    }
}

fn spec(hash: &str) -> ChildSpec {
    ChildSpec::new(hash, b"", ChildType::Worker)
}

async fn scenario(io: Io, label: &str, n: usize, fanout: usize) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // Stage 1: node with only static behaviors; receivers spawned first so caps exist.
    let mut reg: HashMap<String, Arc<dyn Behavior>> = HashMap::new();
    reg.insert("counter-v1".into(), Arc::new(Counter::plain()));
    reg.insert("noop-v1".into(), Arc::new(Noop));
    let config = Config { io, ..Config::default() };
    let node = Node::new(dir.path(), Arc::new(Registry(reg.clone())), Arc::new(DefaultEffects), config.clone()).await?;
    let root = node.root();
    let counter = node.spawn(&root, &spec("counter-v1")).await?;
    let noop = node.spawn(&root, &spec("noop-v1")).await?;
    let mut receivers = Vec::new();
    for _ in 0..fanout {
        receivers.push(node.spawn(&root, &spec("noop-v1")).await?);
    }
    node.run_until_idle().await?;

    // A: external inject -> one handler txn each (INSERT into entries).
    let t = Instant::now();
    for i in 0..n {
        node.send(&counter, &format!("k{i}"), b"x").await?;
    }
    let inject = t.elapsed();
    let t = Instant::now();
    let processed = node.run_until_idle().await?;
    let handle = t.elapsed();
    println!(
        "{label} inject_only: {n} sends in {:.1} ms = {:.0} msg/s ({:.0} us/msg)",
        inject.as_secs_f64() * 1e3,
        n as f64 / inject.as_secs_f64(),
        inject.as_micros() as f64 / n as f64
    );
    println!(
        "{label} handle_sql_insert: processed={processed} in {:.1} ms = {:.0} msg/s ({:.0} us/msg)",
        handle.as_secs_f64() * 1e3,
        n as f64 / handle.as_secs_f64(),
        handle.as_micros() as f64 / n as f64
    );

    // B: noop handler, the transaction floor.
    for i in 0..n {
        node.send(&noop, &format!("k{i}"), b"x").await?;
    }
    let t = Instant::now();
    let processed = node.run_until_idle().await?;
    let handle = t.elapsed();
    println!(
        "{label} handle_noop: processed={processed} in {:.1} ms = {:.0} msg/s ({:.0} us/msg)",
        handle.as_secs_f64() * 1e3,
        n as f64 / handle.as_secs_f64(),
        handle.as_micros() as f64 / n as f64
    );

    // C: actor-to-actor hop (A sends to B on every message) and fan-out to `fanout` receivers.
    // Caps are minted before the node closes; the second node (same dir) resolves the new behaviors.
    let mut caps = Vec::new();
    for r in &receivers {
        caps.push(node.cap_for(r, Rights::SEND).await?);
    }
    let b_cap = node.cap_for(&noop, Rights::SEND).await?;
    node.close().await?;
    if io == Io::Memory {
        // A second in-memory node on the same dir starts empty; the hop and broadcast
        // cases need the receivers, so they run in file mode only.
        return Ok(());
    }
    reg.insert("counter-fwd".into(), Arc::new(Counter { hash: "counter-fwd", target: Some(b_cap), ..Counter::plain() }));
    reg.insert("broadcast-v1".into(), Arc::new(Broadcast { targets: caps }));
    let node = Node::new(dir.path(), Arc::new(Registry(reg)), Arc::new(DefaultEffects), config).await?;
    let fwd = node.spawn(&root, &spec("counter-fwd")).await?;
    let bcast = node.spawn(&root, &spec("broadcast-v1")).await?;
    node.run_until_idle().await?;

    for i in 0..n {
        node.send(&fwd, &format!("h{i}"), b"x").await?;
    }
    let t = Instant::now();
    let processed = node.run_until_idle().await?;
    let hop = t.elapsed();
    println!(
        "{label} hop_a_to_b: processed={processed} (expect {}) in {:.1} ms = {:.0} hops/s ({:.0} us/hop)",
        2 * n,
        hop.as_secs_f64() * 1e3,
        n as f64 / hop.as_secs_f64(),
        hop.as_micros() as f64 / n as f64
    );

    let m = n / fanout.max(1);
    for i in 0..m {
        node.send(&bcast, &format!("b{i}"), b"x").await?;
    }
    let t = Instant::now();
    let processed = node.run_until_idle().await?;
    let bc = t.elapsed();
    let deliveries = m * fanout;
    println!(
        "{label} broadcast_{fanout}: {m} msgs -> {deliveries} deliveries, processed={processed} in {:.1} ms = {:.0} deliveries/s ({:.0} us/delivery)",
        bc.as_secs_f64() * 1e3,
        deliveries as f64 / bc.as_secs_f64(),
        bc.as_micros() as f64 / deliveries as f64
    );
    node.close().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let n: usize = std::env::var("N").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    let fanout: usize = std::env::var("FANOUT").ok().and_then(|v| v.parse().ok()).unwrap_or(20);
    println!(
        "host={} n={n} fanout={fanout}",
        std::process::Command::new("hostname").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default()
    );
    scenario(Io::Auto, "file", n, fanout).await?;
    scenario(Io::Memory, "memory", n, fanout).await?;
    println!("THROUGHPUT-DONE");
    Ok(())
}
