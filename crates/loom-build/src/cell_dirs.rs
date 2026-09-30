//! Bounded on-disk footprint for interactive cells.
//!
//! Every build leaves a directory named by the definition hash, holding its
//! sources, `component.wasm`, `compiled.rs`, `build.log` and `component.inputs`:
//! about ten small files. A REPL makes a new one per cell, so 300 evals left
//! 4,700 files in 2,300 directories (3,100 under 4 KB), the small-file metadata
//! load that makes a Nix store slow on some file systems. A durable definition
//! keeps its directory; an interactive one keeps only its most recent
//! `KEEP`, which is what a repeated cell needs to reuse its build.
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Interactive cell directories kept.
pub const KEEP: usize = 64;

#[derive(Default)]
pub struct CellDirs {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    swept: bool,
    recent: VecDeque<PathBuf>,
}

impl CellDirs {
    /// Note that `directory` was just built or reused as an interactive cell,
    /// and remove the oldest directories beyond `KEEP`. The first call in a
    /// process also adopts the interactive directories an earlier run left.
    pub fn touch(&self, cache: &Path, directory: &Path) {
        let mut inner = self.inner.lock().expect("cell directory list poisoned");
        if !inner.swept {
            inner.swept = true;
            let mut found = left_by_earlier_runs(cache);
            found.sort_by_key(|(modified, _)| *modified);
            inner.recent.extend(found.into_iter().map(|(_, path)| path));
        }
        inner.recent.retain(|path| path != directory);
        inner.recent.push_back(directory.to_owned());
        while inner.recent.len() > KEEP {
            if let Some(oldest) = inner.recent.pop_front() {
                let _ = std::fs::remove_dir_all(oldest);
            }
        }
    }
}

/// Definition-hash directories whose recorded build inputs name the
/// interactive profile, with their modification times.
fn left_by_earlier_runs(cache: &Path) -> Vec<(std::time::SystemTime, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("component.inputs"))
                .is_ok_and(|inputs| inputs.ends_with(":interactive"))
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_most_recent_interactive_directories_stay() {
        let cache = tempfile::tempdir().unwrap();
        let cells = CellDirs::default();
        let make = |index: usize| {
            let directory = cache.path().join(format!("{index:064x}"));
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("component.inputs"), "x:interactive").unwrap();
            directory
        };
        let durable = cache.path().join("d".repeat(64));
        std::fs::create_dir_all(&durable).unwrap();
        std::fs::write(durable.join("component.inputs"), "x:standard").unwrap();

        // A build creates its directory, then reports it.
        let dirs: Vec<_> = (0..KEEP + 6)
            .map(|index| {
                let directory = make(index);
                cells.touch(cache.path(), &directory);
                directory
            })
            .collect();
        for directory in &dirs[..6] {
            assert!(!directory.exists(), "{}", directory.display());
        }
        for directory in &dirs[6..] {
            assert!(directory.exists());
        }
        assert!(durable.exists(), "a durable definition's directory is never pruned");

        // Reusing an old cell makes it the newest, so it outlives the next pruning.
        cells.touch(cache.path(), &dirs[6]);
        let extra = make(KEEP + 6);
        cells.touch(cache.path(), &extra);
        assert!(dirs[6].exists() && !dirs[7].exists());
    }

    #[test]
    fn a_new_process_adopts_and_prunes_what_an_earlier_one_left() {
        let cache = tempfile::tempdir().unwrap();
        let left: Vec<_> = (0..KEEP + 3)
            .map(|index| {
                let directory = cache.path().join(format!("{index:064x}"));
                std::fs::create_dir_all(&directory).unwrap();
                std::fs::write(directory.join("component.inputs"), "x:interactive").unwrap();
                std::thread::sleep(std::time::Duration::from_millis(2));
                directory
            })
            .collect();
        CellDirs::default().touch(cache.path(), &cache.path().join("f".repeat(64)));
        let alive = left.iter().filter(|directory| directory.exists()).count();
        assert_eq!(alive, KEEP - 1, "the newest are kept, room left for the current cell");
        assert!(!left[0].exists() && left[left.len() - 1].exists());
    }
}
