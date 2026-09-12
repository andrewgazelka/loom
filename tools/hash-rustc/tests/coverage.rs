use std::process::Command;

#[test]
fn audits_and_hashes_associated_types_and_pointer_instances() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("input.rs"),
        "pub trait First { type Args; fn first(x: Self::Args); }\n\
         pub trait Second { type Args; fn second(x: Self::Args); }\n\
         #[no_mangle] pub fn pointer(x: *mut u8) -> *mut u8 { x }\n\
         #[no_mangle] pub fn scalar(x: u64) -> u64 { x ^ 7 }",
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hash-rustc"));
    command
        .current_dir(root.path())
        .args(["input.rs", "--crate-type=rlib", "-Copt-level=2"])
        .env_remove("LOOM_OBJECT_CACHE")
        .env_remove("LOOM_ITEM_HASHES")
        .env_remove("LOOM_ITEM_PREIMAGES")
        .env("LOOM_ITEM_COVERAGE", root.path().join("coverage.json"));
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("coverage.json")).unwrap()).unwrap();
    assert_eq!(report["candidates"], 8);
    assert_eq!(report["refused"], 0);
    assert_eq!(report["encoded"], 8);
    assert!(report["reasons"].as_object().unwrap().is_empty());
    assert_eq!(report["mono"]["unique_items"], 2);
    assert_eq!(report["mono"]["refused_unique_items"], 0);
    assert_eq!(report["mono"]["hashes"].as_object().unwrap().len(), 2);
    // The audit and ordinary hashing must agree on this formerly refused surface.
    let output = command
        .env_remove("LOOM_ITEM_COVERAGE")
        .env("LOOM_ITEM_HASHES", root.path().join("hashes.json"))
        .env("LOOM_ITEM_PREIMAGES", root.path().join("preimages"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.path().join("hashes.json").exists());
}
