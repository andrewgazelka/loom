use crate::identity::Driver;
use std::path::PathBuf;

struct Fixture {
    directory: PathBuf,
    store: loom_store::Store,
    definition: loom_check::CheckedDef,
    driver: Driver,
    hash: String,
}
impl Fixture {
    async fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "loom-identity-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(directory.join("item-preimages")).unwrap();
        let preimage = b"verified canonical item";
        let hash = blake3::hash(preimage).to_hex().to_string();
        std::fs::write(directory.join("item-preimages").join(&hash), preimage).unwrap();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "toolchain": "test compiler", "items": {"main": {"hash": hash, "refs": [], "cycle": null}}, "entry": {"main": hash}
        })).unwrap();
        std::fs::write(directory.join("items.json"), bytes).unwrap();
        // This test owns the ingestion boundary, not Rust entry discovery.
        let definition = loom_check::CheckedDef {
            hash: "compilation-input-key".into(),
            lang: loom_proto::Lang::Rust,
            name: "fixture".into(),
            source: "pub fn main() -> i32 { 42 }".into(),
            deps: Default::default(),
            sig: loom_proto::TypeSig {
                exports: vec![loom_proto::ExportSig {
                    name: "main".into(),
                    params: Vec::new(),
                    returns: loom_proto::ValueShape::Number,
                    effects: Default::default(),
                }],
                effects: Default::default(),
            },
            diagnostics: Vec::new(),
        };
        Self {
            directory,
            store: loom_store::Store::memory().unwrap(),
            definition,
            driver: Driver {
                path: "unused-driver".into(),
                toolchain_hash: "test-toolchain".into(),
            },
            hash,
        }
    }
    fn ingest(&self) -> Result<loom_proto::BuildIdentity, crate::BuildError> {
        self.driver
            .ingest(&self.store, &self.directory, &self.definition, b"wasm")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}
#[tokio::test]
async fn ingests_document_and_verified_preimage_under_distinct_hashes() {
    let fixture = Fixture::new().await;
    let identity = fixture.ingest().unwrap();
    assert_eq!(identity.behavior_hash, fixture.hash);
    assert_ne!(identity.item_hashes_ref, fixture.hash);
    assert_eq!(
        fixture.store.get(&fixture.hash).unwrap().unwrap(),
        b"verified canonical item"
    );
    assert_eq!(identity.wasm_hash, blake3::hash(b"wasm").to_hex().as_str());
}
#[tokio::test]
async fn rejects_corrupt_preimage() {
    let fixture = Fixture::new().await;
    std::fs::write(
        fixture.directory.join("item-preimages").join(&fixture.hash),
        b"corrupt",
    )
    .unwrap();
    assert!(
        fixture
            .ingest()
            .unwrap_err()
            .to_string()
            .contains("corrupt item preimage")
    );
}
#[tokio::test]
async fn rejects_missing_preimage_by_path() {
    let fixture = Fixture::new().await;
    std::fs::remove_file(fixture.directory.join("item-preimages").join(&fixture.hash)).unwrap();
    assert!(
        fixture
            .ingest()
            .unwrap_err()
            .to_string()
            .contains(&fixture.hash)
    );
}
#[tokio::test]
async fn rejects_missing_document_by_path() {
    let fixture = Fixture::new().await;
    std::fs::remove_file(fixture.directory.join("items.json")).unwrap();
    assert!(
        fixture
            .ingest()
            .unwrap_err()
            .to_string()
            .contains("items.json")
    );
}
#[tokio::test]
async fn missing_driver_names_manifest() {
    let fixture = Fixture::new().await;
    let error = Driver::prepare(&fixture.directory, &fixture.directory)
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("tools/hash-rustc/Cargo.toml"));
}

#[tokio::test]
async fn rejects_corrupt_cycle_object() {
    let fixture = Fixture::new().await;
    let cycle = blake3::hash(b"cycle preimage");
    let mut preimage = cycle.as_bytes().to_vec();
    preimage.extend_from_slice(&0_u64.to_le_bytes());
    let item_hash = blake3::hash(&preimage).to_hex().to_string();
    std::fs::write(
        fixture.directory.join("item-preimages").join(&item_hash),
        preimage,
    )
    .unwrap();
    let cycles = fixture.directory.join("item-preimages/cycles");
    std::fs::create_dir_all(&cycles).unwrap();
    std::fs::write(cycles.join(cycle.to_hex().as_str()), b"corrupt cycle").unwrap();
    std::fs::write(fixture.directory.join("items.json"), serde_json::to_vec(&serde_json::json!({
        "toolchain": "test compiler", "items": {"main": {"hash": item_hash, "refs": ["main"], "cycle": ["main"]}}, "entry": {"main": item_hash}
    })).unwrap()).unwrap();
    assert!(
        fixture
            .ingest()
            .unwrap_err()
            .to_string()
            .contains("corrupt item preimage")
    );
}
