use crate::identity::{Driver, dependency_crate_name, stage_dependency_items};
use std::collections::BTreeMap;
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
            "toolchain": "test compiler",
            "items": {"main": {"hash": hash, "refs": [], "cycle": null}},
            "entry": {"main": hash},
            "exports": {"main": hash}
        }))
        .unwrap();
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
    fn document(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.directory.join("items.json")).unwrap()).unwrap()
    }
    fn write_document(&self, document: &serde_json::Value) {
        std::fs::write(
            self.directory.join("items.json"),
            serde_json::to_vec(document).unwrap(),
        )
        .unwrap();
    }
    /// Add a verified preimage and return its hash.
    fn preimage(&self, bytes: &[u8]) -> String {
        let hash = blake3::hash(bytes).to_hex().to_string();
        std::fs::write(self.directory.join("item-preimages").join(&hash), bytes).unwrap();
        hash
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
    assert_eq!(
        identity.behavior_hash,
        blake3::hash(&loom_proto::export_identity_preimage(&BTreeMap::from([(
            "main".into(),
            fixture.hash.clone()
        )])))
        .to_hex()
        .as_str()
    );
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
    let item_hash = fixture.preimage(&preimage);
    let cycles = fixture.directory.join("item-preimages/cycles");
    std::fs::create_dir_all(&cycles).unwrap();
    std::fs::write(cycles.join(cycle.to_hex().as_str()), b"corrupt cycle").unwrap();
    fixture.write_document(&serde_json::json!({
        "toolchain": "test compiler",
        "items": {"main": {"hash": item_hash, "refs": ["main"], "cycle": ["main"]}},
        "entry": {"main": item_hash},
        "exports": {"main": item_hash}
    }));
    assert!(
        fixture
            .ingest()
            .unwrap_err()
            .to_string()
            .contains("corrupt item preimage")
    );
}

/// The identity is the Merkle root over exports, so a nested public item that
/// is not an entry still moves the definition hash when its content changes.
#[tokio::test]
async fn every_export_contributes_to_definition_identity() {
    let fixture = Fixture::new().await;
    let mut document = fixture.document();
    document["exports"]["shapes::largest"] = serde_json::json!(fixture.hash);
    document["items"]["shapes::largest"] = document["items"]["main"].clone();
    fixture.write_document(&document);
    let before = fixture.ingest().unwrap();
    let changed_hash = fixture.preimage(b"changed nested export");
    document["exports"]["shapes::largest"] = serde_json::json!(changed_hash);
    document["items"]["shapes::largest"]["hash"] = serde_json::json!(changed_hash);
    fixture.write_document(&document);
    let after = fixture.ingest().unwrap();
    assert_ne!(before.behavior_hash, after.behavior_hash);
    assert_eq!(
        after.behavior_hash,
        blake3::hash(&loom_proto::export_identity_preimage(&BTreeMap::from([
            ("main".into(), fixture.hash.clone()),
            ("shapes::largest".into(), changed_hash),
        ])))
        .to_hex()
        .as_str()
    );
}

/// Entries drive the ABI, not the identity: an unchanged export set yields the
/// same definition hash even though the entry table lost a name.
#[tokio::test]
async fn identity_is_rooted_in_exports_not_entries() {
    let mut fixture = Fixture::new().await;
    // The checker saw both root functions; the compiler's entry table must
    // agree in both directions, so `second` is declared on the checked side.
    let mut second = fixture.definition.sig.exports[0].clone();
    second.name = "second".into();
    fixture.definition.sig.exports.push(second);
    let mut document = fixture.document();
    let other = fixture.preimage(b"second root function");
    document["items"]["second"] = serde_json::json!({"hash": other, "refs": [], "cycle": null});
    document["exports"]["second"] = serde_json::json!(other);
    document["entry"]["second"] = serde_json::json!(other);
    fixture.write_document(&document);
    let with_entry = fixture.ingest().unwrap();
    // Both sides drop the entry: the compiler's table and the checker's list
    // must agree, and the export set is what the identity is rooted in.
    document["entry"].as_object_mut().unwrap().remove("second");
    fixture.definition.sig.exports.pop();
    fixture.write_document(&document);
    let without_entry = fixture.ingest().unwrap();
    assert_eq!(with_entry.behavior_hash, without_entry.behavior_hash);
}

/// A document from the entry-rooted driver has no `exports`. It is refused by
/// field name; no root is recomputed from `entry`.
#[tokio::test]
async fn rejects_document_without_exports() {
    let fixture = Fixture::new().await;
    let mut document = fixture.document();
    document.as_object_mut().unwrap().remove("exports");
    fixture.write_document(&document);
    let error = fixture.ingest().unwrap_err().to_string();
    assert!(error.contains("exports"), "{error}");
    assert!(error.contains("items.json"), "{error}");
}

#[tokio::test]
async fn rejects_entry_missing_from_exports() {
    let fixture = Fixture::new().await;
    let mut document = fixture.document();
    document["exports"] = serde_json::json!({});
    fixture.write_document(&document);
    let error = fixture.ingest().unwrap_err().to_string();
    assert!(
        error.contains("entry main is not among the exports"),
        "{error}"
    );
}

#[tokio::test]
async fn rejects_export_disagreeing_with_item_table() {
    let fixture = Fixture::new().await;
    let mut document = fixture.document();
    let other = fixture.preimage(b"unlisted export");
    document["exports"]["helper"] = serde_json::json!(other);
    fixture.write_document(&document);
    let error = fixture.ingest().unwrap_err().to_string();
    assert!(
        error.contains("export helper disagrees with item table"),
        "{error}"
    );
}

/// Publish a definition with a stored item document, as `loom-api` does after
/// a build, so a dependent's build can stage it for the driver.
fn publish_dependency(store: &loom_store::Store, document: &serde_json::Value) -> String {
    let exports: BTreeMap<String, String> =
        serde_json::from_value(document["exports"].clone()).unwrap();
    let root = store
        .put(
            "export-root",
            &loom_proto::export_identity_preimage(&exports),
        )
        .unwrap();
    let identity = loom_proto::BuildIdentity {
        behavior_hash: root.clone(),
        wasm_hash: store.put("blob", b"wasm").unwrap(),
        toolchain_hash: "toolchain".into(),
        item_hashes_ref: store
            .put("item-hashes", &serde_json::to_vec(document).unwrap())
            .unwrap(),
    };
    let definition = loom_proto::Def {
        hash: root.clone(),
        lang: loom_proto::Lang::Rust,
        component_hash: None,
        sig: Default::default(),
        allowed_effects: None,
        observed_effects: Vec::new(),
    };
    store
        .define_with_identity(
            &definition,
            Some("shapes"),
            "pub fn ping() -> u32 { 1 }",
            &BTreeMap::new(),
            Some(&identity),
        )
        .unwrap();
    root
}

/// The checked dependency closure the builder hands to staging, keyed by hash.
fn closure_of(
    template: &loom_check::CheckedDef,
    hash: &str,
    name: &str,
) -> BTreeMap<String, loom_check::CheckedDef> {
    let mut member = template.clone();
    member.hash = hash.to_owned();
    member.name = name.to_owned();
    BTreeMap::from([(hash.to_owned(), member)])
}

/// The staged file is named by the dependency's rustc crate name and holds the
/// dependency's stored document byte for byte; `configure` points the driver
/// at that directory.
#[tokio::test]
async fn stages_dependency_item_documents_for_the_driver() {
    let mut fixture = Fixture::new().await;
    let item = blake3::hash(b"largest").to_hex().to_string();
    let document = serde_json::json!({
        "toolchain": "test compiler",
        "items": {"shapes::largest": {"hash": item, "refs": [], "cycle": null},
                  "ping": {"hash": item, "refs": [], "cycle": null}},
        "entry": {"ping": item},
        "exports": {"ping": item, "shapes::largest": item}
    });
    let dependency = publish_dependency(&fixture.store, &document);
    fixture.definition.deps = BTreeMap::from([("shapes".to_owned(), dependency.clone())]);
    let closure = closure_of(&fixture.definition, &dependency, "shapes");
    stage_dependency_items(&fixture.store, &closure, &fixture.directory).unwrap();
    let staged = fixture
        .directory
        .join("dependency-items")
        .join(format!("{}.json", dependency_crate_name(&dependency)));
    assert_eq!(
        std::fs::read(&staged).unwrap(),
        serde_json::to_vec(&document).unwrap()
    );
    let mut command = tokio::process::Command::new("unused");
    fixture.driver.configure(&mut command, &fixture.directory);
    let environment: BTreeMap<_, _> = command
        .as_std()
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.unwrap().to_string_lossy().into_owned(),
            )
        })
        .collect();
    assert_eq!(
        PathBuf::from(&environment["LOOM_DEP_ITEMS"]),
        fixture.directory.join("dependency-items")
    );
    assert_eq!(
        PathBuf::from(&environment["LOOM_ITEM_HASHES"]),
        fixture.directory.join("items.json")
    );
    assert_eq!(
        PathBuf::from(&environment["LOOM_ITEM_PREIMAGES"]),
        fixture.directory.join("item-preimages")
    );
    assert_eq!(environment["RUSTC"], "unused-driver");
}

/// Staging is rebuilt from the current dependency set: a document left by an
/// earlier build of the same definition does not survive.
#[tokio::test]
async fn restaging_removes_documents_of_dropped_dependencies() {
    let fixture = Fixture::new().await;
    let staged = fixture.directory.join("dependency-items");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(staged.join("loom_definition_0000000000000000.json"), b"{}").unwrap();
    stage_dependency_items(&fixture.store, &BTreeMap::new(), &fixture.directory).unwrap();
    assert_eq!(std::fs::read_dir(&staged).unwrap().count(), 0);
}

#[tokio::test]
async fn dependency_without_stored_identity_is_named() {
    let mut fixture = Fixture::new().await;
    let missing = "f".repeat(64);
    fixture.definition.deps = BTreeMap::from([("shapes".to_owned(), missing.clone())]);
    let closure = closure_of(&fixture.definition, &missing, "shapes");
    let error = stage_dependency_items(&fixture.store, &closure, &fixture.directory)
        .unwrap_err()
        .to_string();
    assert!(error.contains("shapes"), "{error}");
    assert!(error.contains(&missing), "{error}");
    assert!(error.contains("no stored build identity"), "{error}");
    assert!(!fixture.directory.join("dependency-items").exists());
}

/// A crate-root `pub fn` the compiler saw but the checker did not (macro
/// expansion) is refused by name, not silently accepted as an entry.
#[tokio::test]
async fn rejects_compiler_entry_the_checker_never_saw() {
    let fixture = Fixture::new().await;
    let mut document = fixture.document();
    let other = fixture.preimage(b"expanded root function");
    document["items"]["expanded"] = serde_json::json!({"hash": other, "refs": [], "cycle": null});
    document["exports"]["expanded"] = serde_json::json!(other);
    document["entry"]["expanded"] = serde_json::json!(other);
    fixture.write_document(&document);
    let error = fixture.ingest().unwrap_err().to_string();
    assert!(error.contains("expanded"), "{error}");
    assert!(error.contains("not an export the checker saw"), "{error}");
}
