//! Wall-clock attribution for one build. Every millisecond between the first
//! and last checkpoint belongs to exactly one named stage, so the stage sum is
//! the span by construction and `unattributed_ms` measures only the code that
//! runs outside every checkpoint chain (a few file writes after the log line).
use std::time::Instant;

/// The share of a build that may fall outside named stages before the build log
/// carries a `build_stages_warning` line. `scripts/bench/add-latency.sh` applies
/// the same bound to the API's `build.ms`.
pub(crate) const UNATTRIBUTED_LIMIT_PERCENT: u128 = 5;

pub(crate) struct Stages {
    started: Instant,
    last: Instant,
    /// Name and duration in milliseconds, in checkpoint order.
    entries: Vec<(&'static str, u128)>,
}

impl Stages {
    pub(crate) fn start() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last: now,
            entries: Vec::new(),
        }
    }

    /// Attribute everything since the previous checkpoint (or the start) to `name`.
    /// A name recorded twice is summed, so a loop can checkpoint per iteration.
    pub(crate) fn checkpoint(&mut self, name: &'static str) {
        let now = Instant::now();
        // Attribute whole milliseconds of the cumulative clock, so the stage
        // sum equals the total by construction instead of trailing it by up
        // to one millisecond per checkpoint.
        let previous = self.last.duration_since(self.started).as_millis();
        let current = now.duration_since(self.started).as_millis();
        self.last = now;
        self.add(name, current - previous);
    }

    /// Fold a nested chain in: its stages already sum to its own span, which sat
    /// inside the interval the next `checkpoint` of this chain would attribute.
    pub(crate) fn absorb(&mut self, nested: Stages) {
        for (name, elapsed) in nested.entries {
            self.add(name, elapsed);
        }
        // The nested span ended at `nested.last`; anything this chain measured
        // between its own `last` and that instant is the nested chain's setup
        // cost, which the nested chain has already named. Move the cursor so the
        // interval is not attributed twice.
        if nested.last > self.last {
            self.last = nested.last;
        }
    }

    fn add(&mut self, name: &'static str, elapsed: u128) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.0 == name) {
            entry.1 += elapsed;
        } else {
            self.entries.push((name, elapsed));
        }
    }

    pub(crate) fn sum_ms(&self) -> u128 {
        self.entries.iter().map(|entry| entry.1).sum()
    }

    pub(crate) fn total_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    /// One JSON line with every stage plus `unattributed_ms` (the span not
    /// covered by any checkpoint), and a second warning line only when that
    /// share exceeds [`UNATTRIBUTED_LIMIT_PERCENT`].
    pub(crate) fn log_lines(&self) -> String {
        let total = self.total_ms();
        let sum = self.sum_ms();
        let unattributed = total.saturating_sub(sum);
        let mut stages = serde_json::Map::new();
        for (name, elapsed) in &self.entries {
            stages.insert((*name).into(), serde_json::json!(*elapsed as u64));
        }
        stages.insert(
            "unattributed_ms".into(),
            serde_json::json!(unattributed as u64),
        );
        let mut lines = serde_json::json!({
            "build_stages": stages,
            "build_stages_total_ms": total as u64,
        })
        .to_string();
        if unattributed * 100 > total * UNATTRIBUTED_LIMIT_PERCENT {
            lines.push('\n');
            lines.push_str(
                &serde_json::json!({"build_stages_warning": format!(
                    "{unattributed} ms of {total} ms belong to no stage (limit {UNATTRIBUTED_LIMIT_PERCENT} percent)"
                )})
                .to_string(),
            );
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoints_partition_the_span_and_repeated_names_sum() {
        let mut stages = Stages::start();
        std::thread::sleep(std::time::Duration::from_millis(12));
        stages.checkpoint("first_ms");
        std::thread::sleep(std::time::Duration::from_millis(6));
        stages.checkpoint("second_ms");
        std::thread::sleep(std::time::Duration::from_millis(6));
        stages.checkpoint("second_ms");
        let first = stages.entries[0].1;
        let second = stages.entries[1].1;
        assert_eq!(stages.entries.len(), 2);
        assert!(first >= 12, "{first}");
        assert!(second >= 12, "{second}");
        // Everything measured is attributed: the sum trails the total only by
        // the time this assertion itself takes to reach `total_ms`.
        assert!(stages.total_ms() - stages.sum_ms() <= 2);
        let lines = stages.log_lines();
        let value: serde_json::Value = serde_json::from_str(lines.lines().next().unwrap()).unwrap();
        assert_eq!(
            value["build_stages"]["first_ms"].as_u64().unwrap(),
            first as u64
        );
        assert_eq!(
            value["build_stages"]["second_ms"].as_u64().unwrap(),
            second as u64
        );
        assert!(value["build_stages"]["unattributed_ms"].as_u64().unwrap() <= 2);
        assert!(!lines.contains("build_stages_warning"), "{lines}");
    }

    #[test]
    fn unattributed_time_above_the_limit_is_a_warning_line() {
        let mut stages = Stages::start();
        stages.checkpoint("only_ms");
        // Time passing after the last checkpoint is exactly the unattributed span.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let lines = stages.log_lines();
        let mut parsed = lines.lines().map(|line| {
            serde_json::from_str::<serde_json::Value>(line).expect("stage lines are JSON")
        });
        let stages_line = parsed.next().unwrap();
        assert!(
            stages_line["build_stages"]["unattributed_ms"]
                .as_u64()
                .unwrap()
                >= 20
        );
        let warning = parsed.next().expect("warning line");
        assert!(
            warning["build_stages_warning"]
                .as_str()
                .unwrap()
                .contains("belong to no stage")
        );
    }

    #[test]
    fn absorbed_chain_moves_the_cursor_so_nothing_is_counted_twice() {
        let mut outer = Stages::start();
        std::thread::sleep(std::time::Duration::from_millis(5));
        outer.checkpoint("before_ms");
        let mut inner = Stages::start();
        std::thread::sleep(std::time::Duration::from_millis(10));
        inner.checkpoint("inner_ms");
        outer.absorb(inner);
        outer.checkpoint("after_ms");
        let after = outer
            .entries
            .iter()
            .find(|entry| entry.0 == "after_ms")
            .unwrap()
            .1;
        assert!(after <= 2, "inner span attributed twice: after_ms={after}");
        assert!(outer.total_ms() - outer.sum_ms() <= 2);
    }
}
