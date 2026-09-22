//! Durable optimistic sessions and atomic publication of a dependency-graph update.
use super::*;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct UpdateSession {
    pub id: String,
    pub revision: u64,
    pub state: Value,
}

impl Store {
    pub fn create_update_session(&self, state: &Value) -> Result<UpdateSession> {
        let id: String =
            self.lock()?
                .query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
        self.create_update_session_with_id(&id, state)
    }

    /// Claim a caller-selected id; an existing claim is rejected atomically.
    /// The caller must compare the stored original request before proceeding.
    pub fn create_update_session_with_id(&self, id: &str, state: &Value) -> Result<UpdateSession> {
        ensure!(
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "update request id must be 1..128 ASCII letters, digits, dashes or underscores"
        );
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO update_sessions(id,revision,state) VALUES (?,0,?)",
            params![id, serde_json::to_string(state)?],
        )?;
        let session = read_session(&tx, id)?.context("created update session disappeared")?;
        tx.commit()?;
        Ok(session)
    }

    pub fn update_session(&self, id: &str) -> Result<Option<UpdateSession>> {
        read_session(&*self.lock()?, id)
    }

    /// Advance only the version read by the caller; concurrent agents must reload.
    pub fn save_update_session(
        &self,
        id: &str,
        expected_revision: u64,
        state: &Value,
    ) -> Result<UpdateSession> {
        save_session(&*self.lock()?, id, expected_revision, state)
    }

    /// Publish a complete update and its final session state as one transaction.
    /// Names outside the update must still match the planned graph: newly named
    /// dependents also invalidate the plan. Snapshot names and effects are not imported.
    pub fn commit_update(
        &self,
        staged: &Store,
        publications: &[IntakePublication<'_>],
        expected_names: &BTreeMap<String, String>,
        session_id: &str,
        expected_revision: u64,
        final_state: &Value,
    ) -> Result<UpdateSession> {
        ensure!(
            !Arc::ptr_eq(&self.connection, &staged.connection),
            "update requires a separate staged store"
        );
        staged.recording.barrier(false)?;
        self.recording.barrier(false)?;
        let source = staged.lock()?;
        let mut destination = self.lock()?;
        let tx = destination.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            identity::current_names(&tx)? == *expected_names,
            "update conflict: current names changed; replan the update"
        );
        let session = read_session(&tx, session_id)?.context("update session not found")?;
        ensure!(
            session.revision == expected_revision,
            "update session conflict: expected revision {expected_revision}, found {}",
            session.revision
        );
        publish_staged(&source, &tx, publications)?;
        let session = save_session(&tx, session_id, expected_revision, final_state)?;
        tx.commit()?;
        Ok(session)
    }

    /// Publish an imported dependency graph as one transaction. The live names
    /// must still equal the snapshot the import was planned against, so a name
    /// bound meanwhile cannot be overwritten. Returns the latest sequence.
    pub fn commit_import(
        &self,
        staged: &Store,
        publications: &[IntakePublication<'_>],
        expected_names: &BTreeMap<String, String>,
    ) -> Result<i64> {
        ensure!(
            !Arc::ptr_eq(&self.connection, &staged.connection),
            "import requires a separate staged store"
        );
        staged.recording.barrier(false)?;
        self.recording.barrier(false)?;
        let source = staged.lock()?;
        let mut destination = self.lock()?;
        let tx = destination.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            identity::current_names(&tx)? == *expected_names,
            "import conflict: current names changed while the bundle was building; retry the import"
        );
        publish_staged(&source, &tx, publications)?;
        let seq: i64 = tx.query_row(
            "SELECT coalesce(max(seq),0) FROM definition_records",
            [],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(seq)
    }
}

/// Copy staged build objects, then write every publication with its build event.
fn publish_staged(
    source: &Connection,
    tx: &Connection,
    publications: &[IntakePublication<'_>],
) -> Result<()> {
    intake::import_build_objects(source, tx)?;
    for publication in publications {
        publication::write(
            tx,
            publication.def,
            publication.name,
            publication.source,
            publication.deps,
            publication.identity,
        )?;
        ensure!(
            publication.build_event["type"] == "component_built",
            "staged publication requires a component_built event"
        );
        record_definition_event(tx, publication.build_event)?;
    }
    Ok(())
}

fn read_session(connection: &Connection, id: &str) -> Result<Option<UpdateSession>> {
    let state: Option<String> = connection.query_row(
        "SELECT json_object('id',id,'revision',revision,'state',json(state)) FROM update_sessions WHERE id=?",
        [id], |row| row.get(0),
    ).optional()?;
    state
        .map(|state| serde_json::from_str(&state).map_err(Into::into))
        .transpose()
}

fn save_session(
    connection: &Connection,
    id: &str,
    expected_revision: u64,
    state: &Value,
) -> Result<UpdateSession> {
    let revision = expected_revision
        .checked_add(1)
        .context("update session revision overflow")?;
    let changed = connection.execute(
        "UPDATE update_sessions SET revision=?,state=? WHERE id=? AND revision=?",
        params![
            revision,
            serde_json::to_string(state)?,
            id,
            expected_revision
        ],
    )?;
    ensure!(
        changed == 1,
        "update session conflict: missing session or stale revision {expected_revision}"
    );
    Ok(UpdateSession {
        id: id.to_owned(),
        revision,
        state: state.clone(),
    })
}

#[cfg(test)]
mod tests;
