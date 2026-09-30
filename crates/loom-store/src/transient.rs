//! Definitions that exist only in memory.
//!
//! `eval` compiles a throwaway cell per call. Publishing each one durably (a
//! definition row, a source bundle, a 500 KB component, the build log, the
//! compiled text) grew the store by about 0.6 MB a cell with nothing to collect
//! it, made every later `add`/`update` copy that growth (`stage_intake` backs the
//! whole database up), and collided with real definitions of the same hash: a
//! definition is immutable, while the same source built at another optimization
//! level or under another effect policy is a different component. A transient
//! definition is a `Def` and its component bytes held in a bounded in-memory set.
//! Execution finds it after the durable store misses, so a definition that was
//! added always wins, and it is gone on restart or when older cells are dropped.
use super::*;
use std::collections::{HashMap, VecDeque};

/// Cells kept before the oldest are dropped.
const CELLS: usize = 64;
/// Component bytes kept before the oldest cells are dropped.
const BYTES: usize = 256 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct Transient {
    cells: Mutex<Cells>,
}

#[derive(Default)]
struct Cells {
    order: VecDeque<String>,
    by_hash: HashMap<String, Cell>,
    bytes: usize,
}

struct Cell {
    def: Def,
    component_hash: String,
    component: Arc<Vec<u8>>,
}

impl Transient {
    fn lock(&self) -> MutexGuard<'_, Cells> {
        self.cells.lock().expect("transient definitions poisoned")
    }
    pub(crate) fn definition(&self, hash: &str) -> Option<Def> {
        self.lock().by_hash.get(hash).map(|cell| cell.def.clone())
    }
    pub(crate) fn blob(&self, hash: &str) -> Option<Vec<u8>> {
        self.lock()
            .by_hash
            .values()
            .find(|cell| cell.component_hash == hash)
            .map(|cell| cell.component.as_ref().clone())
    }
}

impl Store {
    /// Make `def` executable from memory only. `def.component_hash` must be the
    /// content hash of `component`. A later install of the same definition hash
    /// replaces the earlier one, since nothing durable refers to it.
    pub fn install_transient(&self, def: Def, component: Vec<u8>) -> Result<()> {
        let component_hash = content_hash(&component);
        ensure!(
            def.component_hash.as_deref() == Some(component_hash.as_str()),
            "transient definition {} names a component that is not the bytes given",
            def.hash
        );
        let mut cells = self.transient.lock();
        if let Some(old) = cells.by_hash.remove(&def.hash) {
            cells.bytes -= old.component.len();
            cells.order.retain(|hash| *hash != def.hash);
        }
        cells.bytes += component.len();
        cells.order.push_back(def.hash.clone());
        cells.by_hash.insert(
            def.hash.clone(),
            Cell {
                def,
                component_hash,
                component: Arc::new(component),
            },
        );
        while cells.order.len() > CELLS || cells.bytes > BYTES {
            let Some(oldest) = cells.order.pop_front() else {
                break;
            };
            if let Some(old) = cells.by_hash.remove(&oldest) {
                cells.bytes -= old.component.len();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(hash: &str, component: &[u8]) -> (Def, Vec<u8>) {
        let def = Def {
            hash: hash.into(),
            lang: loom_proto::Lang::Rust,
            component_hash: Some(content_hash(component)),
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        };
        (def, component.to_vec())
    }

    #[test]
    fn a_transient_definition_executes_and_serves_its_component_from_memory() -> Result<()> {
        let store = Store::memory()?;
        let (def, bytes) = cell("a", b"module one");
        let component = def.component_hash.clone().unwrap();
        assert!(store.executable_definition("a")?.is_none());
        store.install_transient(def, bytes)?;
        assert_eq!(store.executable_definition("a")?.unwrap().hash, "a");
        assert_eq!(store.definition("a")?.unwrap().hash, "a");
        assert_eq!(store.resolve("a")?.unwrap().hash, "a");
        assert_eq!(store.get(&component)?.unwrap(), b"module one");
        // Nothing durable: the definition table and the CAS never saw it.
        let durable: i64 = store.with_connection(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM defs WHERE hash='a'", [], |row| {
                row.get(0)
            })?)
        })?;
        assert_eq!(durable, 0);
        Ok(())
    }

    #[test]
    fn installing_again_replaces_and_a_durable_definition_wins() -> Result<()> {
        let store = Store::memory()?;
        let hash = "ab".repeat(32);
        let (first, first_bytes) = cell(&hash, b"optimized");
        store.install_transient(first, first_bytes)?;
        let (second, second_bytes) = cell(&hash, b"interactive");
        let second_component = second.component_hash.clone().unwrap();
        store.install_transient(second, second_bytes)?;
        assert_eq!(
            store.executable_definition(&hash)?.unwrap().component_hash,
            Some(second_component)
        );
        let (durable, durable_bytes) = cell(&hash, b"durable module");
        let durable_component = store.put("component", &durable_bytes)?;
        assert_eq!(Some(&durable_component), durable.component_hash.as_ref());
        store.define(&durable, Some("kept"), "source", &BTreeMap::new())?;
        assert_eq!(
            store.executable_definition(&hash)?.unwrap().component_hash,
            Some(durable_component),
            "a definition that was added is what runs"
        );
        Ok(())
    }

    #[test]
    fn a_mismatched_component_is_refused_and_old_cells_are_dropped() -> Result<()> {
        let store = Store::memory()?;
        let (def, _) = cell("a", b"one");
        assert!(store.install_transient(def, b"other bytes".to_vec()).is_err());
        for index in 0..(CELLS + 5) {
            let (def, bytes) = cell(&format!("cell-{index}"), format!("module {index}").as_bytes());
            store.install_transient(def, bytes)?;
        }
        assert!(store.executable_definition("cell-0")?.is_none());
        assert!(store.executable_definition(&format!("cell-{}", CELLS + 4))?.is_some());
        Ok(())
    }
}
