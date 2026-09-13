use super::*;
use std::sync::{Mutex, atomic::AtomicU64};

#[derive(Clone, Default)]
pub(super) struct BuildProgress {
    active: Arc<Mutex<BTreeMap<u64, ActiveBuild>>>,
    next_id: Arc<AtomicU64>,
}

struct ActiveBuild {
    name: String,
    stage: &'static str,
    started: Instant,
}

pub(super) struct BuildGuard {
    progress: BuildProgress,
    id: u64,
}

impl BuildProgress {
    pub(super) fn start(&self, name: &str) -> BuildGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.active
            .lock()
            .expect("build progress lock poisoned")
            .insert(
                id,
                ActiveBuild {
                    name: name.into(),
                    stage: "preflight",
                    started: Instant::now(),
                },
            );
        BuildGuard {
            progress: self.clone(),
            id,
        }
    }

    pub(super) fn snapshot(&self) -> Value {
        let active = self.active.lock().expect("build progress lock poisoned");
        let builds: Vec<Value> = active.values().map(|build| json!({
            "name":build.name, "stage":build.stage,
            "elapsed_ms":u64::try_from(build.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })).collect();
        json!({"active":builds.first()})
    }
}

impl BuildGuard {
    pub(super) fn stage(&self, stage: &'static str) {
        if let Some(active) = self
            .progress
            .active
            .lock()
            .expect("build progress lock poisoned")
            .get_mut(&self.id)
        {
            active.stage = stage;
        }
    }
}
impl Drop for BuildGuard {
    fn drop(&mut self) {
        self.progress
            .active
            .lock()
            .expect("build progress lock poisoned")
            .remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_build_guards_only_change_their_own_progress() {
        let progress = BuildProgress::default();
        let first = progress.start("first");
        let second = progress.start("second");
        second.stage("compile");
        assert_eq!(progress.snapshot()["active"]["stage"], "preflight");
        drop(first);
        assert_eq!(progress.snapshot()["active"]["name"], "second");
        assert_eq!(progress.snapshot()["active"]["stage"], "compile");
        drop(second);
        assert!(progress.snapshot()["active"].is_null());
    }
}
