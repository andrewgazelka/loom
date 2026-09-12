use super::*;
use std::sync::Mutex;

#[derive(Clone, Default)]
pub(super) struct BuildProgress {
    active: Arc<Mutex<Option<ActiveBuild>>>,
}

struct ActiveBuild {
    name: String,
    stage: &'static str,
    started: Instant,
}

pub(super) struct BuildGuard {
    progress: BuildProgress,
}

impl BuildProgress {
    // The definition gate serializes callers for the lifetime of this guard.
    pub(super) fn start(&self, name: &str) -> BuildGuard {
        *self.active.lock().expect("build progress lock poisoned") = Some(ActiveBuild {
            name: name.into(),
            stage: "preflight",
            started: Instant::now(),
        });
        BuildGuard {
            progress: self.clone(),
        }
    }

    pub(super) fn snapshot(&self) -> Value {
        let active = self.active.lock().expect("build progress lock poisoned");
        json!({"active": active.as_ref().map(|build| json!({
            "name": build.name,
            "stage": build.stage,
            "elapsed_ms": u64::try_from(build.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }))})
    }
}

impl BuildGuard {
    pub(super) fn stage(&self, stage: &'static str) {
        if let Some(active) = self
            .progress
            .active
            .lock()
            .expect("build progress lock poisoned")
            .as_mut()
        {
            active.stage = stage;
        }
    }
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        *self
            .progress
            .active
            .lock()
            .expect("build progress lock poisoned") = None;
    }
}
