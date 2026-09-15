//! Tenant-local CAS effects shared by ordinary calls and durable actor turns.
use super::*;
use serde::Deserialize;

/// Keeps the worst-case JSON byte-array wire comfortably below V8's 1 MiB turn limit.
pub const CAS_GUEST_MAX_BYTES: usize = 128 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bytes {
    bytes: Vec<u8>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    #[serde(rename = "$ref")]
    cid: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetBytes {
    reference: Reference,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetJson {
    hash: String,
}

impl Store {
    /// Content-addressed writes are idempotent even when an actor's outer SQL
    /// transaction aborts. The unused object may remain; no actor reference commits.
    pub fn guest_cas_effect(&self, operation: &str, args: Value) -> Result<Value> {
        match operation {
            "cas.put_bytes" => {
                let request: Bytes = serde_json::from_value(args)?;
                ensure!(
                    request.bytes.len() <= CAS_GUEST_MAX_BYTES,
                    "CAS bytes exceed guest limit"
                );
                let hash = self.put("blob", &request.bytes)?;
                self.reference(&hash, loom_proto::RAW_CODEC)
            }
            "cas.get_bytes" => {
                let request: GetBytes = serde_json::from_value(args)?;
                let bytes = self.guest_cas_bytes(&request.reference.cid, loom_proto::RAW_CODEC)?;
                Ok(serde_json::to_value(bytes)?)
            }
            "cas.put" => {
                ensure!(
                    serde_json::to_vec(&args)?.len() <= CAS_GUEST_MAX_BYTES,
                    "CAS JSON exceeds guest limit"
                );
                let hash = self.put_value("blob", &args)?;
                self.reference(&hash, loom_proto::DAG_CBOR_CODEC)
            }
            "cas.get" => {
                let request: GetJson = serde_json::from_value(args)?;
                self.guest_cas_bytes(&request.hash, loom_proto::DAG_CBOR_CODEC)?;
                let value: Value = self
                    .get_value(&request.hash)?
                    .context("CAS object not found in this tenant")?;
                ensure!(
                    serde_json::to_vec(&value)?.len() <= CAS_GUEST_MAX_BYTES,
                    "CAS JSON exceeds guest limit"
                );
                Ok(value)
            }
            _ => anyhow::bail!("unknown CAS effect {operation}"),
        }
    }

    fn guest_cas_bytes(&self, cid: &str, codec: u64) -> Result<Vec<u8>> {
        let selected = if codec == loom_proto::DAG_CBOR_CODEC
            && cid.len() == 64
            && cid.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            loom_proto::cid_for_hash(cid, codec).map_err(anyhow::Error::msg)?
        } else {
            cid.to_owned()
        };
        let address = loom_proto::parse_reference(&selected).map_err(anyhow::Error::msg)?;
        ensure!(address.codec == codec, "CAS reference has wrong codec");
        self.recording.barrier(false)?;
        let connection = self.lock()?;
        let length: Option<u64> = connection
            .query_row(
                "SELECT length(bytes) FROM cas WHERE hash=?",
                [&address.hash],
                |row| row.get(0),
            )
            .optional()?;
        let length = length.context("CAS object not found in this tenant")?;
        ensure!(
            length <= CAS_GUEST_MAX_BYTES as u64,
            "CAS object exceeds guest limit"
        );
        drop(connection);
        self.get(cid)?
            .context("CAS object not found in this tenant")
    }
}
