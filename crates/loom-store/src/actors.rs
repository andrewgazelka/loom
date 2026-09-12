use super::*;

impl Store {
    pub fn session(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .query_row("SELECT actor FROM sessions WHERE id=?", [id], |r| r.get(0))
            .optional()?)
    }
    pub fn create_session(&self, id: &str, actor: &str, owner: &str) -> Result<()> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        append(
            &tx,
            "system",
            &serde_json::json!({"type":"session_created","id":id,"actor":actor,"owner":owner}),
            0,
        )?;
        tx.execute(
            "INSERT INTO sessions VALUES (?,?,?)",
            params![id, actor, owner],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn who_runs(&self, behavior: &str) -> Result<Vec<Actor>> {
        Ok(self
            .actors()?
            .into_iter()
            .filter(|a| a.behavior_hash == behavior)
            .collect())
    }
    pub fn create_initialized_actor(&self, actor: &Actor, initial: &Value) -> Result<Actor> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let created_seq = append(
            &tx,
            "system",
            &serde_json::json!({"type":"actor_created","actor":actor}),
            0,
        )?;
        tx.execute(
            "INSERT INTO actors VALUES (?,?,?,?,?,?,?)",
            params![
                actor.id,
                actor.behavior_hash,
                actor.lang.as_str(),
                actor.component_hash,
                created_seq,
                created_seq,
                actor.parent
            ],
        )?;
        let last_seq = append(
            &tx,
            &actor.id,
            &serde_json::json!({"__loom_init":initial}),
            0,
        )?;
        tx.execute(
            "UPDATE actors SET last_seq=? WHERE id=?",
            params![last_seq, actor.id],
        )?;
        let state_hash = put_value(&tx, "state", initial)?;
        tx.execute(
            "INSERT INTO snapshots VALUES (?,?,?,?)",
            params![actor.id, actor.behavior_hash, last_seq, state_hash],
        )?;
        tx.commit()?;
        Ok(Actor {
            last_seq,
            created_seq,
            ..actor.clone()
        })
    }
    pub fn create_actor(&self, actor: &Actor) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let seq = append(
            &tx,
            "system",
            &serde_json::json!({"type":"actor_created","actor":actor}),
            0,
        )?;
        tx.execute(
            "INSERT INTO actors VALUES (?,?,?,?,?,?,?)",
            params![
                actor.id,
                actor.behavior_hash,
                actor.lang.as_str(),
                actor.component_hash,
                seq,
                seq,
                actor.parent
            ],
        )?;
        tx.commit()?;
        Ok(seq)
    }
    pub fn pin_machine_root(&self, actor: &str, state: &Value) -> Result<()> {
        self.recording.barrier(false)?;
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let behavior: String = tx
            .query_row(
                "SELECT behavior_hash FROM actors WHERE id=?",
                [actor],
                |row| row.get(0),
            )
            .optional()?
            .context("machine not found")?;
        ensure!(
            behavior == "loom:machine",
            "root pin requires machine actor"
        );
        let hash = put_value(&tx, "state", state)?;
        let seq = append(
            &tx,
            "system",
            &serde_json::json!({"type":"machine_root_pinned","actor":actor,"state_hash":hash}),
            0,
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO snapshots VALUES (?,?,?,?)",
            params![actor, behavior, seq, hash],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn actor(&self, id: &str) -> Result<Option<Actor>> {
        let connection = self.lock()?;
        let bytes:Option<String>=connection.query_row("SELECT json_object('id',id,'behavior_hash',behavior_hash,'lang',lang,'component_hash',component_hash,'last_seq',last_seq,'created_seq',created_seq,'parent',parent) FROM actors WHERE id=?",[id],|r|r.get(0)).optional()?;
        bytes
            .map(|b| serde_json::from_str(&b).map_err(Into::into))
            .transpose()
    }
    pub fn actors(&self) -> Result<Vec<Actor>> {
        let ids: Vec<String> = {
            let c = self.lock()?;
            let mut q = c.prepare("SELECT id FROM actors ORDER BY created_seq")?;
            q.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        ids.iter()
            .map(|id| self.actor(id)?.context("actor disappeared"))
            .collect()
    }
    pub fn update_actor(&self, actor: &Actor) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let seq = append(
            &tx,
            "system",
            &serde_json::json!({"type":"actor_updated","actor":actor}),
            0,
        )?;
        let changed = tx.execute(
            "UPDATE actors SET behavior_hash=?,lang=?,component_hash=?,last_seq=? WHERE id=?",
            params![
                actor.behavior_hash,
                actor.lang.as_str(),
                actor.component_hash,
                seq,
                actor.id
            ],
        )?;
        ensure!(changed == 1, "unknown actor: {}", actor.id);
        tx.commit()?;
        Ok(seq)
    }
    pub fn snapshot(&self, actor: &str, fold_hash: &str, seq: i64, state: &Value) -> Result<()> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let hash = put_value(&tx, "state", state)?;
        tx.execute(
            "INSERT OR REPLACE INTO snapshots VALUES (?,?,?,?)",
            params![actor, fold_hash, seq, hash],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn latest_snapshot(&self, actor: &str, fold_hash: &str) -> Result<Option<Snapshot>> {
        let c = self.lock()?;
        let mut q=c.prepare("SELECT s.seq,c.bytes FROM snapshots s JOIN cas c ON c.hash=s.state_hash WHERE s.actor=? AND s.fold_hash=? ORDER BY s.seq DESC LIMIT 1")?;
        let mut rows = q.query(params![actor, fold_hash])?;
        match rows.next()? {
            Some(r) => Ok(Some(Snapshot {
                seq: r.get(0)?,
                state: decode(&r.get::<_, Vec<u8>>(1)?)?,
            })),
            None => Ok(None),
        }
    }
}
