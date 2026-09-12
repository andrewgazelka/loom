use super::*;

impl Store {
    pub fn enqueue(&self, actor: &str, msg: &Value) -> Result<PendingMessage> {
        self.enqueue_with_key(actor, msg, None)
    }
    pub fn enqueue_once(&self, actor: &str, msg: &Value, key: &str) -> Result<PendingMessage> {
        self.enqueue_with_key(actor, msg, Some(key))
    }
    fn enqueue_with_key(
        &self,
        actor: &str,
        msg: &Value,
        key: Option<&str>,
    ) -> Result<PendingMessage> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        if let Some(key) = key {
            let mut q=tx.prepare("SELECT k.actor,k.handler_seq,c.bytes FROM message_keys k JOIN cas c ON c.hash=k.msg_hash WHERE k.key=?")?;
            let mut rows = q.query([key])?;
            if let Some(r) = rows.next()? {
                let receipt = PendingMessage {
                    actor: r.get(0)?,
                    handler_seq: r.get(1)?,
                    msg: decode(&r.get::<_, Vec<u8>>(2)?)?,
                };
                ensure!(
                    receipt.actor == actor && receipt.msg == *msg,
                    "message idempotency key conflicts with original request"
                );
                return Ok(receipt);
            }
        }
        ensure!(
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM actors WHERE id=?)",
                [actor],
                |r| r.get::<_, bool>(0)
            )?,
            "unknown actor"
        );
        let seq = append(
            &tx,
            "system",
            &serde_json::json!({"type":"message_enqueued","actor":actor,"msg":msg,"key":key}),
            0,
        )?;
        tx.execute(
            "INSERT INTO inbox VALUES (?,?,?)",
            params![actor, seq, serde_json::to_string(msg)?],
        )?;
        if let Some(key) = key {
            let hash = put_value(&tx, "message", msg)?;
            tx.execute(
                "INSERT INTO message_keys VALUES (?,?,?,?)",
                params![key, actor, seq, hash],
            )?;
        }
        tx.commit()?;
        Ok(PendingMessage {
            actor: actor.into(),
            handler_seq: seq,
            msg: msg.clone(),
        })
    }
    pub fn pending(&self, actor: &str) -> Result<Option<PendingMessage>> {
        pending(&*self.lock()?, actor)
    }
    pub fn pending_messages(&self) -> Result<Vec<PendingMessage>> {
        let c = self.lock()?;
        let mut q = c.prepare("SELECT actor,handler_seq,msg FROM inbox ORDER BY handler_seq")?;
        let mut rows = q.query([])?;
        let mut messages = Vec::new();
        while let Some(r) = rows.next()? {
            messages.push(PendingMessage {
                actor: r.get(0)?,
                handler_seq: r.get(1)?,
                msg: serde_json::from_str(&r.get::<_, String>(2)?)?,
            });
        }
        Ok(messages)
    }
    pub fn message_pending(&self, actor: &str, handler_seq: i64) -> Result<bool> {
        Ok(self.lock()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM inbox WHERE actor=? AND handler_seq=?)",
            params![actor, handler_seq],
            |r| r.get(0),
        )?)
    }
    pub fn complete_message(&self, actor: &str, handler_seq: i64, events: &[Value]) -> Result<i64> {
        self.recording.barrier(false)?;
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let message = pending(&tx, actor)?.context("no pending message")?;
        ensure!(
            message.handler_seq == handler_seq,
            "pending handler sequence mismatch"
        );
        let mut seq: i64 =
            tx.query_row("SELECT last_seq FROM actors WHERE id=?", [actor], |r| {
                r.get(0)
            })?;
        for event in events {
            seq = append(&tx, actor, event, handler_seq)?;
        }
        tx.execute(
            "UPDATE actors SET last_seq=? WHERE id=?",
            params![seq, actor],
        )?;
        let completed = append(
            &tx,
            "system",
            &serde_json::json!({"type":"message_completed","actor":actor,"handler_seq":handler_seq}),
            handler_seq,
        )?;
        tx.execute(
            "DELETE FROM inbox WHERE actor=? AND handler_seq=?",
            params![actor, handler_seq],
        )?;
        tx.commit()?;
        Ok(completed)
    }
}
