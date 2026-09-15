use super::*;

impl Store {
    pub fn put(&self, kind: &str, bytes: &[u8]) -> Result<String> {
        self.recording.barrier(false)?;
        put(&*self.lock()?, kind, bytes)
    }
    pub fn get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        self.recording.barrier(false)?;
        let address = if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            None
        } else {
            Some(loom_proto::parse_reference(hash).map_err(anyhow::Error::msg)?)
        };
        let hash = address.as_ref().map_or(hash, |a| a.hash.as_str());
        let c = self.lock()?;
        if let Some(address) = &address {
            let exists: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM cas WHERE hash=?)",
                [hash],
                |r| r.get(0),
            )?;
            let registered: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM cas_codecs WHERE hash=? AND codec=?)",
                params![hash, address.codec],
                |r| r.get(0),
            )?;
            ensure!(
                !exists || registered,
                "CID codec is not registered for stored object"
            );
        }
        if let Some(bytes) = c
            .query_row("SELECT bytes FROM cas WHERE hash=?", [hash], |r| r.get(0))
            .optional()?
        {
            let bytes: Vec<u8> = bytes;
            ensure!(
                blake3::hash(&bytes).to_hex().as_str() == hash,
                "CAS content hash mismatch"
            );
            return Ok(Some(bytes));
        }
        Ok(None)
    }
    pub fn put_value<T: serde::Serialize>(&self, kind: &str, value: &T) -> Result<String> {
        self.recording.barrier(false)?;
        put_value(&*self.lock()?, kind, value)
    }
    pub fn get_value<T: serde::de::DeserializeOwned>(&self, hash: &str) -> Result<Option<T>> {
        // A typed lookup selects DAG-CBOR for internal hex identities, while an
        // explicit CID must retain its caller-selected codec.
        let dag = if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            Some(loom_proto::cid_for_hash(hash, 113).map_err(anyhow::Error::msg)?)
        } else {
            None
        };
        let hash = dag.as_deref().unwrap_or(hash);
        ensure!(
            self.codec(hash)?.is_none_or(|codec| codec == 113),
            "CAS object is raw bytes, not DAG-CBOR"
        );
        let Some(bytes) = self.get(hash)? else {
            return Ok(None);
        };
        let address = loom_proto::parse_reference(hash).map_err(anyhow::Error::msg)?;
        let kind: Option<String> = self
            .lock()?
            .query_row(
                "SELECT kind FROM cas WHERE hash=?",
                [&address.hash],
                |row| row.get(0),
            )
            .optional()?;
        if kind.as_deref() == Some("trace") {
            let trace = loom_proto::decode_call_trace(&bytes).map_err(anyhow::Error::msg)?;
            return Ok(Some(serde_json::from_value(serde_json::to_value(trace)?)?));
        }
        Ok(Some(decode(&bytes)?))
    }
    pub fn codec(&self, hash: &str) -> Result<Option<u64>> {
        self.recording.barrier(false)?;
        let address = if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            None
        } else {
            Some(loom_proto::parse_reference(hash).map_err(anyhow::Error::msg)?)
        };
        let hash = address.as_ref().map_or(hash, |a| a.hash.as_str());
        let c = self.lock()?;
        let codec: Option<u64> = c
            .query_row("SELECT codec FROM cas WHERE hash=?", params![hash], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(address) = &address {
            if codec.is_none() {
                return Ok(None);
            }
            let registered: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM cas_codecs WHERE hash=? AND codec=?)",
                params![hash, address.codec],
                |r| r.get(0),
            )?;
            ensure!(registered, "CID codec is not registered for stored object");
            return Ok(Some(address.codec));
        }
        Ok(codec)
    }
    pub fn reference(&self, hash: &str, codec: u64) -> Result<Value> {
        let hash = if hash.len() == 64 {
            hash.to_owned()
        } else {
            loom_proto::parse_reference(hash)
                .map_err(anyhow::Error::msg)?
                .hash
        };
        let reference = loom_proto::reference(&hash, codec).map_err(anyhow::Error::msg)?;
        self.codec(
            reference["$ref"]
                .as_str()
                .context("invalid generated reference")?,
        )?
        .context("CAS object not found")?;
        Ok(reference)
    }
}
