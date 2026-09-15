use super::*;

impl Node {
    pub(crate) async fn validate_inner(&self, id: &str, candidate: &str, k: i64) -> Result<Verdict> {
        Ok(self.validate_assertions(id, candidate, k, &[]).await?.verdict)
    }

    pub async fn validate_assertions(&self, id: &str, candidate: &str, k: i64, assertions: &[String]) -> Result<crate::ValidationResult> {
        let _admission = self.admit().await?;
        Ok(self.validation_record(id, candidate, k, assertions, memo::MemoConfig::default()).await?.result)
    }

    pub async fn validate_with_memo_config(
        &self,
        id: &str,
        candidate: &str,
        k: i64,
        assertions: &[String],
        config: memo::MemoConfig,
    ) -> Result<crate::ValidationResult> {
        let _admission = self.admit().await?;
        Ok(self.validation_record(id, candidate, k, assertions, config).await?.result)
    }

    async fn validation_record(
        &self,
        id: &str,
        candidate: &str,
        k: i64,
        assertions: &[String],
        config: memo::MemoConfig,
    ) -> Result<memo::Record> {
        i64::try_from(config.max_rows).context("validation_memo max_rows exceeds SQLite integer")?;
        let actor = self.open_actor(id).await?;
        let source = actor.conn.lock().await;
        let schema = Self::schema_fingerprint(&source).await?;
        self.snapshot_schema_change(&source, id, &schema).await?;
        let cursor = actor::cursor(&source).await?;
        ensure!(
            actor::query(&source, "SELECT value FROM meta WHERE key='schema_snapshot_pending'", ()).await?.rows.is_empty(),
            "actor {id} seq {cursor}: CDC validation requires a contiguous schema snapshot boundary"
        );
        ensure!(k >= 0 && k <= cursor, "actor {id} seq {cursor}: validation window outside history");
        let behavior = crate::view::behavior_on(&self.registry, &source, candidate).await?;
        let key = memo::key(&source, candidate, cursor - k, cursor, assertions).await?;
        if let Some(record) = memo::lookup(self, &key).await? {
            memo::store(self, &key, &record, config).await?;
            return Ok(record);
        }
        if let Some(verdict) = self.cdc_verdict(&source).await? {
            let record = memo::Record {
                key: key.clone(),
                result: crate::ValidationResult { verdict, assertions: Vec::new() },
                tables: table_hashes(&source).await?,
                outbox_hash: memo::outbox_hash(&source, cursor - k, cursor).await?,
            };
            memo::store(self, &key, &record, config).await?;
            return Ok(record);
        }
        let fork = self.fork_from(&source, id, cursor - k).await?;
        let mut conn = fork.conn.lock().await;
        let effects = ReplayEffects::load(&source).await?;
        // Revalidating the installed code must not invent an upgrade hook:
        // it would consume the next recorded startup/shutdown effect sequence.
        if actor::code(&conn).await?.hash != behavior.hash()
            && let Some(verdict) = promote_candidate(&mut conn, behavior.as_ref(), &effects).await?
        {
            let record = memo::Record {
                key: key.clone(),
                result: crate::ValidationResult { verdict, assertions: Vec::new() },
                tables: table_hashes(&conn).await?,
                outbox_hash: memo::outbox_hash(&conn, cursor - k, cursor).await?,
            };
            memo::store(self, &key, &record, config).await?;
            return Ok(record);
        }
        if let Some(verdict) = self.replay(&source, &mut conn, id, cursor, &effects, ReplayMode::Candidate).await? {
            let record = memo::Record {
                key: key.clone(),
                result: crate::ValidationResult { verdict, assertions: Vec::new() },
                tables: table_hashes(&conn).await?,
                outbox_hash: memo::outbox_hash(&conn, cursor - k, cursor).await?,
            };
            memo::store(self, &key, &record, config).await?;
            return Ok(record);
        }
        let original = table_hashes(&source).await?;
        let replayed = table_hashes(&conn).await?;
        let mut differences = Vec::new();
        for name in original.keys().chain(replayed.keys()).collect::<BTreeSet<_>>() {
            if original.get(name) != replayed.get(name) {
                differences.push(TableDifference {
                    name: name.clone(),
                    original_hash: original.get(name).cloned().unwrap_or_else(|| "absent".into()),
                    fork_hash: replayed.get(name).cloned().unwrap_or_else(|| "absent".into()),
                });
            }
        }
        let verdict = if differences.is_empty() {
            Verdict::Matched { tables: original.into_iter().map(|(name, hash)| TableHash { name, hash }).collect() }
        } else {
            Verdict::Differs { tables: differences }
        };
        let mut results = Vec::new();
        for query in assertions {
            let rows =
                actor::inspect_query(&conn, query, ()).await.with_context(|| format!("actor {id} seq {cursor}: validation assertion"))?;
            let passed = rows.rows.len() == 1
                && rows.columns.len() == 1
                && match rows.rows[0].get_value(0)? {
                    Value::Integer(value) => value != 0,
                    Value::Real(value) => value != 0.0,
                    _ => false,
                };
            results.push(crate::AssertionResult { query: query.clone(), passed });
        }
        let record = memo::Record {
            key: key.clone(),
            result: crate::ValidationResult { verdict, assertions: results },
            tables: replayed,
            outbox_hash: memo::outbox_hash(&conn, cursor - k, cursor).await?,
        };
        memo::store(self, &key, &record, config).await?;
        Ok(record)
    }

    /// Invariant 17 is an independent verdict source before candidate behavior replay.
    async fn cdc_verdict(&self, source: &Connection) -> Result<Option<Verdict>> {
        // The CDC base is the newest image whose schema is current: the last schema-change image when one
        // exists (meta cdc_base), otherwise the latest message-boundary snapshot.
        let base = actor::query(source, "SELECT value FROM meta WHERE key='cdc_base'", ()).await?;
        let path: String = match base.rows.first() {
            Some(row) => row.get(0)?,
            None => {
                let snapshot = actor::query(source, "SELECT path FROM snapshots ORDER BY seq DESC LIMIT 1", ()).await?;
                snapshot.rows.first().context("CDC validation requires a snapshot")?.get(0)?
            }
        };
        let mut target = self.snapshot_connection(&path).await?;
        actor::query(&target, "PRAGMA capture_data_changes_conn = 'off'", ()).await?;
        let boundary = actor::query(&target, "SELECT COALESCE(MAX(change_id),0) FROM turso_cdc", ()).await?;
        let after: i64 = boundary.rows.first().context("CDC snapshot boundary query returned no row")?.get(0)?;
        let tx = target.transaction().await?;
        crate::cdc::replay_domain_cdc(source, &tx, after).await?;
        tx.commit().await?;
        let original = table_hashes(source).await?;
        let replayed = table_hashes(&target).await?;
        let mut differences = Vec::new();
        for name in original.keys().chain(replayed.keys()).collect::<BTreeSet<_>>() {
            if original.get(name) != replayed.get(name) {
                differences.push(TableDifference {
                    name: name.clone(),
                    original_hash: original.get(name).cloned().unwrap_or_else(|| "absent".into()),
                    fork_hash: replayed.get(name).cloned().unwrap_or_else(|| "absent".into()),
                });
            }
        }
        Ok((!differences.is_empty()).then_some(Verdict::Differs { tables: differences }))
    }

    /// Validate and promote under the actor lock; the returned cutoff compares historical sends.
    pub async fn promote_report(&self, id: &str, hash: &str, k: i64) -> Result<memo::PromoteReport> {
        let _admission = self.admit().await?;
        let record = self.validation_record(id, hash, k, &[], memo::MemoConfig::default()).await?;
        let actor = self.open_actor(id).await?;
        let mut conn = actor.conn.lock().await;
        let end = actor::cursor(&conn).await?;
        // Validation released the lock. Refuse a moving history instead of reporting a stale cutoff.
        let key = memo::key(&conn, hash, end - k, end, &[]).await?;
        ensure!(key == record.key, "promote_report: history changed after validation; retry");
        let report = self.promotion_cutoff(&conn, record, end - k, end).await?;
        let behavior = crate::view::behavior_on(&self.registry, &conn, hash).await?;
        let schema = Self::schema_fingerprint(&conn).await?;
        actor::promote(
            &mut conn,
            behavior.as_ref(),
            "promote_report",
            "validated candidate",
            &crate::effects::RuntimeEffects { node: self, external: self.effects.as_ref() },
            Some(self),
        )
        .await?;
        self.snapshot_schema_change(&conn, id, &schema).await?;
        self.scheduling()?.index_dirty.insert(id.into());
        self.wake_actor(id)?;
        drop(conn);
        self.sync_index(id).await?;
        Ok(report)
    }

    async fn promotion_cutoff(&self, source: &Connection, record: memo::Record, at: i64, end: i64) -> Result<memo::PromoteReport> {
        let rows =
            actor::query(source, "SELECT DISTINCT target FROM outbox WHERE seq>? AND seq<=? ORDER BY target", turso::params![at, end])
                .await?;
        let mut receivers = Vec::new();
        for row in rows.rows {
            receivers.push(row.get(0)?);
        }
        Ok(memo::PromoteReport {
            downstream_unaffected: record.outbox_hash == memo::outbox_hash(source, at, end).await?,
            verdict: record.result.verdict,
            receivers,
        })
    }
}
