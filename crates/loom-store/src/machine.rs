use super::*;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MachineRoot {
    pub id: String,
    pub root: String,
    pub identity: Value,
}
impl Store {
    pub fn register_machine_root(&self, root: &MachineRoot) -> Result<()> {
        self.recording.barrier(false)?;
        self.lock()?.execute(
            "INSERT INTO machine_roots(id,root,identity) VALUES (?,?,?)",
            params![root.id, root.root, serde_json::to_string(&root.identity)?],
        )?;
        Ok(())
    }
    pub fn machine_root(&self, id: &str) -> Result<Option<MachineRoot>> {
        self.recording.barrier(false)?;
        let connection = self.lock()?;
        let mut query =
            connection.prepare("SELECT id,root,identity FROM machine_roots WHERE id=?")?;
        let mut rows = query.query([id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(MachineRoot {
            id: row.get(0)?,
            root: row.get(1)?,
            identity: serde_json::from_str(&row.get::<_, String>(2)?)?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roots_survive_reopen_and_cannot_be_replaced() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("store.db");
        {
            let store = Store::open(&path)?;
            let root = MachineRoot {
                id: "machine".into(),
                root: "/workspace".into(),
                identity: json!({"device": 1, "inode": 2}),
            };
            store.register_machine_root(&root)?;
            let replacement = MachineRoot {
                root: "/elsewhere".into(),
                ..root
            };
            assert!(store.register_machine_root(&replacement).is_err());
            store.flush()?;
        }
        let store = Store::open(path)?;
        let root = store.machine_root("machine")?.context("root missing")?;
        assert_eq!(root.root, "/workspace");
        assert_eq!(root.identity, json!({"device": 1, "inode": 2}));
        store.rebuild_views()?;
        assert_eq!(store.machine_root("machine")?.unwrap().root, root.root);
        assert!(store.machine_root("absent")?.is_none());
        Ok(())
    }
}
