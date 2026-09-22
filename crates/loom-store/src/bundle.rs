//! Bundle blocks enter the CAS only after every block hashes to its CID.
use super::*;
use loom_proto::bundle::{Frame, decode_car};

/// The largest bundle a node accepts; matches the raw CAS upload limit.
pub const MAX_BUNDLE_BYTES: usize = 512 * 1024 * 1024;

/// A framed block whose bytes have been hashed against its CID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub cid: String,
    pub hash: String,
    pub codec: u64,
    pub bytes: Vec<u8>,
}

/// Header root plus every block in file order; the first block is the root.
#[derive(Debug, Clone)]
pub struct VerifiedBundle {
    pub root: String,
    pub blocks: Vec<Block>,
}

/// A block with the CAS kind it is stored under.
pub struct ImportBlock<'a> {
    pub block: &'a Block,
    pub kind: &'a str,
}

/// Parse a CARv1 bundle and prove every block: BLAKE3-256 CIDs only, raw or
/// DAG-CBOR codecs only, canonical DAG-CBOR, unique CIDs, and a first block
/// equal to the header root. Interpretation of the root is the caller's.
pub fn verify_bundle(bytes: &[u8]) -> Result<VerifiedBundle> {
    ensure!(
        bytes.len() <= MAX_BUNDLE_BYTES,
        "bundle exceeds {MAX_BUNDLE_BYTES} bytes"
    );
    let car = decode_car(bytes).map_err(anyhow::Error::msg)?;
    ensure!(
        car.roots.len() == 1,
        "bundle header must name exactly one root, found {}",
        car.roots.len()
    );
    let root = car.roots.into_iter().next().context("root disappeared")?;
    ensure!(!car.frames.is_empty(), "bundle has no blocks");
    let mut seen = std::collections::BTreeSet::new();
    let mut blocks = Vec::with_capacity(car.frames.len());
    for Frame { cid, bytes } in car.frames {
        let address = loom_proto::parse_reference(&cid).map_err(anyhow::Error::msg)?;
        ensure!(
            address.codec == loom_proto::RAW_CODEC || address.codec == loom_proto::DAG_CBOR_CODEC,
            "bundle block {cid} uses codec {:#x}; only raw and DAG-CBOR blocks are accepted",
            address.codec
        );
        let actual = blake3::hash(&bytes).to_hex().to_string();
        ensure!(
            actual == address.hash,
            "bundle block {cid} is corrupt: CID names BLAKE3 {} but the bytes hash to {actual}",
            address.hash
        );
        if address.codec == loom_proto::DAG_CBOR_CODEC {
            decode::<Value>(&bytes)
                .with_context(|| format!("bundle block {cid} is not canonical DAG-CBOR"))?;
        }
        ensure!(seen.insert(cid.clone()), "bundle block {cid} appears twice");
        blocks.push(Block {
            cid,
            hash: address.hash,
            codec: address.codec,
            bytes,
        });
    }
    ensure!(
        blocks[0].cid == root,
        "bundle root {root} must be the first block, found {}",
        blocks[0].cid
    );
    Ok(VerifiedBundle { root, blocks })
}

impl Store {
    /// Store verified blocks under their recorded kinds in one transaction.
    /// Existing objects keep their bytes and kind; the hash is re-proved here
    /// so a caller cannot insert bytes under a foreign CID.
    pub fn import_blocks(&self, blocks: &[ImportBlock<'_>]) -> Result<()> {
        self.recording.barrier(false)?;
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        for ImportBlock { block, kind } in blocks {
            ensure!(
                !kind.is_empty() && kind.len() <= 128,
                "bundle block {} kind must contain 1..=128 bytes",
                block.cid
            );
            ensure!(
                block.codec == loom_proto::DAG_CBOR_CODEC
                    || !matches!(
                        *kind,
                        "event" | "result" | "state" | "message" | "tree" | "desc"
                    ),
                "bundle block {} kind {kind} requires DAG-CBOR",
                block.cid
            );
            ensure!(
                blake3::hash(&block.bytes).to_hex().as_str() == block.hash,
                "bundle block {} bytes do not hash to {}",
                block.cid,
                block.hash
            );
            tx.execute(
                "INSERT OR IGNORE INTO cas(hash,kind,bytes,created_at,codec) VALUES (?,?,?,unixepoch(),?)",
                params![block.hash, *kind, block.bytes, block.codec],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO cas_codecs VALUES (?,?)",
                params![block.hash, block.codec],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Seed the Rust resolver cache with an imported overlay. A key this node
    /// already resolved keeps its own overlay; the overlay object must exist.
    pub fn set_preparation(&self, key: &str, overlay_hash: &str) -> Result<()> {
        ensure!(
            key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "preparation key must be a 64-character hexadecimal hash"
        );
        ensure!(
            self.codec(overlay_hash)? == Some(loom_proto::DAG_CBOR_CODEC),
            "preparation overlay {overlay_hash} is not a stored DAG-CBOR object"
        );
        self.recording.barrier(false)?;
        let connection = self.lock()?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_preparations (key TEXT PRIMARY KEY, overlay_hash TEXT NOT NULL)")?;
        connection.execute(
            "INSERT OR IGNORE INTO rust_preparations VALUES (?1,?2)",
            params![key, overlay_hash],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_proto::bundle::encode_car;
    use loom_proto::{DAG_CBOR_CODEC, RAW_CODEC, cid_for_hash};

    fn raw(bytes: &[u8]) -> Frame {
        Frame {
            cid: cid_for_hash(blake3::hash(bytes).to_hex().as_str(), RAW_CODEC).unwrap(),
            bytes: bytes.to_vec(),
        }
    }
    fn dag(value: &Value) -> Frame {
        let bytes = encode(value).unwrap();
        Frame {
            cid: cid_for_hash(blake3::hash(&bytes).to_hex().as_str(), DAG_CBOR_CODEC).unwrap(),
            bytes,
        }
    }

    #[test]
    fn verified_blocks_carry_hashes_and_codecs() -> Result<()> {
        let root = dag(&serde_json::json!({"loom_bundle":1}));
        let source = raw(b"pub fn main() {}");
        let bytes = encode_car(&root.cid, &[root.clone(), source.clone()]).unwrap();
        let bundle = verify_bundle(&bytes)?;
        assert_eq!(bundle.root, root.cid);
        assert_eq!(bundle.blocks.len(), 2);
        assert_eq!(bundle.blocks[1].codec, RAW_CODEC);
        assert_eq!(
            bundle.blocks[1].hash,
            blake3::hash(b"pub fn main() {}").to_hex().as_str()
        );
        Ok(())
    }

    #[test]
    fn corrupt_duplicate_and_misrooted_bundles_are_rejected() -> Result<()> {
        let root = dag(&serde_json::json!({"loom_bundle":1}));
        let source = raw(b"pub fn main() {}");
        let mut corrupt = source.clone();
        corrupt.bytes[0] ^= 0xff;
        let error = verify_bundle(&encode_car(&root.cid, &[root.clone(), corrupt]).unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains(&source.cid) && error.contains("corrupt"), "{error}");
        let error = verify_bundle(
            &encode_car(&root.cid, &[root.clone(), source.clone(), source.clone()]).unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("twice"), "{error}");
        let error = verify_bundle(&encode_car(&root.cid, &[source.clone(), root.clone()]).unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("first block"), "{error}");
        // A DAG-CBOR CID over bytes that are not canonical DAG-CBOR is refused.
        let mislabeled = Frame {
            cid: cid_for_hash(blake3::hash(b"text").to_hex().as_str(), DAG_CBOR_CODEC).unwrap(),
            bytes: b"text".to_vec(),
        };
        let error = verify_bundle(&encode_car(&root.cid, &[root.clone(), mislabeled]).unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("canonical DAG-CBOR"), "{error}");
        Ok(())
    }

    #[test]
    fn imported_blocks_keep_kinds_and_never_replace_existing_objects() -> Result<()> {
        let store = Store::memory()?;
        let existing = store.put("blob", b"shared bytes")?;
        let root = dag(&serde_json::json!({"loom_bundle":1}));
        let shared = raw(b"shared bytes");
        let fresh = raw(b"fresh bytes");
        let bundle = verify_bundle(
            &encode_car(&root.cid, &[root.clone(), shared.clone(), fresh.clone()]).unwrap(),
        )?;
        store.import_blocks(&[
            ImportBlock {
                block: &bundle.blocks[1],
                kind: "source_bundle",
            },
            ImportBlock {
                block: &bundle.blocks[2],
                kind: "item-preimage",
            },
        ])?;
        assert_eq!(store.cas_entry(&existing)?.unwrap().kind, "blob");
        let fresh_hash = blake3::hash(b"fresh bytes").to_hex().to_string();
        assert_eq!(store.cas_entry(&fresh_hash)?.unwrap().kind, "item-preimage");
        assert_eq!(store.get(&fresh_hash)?, Some(b"fresh bytes".to_vec()));
        assert_eq!(store.codec(&fresh_hash)?, Some(RAW_CODEC));
        let mut forged = bundle.blocks[2].clone();
        forged.bytes = b"other bytes".to_vec();
        assert!(
            store
                .import_blocks(&[ImportBlock {
                    block: &forged,
                    kind: "blob"
                }])
                .is_err()
        );
        assert!(
            store
                .import_blocks(&[ImportBlock {
                    block: &bundle.blocks[2],
                    kind: "tree"
                }])
                .is_err(),
            "raw bytes cannot claim a structured kind"
        );
        Ok(())
    }

    #[test]
    fn preparation_seed_needs_the_overlay_and_keeps_local_resolutions() -> Result<()> {
        let store = Store::memory()?;
        let key = "ab".repeat(32);
        let missing = "cd".repeat(32);
        assert!(store.set_preparation(&key, &missing).is_err());
        let overlay = store.put_value(
            "rust-prepared-dependencies",
            &serde_json::json!({"Cargo.lock":"version = 4"}),
        )?;
        store.set_preparation(&key, &overlay)?;
        let other = store.put_value(
            "rust-prepared-dependencies",
            &serde_json::json!({"Cargo.lock":"version = 5"}),
        )?;
        store.set_preparation(&key, &other)?;
        let stored: String = store.with_connection(|connection| {
            Ok(connection.query_row(
                "SELECT overlay_hash FROM rust_preparations WHERE key=?",
                [&key],
                |row| row.get(0),
            )?)
        })?;
        assert_eq!(stored, overlay);
        Ok(())
    }
}
