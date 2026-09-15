//! Bounded receipts for the actor pump's ordered per-sender resource deliveries.
use anyhow::{Context, Result, ensure};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    seq: i64,
    idx: i64,
}

pub struct DriverReceipt {
    incarnation: String,
    position: Position,
}

/// The pump holds `pump:<sender>` and orders each destination by `(seq,idx)`.
/// Thus a resource needs one watermark per sender incarnation, not one entry
/// per message. A dropped resource drops these receipts; spawn receipts prevent
/// retrying old deliveries against a replacement resource.
pub struct DriverReceipts {
    sources: HashMap<String, Position>,
    max_sources: usize,
}
impl DriverReceipts {
    pub fn new(max_sources: usize) -> Self {
        Self { sources: HashMap::new(), max_sources }
    }
    /// Check source capacity before doing any external I/O. Exhaustion rejects
    /// a new sender without evicting receipts and risking duplicate writes.
    pub fn classify(&self, key: &str) -> Result<DriverReceipt> {
        let mut fields = key.rsplitn(3, ':');
        let idx: i64 = fields.next().context("delivery key missing idx")?.parse()?;
        let seq: i64 = fields.next().context("delivery key missing seq")?.parse()?;
        let incarnation = fields.next().context("delivery key missing sender incarnation")?;
        ensure!(!incarnation.is_empty() && seq >= 0 && idx >= 0, "invalid delivery key");
        ensure!(
            self.sources.contains_key(incarnation) || self.sources.len() < self.max_sources,
            "driver receipt sender limit {} reached",
            self.max_sources
        );
        Ok(DriverReceipt { incarnation: incarnation.into(), position: Position { seq, idx } })
    }
    pub fn contains(&self, receipt: &DriverReceipt) -> bool {
        self.sources.get(&receipt.incarnation).is_some_and(|position| *position >= receipt.position)
    }
    /// Call only after I/O succeeds and before acknowledging the delivery.
    pub fn commit(&mut self, receipt: DriverReceipt) {
        self.sources
            .entry(receipt.incarnation)
            .and_modify(|position| *position = (*position).max(receipt.position))
            .or_insert(receipt.position);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordered_retries_keep_bounded_receipts_and_reject_new_sources_at_limit() {
        let mut receipts = DriverReceipts::new(2);
        for seq in 0..10_000 {
            let receipt = receipts.classify(&format!("actor:{seq}:2")).unwrap();
            assert!(!receipts.contains(&receipt));
            receipts.commit(receipt);
        }
        assert_eq!(receipts.sources.len(), 1);
        assert!(receipts.contains(&receipts.classify("actor:1:2").unwrap()));
        let other = receipts.classify("actor@1:0:1").unwrap();
        assert!(!receipts.contains(&other));
        receipts.commit(other);
        assert!(receipts.classify("third:0:1").is_err());
        assert!(receipts.classify("malformed").is_err());
        assert!(receipts.classify("actor:-1:2").is_err());
    }
}
