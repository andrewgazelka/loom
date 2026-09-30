//! What the root calls of this service have cost, for a report that wants to say what Loom did and saved.
//!
//! Monotonic counters per verb (`run`, `run_many`, `eval`), served by the `stats` command under `calls`:
//! take two snapshots and subtract for an interval. They count requests, not cache entries; the result
//! cache's own hit and miss counts and the time it saved are under `call_results` in the same reply.
use super::*;
use std::{collections::BTreeMap, future::Future, sync::Mutex};

#[derive(Default, Clone, Copy)]
struct VerbStats {
    requests: u64,
    failed: u64,
    wall_ns: u128,
    slowest_ns: u64,
    /// `run_many`: calls in the batches, and how they were answered.
    batch_calls: u64,
    batch_hits: u64,
    batch_misses: u64,
    batch_failures: u64,
}

#[derive(Clone)]
pub(crate) struct CallLedger(Arc<Mutex<(Instant, BTreeMap<&'static str, VerbStats>)>>);

impl Default for CallLedger {
    fn default() -> Self {
        Self(Arc::new(Mutex::new((Instant::now(), BTreeMap::new()))))
    }
}

impl CallLedger {
    fn record(&self, verb: &'static str, took: Duration, result: &Result<Value>) {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let stats = guard.1.entry(verb).or_default();
        stats.requests += 1;
        stats.wall_ns += took.as_nanos();
        stats.slowest_ns = stats.slowest_ns.max(u64::try_from(took.as_nanos()).unwrap_or(u64::MAX));
        match result {
            Err(_) => stats.failed += 1,
            Ok(value) if verb == "run_many" => {
                let count = |key: &str| value[key].as_u64().unwrap_or(0);
                stats.batch_hits += count("hits");
                stats.batch_misses += count("misses");
                stats.batch_failures += count("failures");
                stats.batch_calls += count("hits") + count("misses") + count("failures");
            }
            Ok(_) => {}
        }
    }

    pub(crate) fn snapshot(&self) -> Value {
        let guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let verbs: serde_json::Map<String, Value> = guard
            .1
            .iter()
            .map(|(verb, s)| {
                let mut row = json!({
                    "requests": s.requests,
                    "failed": s.failed,
                    "wall_ms_total": s.wall_ns as f64 / 1e6,
                    "wall_ms_mean": if s.requests == 0 { 0.0 } else { s.wall_ns as f64 / 1e6 / s.requests as f64 },
                    "wall_ms_slowest": s.slowest_ns as f64 / 1e6,
                });
                if *verb == "run_many" {
                    row["calls"] = json!(s.batch_calls);
                    row["cache_hits"] = json!(s.batch_hits);
                    row["cache_misses"] = json!(s.batch_misses);
                    row["call_failures"] = json!(s.batch_failures);
                }
                (verb.to_string(), row)
            })
            .collect();
        json!({"since_seconds": guard.0.elapsed().as_secs_f64(), "verbs": verbs})
    }
}

impl Service {
    /// Run `work` for verb `verb` and record how long it took and whether it failed.
    pub(crate) async fn recorded(
        &self,
        verb: &'static str,
        work: impl Future<Output = Result<Value>>,
    ) -> Result<Value> {
        let started = Instant::now();
        let result = work.await;
        self.ledger.record(verb, started.elapsed(), &result);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stats_reports_what_the_root_calls_cost_and_how_batches_were_answered() {
        let service = Service::new(
            Store::memory().unwrap(),
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            vec![],
        )
        .unwrap();
        let command = |command: &str, args: Value| loom_proto::CommandRequest {
            session: None,
            command: command.into(),
            args,
        };
        let before = service.command(command("stats", json!({}))).await;
        assert!(before.ok, "{before:?}");
        assert_eq!(before.result["calls"]["verbs"], json!({}), "nothing has run yet");
        let batch = json!({"calls": [{"target": "nope"}, {"target": "also-nope"}]});
        assert!(service.command(command("run_many", batch)).await.ok);
        let failed = service.command(command("run", json!({"target": "nope"}))).await;
        assert!(!failed.ok);
        let after = service.command(command("stats", json!({}))).await;
        let verbs = &after.result["calls"]["verbs"];
        assert_eq!(verbs["run_many"]["requests"], 1);
        assert_eq!(verbs["run_many"]["calls"], 2);
        assert_eq!(verbs["run_many"]["call_failures"], 2);
        assert_eq!(verbs["run_many"]["cache_hits"], 0);
        assert_eq!(verbs["run"]["requests"], 1);
        assert_eq!(verbs["run"]["failed"], 1);
        assert!(verbs["run"]["wall_ms_total"].as_f64().unwrap() >= 0.0);
        assert!(after.result["calls"]["since_seconds"].as_f64().unwrap() >= 0.0);
    }
}
